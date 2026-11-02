use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    pub source: String,
    #[serde(default = "default_mailbox")]
    pub default_mailbox: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiConfig {
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u16,
    #[serde(default = "default_message_width_percent")]
    pub message_width_percent: u16,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: default_sidebar_width(),
            message_width_percent: default_message_width_percent(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path.map_or_else(default_config_path, Path::to_path_buf);
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("parsing config {}", path.display()))?;
        if config.accounts.is_empty() {
            return Err(anyhow!("config contains no [[accounts]] entries"));
        }
        Ok(config)
    }

    #[must_use]
    pub fn single_maildir(name: String, path: PathBuf) -> Self {
        Self {
            accounts: vec![AccountConfig {
                name,
                source: format!("maildir://{}", path.display()),
                default_mailbox: default_mailbox(),
            }],
            ui: UiConfig::default(),
        }
    }
}

#[must_use]
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("aerc-rs")
        .join("config.toml")
}

fn default_mailbox() -> String {
    "INBOX".to_owned()
}

const fn default_sidebar_width() -> u16 {
    24
}

const fn default_message_width_percent() -> u16 {
    48
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn parses_account_config() {
        let config: Config = toml::from_str(
            r#"
                [[accounts]]
                name = "Kernel"
                source = "maildir:///tmp/kernel-mail"
                default_mailbox = "INBOX"

                [ui]
                sidebar_width = 27
            "#,
        )
        .unwrap();
        assert_eq!(config.accounts[0].name, "Kernel");
        assert_eq!(config.ui.sidebar_width, 27);
    }

}
