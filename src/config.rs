use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub contacts: ContactsConfig,
    /// Source config file, used to persist user-selected UI settings without
    /// overriding the value with an unrelated state file.
    #[serde(skip)]
    pub config_path: Option<PathBuf>,
}

/// Optional native contacts policy. No network scopes or provider APIs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ContactsConfig {
    pub autocomplete: bool,
    pub learn_from_inbox: bool,
    pub learn_from_sent: bool,
}

impl Default for ContactsConfig {
    fn default() -> Self {
        Self { autocomplete: true, learn_from_inbox: true, learn_from_sent: true }
    }
}

#[derive(Clone, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    #[serde(default, alias = "incoming")]
    pub source: String,
    /// Plain email address for shorthand IMAP/SMTP configuration. Never needs
    /// URL escaping. Existing source/outgoing URIs continue to work unchanged.
    #[serde(default)]
    pub email: Option<String>,
    /// IMAP login identity, which may differ from the displayed email address.
    /// Applies to `source = "imap"` shorthand; full URIs keep their own username.
    #[serde(default)]
    pub imap_username: Option<String>,
    /// SMTP login identity, which may differ from the displayed email address.
    /// Applies to `outgoing = "smtp"`/`starttls`/`smtps` shorthand.
    #[serde(default)]
    pub smtp_username: Option<String>,
    /// Optional server overrides for non-preset providers. Hostnames only,
    /// without a scheme, username, port or password.
    #[serde(default)]
    pub imap_host: Option<String>,
    #[serde(default)]
    pub smtp_host: Option<String>,
    #[serde(default)]
    pub imap_port: Option<u16>,
    #[serde(default)]
    pub smtp_port: Option<u16>,
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

#[derive(Debug, Clone, Default, Deserialize)]
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
    /// Login flow: browser (local callback), nativeclient (manual URL), or
    /// devicecode (Microsoft sign-in code). Defaults to browser.
    #[serde(default)]
    pub flow: Option<String>,
    /// Optional browser redirect hostname. Choose only localhost or 127.0.0.1
    /// to match the redirect URI registered for *your* OAuth application.
    #[serde(default)]
    pub callback_host: Option<String>,
    /// Optional browser redirect path. Default: /oauth/callback.
    #[serde(default)]
    pub callback_path: Option<String>,
    /// Provider's consent-screen choice (e.g. select_account or consent).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Optional email hint prefilled on the provider's login page.
    #[serde(default)]
    pub login_hint: Option<String>,
    /// Override requested OAuth scopes (advanced; must include IMAP/SMTP
    /// permissions and offline access if those are needed).
    #[serde(default)]
    pub scope: Option<String>,
    /// Optional separate credential store for an OAuth experiment. Use a
    /// distinct value in a local fork so sign-in cannot replace production
    /// refresh tokens for an account with the same name.
    #[serde(default)]
    pub credential_namespace: Option<String>,
}

impl std::fmt::Debug for AccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = self.source.split_once("://").map_or("<invalid>", |(scheme, _)| scheme);
        f.debug_struct("AccountConfig")
            .field("name", &self.name)
            .field("source", &format_args!("{scheme}://<redacted>"))
            .field("email_configured", &self.email.is_some())
            .field("imap_username_configured", &self.imap_username.is_some())
            .field("smtp_username_configured", &self.smtp_username.is_some())
            .field("imap_host", &self.imap_host)
            .field("smtp_host", &self.smtp_host)
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

/// Glyph choice cannot be inferred reliably from a terminal's font. Default
/// to universally available Unicode and let Nerd Font users opt in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationIconStyle {
    Nerd,
    #[default]
    Unicode,
}

/// How `/` filters messages in the mail list. Fuzzy matching ranks ordered
/// character matches first, while strict preserves the original substring filter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailSearchMode {
    #[default]
    Fuzzy,
    Strict,
}

