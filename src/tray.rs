use std::net::SocketAddr;

use anyhow::Result;

#[derive(Clone, Debug)]
pub(crate) struct TrayConfig {
    pub(crate) addr: SocketAddr,
    pub(crate) local_api_key: Option<String>,
    pub(crate) allow_no_local_api_key: bool,
    pub(crate) models: Vec<String>,
    pub(crate) service_tier: Option<String>,
}

pub(crate) async fn run_tray(config: TrayConfig) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::run(config).await
    }

    #[cfg(not(target_os = "linux"))]
    {
        let TrayConfig {
            addr,
            local_api_key,
            allow_no_local_api_key,
            models,
            service_tier,
        } = config;
        let _ = (
            addr,
            local_api_key,
            allow_no_local_api_key,
            models,
            service_tier,
        );
        Err(anyhow::anyhow!(
            "system tray mode is currently supported on Linux desktops with StatusNotifier/AppIndicator support"
        ))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Write;
    use std::process::Stdio;

    use anyhow::{Context, Result};
    use ksni::menu::StandardItem;
    use ksni::{MenuItem, Status, ToolTip, Tray, TrayMethods};
    use tokio::sync::{mpsc, oneshot};
    use tokio::task::JoinHandle;

    use super::TrayConfig;
    use crate::auth::{AuthManager, StoredAuth, login_browser};
    use crate::config::{DEFAULT_OAUTH_CALLBACK_PORT, build_version};
    use crate::local_config::{LocalApiKeySource, ensure_local_api_key};
    use crate::logging::compat_log_file;
    use crate::server::serve_until_shutdown;

    pub(super) async fn run(mut config: TrayConfig) -> Result<()> {
        let local_key = ensure_local_api_key(config.local_api_key.clone()).await?;
        config.local_api_key = Some(local_key.value.clone());
        config.allow_no_local_api_key = false;

        let auth = AuthManager::new().await?;
        let auth_status = auth_status_from(auth.cached().await);
        let initial_message = match local_key.source {
            LocalApiKeySource::Generated => format!(
                "Generated a local proxy API key at {}",
                local_key.config_path.display()
            ),
            LocalApiKeySource::Stored => "Loaded saved local proxy API key".into(),
            LocalApiKeySource::Provided => "Using configured local proxy API key".into(),
        };
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let config_for_server = config.clone();
        let tray = ProxyTray {
            config,
            status: ProxyStatus::Stopped,
            auth_status,
            local_api_key: local_key.value,
            last_message: Some(initial_message),
            command_tx: command_tx.clone(),
        };
        let handle = tray.spawn().await.context(
            "failed to create Linux system tray item; install/enable StatusNotifier/AppIndicator support",
        )?;

        let mut running: Option<RunningServer> = None;
        let mut stop_requested = false;

        while let Some(command) = command_rx.recv().await {
            match command {
                TrayCommand::Start => {
                    if running.is_some() {
                        set_last_message(&handle, "Proxy is already starting or running".into())
                            .await;
                        continue;
                    }
                    let start_auth = AuthManager::new().await?;
                    let start_auth_status = auth_status_from(start_auth.cached().await);
                    if !start_auth_status.is_connected() {
                        set_auth_status(&handle, AuthStatus::Disconnected).await;
                        set_last_message(
                            &handle,
                            "Sign in to ChatGPT before starting the proxy".into(),
                        )
                        .await;
                        continue;
                    }
                    set_auth_status(&handle, start_auth_status).await;

                    stop_requested = false;
                    let (shutdown_tx, shutdown_rx) = oneshot::channel();
                    let (ready_tx, ready_rx) = oneshot::channel();
                    let server_config = config_for_server.clone();
                    let requested_addr = server_config.addr;
                    let server_command_tx = command_tx.clone();

                    let task = tokio::spawn(async move {
                        let result = serve_until_shutdown(
                            server_config.addr,
                            server_config.local_api_key,
                            server_config.allow_no_local_api_key,
                            server_config.models,
                            server_config.service_tier,
                            async {
                                let _ = shutdown_rx.await;
                            },
                            Some(ready_tx),
                        )
                        .await
                        .map_err(|err| err.to_string());
                        let _ = server_command_tx.send(TrayCommand::ServerExited(result));
                    });

                    let ready_command_tx = command_tx.clone();
                    tokio::spawn(async move {
                        if let Ok(addr) = ready_rx.await {
                            let _ = ready_command_tx.send(TrayCommand::ServerReady(addr));
                        }
                    });

                    running = Some(RunningServer {
                        shutdown: Some(shutdown_tx),
                        task,
                    });
                    set_tray(
                        &handle,
                        ProxyStatus::Starting,
                        Some(format!("Starting proxy on http://{requested_addr}")),
                    )
                    .await;
                }
                TrayCommand::Stop => {
                    stop_requested = true;
                    if request_stop(&mut running, &handle, "Stopping proxy".into()).await {
                        continue;
                    }

                    stop_requested = false;
                    set_tray(
                        &handle,
                        ProxyStatus::Stopped,
                        Some("Proxy is already stopped".into()),
                    )
                    .await;
                }
                TrayCommand::Login => {
                    set_auth_status(&handle, AuthStatus::LoggingIn).await;
                    set_last_message(&handle, "Opening ChatGPT login in your browser".into()).await;
                    let login_command_tx = command_tx.clone();
                    tokio::spawn(async move {
                        let result = async {
                            let auth = AuthManager::new().await?;
                            login_browser(&auth, DEFAULT_OAUTH_CALLBACK_PORT).await?;
                            Ok::<_, anyhow::Error>(auth_status_from(auth.cached().await))
                        }
                        .await
                        .map_err(|err| err.to_string());
                        let _ = login_command_tx.send(TrayCommand::LoginFinished(result));
                    });
                }
                TrayCommand::LoginFinished(result) => match result {
                    Ok(status) => {
                        let message = format!("ChatGPT {}", status.label().to_lowercase());
                        set_auth_status(&handle, status).await;
                        set_last_message(&handle, message).await;
                    }
                    Err(message) => {
                        set_auth_status(&handle, auth_status_from(auth.cached().await)).await;
                        set_last_message(
                            &handle,
                            format!("ChatGPT login failed: {}", clip(&message, 120)),
                        )
                        .await;
                    }
                },
                TrayCommand::Logout => {
                    let stopped = if running.is_some() {
                        stop_requested = true;
                        request_stop(&mut running, &handle, "Stopping proxy before logout".into())
                            .await
                    } else {
                        stop_requested = false;
                        false
                    };

                    match auth.clear().await {
                        Ok(()) => {
                            set_auth_status(&handle, AuthStatus::Disconnected).await;
                            let message = if stopped {
                                "Logged out of ChatGPT; proxy is stopping"
                            } else {
                                "Logged out of ChatGPT"
                            };
                            set_last_message(&handle, message.into()).await;
                        }
                        Err(err) => {
                            set_last_message(
                                &handle,
                                format!("ChatGPT logout failed: {}", clip(&err.to_string(), 120)),
                            )
                            .await;
                        }
                    }
                }
                TrayCommand::CopyBaseUrl => {
                    let message = copy_to_clipboard(&base_url(config_for_server.addr), "base URL");
                    set_last_message(&handle, message).await;
                }
                TrayCommand::CopyApiKey => {
                    let key = config_for_server.local_api_key.clone().unwrap_or_default();
                    let message = copy_to_clipboard(&key, "local API key");
                    set_last_message(&handle, message).await;
                }
                TrayCommand::CopyClientSettings => {
                    let key = config_for_server.local_api_key.clone().unwrap_or_default();
                    let settings = client_settings(config_for_server.addr, &key);
                    let message = copy_to_clipboard(&settings, "client settings");
                    set_last_message(&handle, message).await;
                }
                TrayCommand::OpenLogs => {
                    let message = open_logs();
                    set_last_message(&handle, message).await;
                }
                TrayCommand::Quit => {
                    if let Some(mut server) = running.take() {
                        if let Some(shutdown) = server.shutdown.take() {
                            let _ = shutdown.send(());
                        }
                        let _ = server.task.await;
                    }
                    handle.shutdown().await;
                    break;
                }
                TrayCommand::ServerReady(addr) => {
                    if running.is_some() && !stop_requested {
                        set_tray(
                            &handle,
                            ProxyStatus::Running,
                            Some(format!("Proxy is running at http://{addr}/v1")),
                        )
                        .await;
                    }
                }
                TrayCommand::ServerExited(result) => {
                    running = None;
                    stop_requested = false;
                    match result {
                        Ok(()) => {
                            set_tray(&handle, ProxyStatus::Stopped, Some("Proxy stopped".into()))
                                .await;
                        }
                        Err(message) => {
                            set_tray(
                                &handle,
                                ProxyStatus::Failed(message.clone()),
                                Some(format!("Proxy failed: {}", clip(&message, 120))),
                            )
                            .await;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    struct RunningServer {
        shutdown: Option<oneshot::Sender<()>>,
        task: JoinHandle<()>,
    }

    #[derive(Clone)]
    enum TrayCommand {
        Start,
        Stop,
        Login,
        LoginFinished(std::result::Result<AuthStatus, String>),
        Logout,
        CopyBaseUrl,
        CopyApiKey,
        CopyClientSettings,
        OpenLogs,
        Quit,
        ServerReady(std::net::SocketAddr),
        ServerExited(std::result::Result<(), String>),
    }

    #[derive(Clone)]
    enum ProxyStatus {
        Stopped,
        Starting,
        Running,
        Stopping,
        Failed(String),
    }

    #[derive(Clone)]
    enum AuthStatus {
        Connected {
            account_id: Option<String>,
            plan_type: Option<String>,
        },
        Disconnected,
        LoggingIn,
    }

    impl AuthStatus {
        fn label(&self) -> String {
            match self {
                Self::Connected {
                    account_id: _,
                    plan_type,
                } => {
                    let plan = plan_type.as_deref().unwrap_or("unknown plan");
                    format!("Connected ({plan})")
                }
                Self::Disconnected => "Not signed in".into(),
                Self::LoggingIn => "Signing in".into(),
            }
        }

        fn detail(&self) -> String {
            match self {
                Self::Connected {
                    account_id,
                    plan_type,
                } => {
                    let account = if account_id.is_some() {
                        "account present"
                    } else {
                        "account missing"
                    };
                    format!(
                        "ChatGPT: connected, {}, {}",
                        plan_type.as_deref().unwrap_or("unknown plan"),
                        account
                    )
                }
                Self::Disconnected => "ChatGPT: not signed in".into(),
                Self::LoggingIn => "ChatGPT: signing in".into(),
            }
        }

        fn is_connected(&self) -> bool {
            matches!(self, Self::Connected { .. })
        }

        fn can_login(&self) -> bool {
            matches!(self, Self::Disconnected)
        }

        fn can_logout(&self) -> bool {
            matches!(self, Self::Connected { .. })
        }
    }

    impl ProxyStatus {
        fn label(&self) -> String {
            match self {
                Self::Stopped => "Stopped".into(),
                Self::Starting => "Starting".into(),
                Self::Running => "Running".into(),
                Self::Stopping => "Stopping".into(),
                Self::Failed(message) => format!("Failed: {}", clip(message, 80)),
            }
        }

        fn can_start(&self) -> bool {
            matches!(self, Self::Stopped | Self::Failed(_))
        }

        fn can_stop(&self) -> bool {
            matches!(self, Self::Starting | Self::Running)
        }
    }

    struct ProxyTray {
        config: TrayConfig,
        status: ProxyStatus,
        auth_status: AuthStatus,
        local_api_key: String,
        last_message: Option<String>,
        command_tx: mpsc::UnboundedSender<TrayCommand>,
    }

    impl ProxyTray {
        fn send_command(&self, command: TrayCommand) {
            let _ = self.command_tx.send(command);
        }

        fn disabled_item(label: impl Into<String>) -> MenuItem<Self> {
            StandardItem {
                label: label.into(),
                enabled: false,
                ..Default::default()
            }
            .into()
        }

        fn command_item(
            label: impl Into<String>,
            icon_name: impl Into<String>,
            enabled: bool,
            command: TrayCommand,
            status_after_click: Option<ProxyStatus>,
            message_after_click: Option<String>,
        ) -> MenuItem<Self> {
            StandardItem {
                label: label.into(),
                icon_name: icon_name.into(),
                enabled,
                activate: Box::new(move |tray: &mut Self| {
                    if let Some(status) = status_after_click.clone() {
                        tray.status = status;
                    }
                    if let Some(message) = message_after_click.clone() {
                        tray.last_message = Some(message);
                    }
                    tray.send_command(command.clone());
                }),
                ..Default::default()
            }
            .into()
        }
    }

    impl Tray for ProxyTray {
        const MENU_ON_ACTIVATE: bool = true;

        fn id(&self) -> String {
            "openai-codex-proxy".into()
        }

        fn title(&self) -> String {
            format!("OpenAI Codex Proxy ({})", build_version())
        }

        fn status(&self) -> Status {
            match &self.status {
                ProxyStatus::Failed(_) => Status::NeedsAttention,
                _ => Status::Active,
            }
        }

        fn icon_name(&self) -> String {
            match &self.status {
                ProxyStatus::Stopped => "network-offline".into(),
                ProxyStatus::Starting | ProxyStatus::Stopping => "view-refresh".into(),
                ProxyStatus::Running => "network-server".into(),
                ProxyStatus::Failed(_) => "dialog-warning".into(),
            }
        }

        fn attention_icon_name(&self) -> String {
            "dialog-warning".into()
        }

        fn tool_tip(&self) -> ToolTip {
            ToolTip {
                icon_name: self.icon_name(),
                title: self.title(),
                description: format!(
                    "Status: {}\n{}\nRelease: {}\nBase URL: {}",
                    self.status.label(),
                    self.auth_status.detail(),
                    build_version(),
                    base_url(self.config.addr)
                ),
                ..Default::default()
            }
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            let start_enabled = self.status.can_start() && self.auth_status.is_connected();
            let stop_enabled = self.status.can_stop();
            let mut items = vec![
                Self::disabled_item(format!("Status: {}", self.status.label())),
                Self::disabled_item(self.auth_status.detail()),
                Self::disabled_item(format!("Release: {}", build_version())),
                Self::disabled_item(format!("Base URL: {}", base_url(self.config.addr))),
                Self::disabled_item("Local API key: configured"),
            ];

            if let Some(message) = self.last_message.as_deref() {
                items.push(Self::disabled_item(format!("Last: {}", clip(message, 96))));
            }

            items.extend([
                MenuItem::Separator,
                Self::command_item(
                    "Start Proxy",
                    "media-playback-start",
                    start_enabled,
                    TrayCommand::Start,
                    Some(ProxyStatus::Starting),
                    Some("Starting proxy".into()),
                ),
                Self::command_item(
                    "Stop Proxy",
                    "media-playback-stop",
                    stop_enabled,
                    TrayCommand::Stop,
                    Some(ProxyStatus::Stopping),
                    Some("Stopping proxy".into()),
                ),
                MenuItem::Separator,
                Self::command_item(
                    "Log in to ChatGPT",
                    "dialog-password",
                    self.auth_status.can_login(),
                    TrayCommand::Login,
                    None,
                    Some("Opening ChatGPT login in your browser".into()),
                ),
                Self::command_item(
                    "Log out of ChatGPT",
                    "system-log-out",
                    self.auth_status.can_logout(),
                    TrayCommand::Logout,
                    None,
                    Some("Logging out of ChatGPT".into()),
                ),
                MenuItem::Separator,
                Self::command_item(
                    "Copy Base URL",
                    "edit-copy",
                    true,
                    TrayCommand::CopyBaseUrl,
                    None,
                    Some("Copying base URL".into()),
                ),
                Self::command_item(
                    "Copy API Key",
                    "edit-copy",
                    !self.local_api_key.is_empty(),
                    TrayCommand::CopyApiKey,
                    None,
                    Some("Copying local API key".into()),
                ),
                Self::command_item(
                    "Copy Client Settings",
                    "edit-copy",
                    !self.local_api_key.is_empty(),
                    TrayCommand::CopyClientSettings,
                    None,
                    Some("Copying client settings".into()),
                ),
                MenuItem::Separator,
                Self::command_item(
                    "Open Logs",
                    "text-x-generic",
                    true,
                    TrayCommand::OpenLogs,
                    None,
                    Some("Opening compatibility log".into()),
                ),
                MenuItem::Separator,
                Self::command_item(
                    "Quit",
                    "application-exit",
                    true,
                    TrayCommand::Quit,
                    Some(ProxyStatus::Stopping),
                    Some("Quitting".into()),
                ),
            ]);

            items
        }
    }

    async fn set_tray(
        handle: &ksni::Handle<ProxyTray>,
        status: ProxyStatus,
        message: Option<String>,
    ) {
        let _ = handle
            .update(|tray| {
                tray.status = status;
                tray.last_message = message;
            })
            .await;
    }

    async fn set_auth_status(handle: &ksni::Handle<ProxyTray>, auth_status: AuthStatus) {
        let _ = handle
            .update(|tray| {
                tray.auth_status = auth_status;
            })
            .await;
    }

    async fn set_last_message(handle: &ksni::Handle<ProxyTray>, message: String) {
        let _ = handle
            .update(|tray| {
                tray.last_message = Some(message);
            })
            .await;
    }

    async fn request_stop(
        running: &mut Option<RunningServer>,
        handle: &ksni::Handle<ProxyTray>,
        message: String,
    ) -> bool {
        if let Some(server) = running.as_mut() {
            if let Some(shutdown) = server.shutdown.take() {
                let _ = shutdown.send(());
                set_tray(handle, ProxyStatus::Stopping, Some(message)).await;
            } else {
                set_tray(
                    handle,
                    ProxyStatus::Stopping,
                    Some("Proxy is already stopping".into()),
                )
                .await;
            }
            true
        } else {
            false
        }
    }

    fn auth_status_from(auth: Option<StoredAuth>) -> AuthStatus {
        match auth {
            Some(auth) => AuthStatus::Connected {
                account_id: auth.account_id,
                plan_type: auth.plan_type,
            },
            None => AuthStatus::Disconnected,
        }
    }

    fn base_url(addr: std::net::SocketAddr) -> String {
        format!("http://{addr}/v1")
    }

    fn client_settings(addr: std::net::SocketAddr, local_api_key: &str) -> String {
        format!(
            "Base URL: {}\nAPI key: {}\nAuthorization: Bearer {}",
            base_url(addr),
            local_api_key,
            local_api_key
        )
    }

    fn copy_to_clipboard(value: &str, label: &str) -> String {
        for (program, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
        ] {
            if copy_with_command(program, args, value).is_ok() {
                return format!("Copied {label}");
            }
        }

        format!("Could not copy {label}; install wl-copy, xclip, or xsel")
    }

    fn copy_with_command(program: &str, args: &[&str], value: &str) -> std::io::Result<()> {
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(value.as_bytes())?;
        }

        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "{program} exited with {status}"
            )))
        }
    }

    fn open_logs() -> String {
        let Some(path) = compat_log_file() else {
            return "Compatibility file logging is disabled".into();
        };

        if !path.exists() {
            return format!("Log file does not exist yet: {}", path.display());
        }

        match std::process::Command::new("xdg-open").arg(&path).spawn() {
            Ok(_) => format!("Opened log file: {}", path.display()),
            Err(err) => format!("Could not open log file {}: {err}", path.display()),
        }
    }

    fn clip(value: &str, max_chars: usize) -> String {
        let mut clipped = value.chars().take(max_chars).collect::<String>();
        if value.chars().count() > max_chars {
            clipped.push_str("...");
        }
        clipped
    }
}
