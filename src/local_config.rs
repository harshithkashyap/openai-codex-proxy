use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::auth::random_urlsafe;

pub(crate) const CONFIG_FILE_ENV: &str = "CODEX_PROXY_CONFIG_FILE";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct LocalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) local_api_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalApiKeySource {
    Provided,
    Stored,
    Generated,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalApiKey {
    pub(crate) value: String,
    pub(crate) source: LocalApiKeySource,
    pub(crate) config_path: PathBuf,
}

pub(crate) fn config_file_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var(CONFIG_FILE_ENV)
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    let config_dir = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
        .ok_or_else(|| anyhow!("could not determine a config directory"))?;
    Ok(config_dir.join("openai-codex-proxy").join("config.json"))
}

pub(crate) async fn load_local_config_from_path(path: &Path) -> Result<LocalConfig> {
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => serde_json::from_str(&raw).context("failed to parse local proxy config"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(LocalConfig::default()),
        Err(err) => Err(err).context("failed to read local proxy config"),
    }
}

pub(crate) async fn save_local_config_to_path(path: &Path, config: &LocalConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let path = path.to_path_buf();
    let data = serde_json::to_vec_pretty(config)?;
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

pub(crate) async fn ensure_local_api_key(provided: Option<String>) -> Result<LocalApiKey> {
    let config_path = config_file_path()?;
    if let Some(value) = provided.filter(|value| !value.trim().is_empty()) {
        return Ok(LocalApiKey {
            value,
            source: LocalApiKeySource::Provided,
            config_path,
        });
    }

    let mut config = load_local_config_from_path(&config_path).await?;
    if let Some(value) = config
        .local_api_key
        .clone()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(LocalApiKey {
            value,
            source: LocalApiKeySource::Stored,
            config_path,
        });
    }

    let value = format!("ocp_{}", random_urlsafe(32)?);
    config.local_api_key = Some(value.clone());
    save_local_config_to_path(&config_path, &config).await?;

    Ok(LocalApiKey {
        value,
        source: LocalApiKeySource::Generated,
        config_path,
    })
}
