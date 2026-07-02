use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde_json::{Value, json};
use tracing::error;

use crate::config::{CODEX_RESPONSES_COMPACT_URL, CODEX_RESPONSES_URL, ORIGINATOR};
use crate::errors::ProxyError;
use crate::logging::{append_compat_log, header_str};
use crate::reasoning::apply_default_reasoning_effort;
use crate::server::AppState;
use crate::service_tier::apply_default_service_tier_to_responses_body;

pub(crate) async fn responses_proxy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    match forward_codex_responses(&state, CODEX_RESPONSES_URL, &headers, body).await {
        Ok(resp) => resp,
        Err(err) => {
            error!("responses proxy failed: {err:#}");
            err.into_response()
        }
    }
}

pub(crate) async fn responses_compact_proxy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    match forward_codex_responses(&state, CODEX_RESPONSES_COMPACT_URL, &headers, body).await {
        Ok(resp) => resp,
        Err(err) => {
            error!("responses compact proxy failed: {err:#}");
            err.into_response()
        }
    }
}

pub(crate) async fn forward_codex_responses(
    state: &AppState,
    upstream_url: &'static str,
    downstream_headers: &HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, ProxyError> {
    let original_body_len = body.len();
    let (body, reasoning_effort) = if upstream_url == CODEX_RESPONSES_URL {
        apply_default_reasoning_to_responses_body(body)?
    } else {
        (body, None)
    };
    let (body, service_tier) =
        apply_default_service_tier_to_responses_body(body, state.service_tier.as_deref())?;
    append_compat_log(
        "responses.forward",
        json!({
            "upstream": upstream_url,
            "body_bytes": original_body_len,
            "forwarded_body_bytes": body.len(),
            "reasoning_effort": reasoning_effort,
            "service_tier": service_tier,
            "accept": header_str(downstream_headers, axum::http::header::ACCEPT.as_str()),
            "content_type": header_str(downstream_headers, axum::http::header::CONTENT_TYPE.as_str()),
        }),
    );

    let (access_token, account_id) = state
        .auth
        .access_for_request()
        .await
        .map_err(ProxyError::auth)?;

    let mut req = state
        .client
        .post(upstream_url)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header("ChatGPT-Account-ID", account_id)
        .header(
            USER_AGENT,
            format!("openai-codex-proxy/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("originator", ORIGINATOR)
        .header("version", env!("CARGO_PKG_VERSION"));

    if let Some(content_type) = downstream_headers.get(axum::http::header::CONTENT_TYPE) {
        req = req.header(CONTENT_TYPE, content_type);
    } else {
        req = req.header(CONTENT_TYPE, "application/json");
    }

    if let Some(accept) = downstream_headers.get(axum::http::header::ACCEPT)
        && let Ok(accept) = accept.to_str()
    {
        req = req.header(ACCEPT, accept);
    } else {
        req = req.header(ACCEPT, "text/event-stream");
    }

    for name in passthrough_request_headers() {
        if let Some(value) = downstream_headers.get(*name) {
            req = req.header(*name, value);
        }
    }

    let upstream = req.body(body).send().await.map_err(ProxyError::upstream)?;
    let status = upstream.status();
    append_compat_log(
        "responses.upstream",
        json!({
            "upstream": upstream_url,
            "status": status.as_u16(),
        }),
    );
    let mut builder = Response::builder().status(status);
    for name in passthrough_response_headers() {
        if let Some(value) = upstream.headers().get(*name) {
            builder = builder.header(*name, value);
        }
    }

    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    builder
        .body(Body::from_stream(stream))
        .map_err(ProxyError::upstream)
}

pub(crate) fn apply_default_reasoning_to_responses_body(
    body: Bytes,
) -> Result<(Bytes, Option<String>), ProxyError> {
    let mut value: Value = serde_json::from_slice(&body)
        .map_err(|err| ProxyError::bad_request(format!("invalid JSON request body: {err}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| ProxyError::bad_request("responses request body must be a JSON object"))?;
    let Some(model) = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok((body, None));
    };

    let reasoning_effort = apply_default_reasoning_effort(&model, &mut value)?;
    let Some(reasoning_effort) = reasoning_effort else {
        return Ok((body, None));
    };

    let body = serde_json::to_vec(&value).map_err(ProxyError::upstream)?;
    Ok((Bytes::from(body), Some(reasoning_effort)))
}

pub(crate) fn passthrough_request_headers() -> &'static [&'static str] {
    static HEADERS: &[&str] = &[
        "content-encoding",
        "accept-encoding",
        "session-id",
        "thread-id",
        "x-codex-installation-id",
        "x-codex-turn-metadata",
        "x-codex-window-id",
        "x-codex-parent-thread-id",
        "x-openai-subagent",
        "openai-beta",
        "openai-organization",
        "openai-project",
        "x-oai-attestation",
        "traceparent",
        "tracestate",
        "baggage",
    ];
    HEADERS
}

pub(crate) fn passthrough_response_headers() -> &'static [&'static str] {
    static HEADERS: &[&str] = &[
        "content-type",
        "content-encoding",
        "content-disposition",
        "cache-control",
        "retry-after",
        "request-id",
        "x-request-id",
        "openai-processing-ms",
        "x-ratelimit-limit-requests",
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-remaining-tokens",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-tokens",
        "x-codex-primary-used-percent",
        "x-codex-secondary-used-percent",
        "x-codex-primary-window-minutes",
        "x-codex-secondary-window-minutes",
        "x-codex-primary-reset-at",
        "x-codex-secondary-reset-at",
        "x-codex-primary-over-secondary-limit-percent",
        "x-codex-secondary-over-primary-limit-percent",
    ];
    HEADERS
}
