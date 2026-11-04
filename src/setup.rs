//! Editable, non-destructive first-run and add-account wizard.
//! Discovery is a suggestion; only an explicit Save persists settings.
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{AccountConfig, Config};
use crate::discovery::MailConfiguration;

pub const FIELD_COUNT: usize = 12;
pub const EMAIL: usize = 0;
pub const NAME: usize = 1;
pub const PROVIDER: usize = 2;
pub const IMAP_HOST: usize = 3;
pub const IMAP_PORT: usize = 4;
pub const IMAP_LOGIN: usize = 5;
pub const SMTP_HOST: usize = 6;
pub const SMTP_PORT: usize = 7;
pub const SMTP_LOGIN: usize = 8;
pub const CLIENT_ID: usize = 9;
pub const TENANT: usize = 10;
pub const GOOGLE_SECRET: usize = 11;

pub const LABELS: [&str; FIELD_COUNT] = [
    "Email address", "Account name", "Mail provider", "IMAP server", "IMAP port",
    "IMAP username", "SMTP server", "SMTP port", "SMTP username",
    "OAuth application ID", "Microsoft tenant", "Google desktop secret",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardAction {
    Nothing,
    Close,
    Discover,
    Save,
    SaveOffline,
}

pub struct SetupWizard {
    pub fields: [String; FIELD_COUNT],
    pub edited: [bool; FIELD_COUNT],
    pub selected: usize,
    pub message: String,
    pub error: bool,
    pub discovering: bool,
    pub authenticating: bool,
    pub saved: bool,
    pub authenticated: bool,
    pub login_failed: bool,
    pub source: Option<String>,
    pub first_run: bool,
}

impl SetupWizard {
    pub fn new(first_run: bool) -> Self {
        let mut fields: [String; FIELD_COUNT] = std::array::from_fn(|_| String::new());
        fields[PROVIDER] = "Auto".into();
        fields[TENANT] = "common".into();
        Self {
            fields,
            edited: [false; FIELD_COUNT],
            selected: EMAIL,
            message: "Enter an address, then Tab to discover settings. F5 rediscovers.".into(),
            error: false,
            discovering: false,
            authenticating: false,
            saved: false,
            authenticated: false,
            login_failed: false,
            source: None,
            first_run,
        }
    }

    pub fn provider_hint(&self) -> &'static str {
        match self.fields[PROVIDER].as_str() {
            "Microsoft 365" => "microsoft",
            "Google" => "google",
            _ => "auto",
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> WizardAction {
        if self.authenticating {
            return if key.modifiers == KeyModifiers::CONTROL
                && key.code == KeyCode::Char('c') { WizardAction::Close }
            else { WizardAction::Nothing };
        }
        if key.code == KeyCode::F(7) && self.login_failed {
            return WizardAction::SaveOffline;
        }
        if self.login_failed && key.code == KeyCode::F(6) {
            self.login_failed = false;
            return WizardAction::Save;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('s') => WizardAction::Save,
                _ => WizardAction::Nothing,
            };
        }
        let identity_before = [EMAIL, NAME, PROVIDER, CLIENT_ID, TENANT, GOOGLE_SECRET]
            .map(|index| self.fields[index].clone());
        let action = match key.code {
            KeyCode::Esc => WizardAction::Close,
            KeyCode::F(5) if !self.discovering => WizardAction::Discover,
            KeyCode::Up | KeyCode::BackTab => {
                self.selected = self.selected.saturating_sub(1);
                WizardAction::Nothing
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Enter => {
                if self.selected == EMAIL && !self.edited[NAME] {
                    if let Some((_, domain)) = self.fields[EMAIL].trim().rsplit_once('@') {
                        let name = domain.split('.').next().unwrap_or("Mail");
                        let mut letters = name.chars();
                        if let Some(first) = letters.next() {
                            self.fields[NAME] = first.to_uppercase().collect::<String>() + letters.as_str();
                        }
                    }
                }
                let discover = self.selected == EMAIL && self.source.is_none()
                    && !self.discovering && self.fields[EMAIL].contains('@');
                if self.selected == FIELD_COUNT - 1 && key.code == KeyCode::Enter {
                    return WizardAction::Save;
                }
                self.selected = (self.selected + 1).min(FIELD_COUNT - 1);
                if discover { WizardAction::Discover } else { WizardAction::Nothing }
            }
            KeyCode::Left | KeyCode::Right if self.selected == PROVIDER => {
                let items = ["Auto", "Microsoft 365", "Google"];
                let current = items.iter().position(|item| *item == self.fields[PROVIDER]).unwrap_or(0);
                let next = if key.code == KeyCode::Left {
                    (current + items.len() - 1) % items.len()
                } else { (current + 1) % items.len() };
                self.fields[PROVIDER] = items[next].to_owned();
                self.edited[PROVIDER] = true;
                WizardAction::Nothing
            }
            KeyCode::Backspace if self.selected != PROVIDER => {
                self.fields[self.selected].pop();
                self.edited[self.selected] = true;
                if self.selected == EMAIL { self.source = None; }
                WizardAction::Nothing
            }
            KeyCode::Char(ch) if self.selected != PROVIDER && !ch.is_control() => {
                if self.fields[self.selected].len() < 256 {
                    self.fields[self.selected].push(ch);
                    self.edited[self.selected] = true;
                    if self.selected == EMAIL { self.source = None; }
                }
                WizardAction::Nothing
            }
            _ => WizardAction::Nothing,
        };
        if self.authenticated && [EMAIL, NAME, PROVIDER, CLIENT_ID, TENANT, GOOGLE_SECRET]
            .iter().enumerate().any(|(i, index)| self.fields[*index] != identity_before[i])
        {
            self.authenticated = false;
            self.login_failed = false;
            self.error = false;
            self.message = "Account identity changed. Sign in again before saving.".into();
        }
        action
    }

    pub fn apply_discovery(&mut self, proposal: MailConfiguration) {
        let microsoft = proposal.imap.host.eq_ignore_ascii_case("outlook.office365.com");
        let google = proposal.imap.host.eq_ignore_ascii_case("imap.gmail.com");
        if !self.edited[PROVIDER] {
            self.fields[PROVIDER] = if microsoft { "Microsoft 365" }
                else if google { "Google" } else { "Auto" }.into();
        }
        for (index, value) in [
            (IMAP_HOST, proposal.imap.host),
            (IMAP_PORT, proposal.imap.port.to_string()),
            (IMAP_LOGIN, proposal.imap.username),
            (SMTP_HOST, proposal.smtp.host),
            (SMTP_PORT, proposal.smtp.port.to_string()),
            (SMTP_LOGIN, proposal.smtp.username),
        ] {
            if !self.edited[index] {
                self.fields[index] = value;
            }
        }
        self.source = Some(format!("{} ({})", proposal.provider, proposal.source));
        self.message = "Settings discovered. Review and edit every field before saving.".into();
        self.discovering = false;
        self.error = false;
    }

    /// OAuth may reveal a different UPN after the user signs in. Never replace
    /// a username that was edited by hand: show the suggestion for review.
    pub fn suggest_login(&mut self, login: Option<String>) {
        self.authenticated = true;
        self.authenticating = false;
        self.login_failed = false;
        if let Some(login) = login {
            for index in [IMAP_LOGIN, SMTP_LOGIN] {
                if !self.edited[index] { self.fields[index] = login.clone(); }
            }
            self.message = "OAuth succeeded. Verify the suggested login UPN, then Ctrl-S saves.".into();
        } else {
            self.message = "OAuth succeeded. Confirm both login usernames, then Ctrl-S saves.".into();
        }
        self.error = false;
    }

    pub fn discovery_failed(&mut self, message: &str) {
        self.discovering = false;
        self.error = true;
        self.message = format!("Discovery unavailable: {message}. Choose a provider and press F5.");
    }

    fn validate(&self) -> Result<String> {
        let values = &self.fields;
        let email = values[EMAIL].trim();
        ensure!(valid_mailbox_name(email), "enter a complete email address");
        let name = values[NAME].trim();
        ensure!(!name.is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
            "account name must contain 1–80 printable characters");
        for (idx, label) in [(IMAP_HOST, "IMAP"), (SMTP_HOST, "SMTP")] {
            ensure!(valid_hostname(values[idx].trim()), "{label} hostname must be a valid hostname (no URI or credentials)");
        }
        let imap_port: u16 = values[IMAP_PORT].trim().parse().context("enter a valid IMAP port")?;
        let smtp_port: u16 = values[SMTP_PORT].trim().parse().context("enter a valid SMTP port")?;
        ensure!(imap_port == 993, "this IMAP backend requires TLS on port 993");
        ensure!(smtp_port == 587 || smtp_port == 465, "use SMTP STARTTLS on 587 or implicit TLS on 465");
        for (idx, label) in [(IMAP_LOGIN, "IMAP"), (SMTP_LOGIN, "SMTP")] {
            let value = values[idx].trim();
            ensure!(value.is_empty() || valid_mailbox_name(value), "{label} username must be an email-style login");
        }
        let provider = match self.provider_hint() {
            "microsoft" => "microsoft",
            "google" => "google",
            _ if values[IMAP_HOST].eq_ignore_ascii_case("outlook.office365.com") => "microsoft",
            _ if values[IMAP_HOST].eq_ignore_ascii_case("imap.gmail.com") => "google",
            _ => bail!("choose Microsoft 365 or Google for OAuth, or configure this custom provider manually"),
        };
        let client_id = values[CLIENT_ID].trim();
        ensure!(!client_id.chars().any(|ch| ch.is_whitespace() || ch.is_control()),
            "OAuth application ID must be one line");
        // The Google OAuth client ID includes its DNS suffix. It is not an
        // optional decoration: removing it produces a different client ID.
        // Also validate the publisher's build-time default, if one exists.
        let effective_client_id = if !client_id.is_empty() {
            Some(client_id)
        } else {
            crate::oauth::embedded_client_id(provider)
        };
        if provider == "google" {
            if let Some(id) = effective_client_id {
                ensure!(crate::oauth::valid_google_desktop_client_id(id),
                    "Google OAuth client ID is incomplete: keep the full .apps.googleusercontent.com suffix");
            }
        }
        let google_secret = values[GOOGLE_SECRET].trim();
        ensure!(!google_secret.chars().any(char::is_control),
            "Google desktop client secret must be a single line");
        let tenant = values[TENANT].trim();
        ensure!(!tenant.chars().any(|c| c.is_control() || c.is_whitespace()), "invalid tenant");

        let mut text = format!("[[accounts]]\nname = {}\nemail = {}\nsource = \"imap\"\noutgoing = {}\n",
            quoted(name), quoted(email), quoted(if smtp_port == 465 { "smtps" } else { "starttls" }));
        for (key, index) in [("imap_host", IMAP_HOST), ("smtp_host", SMTP_HOST)] {
            text.push_str(&format!("{key} = {}\n", quoted(values[index].trim())));
        }
        text.push_str(&format!("imap_port = {imap_port}\nsmtp_port = {smtp_port}\n"));
        for (key, index) in [("imap_username", IMAP_LOGIN), ("smtp_username", SMTP_LOGIN)] {
            if !values[index].trim().is_empty() && values[index].trim() != email {
                text.push_str(&format!("{key} = {}\n", quoted(values[index].trim())));
            }
        }
        text.push_str(&format!("[accounts.oauth]\nprovider = {}\nflow = \"browser\"\ncallback_host = \"localhost\"\ncallback_path = \"/\"\n", quoted(provider)));
        if !client_id.is_empty() { text.push_str(&format!("client_id = {}\n", quoted(client_id))); }
        // A Google *desktop* application's client secret is a public client
        // credential, not a confidential web-server secret. Store it only when
        // explicitly entered, or use the publisher build-time default.
        if provider == "google" && !google_secret.is_empty() {
            text.push_str(&format!("client_secret = {}\n", quoted(google_secret)));
        }
        if provider == "microsoft" && !tenant.is_empty() {
            text.push_str(&format!("tenant = {}\n", quoted(tenant)));
        }
        // Check that we can actually construct OAuth settings before writing
        // anything, including optional publisher-configured client IDs.
        let parsed: Config = toml::from_str(&text).context("validating generated account TOML")?;
        let account = parsed.accounts.first().ok_or_else(|| anyhow!("no generated account"))?;
        crate::oauth::OAuthSettings::for_account(account)?
            .ok_or_else(|| anyhow!("an OAuth provider is required"))?;
        Ok(text)
    }

    pub fn preview_account(&self) -> Result<AccountConfig> {
        let fragment = self.validate()?;
        let parsed: Config = toml::from_str(&fragment)?;
        parsed.accounts.into_iter().next().ok_or_else(|| anyhow!("missing new account"))
    }

    /// Preserve all existing settings/comments, then atomically replace the file.
    /// No credentials are written; only the public application ID is persisted.
    pub fn save(&self, path: &Path) -> Result<AccountConfig> {
        let fragment = self.validate()?;
        // Preserve an existing dotfiles symlink rather than replacing it.
        let destination = if path.exists() { path.canonicalize()? } else { path.to_owned() };
        let path = destination.as_path();
        let old = match fs::read_to_string(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let mut existing: Config = if old.trim().is_empty() {
            toml::from_str("[ui]\n")?
        } else { toml::from_str(&old).context("parsing existing config before updating")? };
        ensure!(!existing.accounts.iter().any(|account| account.name == self.fields[NAME].trim()),
            "an account with this name already exists; choose a unique account name");
        let mut merged = old;
        if !merged.is_empty() && !merged.ends_with('\n') { merged.push('\n'); }
        merged.push('\n');
        merged.push_str(&fragment);
        existing = toml::from_str(&merged).context("validating merged config")?;
        let new_account = existing.accounts.last().cloned().ok_or_else(|| anyhow!("missing saved account"))?;
        let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            // Never loosen the permissions of a private existing config.
            let mode = fs::metadata(path).ok().map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
            tmp.as_file().set_permissions(fs::Permissions::from_mode(mode & 0o600))?;
        }
        tmp.write_all(merged.as_bytes())?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|err| anyhow!("persisting account config: {}", err.error))?;
        Ok(new_account)
    }
}

fn quoted(text: &str) -> String { toml::Value::String(text.to_owned()).to_string() }
fn valid_mailbox_name(text: &str) -> bool {
    let Some((local, domain)) = text.split_once('@') else { return false; };
    !local.is_empty() && !local.contains('@') && valid_hostname(domain)
        && !text.chars().any(|ch| ch.is_control() || ch.is_whitespace())
}
fn valid_hostname(host: &str) -> bool {
    host.len() <= 253 && host.contains('.') && !host.contains('/') &&
        host.split('.').all(|part| !part.is_empty() && part.len() <= 63 &&
            !part.starts_with('-') && !part.ends_with('-') &&
            part.bytes().all(|ch| ch.is_ascii_alphanumeric() || ch == b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_config_keeps_alias_separate_from_login() {
        let mut wizard = SetupWizard::new(true);
        wizard.fields[EMAIL] = "alias@example.edu".into();
        wizard.fields[NAME] = "University".into();
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        wizard.fields[IMAP_HOST] = "outlook.office365.com".into();
        wizard.fields[SMTP_HOST] = "smtp.office365.com".into();
        wizard.fields[IMAP_PORT] = "993".into();
        wizard.fields[SMTP_PORT] = "587".into();
        wizard.fields[IMAP_LOGIN] = "netid@example.edu".into();
        wizard.fields[SMTP_LOGIN] = "netid@example.edu".into();
        wizard.fields[CLIENT_ID] = "example-public-client".into();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[ui]\ntheme = \"nord\"\n").unwrap();
        let saved = wizard.save(&path).unwrap();
        assert_eq!(saved.email.as_deref(), Some("alias@example.edu"));
        assert_eq!(saved.smtp_username.as_deref(), Some("netid@example.edu"));
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.ui.theme, "nord");
        assert_eq!(config.accounts[0].smtp_username.as_deref(), Some("netid@example.edu"));
    }

    #[test]
    fn duplicate_account_never_modifies_config() {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[EMAIL] = "alias@example.edu".into();
        wizard.fields[NAME] = "University".into();
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        wizard.fields[IMAP_HOST] = "outlook.office365.com".into();
        wizard.fields[SMTP_HOST] = "smtp.office365.com".into();
        wizard.fields[IMAP_PORT] = "993".into();
        wizard.fields[SMTP_PORT] = "587".into();
        wizard.fields[CLIENT_ID] = "example-client".into();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        wizard.save(&path).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(wizard.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn invalid_host_rejected_without_writing() {
        let mut wizard = SetupWizard::new(true);
        wizard.fields[EMAIL] = "a@example.edu".into();
        wizard.fields[NAME] = "School".into();
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        wizard.fields[IMAP_HOST] = "http://attacker.example".into();
        wizard.fields[SMTP_HOST] = "smtp.office365.com".into();
        wizard.fields[IMAP_PORT] = "993".into();
        wizard.fields[SMTP_PORT] = "587".into();
        wizard.fields[CLIENT_ID] = "client".into();
        let path = tempfile::tempdir().unwrap().path().join("config.toml");
        assert!(wizard.save(&path).is_err());
    }

    #[test]
    fn discovered_values_remain_editable_and_login_hint_does_not_override_manual_input() {
        let mut wizard = SetupWizard::new(true);
        wizard.fields[EMAIL] = "alias@example.edu".into();
        let change = wizard.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(change, WizardAction::Discover);
        assert_eq!(wizard.fields[NAME], "Example");
        let config = crate::discovery::discover_with_provider("alias@example.edu", "microsoft").unwrap();
        wizard.apply_discovery(config);
        assert_eq!(wizard.fields[SMTP_HOST], "smtp.office365.com");
        wizard.fields[IMAP_LOGIN] = "manual@example.edu".into();
        wizard.edited[IMAP_LOGIN] = true;
        wizard.suggest_login(Some("netid@example.edu".into()));
        assert_eq!(wizard.fields[IMAP_LOGIN], "manual@example.edu");
        assert_eq!(wizard.fields[SMTP_LOGIN], "netid@example.edu");
    }

    #[test]
    fn gmail_client_id_suffix_cannot_be_removed() {
        let mut wizard = SetupWizard::new(true);
        wizard.fields[EMAIL] = "name@gmail.com".into();
        wizard.fields[NAME] = "Gmail".into();
        wizard.fields[PROVIDER] = "Google".into();
        wizard.fields[IMAP_HOST] = "imap.gmail.com".into();
        wizard.fields[SMTP_HOST] = "smtp.gmail.com".into();
        wizard.fields[IMAP_PORT] = "993".into();
        wizard.fields[SMTP_PORT] = "465".into();
        wizard.fields[CLIENT_ID] = "123-abc".into();
        let error = wizard.preview_account().err().unwrap().to_string();
        assert!(error.contains(".apps.googleusercontent.com"));
        wizard.fields[CLIENT_ID] = "123-abc.apps.googleusercontent.com".into();
        wizard.fields[GOOGLE_SECRET] = "desktop-public-secret".into();
        let account = wizard.preview_account().unwrap();
        assert_eq!(account.oauth.unwrap().client_secret.as_deref(), Some("desktop-public-secret"));
    }

    #[test]
    fn failed_login_does_not_silently_save() {
        let mut wizard = SetupWizard::new(false);
        wizard.login_failed = true;
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)), WizardAction::Save);
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)), WizardAction::SaveOffline);
    }

    #[test]
    fn missing_or_empty_config_starts_with_zero_accounts() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("missing.toml");
        let config = Config::load_or_empty(Some(&path)).unwrap();
        assert!(config.accounts.is_empty());
        fs::write(&path, "# Only theme settings so far\n[ui]\ntheme = \"nord\"\n").unwrap();
        let config = Config::load_or_empty(Some(&path)).unwrap();
        assert!(config.accounts.is_empty());
        assert_eq!(config.ui.theme, "nord");
    }

}
