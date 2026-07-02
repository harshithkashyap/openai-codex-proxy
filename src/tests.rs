use super::*;
use axum::http::{HeaderMap, HeaderValue};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use chrono::Utc;
use reqwest::cookie::CookieStore;
use serde_json::json;

fn jwt_with_auth(account_id: &str, plan_type: &str, exp: Option<i64>) -> String {
    let header = json!({ "alg": "none", "typ": "JWT" });
    let mut payload = json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
            "chatgpt_plan_type": plan_type
        }
    });
    if let Some(exp) = exp {
        payload["exp"] = json!(exp);
    }
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
    format!("{header}.{payload}.sig")
}

#[test]
fn parses_codex_auth_json_and_preserves_extra_fields() {
    let id_token = jwt_with_auth("account-123", "pro", None);
    let raw = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": "access-token",
            "refresh_token": "refresh-token"
        },
        "last_refresh": "2026-06-29T00:00:00Z",
        "agent_identity": { "some": "future-field" }
    })
    .to_string();

    let state = parse_auth_json(&raw).expect("auth should parse");

    assert_eq!(state.auth.account_id.as_deref(), Some("account-123"));
    assert_eq!(state.auth.plan_type.as_deref(), Some("pro"));
    assert!(state.file.extra.contains_key("agent_identity"));
    assert_eq!(
        state
            .file
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.account_id.as_deref()),
        Some("account-123")
    );
}

#[test]
fn codex_auth_json_replaces_stale_account_id_from_token_claims() {
    let id_token = jwt_with_auth("current-account", "pro", None);
    let raw = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": "access-token",
            "refresh_token": "refresh-token",
            "account_id": "stale-account"
        }
    })
    .to_string();

    let state = parse_auth_json(&raw).expect("auth should parse");

    assert_eq!(state.auth.account_id.as_deref(), Some("current-account"));
    assert_eq!(
        state
            .file
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.account_id.as_deref()),
        Some("current-account")
    );
}

#[test]
fn converts_legacy_flat_auth_to_codex_auth_shape() {
    let access_token = jwt_with_auth("account-legacy", "plus", None);
    let raw = json!({
        "id_token": null,
        "access_token": access_token,
        "refresh_token": "refresh-token",
        "last_refresh": "2026-06-29T00:00:00Z"
    })
    .to_string();

    let state = parse_auth_json(&raw).expect("legacy auth should parse");

    assert_eq!(state.file.auth_mode.as_deref(), Some("chatgpt"));
    assert!(state.file.tokens.is_some());
    assert_eq!(state.auth.account_id.as_deref(), Some("account-legacy"));
}

#[test]
fn refreshed_token_metadata_overwrites_stale_account_id() {
    let mut auth = StoredAuth {
        id_token: Some(jwt_with_auth("old-account", "pro", None)),
        access_token: "old-access-token".to_string(),
        refresh_token: "refresh-token".to_string(),
        account_id: Some("old-account".to_string()),
        plan_type: Some("pro".to_string()),
        last_refresh: Some(Utc::now()),
    };
    auth.id_token = Some(jwt_with_auth("new-account", "business", None));
    auth.access_token = jwt_with_auth("new-account", "business", None);
    hydrate_metadata(&mut auth);

    assert_eq!(auth.account_id.as_deref(), Some("new-account"));
    assert_eq!(auth.plan_type.as_deref(), Some("business"));
}

#[test]
fn local_api_key_accepts_bearer_or_x_api_key() {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer secret"),
    );
    assert!(local_api_key_authorized(&headers, "secret"));

    headers.clear();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    assert!(local_api_key_authorized(&headers, "secret"));

    headers.insert("x-api-key", HeaderValue::from_static("wrong"));
    assert!(!local_api_key_authorized(&headers, "secret"));
}

