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
use serde_json::{Map, Value, json};
use tracing::error;

use crate::chat::{
    build_responses_body_from_chat_with_defaults, extract_chat_message_from_responses,
    parse_sse_frame, response_function_call_to_chat_tool_call,
};
use crate::config::{CODEX_RESPONSES_URL, ORIGINATOR};
use crate::errors::{ProxyError, response_json};
use crate::logging::{append_compat_log, error_summary};
use crate::models::codex_model_for_anthropic_alias;
use crate::server::AppState;

pub(crate) async fn anthropic_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    match anthropic_messages_impl(&state, &headers, body).await {
        Ok(resp) => resp,
        Err(err) => {
            error!("anthropic messages shim failed: {err:#}");
            append_compat_log(
                "anthropic.error",
                json!({
                    "error_type": err.error_type(),
                    "message": err.to_string(),
                    "status": err.status().as_u16(),
                }),
            );
            anthropic_error_response(err)
        }
    }
}

pub(crate) async fn anthropic_messages_impl(
    state: &AppState,
    _headers: &HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, ProxyError> {
    let input: Value = serde_json::from_slice(&body)
        .map_err(|err| ProxyError::bad_request(format!("invalid JSON request body: {err}")))?;
    append_compat_log(
        "anthropic.request",
        anthropic_request_summary(&input, body.len()),
    );

    let requested_model = input
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| state.default_model.clone());
    let stream = input
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let chat_input = anthropic_to_chat_input(&input, &state.default_model)?;
    let (codex_model, mut responses_body) = build_responses_body_from_chat_with_defaults(
        &chat_input,
        &state.default_model,
        &state.default_reasoning_effort,
        state.service_tier.as_deref(),
    )?;
    // The ChatGPT Codex Responses backend requires streaming. For non-streaming
    // Anthropic clients, stream upstream internally and aggregate below.
    responses_body["stream"] = Value::Bool(true);
    append_compat_log(
        "anthropic.translated",
        json!({
            "requested_model": requested_model,
            "codex_model": codex_model,
            "stream": stream,
            "input_items": responses_body.get("input").and_then(Value::as_array).map_or(0, Vec::len),
            "tools": responses_body.get("tools").and_then(Value::as_array).map_or(0, Vec::len),
            "reasoning_effort": responses_body
                .get("reasoning")
                .and_then(|reasoning| reasoning.get("effort"))
                .and_then(Value::as_str),
            "reasoning_summary": responses_body
                .get("reasoning")
                .and_then(|reasoning| reasoning.get("summary"))
                .and_then(Value::as_str),
            "service_tier": responses_body.get("service_tier").and_then(Value::as_str),
        }),
    );

    if stream {
        stream_anthropic_messages(state, requested_model, responses_body).await
    } else {
        non_stream_anthropic_messages(state, requested_model, responses_body).await
    }
}

pub(crate) async fn non_stream_anthropic_messages(
    state: &AppState,
    requested_model: String,
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
        "anthropic.upstream",
        json!({
            "stream": true,
            "downstream_stream": false,
            "status": status.as_u16(),
        }),
    );
    if !status.is_success() {
        let value: Value = upstream.json().await.unwrap_or_else(|_| json!({}));
        return Ok(response_json(status, value));
    }

    let body = collect_anthropic_message_from_responses_stream(upstream, &requested_model).await?;
    Ok(response_json(StatusCode::OK, body))
}