/// How visually selected or marked rows are painted in mail and contacts lists.
/// The active cursor row remains dark gray in gutter mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VisualSelectionStyle {
    Row,
    #[default]
    Gutter,
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
    /// Right reader column width as a percentage of the space after the sidebar.
    #[serde(default = "default_preview_width_percent")]
    pub preview_width_percent: u16,
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
    /// `unicode` (●/○, default) or `nerd` (Material Design bell glyphs).
    #[serde(default)]
    pub notification_icon_style: NotificationIconStyle,
    /// Message-list `/` matching: `fuzzy` (default) or `strict` (substring).
    #[serde(default)]
    pub search_mode: MailSearchMode,
    /// `row` colors entire visual/marked mail and contacts rows; `gutter`
    /// colors only their left-side marker (default).
    #[serde(default)]
    pub visual_selection_style: VisualSelectionStyle,
    /// freedesktop sound theme name; COSMIC-installed themes can provide this.
    #[serde(default = "default_mail_sound_name")]
    pub new_mail_sound_name: String,
    /// Optional explicit audio file for desktops that ignore notification sound hints.
    /// Requires pw-play or paplay in PATH; otherwise the sound hint still works.
    #[serde(default)]
    pub new_mail_sound_file: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: default_sidebar_width(),
            message_width_percent: default_message_width_percent(),
            mailbox_width_percent: default_mailbox_width_percent(),
            preview_width_percent: default_preview_width_percent(),
            theme: default_theme(),
            remote_images: false,
            inbox_poll_seconds: default_inbox_poll_seconds(),
            new_mail_bell: false,
            new_mail_notifications: true,
            notification_icon_style: NotificationIconStyle::default(),
            search_mode: MailSearchMode::default(),
            visual_selection_style: VisualSelectionStyle::default(),
            new_mail_sound_name: default_mail_sound_name(),
            new_mail_sound_file: None,
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
        config.normalize_accounts();
        if config.accounts.is_empty() {
            return Err(anyhow!("config contains no [[accounts]] entries"));
        }
        Ok(config)
    }

    /// Only the interactive TUI may start with no account configuration.
    /// Explicit commands such as --oauth-probe still use strict `load()`.
    pub fn load_or_empty(path: Option<&Path>) -> Result<Self> {
        let path = path.map_or_else(default_config_path, Path::to_path_buf);
        match fs::read_to_string(&path) {
            Ok(contents) => {
                let mut config: Self = if contents.trim().is_empty() {
                    toml::from_str("[ui]\n")?
                } else {
                    toml::from_str(&contents)
                        .with_context(|| format!("parsing config {}", path.display()))?
                };
                config.config_path = Some(path);
                config.normalize_accounts();
                Ok(config)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                accounts: Vec::new(), ui: UiConfig::default(),
                contacts: ContactsConfig::default(), config_path: Some(path),
            }),
            Err(err) => Err(err).with_context(|| format!("reading config {}", path.display())),
        }
    }

    fn normalize_accounts(&mut self) {
        for account in &mut self.accounts {
            // Preserve multi-account recovery: a malformed shortcut must not
            // prevent an unrelated valid account from launching. Leave its
            // unexpanded scheme in place so the existing startup validation
            // can skip the bad incoming account or disable only its outgoing
            // transport (and show the warning in the TUI).
            let _ = account.normalize_shorthand();
        }
    }

    #[must_use]
    pub fn single_maildir(name: String, path: PathBuf) -> Self {
        Self {
            accounts: vec![AccountConfig {
                name,
                source: format!("maildir://{}", path.display()),
                email: None,
                imap_username: None,
                smtp_username: None,
                imap_host: None,
                smtp_host: None,
                imap_port: None,
                smtp_port: None,
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
            contacts: ContactsConfig::default(),
            config_path: None,
        }
    }
}

/// Known provider settings. This is intentionally not a generic DNS guess:
/// custom domains must supply their actual mail hosts unless the user selected
/// an OAuth provider that tells us where their mailbox is hosted.
#[derive(Clone, Copy)]
enum MailPreset {
    Google,
    Microsoft365,
    OutlookCom,
}

impl MailPreset {
    fn for_account(account: &AccountConfig, email: &str) -> Option<Self> {
        let domain = email.rsplit_once('@')?.1.to_ascii_lowercase();
        let microsoft_consumer = matches!(domain.as_str(),
            "outlook.com" | "hotmail.com" | "live.com" | "msn.com");
        match account.oauth.as_ref().map(|oauth| oauth.provider.to_ascii_lowercase()) {
            Some(provider) if provider == "google" => Some(Self::Google),
            Some(provider) if provider == "microsoft" => Some(if microsoft_consumer {
                Self::OutlookCom
            } else {
                Self::Microsoft365
            }),
            _ => match domain.as_str() {
                "gmail.com" | "googlemail.com" => Some(Self::Google),
                _ if microsoft_consumer => Some(Self::OutlookCom),
                _ => None,
            },
        }
    }

    const fn imap_host(self) -> &'static str {
        match self {
            Self::Google => "imap.gmail.com",
            Self::Microsoft365 | Self::OutlookCom => "outlook.office365.com",
        }
    }

    const fn smtp_host(self) -> &'static str {
        match self {
            Self::Google => "smtp.gmail.com",
            Self::Microsoft365 => "smtp.office365.com",
            Self::OutlookCom => "smtp-mail.outlook.com",
        }
    }

    const fn smtp_starttls(self) -> bool {
        !matches!(self, Self::Google)
    }
}

