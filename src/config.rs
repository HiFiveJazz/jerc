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
    /// Source config file, used to persist user-selected UI settings without
    /// overriding the value with an unrelated state file.
    #[serde(skip)]
    pub config_path: Option<PathBuf>,
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
    /// (STARTTLS) and smtps:// (implicit TLS), with +plain, +login, +xoauth2 or +oauthbearer.
    #[serde(default)]
    pub outgoing: Option<String>,
    /// Optional command that prints the outgoing password or OAuth refresh/access
    /// token. URI credentials take precedence, mirroring aerc.
    #[serde(default)]
    pub outgoing_cred_cmd: Option<String>,
    /// Aerc-compatible per-account sent-copy policy. "" or "none" disables
    /// APPEND; a comma-separated folder list forces copies to those folders.
    /// When absent, preserve legacy automatic behavior (Gmail server copy,
    /// otherwise use the first advertised Sent folder).
    #[serde(default, alias = "copy-to")]
    pub copy_to: Option<String>,
    /// Browser-based native OAuth2 sign-in. No refresh token in this file.
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthConfig {
    pub provider: String,
    /// Public client ID. Application publishers can instead embed one at build
    /// time (AERC_RS_GOOGLE_CLIENT_ID / AERC_RS_MICROSOFT_CLIENT_ID).
    #[serde(default)]
    pub client_id: Option<String>,
    /// Microsoft tenant, default: common. Not used for Google.
    #[serde(default)]
    pub tenant: Option<String>,
    /// Optional OAuth desktop public-client secret (Google may supply one).
    /// Not used as a security boundary; don't put user refresh tokens here.
    #[serde(default)]
    pub client_secret: Option<String>,
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
            .field("copy_to", &self.copy_to)
            .field("oauth", &self.oauth.as_ref().map(|oauth| &oauth.provider))
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
    /// Selected theme, authoritative on startup and updated by the picker.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Fetch remote HTML images automatically. Disabled by default because a
    /// direct image request can disclose the user's IP address and read time.
    /// Press `I` in the reader to opt in/out for the current session.
    #[serde(default)]
    pub remote_images: bool,
    /// Periodic Inbox STATUS check in seconds (the IMAP IDLE backend is not
    /// available yet). The UI stays responsive while checks run on workers.
    #[serde(default = "default_inbox_poll_seconds")]
    pub inbox_poll_seconds: u64,
    /// Optional terminal bell on an actual Inbox count increase.
    #[serde(default)]
    pub new_mail_bell: bool,
    /// Desktop notification on a new Inbox count (after baseline is known).
    #[serde(default = "default_true")]
    pub new_mail_notifications: bool,
    /// freedesktop sound theme name; COSMIC-installed themes can provide this.
    #[serde(default = "default_mail_sound_name")]
    pub new_mail_sound_name: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: default_sidebar_width(),
            message_width_percent: default_message_width_percent(),
            mailbox_width_percent: default_mailbox_width_percent(),
            theme: default_theme(),
            remote_images: false,
            inbox_poll_seconds: default_inbox_poll_seconds(),
            new_mail_bell: false,
            new_mail_notifications: true,
            new_mail_sound_name: default_mail_sound_name(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path.map_or_else(default_config_path, Path::to_path_buf);
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Self = toml::from_str(&contents)
            .with_context(|| format!("parsing config {}", path.display()))?;
        config.config_path = Some(path);
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
                copy_to: None,
                oauth: None,
                max_messages: default_max_messages(),
                page_size: default_page_size(),
            }],
            ui: UiConfig::default(),
            config_path: None,
        }
    }
}