pub(crate) async fn collect_anthropic_message_from_responses_stream(
    upstream: reqwest::Response,
    requested_model: &str,
) -> Result<Value, ProxyError> {
    let mut stream = upstream.bytes_stream();
    let mut buffer = Vec::new();
    let mut text = String::new();
    let mut tool_uses = Vec::new();
    let mut response_id: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ProxyError::upstream)?;
        buffer.extend_from_slice(&chunk);
        while let Some((frame, rest)) = take_sse_frame(&buffer) {
            buffer = rest;
            let frame = String::from_utf8_lossy(&frame);
            let Some((_event, data)) = parse_sse_frame(&frame) else {
                continue;
            };
            if data.trim() == "[DONE]" {
                break;
            }
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            match value.get("type").and_then(Value::as_str) {
                Some("response.output_text.delta") => {
                    if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                        text.push_str(delta);
                    }
                }
                Some("response.output_item.done") => {
                    if let Some(item) = value.get("item")
                        && let Some(tool_call) =
                            response_function_call_to_chat_tool_call(item, tool_uses.len())
                        && let Some(tool_use) = chat_tool_call_to_anthropic_tool_use(&tool_call)
                    {
                        tool_uses.push(tool_use);
                    }
                }
                Some("response.completed") => {
                    if response_id.is_none() {
                        response_id = value
                            .get("response")
                            .and_then(|response| response.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    if text.is_empty()
                        && tool_uses.is_empty()
                        && let Some(response) = value.get("response")
                    {
                        let message = responses_to_anthropic_message(response, requested_model);
                        return Ok(message);
                    }
                }
                Some("error") => {
                    return Err(ProxyError::upstream(error_summary(&value).to_string()));
                }
                _ => {}
            }
        }
    }

    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    content.extend(tool_uses);
    let stop_reason = if content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        "tool_use"
    } else {
        "end_turn"
    };

    Ok(json!({
        "id": response_id.unwrap_or_else(|| format!("msg_codex_proxy_{}", Utc::now().timestamp_millis())),
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 0, "output_tokens": 0},
    }))
}

pub(crate) async fn stream_anthropic_messages(
    state: &AppState,
    requested_model: String,
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
        "anthropic.upstream",
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
    let translator = AnthropicSseTranslator::new(requested_model);
    let stream = futures_util::stream::unfold(
        (upstream_stream, translator, Vec::<u8>::new(), false),
        |mut state| async move {
            loop {
                let buffered = drain_anthropic_sse_buffer(&mut state.1, &mut state.2);
                if !buffered.is_empty() {
                    return Some((Ok::<Bytes, std::io::Error>(Bytes::from(buffered)), state));
                }

                if state.3 {
                    return None;
                }
                match state.0.as_mut().next().await {
                    Some(Ok(chunk)) => {
                        state.2.extend_from_slice(&chunk);
                    }
                    Some(Err(err)) => {
                        state.3 = true;
                        return Some((Err(std::io::Error::other(err)), state));
                    }
                    None => {
                        state.3 = true;
                        let mut out = drain_anthropic_sse_buffer(&mut state.1, &mut state.2);
                        out.extend(state.1.finish());
                        if out.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(out)), state));
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

pub(crate) fn anthropic_to_chat_input(
    input: &Value,
    default_model: &str,
) -> Result<Value, ProxyError> {
    let codex_model = input
        .get("model")
        .and_then(Value::as_str)
        .map(codex_model_for_anthropic_alias)
        .unwrap_or(default_model);
    let mut messages = Vec::new();

    if let Some(system) = input.get("system") {
        let content = anthropic_system_as_text(system)?;
        if !content.is_empty() {
            messages.push(json!({"role": "system", "content": content}));
        }
    }

    let anthropic_messages = input
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProxyError::bad_request("messages must be an array"))?;
    for message in anthropic_messages {
        append_anthropic_message_as_chat(message, &mut messages)?;
    }

    let mut chat = json!({
        "model": codex_model,
        "messages": messages,
        "stream": input.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });

    if let Some(tools) = input.get("tools") {
        chat["tools"] = anthropic_tools_to_chat_tools(tools)?;
    }
    if let Some(tool_choice) = input.get("tool_choice") {
        chat["tool_choice"] = anthropic_tool_choice_to_chat_tool_choice(tool_choice)?;
    }
    if let Some(metadata) = input.get("metadata") {
        chat["metadata"] = metadata.clone();
    }
    Ok(chat)
}

pub(crate) fn anthropic_system_as_text(system: &Value) -> Result<String, ProxyError> {
    match system {
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => anthropic_content_blocks_as_text(parts),
        Value::Null => Ok(String::new()),
        _ => Err(ProxyError::bad_request(
            "system must be a string or content block array",
        )),
    }
}

pub(crate) fn append_anthropic_message_as_chat(
    message: &Value,
    out: &mut Vec<Value>,
) -> Result<(), ProxyError> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| ProxyError::bad_request("message.role must be a string"))?;
    let content = message
        .get("content")
        .ok_or_else(|| ProxyError::bad_request("message.content is required"))?;

    match content {
        Value::String(text) => out.push(json!({"role": role, "content": text})),
        Value::Array(blocks) => {
            let mut text_parts = Vec::new();
            let mut image_parts = Vec::new();
            let mut tool_calls = Vec::new();
            for block in blocks {
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("text");
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            text_parts.push(text.to_string());
                        }
                    }
                    "image" => {
                        if let Some(image) = anthropic_image_block_to_chat_part(block)? {
                            image_parts.push(image);
                        }
                    }
                    "tool_use" => {
                        let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                            ProxyError::bad_request("tool_use.id must be a string")
                        })?;
                        let name = block.get("name").and_then(Value::as_str).ok_or_else(|| {
                            ProxyError::bad_request("tool_use.name must be a string")
                        })?;
                        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": input.to_string(),
                            }
                        }));
                    }
                    "tool_result" => {
                        let tool_call_id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                ProxyError::bad_request("tool_result.tool_use_id must be a string")
                            })?;
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_call_id,
                            "content": anthropic_tool_result_as_text(block.get("content"))?,
                        }));
                    }
                    _ => {}
                }
            }

            if role == "assistant" && !tool_calls.is_empty() {
                out.push(json!({
                    "role": "assistant",
                    "content": if text_parts.is_empty() { Value::Null } else { Value::String(text_parts.join("\n")) },
                    "tool_calls": tool_calls,
                }));
            } else if !text_parts.is_empty() || !image_parts.is_empty() {
                let content = if image_parts.is_empty() {
                    Value::String(text_parts.join("\n"))
                } else {
                    let mut parts = text_parts
                        .into_iter()
                        .map(|text| json!({"type": "text", "text": text}))
                        .collect::<Vec<_>>();
                    parts.extend(image_parts);
                    Value::Array(parts)
                };
                out.push(json!({"role": role, "content": content}));
            }
        }
        _ => {
            return Err(ProxyError::bad_request(
                "message.content must be a string or array",
            ));
        }
    }
    Ok(())
}

