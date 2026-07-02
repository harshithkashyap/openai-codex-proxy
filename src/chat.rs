use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode};
use bytes::Bytes;
use chrono::Utc;
use futures_util::{Stream, StreamExt};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde_json::{Value, json};

use crate::config::{CODEX_RESPONSES_URL, DEFAULT_MODEL, ORIGINATOR, UNSUPPORTED_CHAT_FIELDS};
use crate::errors::{ProxyError, response_json};
use crate::logging::{
    append_compat_log, error_summary, sample_strings, tool_choice_summary, value_kind,
};
use crate::server::AppState;
use crate::service_tier::{normalize_service_tier, service_tier_for_model};

pub(crate) async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    match chat_completions_impl(&state, &headers, body).await {
        Ok(resp) => resp,
        Err(err) => {
            append_compat_log(
                "chat.error",
                json!({
                    "error_type": err.error_type(),
                    "message": err.to_string(),
                    "status": err.status().as_u16(),
                }),
            );
            err.into_response()
        }
    }
}

pub(crate) async fn chat_completions_impl(
    state: &AppState,
    _headers: &HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, ProxyError> {
    let input: Value = match serde_json::from_slice(&body) {
        Ok(input) => input,
        Err(err) => {
            append_compat_log(
                "chat.parse_failed",
                json!({
                    "body_bytes": body.len(),
                    "error": err.to_string(),
                }),
            );
            return Err(ProxyError::bad_request(format!(
                "invalid JSON request body: {err}"
            )));
        }
    };
    append_compat_log("chat.request", chat_request_summary(&input, body.len()));
    let stream = input
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (model, mut responses_body) =
        build_responses_body_from_chat_with_service_tier(&input, state.service_tier.as_deref())?;
    if stream {
        responses_body["stream"] = Value::Bool(true);
        append_compat_log(
            "chat.translated",
            responses_request_summary(&responses_body),
        );
        return stream_chat_completions(state, model, responses_body).await;
    }
    append_compat_log(
        "chat.translated",
        responses_request_summary(&responses_body),
    );

    let (access_token, account_id) = state
        .auth
        .access_for_request()
        .await
        .map_err(ProxyError::auth)?;
    let upstream = state
        .client
        .post(CODEX_RESPONSES_URL)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header("ChatGPT-Account-ID", account_id)
        .header(CONTENT_TYPE, "application/json")
        .header(
            USER_AGENT,
            format!("openai-codex-proxy/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("originator", ORIGINATOR)
        .json(&responses_body)
        .send()
        .await
        .map_err(ProxyError::upstream)?;

    let status = upstream.status();
    append_compat_log(
        "chat.upstream",
        json!({
            "stream": false,
            "status": status.as_u16(),
        }),
    );
    let value: Value = upstream.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        return Ok(response_json(status, value));
    }

    let (content, tool_calls) = extract_chat_message_from_responses(&value);
    let finish_reason = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let mut message = json!({
        "role": "assistant",
        "content": if content.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(content)
        },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    Ok(response_json(
        StatusCode::OK,
        json!({
            "id": value.get("id").and_then(Value::as_str).unwrap_or("chatcmpl-proxy"),
            "object": "chat.completion",
            "created": Utc::now().timestamp(),
            "model": model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason
            }]
        }),
    ))
}

pub(crate) async fn stream_chat_completions(
    state: &AppState,
    model: String,
    responses_body: Value,
) -> Result<Response<Body>, ProxyError> {
    let (access_token, account_id) = state
        .auth
        .access_for_request()
        .await
        .map_err(ProxyError::auth)?;
    let upstream = state
        .client
        .post(CODEX_RESPONSES_URL)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header("ChatGPT-Account-ID", account_id)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "text/event-stream")
        .header(
            USER_AGENT,
            format!("openai-codex-proxy/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("originator", ORIGINATOR)
        .json(&responses_body)
        .send()
        .await
        .map_err(ProxyError::upstream)?;

    let status = upstream.status();
    append_compat_log(
        "chat.upstream",
        json!({
            "stream": true,
            "status": status.as_u16(),
        }),
    );
    if !status.is_success() {
        let value: Value = upstream.json().await.unwrap_or_else(|_| json!({}));
        return Ok(response_json(status, value));
    }

    let upstream_stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
        Box::pin(upstream.bytes_stream());
    let mut translator = ChatCompletionSseTranslator::new(
        format!("chatcmpl-proxy-{}", Utc::now().timestamp_millis()),
        model,
        Utc::now().timestamp(),
    );
    let pending = VecDeque::from(translator.initial_frames());
    let stream = futures_util::stream::unfold(
        (upstream_stream, translator, pending, false),
        |mut state| async move {
            loop {
                if let Some(item) = state.2.pop_front() {
                    return Some((item, state));
                }
                if state.3 {
                    return None;
                }

                match state.0.as_mut().next().await {
                    Some(Ok(chunk)) => {
                        state.2.extend(state.1.feed(&chunk));
                    }
                    Some(Err(err)) => {
                        state.2.push_back(Err(std::io::Error::other(err)));
                        state.3 = true;
                    }
                    None => {
                        state.2.extend(state.1.finish());
                        state.3 = true;
                    }
                }
            }
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
        .map_err(ProxyError::upstream)
}

pub(crate) struct ChatCompletionSseTranslator {
    chat_id: String,
    model: String,
    created: i64,
    buffer: Vec<u8>,
    role_sent: bool,
    finished: bool,
    saw_tool_call: bool,
    tool_call_indexes: HashMap<String, usize>,
    stream_summary: ChatStreamSummary,
}

#[derive(Default)]
pub(crate) struct ChatStreamSummary {
    text_delta_events: usize,
    text_delta_chars: usize,
    tool_call_count: usize,
    tool_argument_chars: usize,
    tool_names: Vec<String>,
}

impl ChatCompletionSseTranslator {
    pub(crate) fn new(chat_id: String, model: String, created: i64) -> Self {
        Self {
            chat_id,
            model,
            created,
            buffer: Vec::new(),
            role_sent: false,
            finished: false,
            saw_tool_call: false,
            tool_call_indexes: HashMap::new(),
            stream_summary: ChatStreamSummary::default(),
        }
    }

    pub(crate) fn initial_frames(&mut self) -> Vec<Result<Bytes, std::io::Error>> {
        if self.role_sent {
            return Vec::new();
        }
        self.role_sent = true;
        vec![Ok(self.chat_sse_chunk(json!({ "role": "assistant" }), None))]
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<Result<Bytes, std::io::Error>> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, separator_len)) = find_sse_frame_end(&self.buffer) {
            let frame = self.buffer.drain(..end + separator_len).collect::<Vec<_>>();
            let frame = &frame[..end];
            match std::str::from_utf8(frame) {
                Ok(frame) => self.translate_frame(frame, &mut out),
                Err(err) => out.push(Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    err,
                ))),
            }
        }
        out
    }

    pub(crate) fn finish(&mut self) -> Vec<Result<Bytes, std::io::Error>> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let frame = std::mem::take(&mut self.buffer);
            match std::str::from_utf8(&frame) {
                Ok(frame) => self.translate_frame(frame, &mut out),
                Err(err) => out.push(Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    err,
                ))),
            }
        }
        self.finish_with_reason("stop", &mut out);
        out
    }

    pub(crate) fn translate_frame(
        &mut self,
        frame: &str,
        out: &mut Vec<Result<Bytes, std::io::Error>>,
    ) {
        if self.finished {
            return;
        }

        let Some((event_type, data)) = parse_sse_frame(frame) else {
            return;
        };
        if data.trim() == "[DONE]" {
            self.finish_with_reason("stop", out);
            return;
        }

        let value = match serde_json::from_str::<Value>(&data) {
            Ok(value) => value,
            Err(err) => {
                out.push(Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    err,
                )));
                return;
            }
        };
        let event_type = event_type
            .as_deref()
            .or_else(|| value.get("type").and_then(Value::as_str));

        match event_type {
            Some("response.output_text.delta") => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    self.stream_summary.text_delta_events += 1;
                    self.stream_summary.text_delta_chars += delta.chars().count();
                    out.push(Ok(self.chat_sse_chunk(json!({ "content": delta }), None)));
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item") {
                    self.translate_function_call_item(item, out);
                }
            }
            Some("response.completed") => {
                let reason = if self.saw_tool_call {
                    "tool_calls"
                } else {
                    "stop"
                };
                self.finish_with_reason(reason, out);
            }
            Some("response.incomplete") => {
                self.finish_with_reason("length", out);
            }
            Some("response.failed" | "error") => {
                let error = value.get("error").cloned().unwrap_or_else(|| value.clone());
                append_compat_log(
                    "chat.stream.error",
                    json!({
                        "chat_id": self.chat_id,
                        "upstream_event": event_type,
                        "error": error_summary(&error),
                    }),
                );
                out.push(Ok(sse_data(&json!({ "error": error }))));
                self.finish_with_reason("stop", out);
            }
            _ => {}
        }
    }

    pub(crate) fn translate_function_call_item(
        &mut self,
        item: &Value,
        out: &mut Vec<Result<Bytes, std::io::Error>>,
    ) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }

        let Some(tool_call) =
            response_function_call_to_chat_tool_call(item, self.tool_call_indexes.len())
        else {
            return;
        };
        let tool_call_id = tool_call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let index = if let Some(index) = self.tool_call_indexes.get(&tool_call_id) {
            *index
        } else {
            let index = self.tool_call_indexes.len();
            self.tool_call_indexes.insert(tool_call_id, index);
            index
        };

        self.saw_tool_call = true;
        let function = tool_call
            .get("function")
            .cloned()
            .unwrap_or_else(|| json!({}));
        self.stream_summary.tool_call_count += 1;
        if let Some(name) = function.get("name").and_then(Value::as_str) {
            self.stream_summary.tool_names.push(name.to_string());
        }
        self.stream_summary.tool_argument_chars += function
            .get("arguments")
            .and_then(Value::as_str)
            .map(|arguments| arguments.chars().count())
            .unwrap_or_default();
        out.push(Ok(self.chat_sse_chunk(
            json!({
                "tool_calls": [{
                    "index": index,
                    "id": tool_call.get("id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": function,
                }]
            }),
            None,
        )));
    }

    pub(crate) fn finish_with_reason(
        &mut self,
        reason: &str,
        out: &mut Vec<Result<Bytes, std::io::Error>>,
    ) {
        if self.finished {
            return;
        }
        self.finished = true;
        append_compat_log(
            "chat.stream.summary",
            json!({
                "chat_id": self.chat_id,
                "finish_reason": reason,
                "text_delta_events": self.stream_summary.text_delta_events,
                "text_delta_chars": self.stream_summary.text_delta_chars,
                "tool_calls": self.stream_summary.tool_call_count,
                "tool_argument_chars": self.stream_summary.tool_argument_chars,
                "tool_names": sample_strings(&self.stream_summary.tool_names, 12),
            }),
        );
        out.push(Ok(self.chat_sse_chunk(json!({}), Some(reason))));
        out.push(Ok(Bytes::from_static(b"data: [DONE]\n\n")));
    }

    pub(crate) fn chat_sse_chunk(&self, delta: Value, finish_reason: Option<&str>) -> Bytes {
        sse_data(&json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]
        }))
    }
}