#[test]
fn pkce_challenge_matches_rfc7636_example() {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert_eq!(
        pkce_challenge(verifier),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn browser_authorize_url_uses_codex_oauth_shape() {
    let pkce = PkceCodes {
        verifier: "verifier".to_string(),
        challenge: "challenge".to_string(),
    };
    let url = build_authorize_url("http://localhost:1455/auth/callback", &pkce, "state-value")
        .expect("authorize URL should build");
    let url = reqwest::Url::parse(&url).expect("authorize URL should parse");
    let query = url
        .query_pairs()
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();

    assert_eq!(
        url.as_str().split('?').next(),
        Some("https://auth.openai.com/oauth/authorize")
    );
    assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(query.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some("http://localhost:1455/auth/callback")
    );
    assert_eq!(
        query.get("scope").map(String::as_str),
        Some("openid profile email offline_access")
    );
    assert_eq!(
        query.get("code_challenge").map(String::as_str),
        Some("challenge")
    );
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(
        query.get("codex_cli_simplified_flow").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        query.get("id_token_add_organizations").map(String::as_str),
        Some("true")
    );
    assert_eq!(query.get("state").map(String::as_str), Some("state-value"));
    assert_eq!(
        query.get("originator").map(String::as_str),
        Some(ORIGINATOR)
    );
}

#[test]
fn default_model_catalog_includes_codex_style_models() {
    assert_eq!(
        default_advertised_models(),
        vec![
            "gpt-5.5".to_string(),
            "gpt-5.4".to_string(),
            "gpt-5.4-mini".to_string(),
            "gpt-5.3-codex-spark".to_string()
        ]
    );
}

#[test]
fn model_discovery_format_defaults_to_openai_and_accepts_anthropic_header() {
    let headers = HeaderMap::new();
    assert_eq!(model_response_format(&headers), ModelResponseFormat::OpenAi);

    let mut headers = HeaderMap::new();
    headers.insert(
        CODEX_PROXY_FORMAT_HEADER,
        HeaderValue::from_static("anthropic"),
    );
    assert_eq!(
        model_response_format(&headers),
        ModelResponseFormat::Anthropic
    );

    let mut headers = HeaderMap::new();
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    assert_eq!(
        model_response_format(&headers),
        ModelResponseFormat::Anthropic
    );
}

#[test]
fn anthropic_model_catalog_uses_anthropic_list_shape() {
    let models = vec!["gpt-5.5".to_string(), "gpt-5.4".to_string()];
    let payload = anthropic_models_payload(&models);

    assert!(payload.get("object").is_none());
    assert_eq!(payload["has_more"], false);
    assert_eq!(payload["first_id"], "claude-opus-4-8");
    assert_eq!(payload["last_id"], "claude-opus-4-7");
    assert_eq!(payload["data"][0]["type"], "model");
    assert_eq!(payload["data"][0]["id"], "claude-opus-4-8");
    assert_eq!(payload["data"][0]["display_name"], "claude-opus-4-8");
    assert_eq!(
        codex_model_for_anthropic_alias("claude-opus-4-8"),
        "gpt-5.5"
    );
    assert_eq!(
        codex_model_for_anthropic_alias("claude-opus-4-7"),
        "gpt-5.4"
    );
    assert_eq!(
        codex_model_for_anthropic_alias("claude-haiku-4-5-20251001"),
        "gpt-5.4-mini"
    );
}

#[test]
fn anthropic_messages_request_maps_opus_alias_to_codex_chat_shape() {
    let input = json!({
        "model": "claude-opus-4-1-20250805",
        "system": "You are concise.",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}],
        "tools": [{
            "name": "lookup",
            "description": "Lookup a value",
            "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}
        }],
        "tool_choice": {"type": "tool", "name": "lookup"}
    });

    let chat =
        anthropic_to_chat_input(&input, DEFAULT_MODEL).expect("anthropic request should translate");

    assert_eq!(chat["model"], "gpt-5.5");
    assert_eq!(chat["messages"][0]["role"], "system");
    assert_eq!(chat["messages"][1]["content"], "Hello");
    assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
    assert_eq!(chat["tool_choice"]["function"]["name"], "lookup");
}

