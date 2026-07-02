use bytes::Bytes;
use serde_json::Value;

use crate::errors::ProxyError;

pub(crate) const FAST_SERVICE_TIER_ALIAS: &str = "fast";
pub(crate) const FAST_SERVICE_TIER_REQUEST_VALUE: &str = "priority";
pub(crate) const DEFAULT_SERVICE_TIER_REQUEST_VALUE: &str = "default";
pub(crate) const SERVICE_TIER_ENV: &str = "CODEX_PROXY_SERVICE_TIER";

pub(crate) fn configured_service_tier(value: Option<String>) -> Option<String> {
    value.and_then(|value| normalize_service_tier(&value))
}

pub(crate) fn normalize_service_tier(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "false" | "none" | "off" | "standard" | DEFAULT_SERVICE_TIER_REQUEST_VALUE => None,
        FAST_SERVICE_TIER_ALIAS | FAST_SERVICE_TIER_REQUEST_VALUE => {
            Some(FAST_SERVICE_TIER_REQUEST_VALUE.to_string())
        }
        "flex" => Some("flex".to_string()),
        _ => Some(trimmed.to_string()),
    }
}

pub(crate) fn supports_codex_service_tier(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gpt-5") || model.contains("codex")
}

pub(crate) fn service_tier_for_model(model: Option<&str>, service_tier: &str) -> Option<String> {
    if service_tier == FAST_SERVICE_TIER_REQUEST_VALUE
        && !model.is_some_and(supports_codex_service_tier)
    {
        return None;
    }
    Some(service_tier.to_string())
}

pub(crate) fn apply_default_service_tier_to_responses_body(
    body: Bytes,
    default_service_tier: Option<&str>,
) -> Result<(Bytes, Option<String>), ProxyError> {
    let Some(default_service_tier) = default_service_tier.and_then(normalize_service_tier) else {
        return Ok((body, None));
    };
    let mut value: Value = serde_json::from_slice(&body)
        .map_err(|err| ProxyError::bad_request(format!("invalid JSON request body: {err}")))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| ProxyError::bad_request("responses request body must be a JSON object"))?;

    if let Some(existing) = object.get("service_tier") {
        if existing.is_null() {
            object.remove("service_tier");
            return serialize_body(value, None);
        }
        let existing = existing
            .as_str()
            .ok_or_else(|| ProxyError::bad_request("service_tier must be a string or null"))?;
        let normalized = normalize_service_tier(existing);
        match normalized {
            Some(service_tier) => {
                object.insert(
                    "service_tier".to_string(),
                    Value::String(service_tier.clone()),
                );
                return serialize_body(value, Some(service_tier));
            }
            None => {
                object.remove("service_tier");
                return serialize_body(value, None);
            }
        }
    }

    let model = object.get("model").and_then(Value::as_str);
    let service_tier = service_tier_for_model(model, &default_service_tier);
    if let Some(service_tier) = service_tier.clone() {
        object.insert("service_tier".to_string(), Value::String(service_tier));
    }
    serialize_body(value, service_tier)
}

fn serialize_body(
    value: Value,
    service_tier: Option<String>,
) -> Result<(Bytes, Option<String>), ProxyError> {
    let body = serde_json::to_vec(&value).map_err(ProxyError::upstream)?;
    Ok((Bytes::from(body), service_tier))
}