pub(crate) fn find_sse_frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    for (index, window) in buffer.windows(2).enumerate() {
        if window == b"\n\n" {
            return Some((index, 2));
        }
    }
    for (index, window) in buffer.windows(4).enumerate() {
        if window == b"\r\n\r\n" {
            return Some((index, 4));
        }
    }
    None
}

pub(crate) fn parse_sse_frame(frame: &str) -> Option<(Option<String>, String)> {
    let mut event_type = None;
    let mut data = Vec::new();
    for raw_line in frame.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = Some(rest.trim_start().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    if data.is_empty() {
        None
    } else {
        Some((event_type, data.join("\n")))
    }
}

pub(crate) fn sse_data(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

#[cfg(test)]
pub(crate) fn build_responses_body_from_chat(input: &Value) -> Result<(String, Value), ProxyError> {
    build_responses_body_from_chat_with_service_tier(input, None)
}

pub(crate) fn build_responses_body_from_chat_with_service_tier(
    input: &Value,
    default_service_tier: Option<&str>,
) -> Result<(String, Value), ProxyError> {
    reject_unsupported_chat_fields(input)?;

    let model = input
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let messages = input
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProxyError::bad_request("messages must be an array"))?;

    let mut instructions = Vec::new();
    let mut response_input = Vec::new();
    for msg in messages {
        append_chat_message_as_responses_input(msg, &mut instructions, &mut response_input)?;
    }

    let mut responses_body = json!({
        "model": model,
        "instructions": if instructions.is_empty() { Value::Null } else { Value::String(instructions.join("\n\n")) },
        "input": response_input,
        "stream": false,
        "store": false,
    });
    apply_chat_reasoning_and_text_options(input, &mut responses_body)?;
    apply_chat_tool_options(input, &mut responses_body)?;
    apply_chat_service_tier_option(input, &mut responses_body, default_service_tier)?;
    Ok((model, responses_body))
}

pub(crate) fn append_chat_message_as_responses_input(
    msg: &Value,
    instructions: &mut Vec<String>,
    response_input: &mut Vec<Value>,
) -> Result<(), ProxyError> {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
    match role {
        "system" | "developer" => {
            let content = chat_message_content_as_text(msg)?;
            if !content.is_empty() {
                instructions.push(content);
            }
        }
        "tool" => {
            let call_id = msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProxyError::bad_request("tool messages must include tool_call_id")
                })?;
            response_input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": chat_message_content_as_text(msg)?,
            }));
        }
        "assistant" => {
            if let Some(content) = chat_message_content_as_responses_content_or_empty(msg)? {
                response_input.push(json!({"role": role, "content": content}));
            }
            if let Some(tool_calls) = msg.get("tool_calls") {
                let tool_calls = tool_calls.as_array().ok_or_else(|| {
                    ProxyError::bad_request("assistant tool_calls must be an array")
                })?;
                for tool_call in tool_calls {
                    response_input.push(chat_tool_call_to_response_function_call(tool_call)?);
                }
            }
        }
        _ => {
            response_input.push(
                json!({"role": role, "content": chat_message_content_as_responses_content(msg)?}),
            );
        }
    }
    Ok(())
}