#[test]
fn anthropic_messages_request_gets_default_codex_reasoning_effort() {
    let input = json!({
        "model": "claude-opus-4-8",
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let chat =
        anthropic_to_chat_input(&input, DEFAULT_MODEL).expect("anthropic request should translate");
    let (_, body) = build_responses_body_from_chat(&chat).expect("chat should translate");

    assert_eq!(body["model"].as_str(), Some("gpt-5.5"));
    assert_eq!(
        body["reasoning"]["effort"].as_str(),
        Some(DEFAULT_REASONING_EFFORT)
    );
}

#[test]
fn anthropic_messages_request_uses_configured_default_model_when_model_is_omitted() {
    let input = json!({
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let chat =
        anthropic_to_chat_input(&input, "gpt-5.4").expect("anthropic request should translate");

    assert_eq!(chat["model"], "gpt-5.4");
}

#[test]
fn responses_output_maps_to_anthropic_message_shape() {
    let response = json!({
        "id": "resp_123",
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": "Hello"}]
        }],
        "usage": {"input_tokens": 10, "output_tokens": 2}
    });

    let message = responses_to_anthropic_message(&response, "claude-opus-4-1-20250805");

    assert_eq!(message["id"], "resp_123");
    assert_eq!(message["type"], "message");
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["model"], "claude-opus-4-1-20250805");
    assert_eq!(message["content"][0]["type"], "text");
    assert_eq!(message["content"][0]["text"], "Hello");
    assert_eq!(message["stop_reason"], "end_turn");
    assert_eq!(message["usage"]["input_tokens"], 10);
}

#[test]
fn passthrough_header_lists_include_codex_proxy_parity_headers() {
    assert!(passthrough_request_headers().contains(&"x-oai-attestation"));
    assert!(passthrough_request_headers().contains(&"accept-encoding"));
    assert!(passthrough_request_headers().contains(&"openai-project"));
    assert!(passthrough_response_headers().contains(&"content-encoding"));
    assert!(passthrough_response_headers().contains(&"retry-after"));
    assert!(passthrough_response_headers().contains(&"x-codex-primary-used-percent"));
}

#[test]
fn responses_body_default_service_tier_maps_fast_to_priority() {
    let body = Bytes::from(
        json!({
            "model": "gpt-5.5",
            "input": "Hello"
        })
        .to_string(),
    );

    let (body, service_tier) =
        apply_default_service_tier_to_responses_body(body, Some("fast")).expect("body should map");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("body should parse");

    assert_eq!(
        service_tier.as_deref(),
        Some(FAST_SERVICE_TIER_REQUEST_VALUE)
    );
    assert_eq!(
        body["service_tier"].as_str(),
        Some(FAST_SERVICE_TIER_REQUEST_VALUE)
    );
}

#[test]
fn responses_body_applies_default_reasoning_effort_for_codex_models() {
    let body = Bytes::from(
        json!({
            "model": "gpt-5.5",
            "input": "Hello"
        })
        .to_string(),
    );

    let (body, reasoning_effort) =
        apply_default_reasoning_to_responses_body(body, DEFAULT_MODEL, DEFAULT_REASONING_EFFORT)
            .expect("body should map");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("body should parse");

    assert_eq!(reasoning_effort.as_deref(), Some(DEFAULT_REASONING_EFFORT));
    assert_eq!(
        body["reasoning"]["effort"].as_str(),
        Some(DEFAULT_REASONING_EFFORT)
    );
}

#[test]
fn responses_body_applies_configured_defaults_when_model_is_omitted() {
    let body = Bytes::from(
        json!({
            "input": "Hello"
        })
        .to_string(),
    );

    let (body, reasoning_effort) =
        apply_default_reasoning_to_responses_body(body, "gpt-5.4", "high")
            .expect("body should map");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("body should parse");

    assert_eq!(body["model"].as_str(), Some("gpt-5.4"));
    assert_eq!(reasoning_effort.as_deref(), Some("high"));
    assert_eq!(body["reasoning"]["effort"].as_str(), Some("high"));
}

#[test]
fn responses_body_applies_non_codex_default_model_without_reasoning() {
    let body = Bytes::from(
        json!({
            "input": "Hello"
        })
        .to_string(),
    );

    let (body, reasoning_effort) =
        apply_default_reasoning_to_responses_body(body, "gpt-4.1", DEFAULT_REASONING_EFFORT)
            .expect("body should map");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("body should parse");

    assert_eq!(body["model"].as_str(), Some("gpt-4.1"));
    assert_eq!(reasoning_effort, None);
    assert!(body.get("reasoning").is_none());
}

#[test]
fn responses_body_preserves_explicit_reasoning_effort() {
    let body = Bytes::from(
        json!({
            "model": "gpt-5.5",
            "input": "Hello",
            "reasoning": { "effort": "low" }
        })
        .to_string(),
    );

    let (body, reasoning_effort) =
        apply_default_reasoning_to_responses_body(body, DEFAULT_MODEL, DEFAULT_REASONING_EFFORT)
            .expect("body should map");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("body should parse");

    assert_eq!(reasoning_effort.as_deref(), Some("low"));
    assert_eq!(body["reasoning"]["effort"].as_str(), Some("low"));
}

#[test]
fn responses_body_default_service_tier_skips_unknown_models() {
    let body = Bytes::from(
        json!({
            "model": "gpt-4.1",
            "input": "Hello"
        })
        .to_string(),
    );

    let (body, service_tier) =
        apply_default_service_tier_to_responses_body(body, Some("fast")).expect("body should map");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("body should parse");

    assert_eq!(service_tier, None);
    assert!(body.get("service_tier").is_none());
}

#[test]
fn responses_body_skips_default_reasoning_effort_for_unknown_models() {
    let body = Bytes::from(
        json!({
            "model": "gpt-4.1",
            "input": "Hello"
        })
        .to_string(),
    );

    let (body, reasoning_effort) =
        apply_default_reasoning_to_responses_body(body, DEFAULT_MODEL, DEFAULT_REASONING_EFFORT)
            .expect("body should map");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("body should parse");

    assert_eq!(reasoning_effort, None);
    assert!(body.get("reasoning").is_none());
}

#[test]
fn stores_and_returns_cloudflare_cookies_for_chatgpt_hosts() {
    let store = ChatGptCloudflareCookieStore::default();
    let url = reqwest::Url::parse(CODEX_RESPONSES_URL).unwrap();
    let load_balancer = HeaderValue::from_static("__cflb=west; Path=/; Secure; HttpOnly");
    let cfuvid = HeaderValue::from_static("_cfuvid=visitor; Path=/; Secure; HttpOnly");
    let clearance = HeaderValue::from_static("cf_clearance=clearance; Path=/; Secure; HttpOnly");

    store.set_cookies(&mut [&load_balancer, &cfuvid, &clearance].into_iter(), &url);

    let mut cookies = store
        .cookies(&url)
        .and_then(|value| value.to_str().ok().map(str::to_string))
        .map(|header| {
            header
                .split("; ")
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    cookies.sort();

    assert_eq!(
        cookies,
        vec![
            "__cflb=west".to_string(),
            "_cfuvid=visitor".to_string(),
            "cf_clearance=clearance".to_string()
        ]
    );
}

#[test]
fn rejects_non_cloudflare_chatgpt_cookies() {
    let store = ChatGptCloudflareCookieStore::default();
    let url = reqwest::Url::parse(CODEX_RESPONSES_URL).unwrap();
    let cfuvid = HeaderValue::from_static("_cfuvid=visitor; Path=/; Secure; HttpOnly");
    let session = HeaderValue::from_static("chatgpt_session=secret; Path=/; Secure; HttpOnly");

    store.set_cookies(&mut [&cfuvid, &session].into_iter(), &url);

    assert_eq!(
        store
            .cookies(&url)
            .and_then(|value| value.to_str().ok().map(str::to_string)),
        Some("_cfuvid=visitor".to_string())
    );
}

#[test]
fn cloudflare_cookie_store_requires_https_chatgpt_hosts() {
    let store = ChatGptCloudflareCookieStore::default();
    let http_url = reqwest::Url::parse("http://chatgpt.com/backend-api/codex/responses")
        .expect("URL should parse");
    let openai_url =
        reqwest::Url::parse("https://api.openai.com/v1/responses").expect("URL should parse");
    let set_cookie = HeaderValue::from_static("_cfuvid=visitor; Path=/; Secure; HttpOnly");

    store.set_cookies(&mut std::iter::once(&set_cookie), &http_url);
    store.set_cookies(&mut std::iter::once(&set_cookie), &openai_url);

    assert_eq!(store.cookies(&http_url), None);
    assert_eq!(store.cookies(&openai_url), None);
}

#[test]
fn cloudflare_cookie_allowlist_is_narrow() {
    for name in [
        "__cf_bm",
        "__cflb",
        "__cfruid",
        "__cfseq",
        "__cfwaitingroom",
        "_cfuvid",
        "cf_clearance",
        "cf_ob_info",
        "cf_use_ob",
        "cf_chl_rc_i",
    ] {
        assert!(is_allowed_cloudflare_cookie_name(name));
    }

    for name in [
        "__Secure-next-auth.session-token",
        "chatgpt_session",
        "oai-auth-token",
        "not_cf_clearance",
    ] {
        assert!(!is_allowed_cloudflare_cookie_name(name));
    }
}

#[test]
fn chat_shim_builds_simple_text_responses_request() {
    let input = json!({
        "model": "gpt-5.4",
        "messages": [
            { "role": "system", "content": "Be concise." },
            { "role": "user", "content": "Hello" }
        ]
    });

    let (model, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(model, "gpt-5.4");
    assert_eq!(body["instructions"].as_str(), Some("Be concise."));
    assert_eq!(body["input"][0]["role"].as_str(), Some("user"));
    assert_eq!(body["input"][0]["content"].as_str(), Some("Hello"));
    assert_eq!(body["store"].as_bool(), Some(false));
}

#[test]
fn chat_shim_defaults_to_current_model_when_model_is_omitted() {
    let input = json!({
        "messages": [{ "role": "user", "content": "Hello" }]
    });

    let (model, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(DEFAULT_MODEL, "gpt-5.5");
    assert_eq!(model, DEFAULT_MODEL);
    assert_eq!(body["model"].as_str(), Some(DEFAULT_MODEL));
}

#[test]
fn chat_shim_uses_configured_default_model_when_model_is_omitted() {
    let input = json!({
        "messages": [{ "role": "user", "content": "Hello" }]
    });

    let (model, body) =
        build_responses_body_from_chat_with_defaults(&input, "gpt-5.4", "high", None)
            .expect("chat should translate");

    assert_eq!(model, "gpt-5.4");
    assert_eq!(body["model"].as_str(), Some("gpt-5.4"));
    assert_eq!(body["reasoning"]["effort"].as_str(), Some("high"));
}

#[test]
fn chat_shim_applies_default_reasoning_effort_for_codex_models() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [{ "role": "user", "content": "Hello" }]
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(
        body["reasoning"]["effort"].as_str(),
        Some(DEFAULT_REASONING_EFFORT)
    );
}

#[test]
fn chat_shim_preserves_explicit_reasoning_effort() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [{ "role": "user", "content": "Hello" }],
        "reasoning": { "effort": "low" }
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(body["reasoning"]["effort"].as_str(), Some("low"));
}

#[test]
fn model_catalog_advertises_codex_reasoning_metadata() {
    let model = model_catalog_entry("gpt-5.5", None, DEFAULT_REASONING_EFFORT);

    assert_eq!(model["id"].as_str(), Some("gpt-5.5"));
    assert_eq!(
        model["default_reasoning_level"].as_str(),
        Some(DEFAULT_REASONING_EFFORT)
    );
    assert_eq!(model["support_verbosity"].as_bool(), Some(true));
    assert!(
        model["supported_verbosities"]
            .as_array()
            .expect("supported verbosities should be an array")
            .iter()
            .any(|verbosity| verbosity.as_str() == Some("medium"))
    );
    assert!(
        model["supported_reasoning_levels"]
            .as_array()
            .expect("reasoning levels should be an array")
            .iter()
            .any(|level| level["effort"].as_str() == Some("xhigh"))
    );
    assert!(
        model["capabilities"]["supports"]["reasoning_effort"]
            .as_array()
            .expect("reasoning efforts should be an array")
            .iter()
            .any(|effort| effort.as_str() == Some("high"))
    );
    assert_eq!(
        model["service_tiers"][0]["id"].as_str(),
        Some(FAST_SERVICE_TIER_REQUEST_VALUE)
    );
    assert_eq!(model["service_tiers"][0]["name"].as_str(), Some("Fast"));
    assert!(
        model["additional_speed_tiers"]
            .as_array()
            .expect("speed tiers should be an array")
            .iter()
            .any(|tier| tier.as_str() == Some(FAST_SERVICE_TIER_ALIAS))
    );
    assert!(model.get("default_service_tier").is_none());
}

#[test]
fn model_catalog_advertises_configured_default_reasoning_effort() {
    let model = model_catalog_entry("gpt-5.5", None, "high");

    assert_eq!(model["default_reasoning_level"].as_str(), Some("high"));
}

#[test]
fn model_catalog_advertises_configured_default_service_tier() {
    let model = model_catalog_entry("gpt-5.5", Some("fast"), DEFAULT_REASONING_EFFORT);

    assert_eq!(
        model["default_service_tier"].as_str(),
        Some(FAST_SERVICE_TIER_REQUEST_VALUE)
    );
}

#[test]
fn model_catalog_does_not_overclaim_unknown_model_reasoning() {
    let model = model_catalog_entry("gpt-4.1", Some("fast"), DEFAULT_REASONING_EFFORT);

    assert!(model.get("supported_reasoning_levels").is_none());
    assert!(model.get("capabilities").is_none());
    assert!(model.get("service_tiers").is_none());
}

#[test]
fn chat_shim_maps_reasoning_and_verbosity_options() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [{ "role": "user", "content": "Hello" }],
        "reasoning_effort": "xhigh",
        "reasoning_summary": "auto",
        "verbosity": "low"
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(body["reasoning"]["effort"].as_str(), Some("xhigh"));
    assert_eq!(body["reasoning"]["summary"].as_str(), Some("auto"));
    assert_eq!(body["text"]["verbosity"].as_str(), Some("low"));
}

#[test]
fn chat_shim_maps_service_tier_aliases() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [{ "role": "user", "content": "Hello" }],
        "service_tier": "fast"
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(
        body["service_tier"].as_str(),
        Some(FAST_SERVICE_TIER_REQUEST_VALUE)
    );
}

#[test]
fn chat_shim_applies_default_service_tier_for_supported_models() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [{ "role": "user", "content": "Hello" }]
    });

    let (_, body) = build_responses_body_from_chat_with_service_tier(&input, Some("fast"))
        .expect("chat should translate");

    assert_eq!(
        body["service_tier"].as_str(),
        Some(FAST_SERVICE_TIER_REQUEST_VALUE)
    );
}