pub(crate) fn anthropic_content_blocks_as_text(parts: &[Value]) -> Result<String, ProxyError> {
    Ok(parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n"))
}

pub(crate) fn anthropic_tool_result_as_text(content: Option<&Value>) -> Result<String, ProxyError> {
    match content {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => anthropic_content_blocks_as_text(parts),
        Some(Value::Null) | None => Ok(String::new()),
        Some(other) => Ok(other.to_string()),
    }
}

pub(crate) fn anthropic_image_block_to_chat_part(
    block: &Value,
) -> Result<Option<Value>, ProxyError> {
    let Some(source) = block.get("source") else {
        return Ok(None);
    };
    let source_type = source.get("type").and_then(Value::as_str);
    match source_type {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| ProxyError::bad_request("image source.data must be a string"))?;
            Ok(Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media_type};base64,{data}") }
            })))
        }
        Some("url") => {
            let url = source
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| ProxyError::bad_request("image source.url must be a string"))?;
            Ok(Some(
                json!({"type": "image_url", "image_url": { "url": url }}),
            ))
        }
        _ => Ok(None),
    }
}

pub(crate) fn anthropic_tools_to_chat_tools(tools: &Value) -> Result<Value, ProxyError> {
    let tools = tools
        .as_array()
        .ok_or_else(|| ProxyError::bad_request("tools must be an array"))?;
    Ok(Value::Array(
        tools
            .iter()
            .map(|tool| {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProxyError::bad_request("tool.name must be a string"))?;
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let parameters = tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Ok(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    }
                }))
            })
            .collect::<Result<Vec<_>, ProxyError>>()?,
    ))
}