pub(crate) fn chat_message_content_as_text(msg: &Value) -> Result<String, ProxyError> {
    match msg.get("content") {
        Some(Value::Null) | None => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => chat_content_parts_as_text(parts),
        Some(_) => Err(ProxyError::bad_request(
            "chat message content must be a string, null, or array of content parts",
        )),
    }
}

pub(crate) fn chat_message_content_as_responses_content(msg: &Value) -> Result<Value, ProxyError> {
    match msg.get("content") {
        Some(Value::Null) | None => Ok(Value::String(String::new())),
        Some(Value::String(text)) => Ok(Value::String(text.clone())),
        Some(Value::Array(parts)) => Ok(Value::Array(chat_content_parts_to_responses(parts)?)),
        Some(_) => Err(ProxyError::bad_request(
            "chat message content must be a string, null, or array of content parts",
        )),
    }
}

pub(crate) fn chat_message_content_as_responses_content_or_empty(
    msg: &Value,
) -> Result<Option<Value>, ProxyError> {
    match chat_message_content_as_responses_content(msg)? {
        Value::String(text) if text.is_empty() => Ok(None),
        Value::Array(parts) if parts.is_empty() => Ok(None),
        content => Ok(Some(content)),
    }
}

pub(crate) fn chat_content_parts_as_text(parts: &[Value]) -> Result<String, ProxyError> {
    let mut text = Vec::new();
    for part in parts {
        if let Some(part_text) = chat_content_part_text(part)? {
            text.push(part_text);
        }
    }
    Ok(text.join("\n"))
}