#[test]
fn chat_shim_does_not_apply_fast_default_to_unknown_models() {
    let input = json!({
        "model": "gpt-4.1",
        "messages": [{ "role": "user", "content": "Hello" }]
    });

    let (_, body) = build_responses_body_from_chat_with_service_tier(&input, Some("fast"))
        .expect("chat should translate");

    assert!(body.get("service_tier").is_none());
    assert!(body.get("reasoning").is_none());
}

#[test]
fn chat_shim_preserves_reasoning_object_and_accepts_camel_case_aliases() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [{ "role": "user", "content": "Hello" }],
        "reasoning": { "effort": "low", "summary": "concise" },
        "text": { "verbosity": "medium" },
        "reasoningEffort": "high",
        "reasoningSummary": "detailed",
        "textVerbosity": "high"
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(body["reasoning"]["effort"].as_str(), Some("high"));
    assert_eq!(body["reasoning"]["summary"].as_str(), Some("detailed"));
    assert_eq!(body["text"]["verbosity"].as_str(), Some("high"));
}

#[test]
fn chat_shim_maps_tools_to_responses_tools() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [{ "role": "user", "content": "Use the tool" }],
        "tools": [{
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "Look up a value",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    },
                    "required": ["query"]
                },
                "strict": true
            }
        }],
        "tool_choice": {
            "type": "function",
            "function": { "name": "lookup" }
        },
        "parallel_tool_calls": false
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(body["tools"][0]["type"].as_str(), Some("function"));
    assert_eq!(body["tools"][0]["name"].as_str(), Some("lookup"));
    assert_eq!(
        body["tools"][0]["parameters"]["properties"]["query"]["type"].as_str(),
        Some("string")
    );
    assert_eq!(body["tools"][0]["strict"].as_bool(), Some(true));
    assert_eq!(body["tool_choice"]["type"].as_str(), Some("function"));
    assert_eq!(body["tool_choice"]["name"].as_str(), Some("lookup"));
    assert_eq!(body["parallel_tool_calls"].as_bool(), Some(false));
}