pub(crate) fn anthropic_tool_choice_to_chat_tool_choice(
    tool_choice: &Value,
) -> Result<Value, ProxyError> {
    let choice_type = tool_choice
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| ProxyError::bad_request("tool_choice.type must be a string"))?;
    match choice_type {
        "auto" => Ok(Value::String("auto".to_string())),
        "any" => Ok(Value::String("required".to_string())),
        "tool" => {
            let name = tool_choice
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ProxyError::bad_request("tool_choice.name must be a string"))?;
            Ok(json!({"type": "function", "function": {"name": name}}))
        }
        "none" => Ok(Value::String("none".to_string())),
        _ => Err(ProxyError::bad_request(format!(
            "unsupported tool_choice.type: {choice_type}"
        ))),
    }
}

pub(crate) fn responses_to_anthropic_message(value: &Value, requested_model: &str) -> Value {
    let (text, chat_tool_calls) = extract_chat_message_from_responses(value);
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    for tool_call in chat_tool_calls {
        if let Some(block) = chat_tool_call_to_anthropic_tool_use(&tool_call) {
            content.push(block);
        }
    }
    let stop_reason = if content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        "tool_use"
    } else {
        "end_turn"
    };
    json!({
        "id": value.get("id").and_then(Value::as_str).unwrap_or("msg_codex_proxy"),
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": anthropic_usage(value),
    })
}

pub(crate) fn chat_tool_call_to_anthropic_tool_use(tool_call: &Value) -> Option<Value> {
    let function = tool_call.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?;
    let id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("toolu_codex_proxy");
    let input = function
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_else(|| json!({}));
    Some(json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input,
    }))
}

pub(crate) fn anthropic_usage(value: &Value) -> Value {
    let usage = value.get("usage").unwrap_or(&Value::Null);
    json!({
        "input_tokens": usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        "output_tokens": usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
    })
}

pub(crate) fn anthropic_error_response(err: ProxyError) -> Response<Body> {
    response_json(
        err.status(),
        json!({
            "type": "error",
            "error": {
                "type": err.error_type(),
                "message": err.to_string(),
            }
        }),
    )
}

pub(crate) async fn anthropic_count_tokens(body: Bytes) -> Response<Body> {
    let input: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let input_tokens = estimate_anthropic_input_tokens(&input);
    append_compat_log(
        "anthropic.count_tokens",
        json!({
            "body_bytes": body.len(),
            "input_tokens": input_tokens,
            "model": input.get("model").and_then(Value::as_str),
        }),
    );
    response_json(StatusCode::OK, json!({"input_tokens": input_tokens}))
}

pub(crate) fn estimate_anthropic_input_tokens(input: &Value) -> usize {
    let chars = input.to_string().chars().count();
    (chars / 4).max(1)
}

pub(crate) fn anthropic_request_summary(input: &Value, body_bytes: usize) -> Value {
    json!({
        "body_bytes": body_bytes,
        "model": input.get("model").and_then(Value::as_str),
        "messages": input.get("messages").and_then(Value::as_array).map_or(0, Vec::len),
        "stream": input.get("stream").and_then(Value::as_bool).unwrap_or(false),
        "tools": input.get("tools").and_then(Value::as_array).map_or(0, Vec::len),
    })
}

pub(crate) struct AnthropicSseTranslator {
    message_id: String,
    model: String,
    started: bool,
    text_index: Option<usize>,
    next_content_index: usize,
    saw_tool_use: bool,
    finished: bool,
}

impl AnthropicSseTranslator {
    pub(crate) fn new(model: String) -> Self {
        Self {
            message_id: format!("msg_codex_proxy_{}", Utc::now().timestamp_millis()),
            model,
            started: false,
            text_index: None,
            next_content_index: 0,
            saw_tool_use: false,
            finished: false,
        }
    }