pub(crate) fn chat_content_parts_to_responses(parts: &[Value]) -> Result<Vec<Value>, ProxyError> {
    let mut converted = Vec::new();
    for part in parts {
        if let Some(part_text) = chat_content_part_text(part)? {
            converted.push(json!({
                "type": "input_text",
                "text": part_text,
            }));
            continue;
        }
        if let Some(image) = chat_content_part_image(part)? {
            converted.push(image);
            continue;
        }
        let part_type = part
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(ProxyError::bad_request(format!(
            "unsupported chat message content part type: {part_type}"
        )));
    }
    Ok(converted)
}

pub(crate) fn chat_content_part_text(part: &Value) -> Result<Option<String>, ProxyError> {
    let part = part
        .as_object()
        .ok_or_else(|| ProxyError::bad_request("chat message content parts must be objects"))?;
    let part_type = part.get("type").and_then(Value::as_str);
    match part_type {
        Some("text" | "input_text") | None => {
            Ok(part.get("text").and_then(Value::as_str).map(str::to_string))
        }
        Some("refusal") => Ok(part
            .get("refusal")
            .and_then(Value::as_str)
            .map(str::to_string)),
        Some("image_url" | "input_image" | "image") => Ok(None),
        Some(_) => Ok(part.get("text").and_then(Value::as_str).map(str::to_string)),
    }
}

