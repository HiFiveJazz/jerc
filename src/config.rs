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

#[derive(Clone, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    pub source: String,
    #[serde(default = "default_mailbox")]
    pub default_mailbox: String,
    /// Optional command that prints the account password/refresh token/access
    /// token on stdout. This mirrors aerc's source-cred-cmd idea and avoids
    /// storing long-lived credentials in config.toml.
    #[serde(default)]
    pub source_cred_cmd: Option<String>,
    /// RFC 5322 From value used for new outgoing messages, e.g.
    /// `Jazz Bhatia <jazz@example.com>`. If omitted, the SMTP username is used.
    #[serde(default)]
    pub from: Option<String>,
    /// Optional aerc-style SMTP URI. Supported secure schemes are smtp://
    /// (STARTTLS) and smtps:// (implicit TLS), with +plain, +login or +xoauth2.
    #[serde(default)]
    pub outgoing: Option<String>,
    /// Optional command that prints the outgoing password or OAuth refresh/access
    /// token. URI credentials take precedence, mirroring aerc.
    #[serde(default)]
    pub outgoing_cred_cmd: Option<String>,
    /// Legacy/backend fallback header limit. Headless checks still use this
    /// value; the interactive TUI uses `page_size` and paginates older mail.
    #[serde(default = "default_max_messages")]
    pub max_messages: usize,
    /// Number of message headers fetched per interactive page. This is kept
    /// separate from max_messages so old configs that used a large initial
    /// window become fast automatically when milestone-5 pagination is enabled.
    #[serde(default = "default_page_size")]
    pub page_size: usize,
}

impl std::fmt::Debug for AccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = self.source.split_once("://").map_or("<invalid>", |(scheme, _)| scheme);
        f.debug_struct("AccountConfig")
            .field("name", &self.name)
            .field("source", &format_args!("{scheme}://<redacted>"))
            .field("default_mailbox", &self.default_mailbox)
            .field(
                "source_cred_cmd",
                &self.source_cred_cmd.as_ref().map(|_| "<configured>"),
            )
            .field("from", &self.from)
            .field(
                "outgoing",
                &self.outgoing.as_ref().map(|value| {
                    let scheme = value.split_once("://").map_or("<invalid>", |(scheme, _)| scheme);
                    format!("{scheme}://<redacted>")
                }),
            )
            .field(
                "outgoing_cred_cmd",
                &self.outgoing_cred_cmd.as_ref().map(|_| "<configured>"),
            )
            .field("max_messages", &self.max_messages)
            .field("page_size", &self.page_size)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiConfig {
    /// Legacy milestone-3 setting. Kept so existing config files continue to
    /// parse; the browser now uses a proportional mailbox column instead.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u16,
    /// Legacy split-preview width setting. Milestone 5 restored a responsive
    /// preview split, but keeps this field only so older configs still parse.
    #[serde(default = "default_message_width_percent")]
    pub message_width_percent: u16,
    /// Width of the mailbox column while browsing. The message list consumes
    /// the remaining horizontal space.
    #[serde(default = "default_mailbox_width_percent")]
    pub mailbox_width_percent: u16,
    /// Initial theme. A theme selected from the in-TUI picker is persisted and
    /// takes precedence on the next launch.
    #[serde(default = "default_theme")]
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: default_sidebar_width(),
            message_width_percent: default_message_width_percent(),
            mailbox_width_percent: default_mailbox_width_percent(),
            theme: default_theme(),
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
                source_cred_cmd: None,
                from: None,
                outgoing: None,
                outgoing_cred_cmd: None,
                max_messages: default_max_messages(),
                page_size: default_page_size(),
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

const fn default_max_messages() -> usize {
    200
}

const fn default_page_size() -> usize {
    100
}

const fn default_sidebar_width() -> u16 {
    24
}

const fn default_message_width_percent() -> u16 {
    48
}

const fn default_mailbox_width_percent() -> u16 {
    33
}

fn default_theme() -> String {
    "default".to_owned()
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
                mailbox_width_percent = 35
                theme = "nord"
            "#,
        )
        .unwrap();
        assert_eq!(config.accounts[0].name, "Kernel");
        assert_eq!(config.accounts[0].max_messages, 200);
        assert_eq!(config.accounts[0].page_size, 100);
        assert_eq!(config.ui.sidebar_width, 27);
        assert_eq!(config.ui.mailbox_width_percent, 35);
        assert_eq!(config.ui.theme, "nord");
    }

    #[test]
    fn parses_external_credential_command() {
        let config: Config = toml::from_str(
            r#"
                [[accounts]]
                name = "Personal"
                source = "imaps://user%40example.com@imap.example.com:993"
                source_cred_cmd = "pass show mail/example"
                max_messages = 250
                page_size = 64
            "#,
        )
        .unwrap();
        assert_eq!(
            config.accounts[0].source_cred_cmd.as_deref(),
            Some("pass show mail/example")
        );
        assert_eq!(config.accounts[0].max_messages, 250);
        assert_eq!(config.accounts[0].page_size, 64);
    }

    #[test]
    fn parses_outgoing_transport_without_printing_credentials() {
        let config: Config = toml::from_str(
            r#"
                [[accounts]]
                name = "Personal"
                source = "imaps://user%40example.com@imap.example.com:993"
                from = "User <user@example.com>"
                outgoing = "smtps://user%40example.com:secret@smtp.example.com:465"
            "#,
        )
        .unwrap();
        assert_eq!(config.accounts[0].from.as_deref(), Some("User <user@example.com>"));
        let debug = format!("{:?}", config.accounts[0]);
        assert!(!debug.contains("secret"));
        assert!(debug.contains("smtps://<redacted>"));
    }
}