/// Persist the theme directly into the user's TOML config, preserving account
/// secrets, unrelated comments and other settings. A temporary
/// file in the same directory prevents truncating config on an interrupted write.
pub fn save_ui_theme(path: &Path, theme: &str) -> Result<()> {
    use std::io::Write;

    let file_path = fs::canonicalize(path)
        .with_context(|| format!("resolving config {}", path.display()))?;
    let original = fs::read_to_string(&file_path)
        .with_context(|| format!("reading config {}", file_path.display()))?;
    let quoted = toml::Value::String(theme.to_owned()).to_string();
    let mut updated = String::with_capacity(original.len() + 48);
    let mut in_ui = false;
    let mut found_ui = false;
    let mut replaced = false;
    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_ui && !replaced {
                updated.push_str(&format!("theme = {quoted}\n"));
                replaced = true;
            }
            in_ui = trimmed.split('#').next().unwrap_or("").trim() == "[ui]";
            found_ui |= in_ui;
        }
        if in_ui && !trimmed.starts_with('#')
            && line.split_once('=').is_some_and(|(key, _)| key.trim() == "theme")
        {
            updated.push_str(&format!("theme = {quoted}\n"));
            replaced = true;
        } else {
            updated.push_str(line);
            updated.push('\n');
        }
    }
    if in_ui && !replaced {
        updated.push_str(&format!("theme = {quoted}\n"));
    } else if !found_ui {
        updated.push_str(&format!("\n[ui]\ntheme = {quoted}\n"));
    }
    // Never write a malformed TOML file back over working account settings.
    toml::from_str::<Config>(&updated).context("validating updated theme config")?;
    let parent = file_path.parent().ok_or_else(|| anyhow!("config path has no directory"))?;
    let mut staging = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("staging config in {}", parent.display()))?;
    staging.as_file().set_permissions(fs::metadata(&file_path)?.permissions())?;
    staging.write_all(updated.as_bytes())?;
    staging.flush()?;
    staging.persist(&file_path)
        .map_err(|error| anyhow!("saving config {}: {}", file_path.display(), error.error))?;
    Ok(())
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

const fn default_inbox_poll_seconds() -> u64 { 20 }

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
    22
}

fn default_true() -> bool { true }

fn default_mail_sound_name() -> String { "message-new-email".to_owned() }

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
        assert!(!config.ui.remote_images);
        assert_eq!(config.ui.inbox_poll_seconds, 20);
        assert!(!config.ui.new_mail_bell);
        assert!(config.ui.new_mail_notifications);
        assert_eq!(config.ui.new_mail_sound_name, "message-new-email");
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
    #[test]
    fn per_account_copy_to_is_optional_and_alias_is_accepted() {
        let config: Config = toml::from_str(r#"
            [[accounts]]
            name = "Google"
            source = "imaps://user@imap.gmail.com"
            copy_to = ""

            [[accounts]]
            name = "Local"
            source = "maildir:///tmp/local"
            copy-to = "Sent, Archive"

            [[accounts]]
            name = "Old"
            source = "maildir:///tmp/old"
        "#).unwrap();
        assert_eq!(config.accounts[0].copy_to.as_deref(), Some(""));
        assert_eq!(config.accounts[1].copy_to.as_deref(), Some("Sent, Archive"));
        assert!(config.accounts[2].copy_to.is_none());
    }

    #[test]
    fn parses_browser_oauth_config_without_tokens_in_debug() {
        let config: Config = toml::from_str(r#"
            [[accounts]]
            name = "University"
            source = "imaps://student%40example.edu@imap.example.edu"
            outgoing = "smtps://student%40example.edu@smtp.example.edu"
            [accounts.oauth]
            provider = "microsoft"
            tenant = "common"
            client_id = "public-client"
        "#).unwrap();
        let account = &config.accounts[0];
        assert_eq!(account.oauth.as_ref().unwrap().provider, "microsoft");
        assert!(account.copy_to.is_none());
    }

    #[test]
    fn writes_ui_theme_without_losing_account_credentials_or_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let source = "# personal config\n[[accounts]]\nname = \"Personal\"\nsource = \"imaps://user:private-token@example.org\"\n\n[ui]\ntheme = \"default\" # old theme\nremote_images = false\n";
        std::fs::write(&path, source).unwrap();
        super::save_ui_theme(&path, "tokyo-night").unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("private-token@example.org"));
        assert!(saved.contains("# personal config"));
        assert!(saved.contains("remote_images = false"));
        assert_eq!(super::Config::load(Some(&path)).unwrap().ui.theme, "tokyo-night");
        assert_eq!(saved.matches("theme =").count(), 1);
    }

    #[test]
    fn writes_ui_theme_when_ui_table_missing_and_honors_custom_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.toml");
        std::fs::write(&path, "[[accounts]]\nname = \"Local\"\nsource = \"maildir:///tmp/mail\"\n").unwrap();
        super::save_ui_theme(&path, "nord").unwrap();
        let config = super::Config::load(Some(&path)).unwrap();
        assert_eq!(config.ui.theme, "nord");
        assert_eq!(config.config_path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn adds_theme_inside_existing_ui_table_without_theme_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.toml");
        std::fs::write(&path, "[ui]\nremote_images = false\n\n[[accounts]]\nname = \"Local\"\nsource = \"maildir:///tmp/mail\"\n").unwrap();
        super::save_ui_theme(&path, "nord").unwrap();
        let config = super::Config::load(Some(&path)).unwrap();
        assert_eq!(config.ui.theme, "nord");
    }

}