pub(crate) fn chat_content_part_image(part: &Value) -> Result<Option<Value>, ProxyError> {
    let part = part
        .as_object()
        .ok_or_else(|| ProxyError::bad_request("chat message content parts must be objects"))?;
    let part_type = part.get("type").and_then(Value::as_str);
    if !matches!(part_type, Some("image_url" | "input_image" | "image")) {
        return Ok(None);
    }

    let image_url_value = part
        .get("image_url")
        .or_else(|| part.get("url"))
        .or_else(|| part.get("image"));
    let mut converted = serde_json::Map::new();
    converted.insert("type".to_string(), Value::String("input_image".to_string()));

    match image_url_value {
        Some(Value::String(url)) => {
            converted.insert("image_url".to_string(), Value::String(url.clone()));
        }
        Some(Value::Object(image_url)) => {
            let url = image_url
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProxyError::bad_request("image_url content parts must include image_url.url")
                })?;
            converted.insert("image_url".to_string(), Value::String(url.to_string()));
            if let Some(detail) = image_url.get("detail").or_else(|| part.get("detail")) {
                converted.insert("detail".to_string(), detail.clone());
            }
        }
        _ => {
            if let Some(file_id) = part.get("file_id").and_then(Value::as_str) {
                converted.insert("file_id".to_string(), Value::String(file_id.to_string()));
            } else {
                return Err(ProxyError::bad_request(
                    "image content parts must include image_url, url, image, or file_id",
                ));
            }
        }
    }

    if let Some(detail) = part.get("detail")
        && !converted.contains_key("detail")
    {
        converted.insert("detail".to_string(), detail.clone());
    }

    Ok(Some(Value::Object(converted)))
}

pub(crate) fn chat_tool_call_to_response_function_call(
    tool_call: &Value,
) -> Result<Value, ProxyError> {
    let tool_type = tool_call
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    let name = chat_tool_call_name(tool_call).ok_or_else(|| {
        ProxyError::bad_request(format!(
            "unsupported {tool_type} tool call shape; tool call name must be present"
        ))
    })?;
    let arguments = chat_tool_call_arguments(tool_call);
    let call_id = tool_call
        .get("id")
        .or_else(|| tool_call.get("call_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| ProxyError::bad_request("tool call id or call_id must be a string"))?;

    Ok(json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    }))
}

pub(crate) fn chat_tool_call_name(tool_call: &Value) -> Option<&str> {
    tool_call
        .get("function")
        .and_then(|function| function.get("name"))
        .or_else(|| {
            tool_call
                .get("custom")
                .and_then(|custom| custom.get("name"))
        })
        .or_else(|| tool_call.get("name"))
        .and_then(Value::as_str)
}

pub(crate) fn chat_tool_call_arguments(tool_call: &Value) -> String {
    let arguments = tool_call
        .get("function")
        .and_then(|function| function.get("arguments"))
        .or_else(|| {
            tool_call
                .get("custom")
                .and_then(|custom| custom.get("arguments"))
        })
        .or_else(|| {
            tool_call
                .get("custom")
                .and_then(|custom| custom.get("input"))
        })
        .or_else(|| tool_call.get("arguments"))
        .or_else(|| tool_call.get("input"));

    match arguments {
        Some(Value::String(arguments)) => arguments.clone(),
        Some(Value::Null) | None => String::new(),
        Some(arguments) => arguments.to_string(),
    }
}