impl AccountConfig {
    /// Expand user-friendly shorthand at load time. Everything downstream still
    /// receives the same fully qualified URIs and OAuth settings as before.
    fn normalize_shorthand(&mut self) -> Result<()> {
        let email = self.email.as_deref().map(str::trim).filter(|email| !email.is_empty());
        if let Some(value) = email {
            let Some((local, domain)) = value.rsplit_once('@') else {
                return Err(anyhow!("email must be a complete address, e.g. you@example.com"));
            };
            if local.is_empty() || local.contains('@') || !valid_hostname(domain)
                || value.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
                return Err(anyhow!("email must be a valid, unescaped email address"));
            }
        }
        for (label, value) in [
            ("imap_username", self.imap_username.as_deref()),
            ("smtp_username", self.smtp_username.as_deref()),
        ] {
            if let Some(username) = value {
                if username.trim().is_empty() || username != username.trim()
                    || username.chars().any(char::is_control)
                {
                    return Err(anyhow!("{label} must be a nonempty single-line login name"));
                }
            }
        }
        let preset = email.and_then(|email| MailPreset::for_account(self, email));

        let incoming = if self.source.trim().is_empty() && email.is_some() {
            "imap"
        } else {
            self.source.trim()
        };
        if matches!(incoming, "imap" | "imaps" | "imaps+login" | "imaps+xoauth2" | "imaps+oauthbearer") {
            let email = email.ok_or_else(|| anyhow!("email is required when source = 'imap'"))?;
            let host = self.imap_host.as_deref()
                .or_else(|| preset.map(MailPreset::imap_host))
                .ok_or_else(|| anyhow!("unknown mail provider: set imap_host for this account"))?;
            let port = self.imap_port.unwrap_or(993);
            let scheme = match incoming { "imap" => "imaps", other => other };
            let username = self.imap_username.as_deref().unwrap_or(email);
            self.source = build_mail_uri(scheme, username, host, port)?;
        } else if incoming.is_empty() {
            return Err(anyhow!("missing incoming mail source; set email and source = 'imap', or use an IMAP/Maildir URI"));
        }

