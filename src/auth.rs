use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use base64::Engine;
use chrono::{DateTime, Utc};
use reqwest::header::CONTENT_TYPE;
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::config::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct StoredAuth {
    pub(crate) id_token: Option<String>,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) account_id: Option<String>,
    pub(crate) plan_type: Option<String>,
    pub(crate) last_refresh: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auth_mode: Option<String>,
    #[serde(
        rename = "OPENAI_API_KEY",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tokens: Option<CodexTokenData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_refresh: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct CodexTokenData {
    pub(crate) id_token: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AuthState {
    pub(crate) file: AuthDotJson,
    pub(crate) auth: StoredAuth,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TokenMetadata {
    pub(crate) account_id: Option<String>,
    pub(crate) plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JwtClaims {
    pub(crate) exp: Option<i64>,
    #[serde(rename = "https://api.openai.com/auth")]
    pub(crate) auth: Option<JwtAuthClaims>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JwtAuthClaims {
    pub(crate) chatgpt_account_id: Option<String>,
    pub(crate) chatgpt_plan_type: Option<String>,
}

#[derive(Clone)]
pub(crate) struct AuthManager {
    pub(crate) path: PathBuf,
    pub(crate) client: reqwest::Client,
    pub(crate) inner: Arc<Mutex<Option<AuthState>>>,
}
impl AuthManager {
    pub(crate) async fn new() -> Result<Self> {
        let path = auth_file_path()?;
        let client = reqwest::Client::builder()
            .user_agent(format!("openai-codex-proxy/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()?;
        let cached = load_auth_from_path(&path).await?;
        Ok(Self {
            path,
            client,
            inner: Arc::new(Mutex::new(cached)),
        })
    }

    pub(crate) async fn cached(&self) -> Option<StoredAuth> {
        self.inner
            .lock()
            .await
            .as_ref()
            .map(|state| state.auth.clone())
    }

    pub(crate) async fn is_logged_in(&self) -> bool {
        self.cached().await.is_some()
    }

    pub(crate) async fn save_and_cache(&self, auth: StoredAuth) -> Result<()> {
        let state = AuthState::from_stored_auth(auth)?;
        save_auth_to_path(&self.path, &state).await?;
        *self.inner.lock().await = Some(state);
        Ok(())
    }

    pub(crate) async fn clear(&self) -> Result<()> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).context("failed to remove ChatGPT auth file"),
        }
        *self.inner.lock().await = None;
        Ok(())
    }

    pub(crate) async fn access_for_request(&self) -> Result<(String, String)> {
        let mut guard = self.inner.lock().await;
        match load_auth_from_path(&self.path).await? {
            Some(disk_state) => *guard = Some(disk_state),
            None => *guard = None,
        }

        let state = guard
            .clone()
            .ok_or_else(|| anyhow!("not logged in; run `openai-codex-proxy login`"))?;
        let auth = state.auth.clone();

        if !should_refresh(&auth) {
            let account_id = auth
                .account_id
                .clone()
                .ok_or_else(|| anyhow!("auth is missing ChatGPT account id; re-run login"))?;
            return Ok((auth.access_token, account_id));
        }

        info!("refreshing ChatGPT OAuth access token");
        let refreshed = refresh_tokens(&self.client, &state).await?;
        save_auth_to_path(&self.path, &refreshed).await?;
        *guard = Some(refreshed.clone());
        let account_id = refreshed
            .auth
            .account_id
            .clone()
            .ok_or_else(|| anyhow!("refreshed auth is missing ChatGPT account id"))?;
        Ok((refreshed.auth.access_token, account_id))
    }
}

pub(crate) fn auth_file_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CODEX_PROXY_AUTH_FILE")
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    if let Ok(codex_home) = std::env::var("CODEX_HOME")
        && !codex_home.trim().is_empty()
    {
        return Ok(PathBuf::from(codex_home).join("auth.json"));
    }

    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine a home directory"))?;
    Ok(home.join(".codex").join("auth.json"))
}

pub(crate) async fn load_auth_from_path(path: &Path) -> Result<Option<AuthState>> {
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => parse_auth_json(&raw).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("failed to read auth.json"),
    }
}

pub(crate) fn parse_auth_json(raw: &str) -> Result<AuthState> {
    let value: Value = serde_json::from_str(raw).context("failed to parse auth.json")?;
    if value.get("tokens").is_some() {
        let file: AuthDotJson =
            serde_json::from_value(value).context("failed to parse Codex auth.json")?;
        AuthState::from_auth_dot_json(file)
    } else {
        let legacy: StoredAuth =
            serde_json::from_value(value).context("failed to parse legacy proxy auth.json")?;
        AuthState::from_stored_auth(legacy)
    }
}

pub(crate) async fn save_auth_to_path(path: &Path, state: &AuthState) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let path = path.to_path_buf();
    let data = serde_json::to_vec_pretty(&state.file)?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&path)?;
        file.write_all(&data)?;
        file.write_all(b"\n")?;
        Ok(())
    })
    .await??;
    Ok(())
}