pub(crate) fn apply_chat_reasoning_and_text_options(
    input: &Value,
    responses_body: &mut Value,
) -> Result<(), ProxyError> {
    if let Some(reasoning) = input.get("reasoning") {
        if !reasoning.is_object() {
            return Err(ProxyError::bad_request("reasoning must be an object"));
        }
        responses_body["reasoning"] = reasoning.clone();
    }

    if let Some(text) = input.get("text") {
        if !text.is_object() {
            return Err(ProxyError::bad_request("text must be an object"));
        }
        responses_body["text"] = text.clone();
    }

    if let Some(effort) = first_string_field(input, &["reasoning_effort", "reasoningEffort"])? {
        set_nested_object_field(
            responses_body,
            "reasoning",
            "effort",
            Value::String(effort.to_string()),
        )?;
    }

    if let Some(summary) = first_string_field(input, &["reasoning_summary", "reasoningSummary"])? {
        set_nested_object_field(
            responses_body,
            "reasoning",
            "summary",
            Value::String(summary.to_string()),
        )?;
    }

    if let Some(verbosity) =
        first_string_field(input, &["verbosity", "text_verbosity", "textVerbosity"])?
    {
        set_nested_object_field(
            responses_body,
            "text",
            "verbosity",
            Value::String(verbosity.to_string()),
        )?;
    }

    Ok(())
}

pub(crate) fn apply_chat_tool_options(
    input: &Value,
    responses_body: &mut Value,
) -> Result<(), ProxyError> {
    if let Some(tools) = input.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| ProxyError::bad_request("tools must be an array"))?;
        let converted = tools
            .iter()
            .map(chat_tool_to_responses_tool)
            .collect::<Result<Vec<_>, _>>()?;
        responses_body["tools"] = Value::Array(converted);
    }

    if let Some(tool_choice) = input.get("tool_choice") {
        responses_body["tool_choice"] = chat_tool_choice_to_responses_tool_choice(tool_choice)?;
    }

    if let Some(parallel_tool_calls) = input.get("parallel_tool_calls") {
        let parallel_tool_calls = parallel_tool_calls
            .as_bool()
            .ok_or_else(|| ProxyError::bad_request("parallel_tool_calls must be a boolean"))?;
        responses_body["parallel_tool_calls"] = Value::Bool(parallel_tool_calls);
    }

    Ok(())
}

pub(crate) fn apply_chat_service_tier_option(
    input: &Value,
    responses_body: &mut Value,
    default_service_tier: Option<&str>,
) -> Result<(), ProxyError> {
    let service_tier = match input.get("service_tier") {
        Some(Value::String(service_tier)) => normalize_service_tier(service_tier),
        Some(Value::Null) => None,
        Some(_) => {
            return Err(ProxyError::bad_request(
                "service_tier must be a string or null",
            ));
        }
        None => default_service_tier.and_then(normalize_service_tier),
    };

    let Some(service_tier) = service_tier else {
        responses_body
            .as_object_mut()
            .expect("responses body should be an object")
            .remove("service_tier");
        return Ok(());
    };

    let model = responses_body.get("model").and_then(Value::as_str);
    if let Some(service_tier) = service_tier_for_model(model, &service_tier) {
        responses_body["service_tier"] = Value::String(service_tier);
    }
    Ok(())
}

pub(crate) fn chat_tool_to_responses_tool(tool: &Value) -> Result<Value, ProxyError> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return Err(ProxyError::bad_request(
            "only function tools are supported by this /v1/chat/completions shim",
        ));
    }
    let function = tool
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| ProxyError::bad_request("tool function must be an object"))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ProxyError::bad_request("tool function.name must be a string"))?;

    let mut converted = serde_json::Map::new();
    converted.insert("type".to_string(), Value::String("function".to_string()));
    converted.insert("name".to_string(), Value::String(name.to_string()));
    for field in ["description", "parameters", "strict"] {
        if let Some(value) = function.get(field) {
            converted.insert(field.to_string(), value.clone());
        }
    }
    Ok(Value::Object(converted))
}

pub(crate) fn chat_tool_choice_to_responses_tool_choice(
    tool_choice: &Value,
) -> Result<Value, ProxyError> {
    if let Some(choice) = tool_choice.as_str() {
        return Ok(Value::String(choice.to_string()));
    }
    let choice = tool_choice
        .as_object()
        .ok_or_else(|| ProxyError::bad_request("tool_choice must be a string or object"))?;
    if choice.get("type").and_then(Value::as_str) != Some("function") {
        return Err(ProxyError::bad_request(
            "only function tool_choice objects are supported",
        ));
    }
    let name = choice
        .get("function")
        .and_then(|function| function.get("name"))
        .or_else(|| choice.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| ProxyError::bad_request("tool_choice function.name must be a string"))?;
    Ok(json!({ "type": "function", "name": name }))
}