        if let Some(outgoing) = self.outgoing.as_deref().map(|value| value.trim().to_owned()) {
            match outgoing.as_str() {
                "none" => self.outgoing = None,
                "smtp" | "smtps" | "starttls" => {
                    let email = email.ok_or_else(|| anyhow!("email is required for outgoing = 'smtp'"))?;
                    let host = self.smtp_host.as_deref()
                        .or_else(|| preset.map(MailPreset::smtp_host))
                        .ok_or_else(|| anyhow!("unknown mail provider: set smtp_host for this account"))?;
                    let starttls = match outgoing.as_str() {
                        "smtps" => false,
                        "starttls" => true,
                        _ => self.smtp_port.map_or_else(
                            || preset.is_none_or(MailPreset::smtp_starttls),
                            |port| port != 465,
                        ),
                    };
                    let scheme = if starttls { "smtp" } else { "smtps" };
                    let port = self.smtp_port.unwrap_or(if starttls { 587 } else { 465 });
                    let username = self.smtp_username.as_deref().unwrap_or(email);
                    self.outgoing = Some(build_mail_uri(scheme, username, host, port)?);
                }
                _ => {} // Keep existing full SMTP and sendmail URLs untouched.
            }
        }
        Ok(())
    }
}

fn valid_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host.split('.').all(|label| !label.is_empty() && label.len() <= 63
            && !label.starts_with('-') && !label.ends_with('-')
            && label.bytes().all(|ch| ch.is_ascii_alphanumeric() || ch == b'-'))
}

