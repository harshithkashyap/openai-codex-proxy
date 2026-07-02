use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use axum::middleware::{self, Next};
use axum::routing::{get, post};
use serde_json::json;
use tokio::sync::oneshot;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::{Level, info, warn};

use crate::anthropic::{anthropic_count_tokens, anthropic_messages};
use crate::auth::AuthManager;
use crate::chat::chat_completions;
use crate::config::{build_version, configured_default_model, configured_default_reasoning_effort};
use crate::cookies::with_chatgpt_cloudflare_cookie_store;
use crate::errors::response_json;
use crate::logging::{append_compat_log, compat_log_file, file_request_log};
use crate::models::{default_advertised_models, health, models_handler};
use crate::responses::{responses_compact_proxy, responses_proxy};
use crate::service_tier::configured_service_tier;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) auth: Arc<AuthManager>,
    pub(crate) client: reqwest::Client,
    pub(crate) local_api_key: Option<String>,
    pub(crate) models: Vec<String>,
    pub(crate) service_tier: Option<String>,
    pub(crate) default_model: String,
    pub(crate) default_reasoning_effort: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ServerConfig {
    pub(crate) addr: SocketAddr,
    pub(crate) local_api_key: Option<String>,
    pub(crate) allow_no_local_api_key: bool,
    pub(crate) models: Vec<String>,
    pub(crate) service_tier: Option<String>,
    pub(crate) default_model: Option<String>,
    pub(crate) default_reasoning_effort: Option<String>,
}
pub(crate) async fn require_local_api_key(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let Some(expected) = state.local_api_key.as_deref() else {
        return next.run(req).await;
    };

    if local_api_key_authorized(req.headers(), expected) {
        next.run(req).await
    } else {
        response_json(
            StatusCode::UNAUTHORIZED,
            json!({"error": {"message": "invalid local API key", "type": "auth_error"}}),
        )
    }
}

pub(crate) fn local_api_key_authorized(headers: &HeaderMap, expected: &str) -> bool {
    let bearer_ok = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|actual| actual == expected);
    let x_api_key_ok = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|actual| actual == expected);
    bearer_ok || x_api_key_ok
}
pub(crate) async fn serve(config: ServerConfig) -> Result<()> {
    serve_until_shutdown(
        config,
        async {
            let _ = tokio::signal::ctrl_c().await;
        },
        None,
    )
    .await
}

pub(crate) async fn serve_until_shutdown<F>(
    config: ServerConfig,
    shutdown: F,
    ready: Option<oneshot::Sender<SocketAddr>>,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let addr = config.addr;
    if !addr.ip().is_loopback() {
        warn!("binding to a non-loopback address");
        if config.local_api_key.is_none() {
            return Err(anyhow!(
                "--local-api-key is required when binding outside loopback"
            ));
        }
    }
    if config.local_api_key.is_none() && !config.allow_no_local_api_key {
        return Err(anyhow!(
            "--local-api-key is required; pass --allow-no-local-api-key only for trusted loopback-only use"
        ));
    }
    if config.local_api_key.is_none() && config.allow_no_local_api_key {
        warn!("no local API key configured; any local process can use the proxy");
    }

    let auth = Arc::new(AuthManager::new().await?);
    if !auth.is_logged_in().await {
        return Err(anyhow!(
            "not logged in; run `openai-codex-proxy login` first"
        ));
    }

    let models = if config.models.is_empty() {
        default_advertised_models()
    } else {
        config.models
    };
    let state = Arc::new(AppState {
        auth,
        client: with_chatgpt_cloudflare_cookie_store(reqwest::Client::builder())
            .user_agent(format!("openai-codex-proxy/{}", build_version()))
            .timeout(Duration::from_secs(300))
            .build()?,
        local_api_key: config.local_api_key,
        models,
        service_tier: configured_service_tier(config.service_tier),
        default_model: configured_default_model(config.default_model),
        default_reasoning_effort: configured_default_reasoning_effort(
            config.default_reasoning_effort,
        ),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models_handler))
        .route("/v1/responses", post(responses_proxy))
        .route("/v1/responses/compact", post(responses_compact_proxy))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/messages/count_tokens", post(anthropic_count_tokens))
        .layer(middleware::from_fn(file_request_log))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_local_api_key,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(DefaultOnRequest::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr()?;
    info!("serving OpenAI-compatible proxy on http://{bound_addr}");
    if let Some(path) = compat_log_file() {
        info!("writing compatibility trace log to {}", path.display());
        append_compat_log(
            "proxy.start",
            json!({
                "addr": bound_addr.to_string(),
                "log_file": path.display().to_string(),
                "models": state.models.clone(),
                "service_tier": state.service_tier.clone(),
                "default_model": state.default_model.clone(),
                "default_reasoning_effort": state.default_reasoning_effort.clone(),
            }),
        );
    }
    if let Some(ready) = ready {
        let _ = ready.send(bound_addr);
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