pub(crate) fn chat_request_summary(input: &Value, body_bytes: usize) -> Value {
    let messages = input.get("messages").and_then(Value::as_array);
    let mut role_counts = serde_json::Map::new();
    if let Some(messages) = messages {
        for message in messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let count = role_counts
                .get(role)
                .and_then(Value::as_u64)
                .unwrap_or_default()
                + 1;
            role_counts.insert(role.to_string(), json!(count));
        }
    }

    let tools = input.get("tools").and_then(Value::as_array);
    json!({
        "body_bytes": body_bytes,
        "model": input.get("model").and_then(Value::as_str),
        "stream": input.get("stream").and_then(Value::as_bool).unwrap_or(false),
        "messages": messages.map_or(0, Vec::len),
        "roles": role_counts,
        "content_kinds": message_content_kinds(messages),
        "assistant_tool_call_types": assistant_tool_call_types(messages),
        "tools": tools.map_or(0, Vec::len),
        "tool_names": sample_strings(&tools.map(|tools| tool_names(tools)).unwrap_or_default(), 12),
        "tool_choice": input.get("tool_choice").map(tool_choice_summary),
        "parallel_tool_calls": input.get("parallel_tool_calls").and_then(Value::as_bool),
        "unsupported_fields": unsupported_chat_fields(input),
        "reasoning": input.get("reasoning").is_some(),
        "reasoning_effort": first_stringish_field(input, &["reasoning_effort", "reasoningEffort"]),
        "reasoning_summary": first_stringish_field(input, &["reasoning_summary", "reasoningSummary"]),
        "verbosity": first_stringish_field(input, &["verbosity", "text_verbosity", "textVerbosity"]),
    })
}

pub(crate) fn message_content_kinds(messages: Option<&Vec<Value>>) -> Value {
    let mut content = serde_json::Map::new();
    let mut parts = serde_json::Map::new();
    if let Some(messages) = messages {
        for message in messages {
            let Some(value) = message.get("content") else {
                continue;
            };
            let kind = value_kind(value);
            let count = content
                .get(kind)
                .and_then(Value::as_u64)
                .unwrap_or_default()
                + 1;
            content.insert(kind.to_string(), json!(count));

            let Some(part_values) = value.as_array() else {
                continue;
            };
            for part in part_values {
                let part_type = part
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let count = parts
                    .get(part_type)
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
                    + 1;
                parts.insert(part_type.to_string(), json!(count));
            }
        }
    }
    json!({
        "content": content,
        "parts": parts,
    })
}

pub(crate) fn assistant_tool_call_types(messages: Option<&Vec<Value>>) -> Value {
    let mut types = serde_json::Map::new();
    if let Some(messages) = messages {
        for message in messages {
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) else {
                continue;
            };
            for tool_call in tool_calls {
                let tool_type = tool_call
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("function");
                let count = types
                    .get(tool_type)
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
                    + 1;
                types.insert(tool_type.to_string(), json!(count));
            }
        }
    }
    Value::Object(types)
}

pub(crate) fn responses_request_summary(body: &Value) -> Value {
    let input_items = body.get("input").and_then(Value::as_array);
    let mut item_types = serde_json::Map::new();
    if let Some(input_items) = input_items {
        for item in input_items {
            let item_type = item
                .get("type")
                .or_else(|| item.get("role"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let count = item_types
                .get(item_type)
                .and_then(Value::as_u64)
                .unwrap_or_default()
                + 1;
            item_types.insert(item_type.to_string(), json!(count));
        }
    }

    let tools = body.get("tools").and_then(Value::as_array);
    json!({
        "model": body.get("model").and_then(Value::as_str),
        "stream": body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        "store": body.get("store").and_then(Value::as_bool),
        "input_items": input_items.map_or(0, Vec::len),
        "input_item_types": item_types,
        "tools": tools.map_or(0, Vec::len),
        "tool_names": sample_strings(&tools.map(|tools| response_tool_names(tools)).unwrap_or_default(), 12),
        "tool_choice": body.get("tool_choice").map(tool_choice_summary),
        "parallel_tool_calls": body.get("parallel_tool_calls").and_then(Value::as_bool),
        "service_tier": body.get("service_tier").and_then(Value::as_str),
        "reasoning_effort": body
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str),
        "reasoning_summary": body
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("summary"))
            .and_then(Value::as_str),
        "verbosity": body
            .get("text")
            .and_then(|text| text.get("verbosity"))
            .and_then(Value::as_str),
    })
}