#[test]
fn chat_shim_maps_tool_call_history_to_responses_input() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [
            { "role": "user", "content": "Use the tool" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_123",
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "arguments": "{\"query\":\"abc\"}"
                    }
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_123",
                "content": "{\"result\":\"ok\"}"
            }
        ]
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(body["input"][1]["type"].as_str(), Some("function_call"));
    assert_eq!(body["input"][1]["call_id"].as_str(), Some("call_123"));
    assert_eq!(body["input"][1]["name"].as_str(), Some("lookup"));
    assert_eq!(
        body["input"][1]["arguments"].as_str(),
        Some("{\"query\":\"abc\"}")
    );
    assert_eq!(
        body["input"][2]["type"].as_str(),
        Some("function_call_output")
    );
    assert_eq!(body["input"][2]["call_id"].as_str(), Some("call_123"));
    assert_eq!(
        body["input"][2]["output"].as_str(),
        Some("{\"result\":\"ok\"}")
    );
}

#[test]
fn chat_shim_maps_multimodal_content_parts_to_responses_input() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "What changed in this UI?" },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "data:image/png;base64,abc123",
                        "detail": "high"
                    }
                }
            ]
        }]
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(body["input"][0]["role"].as_str(), Some("user"));
    assert_eq!(
        body["input"][0]["content"][0]["type"].as_str(),
        Some("input_text")
    );
    assert_eq!(
        body["input"][0]["content"][0]["text"].as_str(),
        Some("What changed in this UI?")
    );
    assert_eq!(
        body["input"][0]["content"][1]["type"].as_str(),
        Some("input_image")
    );
    assert_eq!(
        body["input"][0]["content"][1]["image_url"].as_str(),
        Some("data:image/png;base64,abc123")
    );
    assert_eq!(
        body["input"][0]["content"][1]["detail"].as_str(),
        Some("high")
    );
}