impl AuthState {
    pub(crate) fn from_auth_dot_json(mut file: AuthDotJson) -> Result<Self> {
        let tokens = file
            .tokens
            .as_mut()
            .ok_or_else(|| anyhow!("auth.json does not contain ChatGPT tokens"))?;
        let metadata = token_metadata(tokens.id_token.as_str(), tokens.access_token.as_str());
        if let Some(account_id) = metadata.account_id.clone() {
            tokens.account_id = Some(account_id);
        }
        let auth = StoredAuth {
            id_token: Some(tokens.id_token.clone()).filter(|value| !value.is_empty()),
            access_token: tokens.access_token.clone(),
            refresh_token: tokens.refresh_token.clone(),
            account_id: tokens.account_id.clone().or(metadata.account_id),
            plan_type: metadata.plan_type,
            last_refresh: file.last_refresh,
        };
        Ok(Self { file, auth })
    }

    pub(crate) fn from_stored_auth(mut auth: StoredAuth) -> Result<Self> {
        hydrate_metadata(&mut auth);
        let id_token = auth.id_token.clone().unwrap_or_default();
        let mut file = AuthDotJson {
            auth_mode: Some(AUTH_MODE_CHATGPT.to_string()),
            tokens: Some(CodexTokenData {
                id_token,
                access_token: auth.access_token.clone(),
                refresh_token: auth.refresh_token.clone(),
                account_id: auth.account_id.clone(),
            }),
            last_refresh: auth.last_refresh,
            ..Default::default()
        };
        let state = Self::from_auth_dot_json(file.clone())?;
        file.tokens = state.file.tokens.clone();
        Ok(Self { file, ..state })
    }
}

pub(crate) fn decode_claims(jwt: &str) -> Option<JwtClaims> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _sig = parts.next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn hydrate_metadata(auth: &mut StoredAuth) {
    let metadata = token_metadata(auth.id_token.as_deref().unwrap_or(""), &auth.access_token);
    if metadata.account_id.is_some() {
        auth.account_id = metadata.account_id;
    }
    if metadata.plan_type.is_some() {
        auth.plan_type = metadata.plan_type;
    }
}

pub(crate) fn token_metadata(id_token: &str, access_token: &str) -> TokenMetadata {
    let claims = (!id_token.is_empty())
        .then(|| decode_claims(id_token))
        .flatten()
        .or_else(|| decode_claims(access_token));
    let Some(claims) = claims else {
        return TokenMetadata::default();
    };
    let Some(auth_claims) = claims.auth else {
        return TokenMetadata::default();
    };
    TokenMetadata {
        account_id: auth_claims.chatgpt_account_id,
        plan_type: auth_claims.chatgpt_plan_type,
    }
}