/// Use Url's userinfo encoder; an email address remains plain text in TOML and
/// is escaped only inside the generated URL. No credential is ever inserted.
fn build_mail_uri(scheme: &str, username: &str, host: &str, port: u16) -> Result<String> {
    if !valid_hostname(host) || port == 0 {
        return Err(anyhow!("invalid mail server hostname or port"));
    }
    let mut url = Url::parse(&format!("{scheme}://{host}:{port}"))
        .context("constructing secure mail server URI")?;
    url.set_username(username)
        .map_err(|()| anyhow!("invalid email address for mail server URI"))?;
    Ok(url.to_string())
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

const fn default_preview_width_percent() -> u16 { 57 }

fn default_true() -> bool { true }

fn default_mail_sound_name() -> String { "message-new-email".to_owned() }

fn default_theme() -> String {
    "default".to_owned()
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn notification_icon_style_parses_nerd_fonts_explicitly() {
        let config: Config = toml::from_str("[ui]\nnotification_icon_style = \"nerd\"").unwrap();
        assert_eq!(config.ui.notification_icon_style, super::NotificationIconStyle::Nerd);
    }

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
        assert_eq!(config.ui.notification_icon_style, super::NotificationIconStyle::Unicode);
        assert_eq!(config.ui.search_mode, super::MailSearchMode::Fuzzy);
        assert_eq!(config.ui.new_mail_sound_name, "message-new-email");
    }

    #[test]
    fn visual_selection_style_defaults_to_gutter_and_accepts_row() {
        let default: super::Config = toml::from_str("[ui]\n").unwrap();
        assert_eq!(default.ui.visual_selection_style, super::VisualSelectionStyle::Gutter);
        let row: super::Config = toml::from_str("[ui]\nvisual_selection_style = \"row\"").unwrap();
        assert_eq!(row.ui.visual_selection_style, super::VisualSelectionStyle::Row);
        assert!(toml::from_str::<super::Config>("[ui]\nvisual_selection_style = \"sideways\"").is_err());
    }

    #[test]
    fn mailbox_search_mode_is_fuzzy_unless_configured_strict() {
        let default: super::Config = toml::from_str("[ui]\ntheme = \"tokyo-night\"").unwrap();
        assert_eq!(default.ui.search_mode, super::MailSearchMode::Fuzzy);
        let strict: super::Config = toml::from_str("[ui]\nsearch_mode = \"strict\"").unwrap();
        assert_eq!(strict.ui.search_mode, super::MailSearchMode::Strict);
        let invalid = toml::from_str::<super::Config>("[ui]\nsearch_mode = \"approximate\"");
        assert!(invalid.is_err());
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
    fn oauth_browser_test_settings_parse_per_account() {
        let config: Config = toml::from_str(r#"
            [[accounts]]
            name = "Development"
            email = "dev@example.com"
            source = "imap"
            outgoing = "smtp"
            [accounts.oauth]
            provider = "microsoft"
            client_id = "registered-public-client"
            callback_host = "localhost"
            callback_path = "/"
            prompt = "select_account"
            login_hint = "dev@example.com"
            scope = "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send"
        "#).unwrap();
        let oauth = config.accounts[0].oauth.as_ref().unwrap();
        assert_eq!(oauth.callback_host.as_deref(), Some("localhost"));
        assert_eq!(oauth.callback_path.as_deref(), Some("/"));
        assert_eq!(oauth.prompt.as_deref(), Some("select_account"));
        assert_eq!(oauth.login_hint.as_deref(), Some("dev@example.com"));
        assert!(!format!("{:?}", config.accounts[0]).contains("registered-public-client"));
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
    fn gmail_shorthand_uses_plain_email_and_secure_provider_hosts() {
        let mut config: Config = toml::from_str(r#"
            [[accounts]]
            name = "Personal"
            email = "first.last+tag@gmail.com"
            incoming = "imap"
            outgoing = "smtp"
            copy_to = ""
            [accounts.oauth]
            provider = "google"
            client_id = "test-public-client"
        "#).unwrap();
        config.normalize_accounts();
        let account = &config.accounts[0];
        let incoming = crate::backend::ImapSource::parse(&account.source).unwrap();
        let outgoing = crate::outgoing::SmtpSource::parse(account.outgoing.as_deref().unwrap()).unwrap();
        assert_eq!(incoming.username, "first.last+tag@gmail.com");
        assert_eq!(incoming.host, "imap.gmail.com");
        assert_eq!(incoming.port, 993);
        assert_eq!(outgoing.username, incoming.username);
        assert_eq!(outgoing.host, "smtp.gmail.com");
        assert_eq!(outgoing.port, 465);
        assert!(account.source.contains("%40"));
        assert!(account.source.contains("first.last+tag%40gmail.com"));
        assert_eq!(account.copy_to.as_deref(), Some(""));
        assert!(!format!("{account:?}").contains("first.last"));
    }

    #[test]
    fn google_workspace_uses_oauth_provider_even_with_custom_email_domain() {
        let mut config: Config = toml::from_str(r#"
            [[accounts]]
            name = "University"
            email = "student@university.edu"
            source = "imap"
            outgoing = "starttls"
            [accounts.oauth]
            provider = "google"
            client_id = "test-public-client"
        "#).unwrap();
        config.normalize_accounts();
        let account = &config.accounts[0];
        assert_eq!(crate::backend::ImapSource::parse(&account.source).unwrap().host, "imap.gmail.com");
        let smtp = crate::outgoing::SmtpSource::parse(account.outgoing.as_deref().unwrap()).unwrap();
        assert_eq!(smtp.host, "smtp.gmail.com");
        assert_eq!(smtp.port, 587);
    }

    #[test]
    fn microsoft_preset_uses_starttls_587() {
        let mut config: Config = toml::from_str(r#"
            [[accounts]]
            name = "Work"
            email = "worker@example.edu"
            source = "imap"
            outgoing = "smtp"
            [accounts.oauth]
            provider = "microsoft"
            client_id = "test-public-client"
        "#).unwrap();
        config.normalize_accounts();
        let account = &config.accounts[0];
        assert_eq!(crate::backend::ImapSource::parse(&account.source).unwrap().host, "outlook.office365.com");
        let smtp = crate::outgoing::SmtpSource::parse(account.outgoing.as_deref().unwrap()).unwrap();
        assert_eq!(smtp.host, "smtp.office365.com");
        assert_eq!(smtp.port, 587);
    }

    #[test]
    fn custom_provider_requires_explicit_hosts_and_respects_port_overrides() {
        let mut config: Config = toml::from_str(r#"
            [[accounts]]
            name = "Custom"
            email = "person@example.org"
            incoming = "imap"
            outgoing = "smtp"
            imap_host = "mail.example.org"
            smtp_host = "relay.example.org"
            imap_port = 1993
            smtp_port = 2525
        "#).unwrap();
        config.normalize_accounts();
        let account = &config.accounts[0];
        let imap = crate::backend::ImapSource::parse(&account.source).unwrap();
        let smtp = crate::outgoing::SmtpSource::parse(account.outgoing.as_deref().unwrap()).unwrap();
        assert_eq!(imap.port, 1993);
        assert_eq!(imap.host, "mail.example.org");
        assert_eq!(smtp.port, 2525);
        assert_eq!(smtp.host, "relay.example.org");
        assert!(account.outgoing.as_deref().unwrap().starts_with("smtp://"));
    }

    #[test]
    fn shorthand_fails_closed_on_missing_hosts_or_host_injection() {
        let mut config: Config = toml::from_str(r#"
            [[accounts]]
            name = "Unknown"
            email = "person@unknown.test"
            source = "imap"
            outgoing = "smtp"
        "#).unwrap();
        assert!(config.accounts[0].normalize_shorthand().is_err());
        let mut config: Config = toml::from_str(r#"
            [[accounts]]
            name = "Unknown"
            email = "person@unknown.test"
            source = "imap"
            imap_host = "mail.test@evil.test"
        "#).unwrap();
        assert!(config.accounts[0].normalize_shorthand().is_err());
    }

    #[test]
    fn existing_uris_maildir_and_sendmail_are_not_modified() {
        let mut config: Config = toml::from_str(r#"
            [[accounts]]
            name = "Legacy"
            source = "imaps+oauthbearer://user%40example.com@imap.example.com:993"
            outgoing = "smtps+xoauth2://user%40example.com@smtp.example.com:465"

            [[accounts]]
            name = "Local"
            source = "maildir:///tmp/example"
            outgoing = "/usr/sbin/sendmail"
        "#).unwrap();
        let original = config.accounts.iter().map(|a| (a.source.clone(), a.outgoing.clone())).collect::<Vec<_>>();
        config.normalize_accounts();
        let updated = config.accounts.iter().map(|a| (a.source.clone(), a.outgoing.clone())).collect::<Vec<_>>();
        assert_eq!(original, updated);
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

    #[test]
    fn separate_auth_usernames_preserve_display_identity() {
        let mut config: Config = toml::from_str(r#"
            [[accounts]]
            name = "University"
            email = "preferred.name@dartmouth.edu"
            imap_username = "f00851y@dartmouth.edu"
            smtp_username = "f00851y@dartmouth.edu"
            source = "imap"
            outgoing = "starttls"
            [accounts.oauth]
            provider = "microsoft"
            client_id = "test-public-client"
        "#).unwrap();
        config.normalize_accounts();
        let account = &config.accounts[0];
        let imap = crate::backend::ImapSource::parse(&account.source).unwrap();
        let smtp = crate::outgoing::SmtpSource::parse(account.outgoing.as_deref().unwrap()).unwrap();
        assert_eq!(imap.username, "f00851y@dartmouth.edu");
        assert_eq!(smtp.username, "f00851y@dartmouth.edu");
        assert_eq!(smtp.host, "smtp.office365.com");
        assert_eq!(smtp.port, 587);
        assert_eq!(crate::outgoing::sender_from_account(account), "preferred.name@dartmouth.edu");
        assert!(!format!("{account:?}").contains("f00851y"));
    }

    #[test]
    fn full_smtp_uri_is_not_overwritten_by_shorthand_username() {
        let mut config: Config = toml::from_str(r#"
            [[accounts]]
            name = "Custom"
            email = "display@example.com"
            source = "imap"
            imap_host = "imap.example.com"
            smtp_username = "override@example.com"
            outgoing = "smtp+xoauth2://explicit%40example.com@smtp.example.com:587"
        "#).unwrap();
        config.normalize_accounts();
        let smtp = crate::outgoing::SmtpSource::parse(config.accounts[0].outgoing.as_deref().unwrap()).unwrap();
        assert_eq!(smtp.username, "explicit@example.com");
    }

}