#[test]
fn chat_shim_accepts_custom_labeled_tool_call_history() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [
            { "role": "user", "content": "Patch the file" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_patch",
                    "type": "custom",
                    "custom": {
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** End Patch\n"
                    }
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_patch",
                "content": "Exit code: 0"
            }
        ]
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(body["input"][1]["type"].as_str(), Some("function_call"));
    assert_eq!(body["input"][1]["call_id"].as_str(), Some("call_patch"));
    assert_eq!(body["input"][1]["name"].as_str(), Some("apply_patch"));
    assert_eq!(
        body["input"][1]["arguments"].as_str(),
        Some("*** Begin Patch\n*** End Patch\n")
    );
}

#[test]
fn chat_shim_accepts_response_shaped_tool_call_history() {
    let input = json!({
        "model": "gpt-5.5",
        "messages": [
            { "role": "user", "content": "Use the tool" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "call_id": "call_response_shape",
                    "type": "function_call",
                    "name": "lookup",
                    "arguments": { "query": "abc" }
                }]
            }
        ]
    });

    let (_, body) = build_responses_body_from_chat(&input).expect("chat should translate");

    assert_eq!(
        body["input"][1]["call_id"].as_str(),
        Some("call_response_shape")
    );
    assert_eq!(body["input"][1]["name"].as_str(), Some("lookup"));
    assert_eq!(
        body["input"][1]["arguments"].as_str(),
        Some("{\"query\":\"abc\"}")
    );
}