pub(crate) fn should_refresh(auth: &StoredAuth) -> bool {
    if let Some(claims) = decode_claims(&auth.access_token)
        && let Some(exp) = claims.exp
        && let Some(expires_at) = DateTime::<Utc>::from_timestamp(exp, 0)
    {
        return expires_at <= Utc::now() + chrono::Duration::minutes(TOKEN_REFRESH_WINDOW_MINUTES);
    }

    match auth.last_refresh {
        Some(last_refresh) => {
            last_refresh < Utc::now() - chrono::Duration::days(TOKEN_REFRESH_INTERVAL_DAYS)
        }
        None => false,
    }
}

#[derive(Deserialize)]
pub(crate) struct DeviceUserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(deserialize_with = "deserialize_interval")]
    interval: u64,
}

pub(crate) fn deserialize_interval<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(s) => s.trim().parse().map_err(serde::de::Error::custom),
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("invalid interval")),
        _ => Err(serde::de::Error::custom("invalid interval")),
    }
}

#[derive(Deserialize)]
pub(crate) struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
pub(crate) struct TokenExchangeResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

pub(crate) struct PkceCodes {
    pub(crate) verifier: String,
    pub(crate) challenge: String,
}

#[derive(Clone)]
pub(crate) struct OAuthCallbackState {
    expected_state: String,
    sender: Arc<Mutex<Option<oneshot::Sender<OAuthCallbackResult>>>>,
}

#[derive(Debug)]
pub(crate) enum OAuthCallbackResult {
    Code(String),
    Error(String),
}

