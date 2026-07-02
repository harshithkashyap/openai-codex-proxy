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
    use anyhow::{Context, Result};
    use ksni::menu::StandardItem;
    use ksni::{MenuItem, Status, ToolTip, Tray, TrayMethods};
    use tokio::sync::{mpsc, oneshot};
    use tokio::task::JoinHandle;

    use super::TrayConfig;
    use crate::config::build_version;
    use crate::logging::compat_log_file;
    use crate::server::serve_until_shutdown;

    pub(super) async fn run(config: TrayConfig) -> Result<()> {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let config_for_server = config.clone();
        let tray = ProxyTray {
            config,
            status: ProxyStatus::Stopped,
            last_message: Some("Proxy is stopped".into()),
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
                    if let Some(server) = running.as_mut() {
                        if let Some(shutdown) = server.shutdown.take() {
                            let _ = shutdown.send(());
                            set_tray(
                                &handle,
                                ProxyStatus::Stopping,
                                Some("Stopping proxy".into()),
                            )
                            .await;
                        } else {
                            set_tray(
                                &handle,
                                ProxyStatus::Stopping,
                                Some("Proxy is already stopping".into()),
                            )
                            .await;
                        }
                    } else {
                        stop_requested = false;
                        set_tray(
                            &handle,
                            ProxyStatus::Stopped,
                            Some("Proxy is already stopped".into()),
                        )
                        .await;
                    }
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
                    "Status: {}\nRelease: {}\nBase URL: http://{}/v1",
                    self.status.label(),
                    build_version(),
                    self.config.addr
                ),
                ..Default::default()
            }
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            let start_enabled = self.status.can_start();
            let stop_enabled = self.status.can_stop();
            let mut items = vec![
                Self::disabled_item(format!("Status: {}", self.status.label())),
                Self::disabled_item(format!("Release: {}", build_version())),
                Self::disabled_item(format!("Base URL: http://{}/v1", self.config.addr)),
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

    async fn set_last_message(handle: &ksni::Handle<ProxyTray>, message: String) {
        let _ = handle
            .update(|tray| {
                tray.last_message = Some(message);
            })
            .await;
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
