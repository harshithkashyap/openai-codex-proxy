pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const ISSUER: &str = "https://auth.openai.com";
pub(crate) const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub(crate) const CODEX_RESPONSES_COMPACT_URL: &str =
    "https://chatgpt.com/backend-api/codex/responses/compact";
pub(crate) const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
pub(crate) const DEVICE_USER_CODE_URL: &str =
    "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub(crate) const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub(crate) const DEVICE_PAGE_URL: &str = "https://auth.openai.com/codex/device";
pub(crate) const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub(crate) const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 1455;
pub(crate) const TOKEN_REFRESH_WINDOW_MINUTES: i64 = 5;
pub(crate) const TOKEN_REFRESH_INTERVAL_DAYS: i64 = 8;
pub(crate) const AUTH_MODE_CHATGPT: &str = "chatgpt";
pub(crate) const DEFAULT_MODEL: &str = "gpt-5.5";
pub(crate) const DEFAULT_ADVERTISED_MODELS: &str =
    "gpt-5.5,gpt-5.4,gpt-5.4-mini,gpt-5.3-codex-spark";
pub(crate) const DEFAULT_REASONING_EFFORT: &str = "xhigh";
pub(crate) const SUPPORTED_REASONING_EFFORTS: &[&str] =
    &["minimal", "low", "medium", "high", "xhigh"];
pub(crate) const DEFAULT_VERBOSITY: &str = "medium";
pub(crate) const SUPPORTED_VERBOSITIES: &[&str] = &["low", "medium", "high"];
pub(crate) const ORIGINATOR: &str = "openai_codex_proxy";
pub(crate) const DEFAULT_COMPAT_LOG_FILE: &str = "/tmp/openai-codex-proxy.log";
pub(crate) const UNSUPPORTED_CHAT_FIELDS: &[&str] = &[
    "functions",
    "function_call",
    "response_format",
    "logprobs",
    "top_logprobs",
    "audio",
    "modalities",
    "prediction",
];