pub(crate) fn unsupported_chat_fields(input: &Value) -> Vec<String> {
    UNSUPPORTED_CHAT_FIELDS
        .iter()
        .filter(|field| input.get(**field).is_some())
        .map(|field| (*field).to_string())
        .collect()
}

pub(crate) fn first_stringish_field(input: &Value, fields: &[&str]) -> Option<String> {
    for field in fields {
        if let Some(value) = input.get(*field) {
            return Some(match value.as_str() {
                Some(value) => value.to_string(),
                None => format!("<{}>", value_kind(value)),
            });
        }
    }
    None
}

pub(crate) fn tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

pub(crate) fn response_tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

pub(crate) fn first_string_field<'a>(
    input: &'a Value,
    fields: &[&str],
) -> Result<Option<&'a str>, ProxyError> {
    for field in fields {
        if let Some(value) = input.get(*field) {
            return value
                .as_str()
                .map(Some)
                .ok_or_else(|| ProxyError::bad_request(format!("{field} must be a string")));
        }
    }
    Ok(None)
}

pub(crate) fn set_nested_object_field(
    parent: &mut Value,
    object_key: &str,
    field_key: &str,
    field_value: Value,
) -> Result<(), ProxyError> {
    let parent_object = parent
        .as_object_mut()
        .expect("responses body should be an object");
    if !parent_object.contains_key(object_key) {
        parent_object.insert(object_key.to_string(), json!({}));
    }
    let nested = parent_object
        .get_mut(object_key)
        .expect("nested object should exist after insertion");
    let nested_object = nested
        .as_object_mut()
        .ok_or_else(|| ProxyError::bad_request(format!("{object_key} must be an object")))?;
    nested_object.insert(field_key.to_string(), field_value);
    Ok(())
}

pub(crate) fn reject_unsupported_chat_fields(input: &Value) -> Result<(), ProxyError> {
    for field in UNSUPPORTED_CHAT_FIELDS {
        if input.get(field).is_some() {
            return Err(ProxyError::bad_request(format!(
                "{field} is not supported by this /v1/chat/completions shim; use /v1/responses"
            )));
        }
    }
    if input
        .get("n")
        .and_then(Value::as_u64)
        .is_some_and(|n| n != 1)
    {
        return Err(ProxyError::bad_request(
            "n values other than 1 are not supported by this /v1/chat/completions shim",
        ));
    }
    Ok(())
}

pub(crate) fn extract_chat_message_from_responses(value: &Value) -> (String, Vec<Value>) {
    let mut tool_calls = Vec::new();
    if let Some(arr) = value.get("output").and_then(Value::as_array) {
        for item in arr {
            if let Some(tool_call) =
                response_function_call_to_chat_tool_call(item, tool_calls.len())
            {
                tool_calls.push(tool_call);
            }
        }
    }
    (extract_responses_text(value), tool_calls)
}

pub(crate) fn response_function_call_to_chat_tool_call(
    item: &Value,
    index: usize,
) -> Option<Value> {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }
    let name = item.get("name").and_then(Value::as_str)?;
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("call_{index}"));
    let arguments = match item.get("arguments") {
        Some(Value::String(arguments)) => arguments.clone(),
        Some(arguments) => arguments.to_string(),
        None => String::new(),
    };

    Some(json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        }
    }))
}

pub(crate) fn extract_responses_text(value: &Value) -> String {
    let mut out = String::new();
    if let Some(arr) = value.get("output").and_then(Value::as_array) {
        for item in arr {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for c in content {
                    if let Some(text) = c.get("text").and_then(Value::as_str) {
                        out.push_str(text);
                    }
                }
            }
        }
    }
    if out.is_empty() {
        value
            .get("output_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        out
    }
}
