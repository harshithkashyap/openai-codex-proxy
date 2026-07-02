use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::Instant;

use axum::body::Body;
use axum::http::{HeaderMap, Request, Response};
use axum::middleware::Next;
use chrono::Utc;
use serde_json::{Value, json};
use tracing::warn;

use crate::config::DEFAULT_COMPAT_LOG_FILE;

pub(crate) static COMPAT_LOG_WRITE_LOCK: LazyLock<StdMutex<()>> =
    LazyLock::new(|| StdMutex::new(()));

pub(crate) async fn file_request_log(req: Request<Body>, next: Next) -> Response<Body> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let start = Instant::now();
    let response = next.run(req).await;
    append_compat_log(
        "http.request",
        json!({
            "method": method,
            "path": path,
            "status": response.status().as_u16(),
            "latency_ms": start.elapsed().as_millis(),
        }),
    );
    response
}

pub(crate) fn append_compat_log(event: &str, details: Value) {
    let Some(path) = compat_log_file() else {
        return;
    };
    let _guard = match COMPAT_LOG_WRITE_LOCK.lock() {
        Ok(guard) => guard,
        Err(err) => {
            warn!("compatibility log lock poisoned: {err}");
            return;
        }
    };
    let line = json!({
        "ts": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "event": event,
        "details": details,
    });

    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => {
            if let Err(err) = writeln!(file, "{line}") {
                warn!("failed writing compatibility log {}: {err}", path.display());
            }
        }
        Err(err) => warn!("failed opening compatibility log {}: {err}", path.display()),
    }
}

pub(crate) fn compat_log_file() -> Option<PathBuf> {
    #[cfg(test)]
    if env::var("CODEX_PROXY_LOG_FILE").is_err() {
        return None;
    }

    match env::var("CODEX_PROXY_LOG_FILE") {
        Ok(value) if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("false") => {
            None
        }
        Ok(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => Some(PathBuf::from(DEFAULT_COMPAT_LOG_FILE)),
    }
}
pub(crate) fn sample_strings(values: &[String], limit: usize) -> Value {
    json!({
        "total": values.len(),
        "sample": values.iter().take(limit).collect::<Vec<_>>(),
        "truncated": values.len().saturating_sub(limit),
    })
}

pub(crate) fn tool_choice_summary(tool_choice: &Value) -> Value {
    if let Some(choice) = tool_choice.as_str() {
        return Value::String(choice.to_string());
    }
    json!({
        "type": tool_choice.get("type").and_then(Value::as_str),
        "name": tool_choice
            .get("function")
            .and_then(|function| function.get("name"))
            .or_else(|| tool_choice.get("name"))
            .and_then(Value::as_str),
    })
}

pub(crate) fn error_summary(error: &Value) -> Value {
    json!({
        "type": error.get("type").and_then(Value::as_str),
        "code": error.get("code").and_then(Value::as_str),
        "message": error
            .get("message")
            .and_then(Value::as_str)
            .map(|message| clip_text(message, 300)),
    })
}

pub(crate) fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

pub(crate) fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub(crate) fn clip_text(value: &str, max_chars: usize) -> String {
    let mut clipped = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        clipped.push_str("...");
    }
    clipped
}
