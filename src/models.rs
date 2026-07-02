use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::config::{
    DEFAULT_ADVERTISED_MODELS, DEFAULT_REASONING_EFFORT, DEFAULT_VERBOSITY,
    SUPPORTED_REASONING_EFFORTS, SUPPORTED_VERBOSITIES,
};
use crate::server::AppState;
use crate::service_tier::{
    FAST_SERVICE_TIER_ALIAS, FAST_SERVICE_TIER_REQUEST_VALUE, normalize_service_tier,
    service_tier_for_model, supports_codex_service_tier,
};

pub(crate) async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

pub(crate) async fn models_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    match model_response_format(&headers) {
        ModelResponseFormat::OpenAi => Json(openai_models_payload(
            &state.models,
            state.service_tier.as_deref(),
        )),
        ModelResponseFormat::Anthropic => Json(anthropic_models_payload(&state.models)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelResponseFormat {
    OpenAi,
    Anthropic,
}

pub(crate) const CODEX_PROXY_FORMAT_HEADER: &str = "x-codex-proxy-format";

pub(crate) fn model_response_format(headers: &HeaderMap) -> ModelResponseFormat {
    if headers
        .get(CODEX_PROXY_FORMAT_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_anthropic_format_value)
        || headers.contains_key("anthropic-version")
    {
        ModelResponseFormat::Anthropic
    } else {
        ModelResponseFormat::OpenAi
    }
}

pub(crate) fn is_anthropic_format_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "anthropic" | "claude" | "cloud"
    )
}

pub(crate) fn openai_models_payload(
    models: &[String],
    default_service_tier: Option<&str>,
) -> Value {
    let data = models
        .iter()
        .filter(|model| !model.trim().is_empty())
        .map(|model| model_catalog_entry(model.trim(), default_service_tier))
        .collect::<Vec<_>>();
    json!({
        "object": "list",
        "data": data
    })
}

pub(crate) fn anthropic_models_payload(models: &[String]) -> Value {
    let data = models
        .iter()
        .filter(|model| !model.trim().is_empty())
        .map(|model| anthropic_model_catalog_entry(model.trim()))
        .collect::<Vec<_>>();
    let first_id = data
        .first()
        .and_then(|entry| entry.get("id"))
        .and_then(Value::as_str);
    let last_id = data
        .last()
        .and_then(|entry| entry.get("id"))
        .and_then(Value::as_str);
    json!({
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    })
}

pub(crate) fn anthropic_model_catalog_entry(model: &str) -> Value {
    let alias = anthropic_model_alias(model);
    json!({
        "type": "model",
        "id": alias,
        "display_name": alias,
        "created_at": "2026-01-01T00:00:00Z",
    })
}

pub(crate) fn anthropic_model_alias(model: &str) -> &str {
    match model {
        // Claude Desktop model discovery filters for Anthropic-looking model IDs.
        // These aliases are display/compat names only; requests are mapped back
        // to Codex model IDs in codex_model_for_anthropic_alias below.
        "gpt-5.5" => "claude-opus-4-8",
        "gpt-5.4" => "claude-opus-4-7",
        "gpt-5.4-mini" => "claude-haiku-4-5-20251001",
        "gpt-5.3-codex-spark" => "claude-sonnet-5",
        other => other,
    }
}

pub(crate) fn codex_model_for_anthropic_alias(model: &str) -> &str {
    if model.starts_with("claude-haiku-") {
        return "gpt-5.4-mini";
    }
    match model {
        // Current discovery aliases.
        "claude-opus-4-8" | "claude-4-8" => "gpt-5.5",
        "claude-opus-4-7" => "gpt-5.4",
        "claude-sonnet-5" => "gpt-5.3-codex-spark",
        // Backward-compatible aliases we previously advertised or observed.
        model if model.starts_with("claude-opus-") => "gpt-5.5",
        "claude-sonnet-4-5-20250929" => "gpt-5.5",
        "claude-sonnet-4-20250514" => "gpt-5.4",
        "claude-3-5-haiku-20241022" => "gpt-5.4-mini",
        "claude-3-7-sonnet-20250219" => "gpt-5.3-codex-spark",
        // If the user manually configures a GPT/Codex ID in Claude Desktop,
        // keep it as-is and send it directly upstream.
        other => other,
    }
}

pub(crate) fn model_catalog_entry(model: &str, default_service_tier: Option<&str>) -> Value {
    let mut entry = json!({
        "id": model,
        "object": "model",
        "created": 0,
        "owned_by": "openai",
    });

    if supports_codex_reasoning_metadata(model) {
        let reasoning_levels = SUPPORTED_REASONING_EFFORTS
            .iter()
            .map(|effort| json!({ "effort": effort, "description": effort }))
            .collect::<Vec<_>>();
        let reasoning_efforts = SUPPORTED_REASONING_EFFORTS
            .iter()
            .map(|effort| Value::String((*effort).to_string()))
            .collect::<Vec<_>>();

        let object = entry
            .as_object_mut()
            .expect("model catalog entry should be an object");
        object.insert("name".to_string(), Value::String(model.to_string()));
        object.insert("model_picker_enabled".to_string(), Value::Bool(true));
        object.insert("version".to_string(), Value::String(model.to_string()));
        object.insert(
            "supported_endpoints".to_string(),
            json!(["/v1/responses", "/v1/chat/completions"]),
        );
        object.insert(
            "default_reasoning_level".to_string(),
            Value::String(DEFAULT_REASONING_EFFORT.to_string()),
        );
        object.insert(
            "supported_reasoning_levels".to_string(),
            Value::Array(reasoning_levels),
        );
        object.insert(
            "supports_reasoning_summaries".to_string(),
            Value::Bool(true),
        );
        object.insert("support_verbosity".to_string(), Value::Bool(true));
        object.insert(
            "supported_verbosities".to_string(),
            json!(SUPPORTED_VERBOSITIES),
        );
        object.insert(
            "default_verbosity".to_string(),
            Value::String(DEFAULT_VERBOSITY.to_string()),
        );
        object.insert(
            "capabilities".to_string(),
            json!({
                "reasoning": true,
                "family": model_family(model),
                "supports": {
                    "reasoning_effort": reasoning_efforts,
                    "streaming": true,
                    "tool_calls": true,
                }
            }),
        );

        if supports_codex_service_tier(model) {
            object.insert(
                "service_tiers".to_string(),
                json!([{
                    "id": FAST_SERVICE_TIER_REQUEST_VALUE,
                    "name": "Fast",
                    "description": "1.5x speed, increased usage",
                }]),
            );
            object.insert(
                "additional_speed_tiers".to_string(),
                json!([FAST_SERVICE_TIER_ALIAS]),
            );
            if let Some(service_tier) = default_service_tier
                .and_then(normalize_service_tier)
                .and_then(|tier| service_tier_for_model(Some(model), &tier))
            {
                object.insert(
                    "default_service_tier".to_string(),
                    Value::String(service_tier),
                );
            }
        }
    }

    entry
}

pub(crate) fn supports_codex_reasoning_metadata(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gpt-5") || model.contains("codex")
}

pub(crate) fn model_family(model: &str) -> &'static str {
    if model.to_ascii_lowercase().starts_with("gpt-5") {
        "gpt-5"
    } else {
        "codex"
    }
}

pub(crate) fn default_advertised_models() -> Vec<String> {
    DEFAULT_ADVERTISED_MODELS
        .split(',')
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .collect()
}
