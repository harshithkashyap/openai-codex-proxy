use serde_json::{Map, Value};

use crate::config::DEFAULT_REASONING_EFFORT;
use crate::errors::ProxyError;
use crate::models::supports_codex_reasoning_metadata;

pub(crate) fn apply_default_reasoning_effort(
    model: &str,
    responses_body: &mut Value,
) -> Result<Option<String>, ProxyError> {
    if !supports_codex_reasoning_metadata(model) {
        return Ok(None);
    }

    let parent = responses_body
        .as_object_mut()
        .expect("responses body should be an object");
    let reasoning = ensure_reasoning_object(parent)?;

    if reasoning.get("effort").and_then(Value::as_str).is_none() {
        reasoning.insert(
            "effort".to_string(),
            Value::String(DEFAULT_REASONING_EFFORT.to_string()),
        );
    }

    Ok(reasoning
        .get("effort")
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn ensure_reasoning_object(
    parent: &mut Map<String, Value>,
) -> Result<&mut Map<String, Value>, ProxyError> {
    if !parent.contains_key("reasoning") {
        parent.insert("reasoning".to_string(), Value::Object(Map::new()));
    }

    parent
        .get_mut("reasoning")
        .expect("reasoning should exist after insertion")
        .as_object_mut()
        .ok_or_else(|| ProxyError::bad_request("reasoning must be an object"))
}