#[test]
fn chat_non_streaming_extracts_tool_calls_from_responses_output() {
    let response = json!({
        "output": [{
            "type": "function_call",
            "call_id": "call_123",
            "name": "lookup",
            "arguments": "{\"query\":\"abc\"}"
        }]
    });

    let (content, tool_calls) = extract_chat_message_from_responses(&response);

    assert!(content.is_empty());
    assert_eq!(tool_calls[0]["id"].as_str(), Some("call_123"));
    assert_eq!(tool_calls[0]["function"]["name"].as_str(), Some("lookup"));
    assert_eq!(
        tool_calls[0]["function"]["arguments"].as_str(),
        Some("{\"query\":\"abc\"}")
    );
}

#[test]
fn chat_stream_translates_responses_sse_to_chat_chunks() {
    let mut translator =
        ChatCompletionSseTranslator::new("chatcmpl-test".into(), "gpt-5.5".into(), 123);
    let mut frames = translator.initial_frames();
    frames.extend(
        translator.feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n"),
    );
    frames.extend(
        translator.feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n"),
    );
    frames.extend(
        translator
            .feed(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n"),
    );

    let output = sse_frames_to_string(frames);

    assert!(output.contains("\"object\":\"chat.completion.chunk\""));
    assert!(output.contains("\"role\":\"assistant\""));
    assert!(output.contains("\"content\":\"Hel\""));
    assert!(output.contains("\"content\":\"lo\""));
    assert!(output.contains("\"finish_reason\":\"stop\""));
    assert!(output.contains("data: [DONE]"));
}

#[test]
fn chat_stream_handles_split_event_named_sse_frames() {
    let mut translator =
        ChatCompletionSseTranslator::new("chatcmpl-test".into(), "gpt-5.5".into(), 123);

    assert!(
        translator
            .feed(b"event: response.output_text.delta\ndata: {\"delta\":\"Hel")
            .is_empty()
    );
    let frames = translator.feed(b"lo\"}\n\n");

    let output = sse_frames_to_string(frames);

    assert!(output.contains("\"content\":\"Hello\""));
}

#[test]
fn chat_stream_translates_function_call_items_to_tool_call_chunks() {
    let mut translator =
        ChatCompletionSseTranslator::new("chatcmpl-test".into(), "gpt-5.5".into(), 123);
    let mut frames = translator.initial_frames();
    frames.extend(translator.feed(
            b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_123\",\"name\":\"lookup\",\"arguments\":\"{\\\"query\\\":\\\"abc\\\"}\"}}\n\n",
        ));
    frames.extend(
        translator
            .feed(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n"),
    );

    let output = sse_frames_to_string(frames);

    assert!(output.contains("\"tool_calls\""));
    assert!(output.contains("\"id\":\"call_123\""));
    assert!(output.contains("\"name\":\"lookup\""));
    assert!(output.contains("\\\"query\\\":\\\"abc\\\""));
    assert!(output.contains("\"finish_reason\":\"tool_calls\""));
}

#[test]
fn chat_stream_finish_emits_done_when_upstream_ends_without_completed_event() {
    let mut translator =
        ChatCompletionSseTranslator::new("chatcmpl-test".into(), "gpt-5.5".into(), 123);
    let frames = translator.finish();

    let output = sse_frames_to_string(frames);

    assert!(output.contains("\"finish_reason\":\"stop\""));
    assert!(output.contains("data: [DONE]"));
}

#[test]
fn anthropic_stream_drains_multiple_sse_frames_from_one_buffer() {
    let mut translator = AnthropicSseTranslator::new("claude-opus-4-8".to_string());
    let mut buffer = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n".to_vec();

    let out = drain_anthropic_sse_buffer(&mut translator, &mut buffer);
    let out = String::from_utf8(out).expect("frames should be utf-8");

    assert!(buffer.is_empty());
    assert!(out.contains("event: message_start"));
    assert!(out.contains("\"text\":\"Hel\""));
    assert!(out.contains("\"text\":\"lo\""));
    assert!(out.contains("event: message_stop"));
}

#[test]
fn chat_shim_rejects_unsupported_features() {
    let with_unsupported_content_parts = json!({
        "model": "gpt-5.4",
        "messages": [{ "role": "user", "content": [{ "type": "audio", "audio": "abc" }] }]
    });
    assert!(matches!(
        build_responses_body_from_chat(&with_unsupported_content_parts),
        Err(ProxyError::BadRequest(_))
    ));

    let with_invalid_reasoning = json!({
        "model": "gpt-5.4",
        "messages": [{ "role": "user", "content": "Hello" }],
        "reasoning": "high"
    });
    assert!(matches!(
        build_responses_body_from_chat(&with_invalid_reasoning),
        Err(ProxyError::BadRequest(_))
    ));
}

fn sse_frames_to_string(frames: Vec<Result<Bytes, std::io::Error>>) -> String {
    frames
        .into_iter()
        .map(|frame| {
            String::from_utf8(frame.expect("frame should be ok").to_vec())
                .expect("frame should be utf-8")
        })
        .collect::<Vec<_>>()
        .join("")
}
