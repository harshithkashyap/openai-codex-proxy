use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::auth::{AuthManager, login_browser, login_device, should_refresh};
use crate::config::{DEFAULT_ADVERTISED_MODELS, DEFAULT_OAUTH_CALLBACK_PORT, build_version};
use crate::server::serve;
use crate::service_tier::SERVICE_TIER_ENV;
use crate::tray::{TrayConfig, run_tray};

#[derive(Parser, Debug)]
#[command(name = "openai-codex-proxy")]
#[command(about = "Local OpenAI-compatible proxy for the ChatGPT/Codex backend", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Sign in with ChatGPT using browser OAuth by default.
    Login {
        /// Use device-code OAuth instead of browser callback OAuth.
        #[arg(long, default_value_t = false)]
        device_auth: bool,
        /// Local callback port for browser OAuth.
        #[arg(
            long,
            env = "CODEX_PROXY_OAUTH_CALLBACK_PORT",
            default_value_t = DEFAULT_OAUTH_CALLBACK_PORT
        )]
        callback_port: u16,
    },
    /// Print auth status without revealing tokens.
    Status,
    /// Start the localhost OpenAI-compatible proxy.
    Serve {
        /// Bind address. Keep this on loopback unless you know what you are doing.
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: SocketAddr,
        /// Optional local API key required from downstream clients.
        #[arg(long, env = "CODEX_PROXY_LOCAL_API_KEY")]
        local_api_key: Option<String>,
        /// Allow serving without a local API key. Only use on trusted loopback-only hosts.
        #[arg(
            long,
            env = "CODEX_PROXY_ALLOW_NO_LOCAL_API_KEY",
            default_value_t = false
        )]
        allow_no_local_api_key: bool,
        /// Comma-separated model IDs advertised by /v1/models.
        #[arg(long, env = "CODEX_PROXY_MODELS", value_delimiter = ',', default_value = DEFAULT_ADVERTISED_MODELS)]
        models: Vec<String>,
        /// Optional default service tier. Use "fast" or "priority" for Codex Fast mode.
        #[arg(long, env = SERVICE_TIER_ENV)]
        service_tier: Option<String>,
    },
    /// Start a lightweight Linux system tray controller for the proxy.
    Tray {
        /// Bind address. Keep this on loopback unless you know what you are doing.
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: SocketAddr,
        /// Optional local API key required from downstream clients.
        #[arg(long, env = "CODEX_PROXY_LOCAL_API_KEY")]
        local_api_key: Option<String>,
        /// Allow serving without a local API key. Only use on trusted loopback-only hosts.
        #[arg(
            long,
            env = "CODEX_PROXY_ALLOW_NO_LOCAL_API_KEY",
            default_value_t = false
        )]
        allow_no_local_api_key: bool,
        /// Comma-separated model IDs advertised by /v1/models.
        #[arg(long, env = "CODEX_PROXY_MODELS", value_delimiter = ',', default_value = DEFAULT_ADVERTISED_MODELS)]
        models: Vec<String>,
        /// Optional default service tier. Use "fast" or "priority" for Codex Fast mode.
        #[arg(long, env = SERVICE_TIER_ENV)]
        service_tier: Option<String>,
    },
}
pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openai_codex_proxy=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Login {
            device_auth,
            callback_port,
        } => {
            let auth = AuthManager::new().await?;
            if device_auth {
                login_device(&auth).await?;
            } else {
                login_browser(&auth, callback_port).await?;
            }
        }
        Command::Status => {
            let auth = AuthManager::new().await?;
            println!("version: {}", build_version());
            if let Some(cached) = auth.cached().await {
                println!("logged_in: true");
                println!("auth_file: {}", auth.path.display());
                println!("account_id_present: {}", cached.account_id.is_some());
                println!(
                    "plan_type: {}",
                    cached.plan_type.as_deref().unwrap_or("unknown")
                );
                println!("needs_refresh: {}", should_refresh(&cached));
            } else {
                println!("logged_in: false");
                println!("auth_file: {}", auth.path.display());
            }
        }
        Command::Serve {
            addr,
            local_api_key,
            allow_no_local_api_key,
            models,
            service_tier,
        } => {
            serve(
                addr,
                local_api_key,
                allow_no_local_api_key,
                models,
                service_tier,
            )
            .await?
        }
        Command::Tray {
            addr,
            local_api_key,
            allow_no_local_api_key,
            models,
            service_tier,
        } => {
            run_tray(TrayConfig {
                addr,
                local_api_key,
                allow_no_local_api_key,
                models,
                service_tier,
            })
            .await?
        }
    }
    Ok(())
}