#[derive(Debug, Deserialize)]
pub(crate) struct OAuthCallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub(crate) fn random_urlsafe(byte_len: usize) -> Result<String> {
    let rng = SystemRandom::new();
    let mut bytes = vec![0_u8; byte_len];
    rng.fill(&mut bytes)
        .map_err(|_| anyhow!("failed to generate secure random bytes"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn pkce_challenge(verifier: &str) -> String {
    let hash = digest::digest(&digest::SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash.as_ref())
}

pub(crate) fn generate_pkce() -> Result<PkceCodes> {
    let verifier = random_urlsafe(32)?;
    let challenge = pkce_challenge(&verifier);
    Ok(PkceCodes {
        verifier,
        challenge,
    })
}

pub(crate) fn build_authorize_url(
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!("{ISSUER}/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", ORIGINATOR);
    Ok(url.to_string())
}

#[cfg(target_os = "macos")]
pub(crate) fn browser_open_command(url: &str) -> Option<ProcessCommand> {
    let mut command = ProcessCommand::new("open");
    command.arg(url);
    Some(command)
}

#[cfg(target_os = "windows")]
pub(crate) fn browser_open_command(url: &str) -> Option<ProcessCommand> {
    let mut command = ProcessCommand::new("cmd");
    command.args(["/C", "start", "", url]);
    Some(command)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn browser_open_command(url: &str) -> Option<ProcessCommand> {
    let mut command = ProcessCommand::new("xdg-open");
    command.arg(url);
    Some(command)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    all(unix, not(target_os = "macos"))
)))]
pub(crate) fn browser_open_command(url: &str) -> Option<ProcessCommand> {
    let _ = url;
    None
}

pub(crate) fn try_open_browser(url: &str) -> bool {
    if std::env::var_os("CODEX_PROXY_NO_OPEN_BROWSER").is_some() {
        return false;
    }

    browser_open_command(url)
        .and_then(|mut command| command.spawn().ok())
        .is_some()
}

pub(crate) async fn send_oauth_callback_result(
    state: &OAuthCallbackState,
    result: OAuthCallbackResult,
) {
    if let Some(sender) = state.sender.lock().await.take() {
        let _ = sender.send(result);
    }
}

pub(crate) async fn oauth_callback(
    State(state): State<OAuthCallbackState>,
    Query(params): Query<OAuthCallbackParams>,
) -> Html<String> {
    if let Some(error) = params.error {
        let message = params.error_description.unwrap_or(error);
        send_oauth_callback_result(&state, OAuthCallbackResult::Error(message)).await;
        return Html(
            "<!doctype html><title>OpenAI Codex Proxy</title><h1>Login failed</h1><p>Return to OpenAI Codex Proxy for details.</p>"
                .to_string(),
        );
    }

    let Some(code) = params.code else {
        send_oauth_callback_result(
            &state,
            OAuthCallbackResult::Error(
                "OAuth callback did not include an authorization code".to_string(),
            ),
        )
        .await;
        return Html(
            "<!doctype html><title>OpenAI Codex Proxy</title><h1>Login failed</h1><p>The callback did not include an authorization code.</p>"
                .to_string(),
        );
    };

    if params.state.as_deref() != Some(state.expected_state.as_str()) {
        send_oauth_callback_result(
            &state,
            OAuthCallbackResult::Error("OAuth callback state did not match".to_string()),
        )
        .await;
        return Html(
            "<!doctype html><title>OpenAI Codex Proxy</title><h1>Login failed</h1><p>The OAuth state did not match.</p>"
                .to_string(),
        );
    }

    send_oauth_callback_result(&state, OAuthCallbackResult::Code(code)).await;
    Html(
        "<!doctype html><title>OpenAI Codex Proxy</title><h1>Login complete</h1><p>You can close this window and return to OpenAI Codex Proxy.</p>"
            .to_string(),
    )
}

pub(crate) async fn oauth_cancel(State(state): State<OAuthCallbackState>) -> Html<String> {
    send_oauth_callback_result(
        &state,
        OAuthCallbackResult::Error("OAuth login was cancelled".to_string()),
    )
    .await;
    Html(
        "<!doctype html><title>OpenAI Codex Proxy</title><h1>Login cancelled</h1><p>You can close this window.</p>"
            .to_string(),
    )
}

pub(crate) async fn exchange_authorization_code(
    client: &reqwest::Client,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenExchangeResponse> {
    client
        .post(format!("{ISSUER}/oauth/token"))
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            urlencoding::encode(code),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(CLIENT_ID),
            urlencoding::encode(code_verifier),
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("failed to parse token exchange response")
}

pub(crate) async fn save_token_exchange(
    auth: &AuthManager,
    exchanged: TokenExchangeResponse,
) -> Result<()> {
    let mut stored = StoredAuth {
        id_token: Some(exchanged.id_token),
        access_token: exchanged.access_token,
        refresh_token: exchanged.refresh_token,
        account_id: None,
        plan_type: None,
        last_refresh: Some(Utc::now()),
    };
    hydrate_metadata(&mut stored);
    auth.save_and_cache(stored).await?;
    println!(
        "Logged in. Tokens are stored locally at {}",
        auth.path.display()
    );
    Ok(())
}

pub(crate) async fn login_browser(auth: &AuthManager, callback_port: u16) -> Result<()> {
    let pkce = generate_pkce()?;
    let state = random_urlsafe(32)?;
    let redirect_uri = format!("http://localhost:{callback_port}/auth/callback");
    let authorize_url = build_authorize_url(&redirect_uri, &pkce, &state)?;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", callback_port))
        .await
        .with_context(|| format!("failed to bind OAuth callback server on port {callback_port}"))?;
    let (result_tx, result_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let callback_state = OAuthCallbackState {
        expected_state: state,
        sender: Arc::new(Mutex::new(Some(result_tx))),
    };
    let app = Router::new()
        .route("/auth/callback", get(oauth_callback))
        .route("/cancel", get(oauth_cancel))
        .with_state(callback_state);
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    println!("\nOpen this URL and sign in:\n  {authorize_url}\n");
    if try_open_browser(&authorize_url) {
        println!("Waiting for browser authorization...\n");
    } else {
        println!("Could not open a browser automatically. Open the URL above manually.\n");
        println!("Waiting for browser authorization...\n");
    }

    let callback_result = tokio::time::timeout(Duration::from_secs(5 * 60), result_rx).await;
    let _ = shutdown_tx.send(());
    match server.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!("OAuth callback server stopped with error: {err}"),
        Err(err) => return Err(anyhow!("OAuth callback server task failed: {err}")),
    }

    let code = match callback_result {
        Ok(Ok(OAuthCallbackResult::Code(code))) => code,
        Ok(Ok(OAuthCallbackResult::Error(message))) => return Err(anyhow!(message)),
        Ok(Err(_)) => {
            return Err(anyhow!(
                "OAuth callback channel closed before login completed"
            ));
        }
        Err(_) => return Err(anyhow!("browser login timed out")),
    };

    let exchanged =
        exchange_authorization_code(&auth.client, &code, &redirect_uri, &pkce.verifier).await?;
    save_token_exchange(auth, exchanged).await
}

pub(crate) async fn login_device(auth: &AuthManager) -> Result<()> {
    let user_code_resp: DeviceUserCodeResponse = auth
        .client
        .post(DEVICE_USER_CODE_URL)
        .json(&json!({ "client_id": CLIENT_ID }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    println!("\nOpen this URL and sign in:\n  {DEVICE_PAGE_URL}\n");
    println!(
        "Enter this one-time code:\n  {}\n",
        user_code_resp.user_code
    );
    println!("Waiting for authorization...\n");

    let started = std::time::Instant::now();
    let max_wait = Duration::from_secs(15 * 60);
    let token_resp = loop {
        if started.elapsed() > max_wait {
            return Err(anyhow!("device login timed out"));
        }
        let resp = auth
            .client
            .post(DEVICE_TOKEN_URL)
            .json(&json!({
                "device_auth_id": user_code_resp.device_auth_id,
                "user_code": user_code_resp.user_code,
            }))
            .send()
            .await?;

        if resp.status().is_success() {
            break resp.json::<DeviceTokenResponse>().await?;
        }

        if resp.status() != reqwest::StatusCode::FORBIDDEN
            && resp.status() != reqwest::StatusCode::NOT_FOUND
        {
            return Err(anyhow!("device auth failed with status {}", resp.status()));
        }
        tokio::time::sleep(Duration::from_secs(user_code_resp.interval.max(1))).await;
    };

    let exchanged = exchange_authorization_code(
        &auth.client,
        &token_resp.authorization_code,
        DEVICE_REDIRECT_URI,
        &token_resp.code_verifier,
    )
    .await?;

    save_token_exchange(auth, exchanged).await
}

pub(crate) async fn refresh_tokens(
    client: &reqwest::Client,
    current: &AuthState,
) -> Result<AuthState> {
    let resp = client
        .post(REFRESH_URL)
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": current.auth.refresh_token,
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("token refresh failed: {status}: {body}"));
    }

    #[derive(Deserialize)]
    struct RefreshResponse {
        id_token: Option<String>,
        access_token: Option<String>,
        refresh_token: Option<String>,
    }

    let refresh: RefreshResponse = resp.json().await?;
    let access_token = refresh
        .access_token
        .ok_or_else(|| anyhow!("refresh response did not include access_token"))?;
    let mut file = current.file.clone();
    file.auth_mode
        .get_or_insert_with(|| AUTH_MODE_CHATGPT.to_string());
    file.last_refresh = Some(Utc::now());
    let tokens = file
        .tokens
        .as_mut()
        .ok_or_else(|| anyhow!("auth.json does not contain ChatGPT tokens"))?;
    if let Some(id_token) = refresh.id_token {
        tokens.id_token = id_token;
    }
    tokens.access_token = access_token;
    if let Some(refresh_token) = refresh.refresh_token {
        tokens.refresh_token = refresh_token;
    }

    let metadata = token_metadata(tokens.id_token.as_str(), tokens.access_token.as_str());
    if metadata.account_id.is_some() {
        tokens.account_id = metadata.account_id;
    }
    AuthState::from_auth_dot_json(file)
}