    pub(crate) fn translate_frame(&mut self, frame: &[u8]) -> Vec<u8> {
        let frame = String::from_utf8_lossy(frame);
        let Some((_event, data)) = parse_sse_frame(&frame) else {
            return Vec::new();
        };
        if data.trim() == "[DONE]" {
            return self.finish();
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        self.ensure_started(&mut out);
        match value.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                let delta = value.get("delta").and_then(Value::as_str).unwrap_or("");
                if !delta.is_empty() {
                    self.ensure_text_started(&mut out);
                    out.extend(anthropic_sse(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": self.text_index.unwrap_or(0),
                            "delta": {"type": "text_delta", "text": delta},
                        }),
                    ));
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item")
                    && let Some(tool_call) =
                        response_function_call_to_chat_tool_call(item, self.next_content_index)
                    && let Some(tool_use) = chat_tool_call_to_anthropic_tool_use(&tool_call)
                {
                    self.stop_text_if_started(&mut out);
                    out.extend(self.tool_use_block(tool_use));
                }
            }
            Some("response.completed") => out.extend(self.finish()),
            Some("error") => {
                out.extend(anthropic_sse(
                    "error",
                    &json!({
                        "type": "error",
                        "error": error_summary(&value),
                    }),
                ));
                out.extend(self.finish());
            }
            _ => {}
        }
        out
    }

    pub(crate) fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        out.extend(anthropic_sse(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }
            }),
        ));
    }

    pub(crate) fn ensure_text_started(&mut self, out: &mut Vec<u8>) {
        if self.text_index.is_some() {
            return;
        }
        let index = self.next_content_index;
        self.next_content_index += 1;
        self.text_index = Some(index);
        out.extend(anthropic_sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""},
            }),
        ));
    }

    pub(crate) fn stop_text_if_started(&mut self, out: &mut Vec<u8>) {
        if let Some(index) = self.text_index.take() {
            out.extend(anthropic_sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            ));
        }
    }

    pub(crate) fn tool_use_block(&mut self, tool_use: Value) -> Vec<u8> {
        let index = self.next_content_index;
        self.next_content_index += 1;
        self.saw_tool_use = true;
        let id = tool_use
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("toolu_codex_proxy");
        let name = tool_use
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let input = tool_use.get("input").cloned().unwrap_or_else(|| json!({}));
        let input_json = input.to_string();
        append_compat_log(
            "anthropic.stream.tool_use",
            json!({
                "index": index,
                "name": name,
                "input_bytes": input_json.len(),
            }),
        );

        let mut out = Vec::new();
        out.extend(anthropic_sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": {},
                },
            }),
        ));
        if !input_json.is_empty() && input_json != "{}" {
            out.extend(anthropic_sse(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": input_json,
                    },
                }),
            ));
        }
        out.extend(anthropic_sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
        out
    }

    pub(crate) fn finish(&mut self) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = Vec::new();
        self.ensure_started(&mut out);
        self.stop_text_if_started(&mut out);
        out.extend(anthropic_sse(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": if self.saw_tool_use { "tool_use" } else { "end_turn" }, "stop_sequence": Value::Null},
                "usage": {"output_tokens": 0},
            }),
        ));
        out.extend(anthropic_sse(
            "message_stop",
            &json!({"type": "message_stop"}),
        ));
        out
    }
}

pub(crate) fn anthropic_sse(event: &str, data: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

pub(crate) fn drain_anthropic_sse_buffer(
    translator: &mut AnthropicSseTranslator,
    buffer: &mut Vec<u8>,
) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some((frame, rest)) = take_sse_frame(buffer) {
        *buffer = rest;
        out.extend(translator.translate_frame(&frame));
    }
    out
}

pub(crate) fn take_sse_frame(buffer: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (pos, separator_len) = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|pos| (pos, 2))
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|pos| (pos, 4))
        })?;
    let frame = buffer[..pos + separator_len].to_vec();
    let rest = buffer[pos + separator_len..].to_vec();
    Some((frame, rest))
}

#[allow(dead_code)]
pub(crate) fn value_object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}
