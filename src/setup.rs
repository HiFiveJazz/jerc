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
/// The stage progress now lives in the popup border title, so no
/// additional content rows are reserved above the wizard body.
pub const PROGRESS_ROWS: u16 = 0;
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
pub enum SetupStep {
    /// Only the account name and visible email address are needed initially.
    Identity,
    /// Discovered settings are editable; the sign-in order depends on provider.
    Settings,
    /// Microsoft alias accounts authenticate before their mailbox UPN is known.
    Authenticating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupDisplayRow {
    Field(usize),
    BoxTop(&'static str),
    BoxBottom,
}

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
    pub step: SetupStep,
    pub advanced: bool,
    pub spinner_frame: usize,
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
            step: SetupStep::Identity,
            advanced: false,
            spinner_frame: 0,
            edited: [false; FIELD_COUNT],
            selected: NAME,
            message: "Enter an account name and email, then press Enter to find settings.".into(),
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

    /// Advanced OAuth details are hidden unless the user explicitly requests them.
    /// Most people only need to confirm their incoming/outgoing mail identity.
    pub fn visible_fields(&self) -> Vec<usize> {
        if self.step == SetupStep::Identity { return vec![NAME, EMAIL]; }
        if self.step == SetupStep::Authenticating { return Vec::new(); }
        let mut fields = vec![PROVIDER, IMAP_HOST, IMAP_PORT, IMAP_LOGIN,
            SMTP_HOST, SMTP_PORT, SMTP_LOGIN];
        if self.advanced {
            fields.push(CLIENT_ID);
            match self.provider_hint() {
                "microsoft" => fields.push(TENANT),
                "google" => fields.push(GOOGLE_SECRET),
                _ => { fields.push(TENANT); fields.push(GOOGLE_SECRET); }
            }
        }
        fields
    }

    pub fn display_rows(&self) -> Vec<SetupDisplayRow> {
        use SetupDisplayRow::{Field, BoxTop, BoxBottom};
        match self.step {
            SetupStep::Identity => return vec![Field(NAME), Field(EMAIL)],
            SetupStep::Authenticating => return Vec::new(),
            SetupStep::Settings => {}
        }
        let mut rows = vec![Field(PROVIDER),
            BoxTop("Incoming · IMAP"), Field(IMAP_HOST), Field(IMAP_PORT),
            Field(IMAP_LOGIN), BoxBottom,
            BoxTop("Outgoing · SMTP"), Field(SMTP_HOST), Field(SMTP_PORT),
            Field(SMTP_LOGIN), BoxBottom];
        if self.advanced {
            rows.push(BoxTop("Advanced OAuth settings"));
            rows.push(Field(CLIENT_ID));
            match self.provider_hint() {
                "microsoft" => rows.push(Field(TENANT)),
                "google" => rows.push(Field(GOOGLE_SECRET)),
                _ => { rows.push(Field(TENANT)); rows.push(Field(GOOGLE_SECRET)); }
            }
            rows.push(BoxBottom);
        }
        rows
    }

    pub fn popup_size(&self, screen_width: u16, screen_height: u16) -> (u16, u16) {
        let max_height = match self.step {
            SetupStep::Identity | SetupStep::Authenticating => 13,
            SetupStep::Settings if self.advanced => 28,
            SetupStep::Settings => 23,
        };
        (screen_width.saturating_sub(2).min(96).max(1),
         screen_height.saturating_sub(2).min(max_height).max(1))
    }

    /// The active stage on the fixed four-part setup progress header.
    /// Microsoft can sign in before showing the editable settings, so the
    /// indicator represents the actual screen rather than a monotonic counter.
    pub fn progress_stage(&self) -> usize {
        if self.saved { 3 } else {
            match self.step {
                SetupStep::Identity => 0,
                SetupStep::Settings => 1,
                SetupStep::Authenticating => 2,
            }
        }
    }

    pub fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        FRAMES[self.spinner_frame % FRAMES.len()]
    }

    pub fn spin(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    pub fn discovery_ready(&self) -> Result<()> {
        ensure!(valid_mailbox_name(self.fields[EMAIL].trim()),
            "enter a complete email address");
        let name = self.fields[NAME].trim();
        ensure!(!name.is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
            "enter an account name (1–80 characters)");
        Ok(())
    }

    pub fn key(&mut self, key: KeyEvent) -> WizardAction {
        // The confirmation page is read-only; Enter is handled by App to
        // reload the newly configured account.
        if self.saved { return WizardAction::Nothing; }
        // Do not allow an in-flight browser sign-in to mutate the identity or
        // settings from which its OAuth state was created.
        if self.authenticating {
            return WizardAction::Nothing;
        }
        if self.discovering {
            if key.code == KeyCode::Esc {
                self.discovering = false;
                self.message = "Discovery cancelled. Edit the address or press Enter to retry.".into();
            }
            return WizardAction::Nothing;
        }
        if self.step == SetupStep::Authenticating { return WizardAction::Nothing; }
        if self.step == SetupStep::Settings && key.code == KeyCode::F(3) {
            self.advanced = !self.advanced;
            if !self.visible_fields().contains(&self.selected) { self.selected = PROVIDER; }
            return WizardAction::Nothing;
        }
        if self.step == SetupStep::Settings && self.login_failed {
            if key.code == KeyCode::F(7) { return WizardAction::SaveOffline; }
            if key.code == KeyCode::F(6) { return WizardAction::Save; }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('s') if self.step == SetupStep::Identity => WizardAction::Discover,
                KeyCode::Char('s') => WizardAction::Save,
                _ => WizardAction::Nothing,
            };
        }

        if self.step == SetupStep::Identity {
            return match key.code {
                KeyCode::Esc => WizardAction::Close,
                KeyCode::F(5) => WizardAction::Discover,
                KeyCode::Up | KeyCode::BackTab => {
                    self.selected = NAME;
                    WizardAction::Nothing
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter if self.selected == NAME => {
                    self.selected = EMAIL;
                    WizardAction::Nothing
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter => WizardAction::Discover,
                KeyCode::Backspace => { self.edit_field(None); WizardAction::Nothing }
                KeyCode::Char(ch) if !ch.is_control() => {
                    self.edit_field(Some(ch));
                    WizardAction::Nothing
                }
                _ => WizardAction::Nothing,
            };
        }

        let identity_before = [EMAIL, NAME, PROVIDER, CLIENT_ID, TENANT, GOOGLE_SECRET]
            .map(|index| self.fields[index].clone());
        let action = match key.code {
            KeyCode::Esc | KeyCode::F(4) => {
                self.step = SetupStep::Identity;
                self.selected = EMAIL;
                self.error = false;
                self.message = "Edit your email or account name, then find settings again.".into();
                WizardAction::Nothing
            }
            KeyCode::F(5) => WizardAction::Discover,
            KeyCode::Enter => WizardAction::Save,
            KeyCode::Up | KeyCode::BackTab => {
                let rows = self.visible_fields();
                let at = rows.iter().position(|field| *field == self.selected).unwrap_or(0);
                self.selected = rows[at.saturating_sub(1)];
                WizardAction::Nothing
            }
            KeyCode::Down | KeyCode::Tab => {
                let rows = self.visible_fields();
                let at = rows.iter().position(|field| *field == self.selected).unwrap_or(0);
                self.selected = rows[(at + 1).min(rows.len() - 1)];
                WizardAction::Nothing
            }
            KeyCode::Left | KeyCode::Right if self.selected == PROVIDER => {
                let choices = ["Auto", "Microsoft 365", "Google"];
                let at = choices.iter().position(|choice| *choice == self.fields[PROVIDER])
                    .unwrap_or(0);
                let next = if key.code == KeyCode::Left {
                    (at + choices.len() - 1) % choices.len()
                } else { (at + 1) % choices.len() };
                self.fields[PROVIDER] = choices[next].into();
                self.edited[PROVIDER] = true;
                WizardAction::Nothing
            }
            KeyCode::Backspace if self.selected != PROVIDER => {
                self.edit_field(None);
                WizardAction::Nothing
            }
            KeyCode::Char(ch) if self.selected != PROVIDER && !ch.is_control() => {
                self.edit_field(Some(ch));
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

    fn edit_field(&mut self, ch: Option<char>) {
        if let Some(ch) = ch {
            if self.fields[self.selected].len() < 256 {
                self.fields[self.selected].push(ch);
            }
        } else {
            self.fields[self.selected].pop();
        }
        self.edited[self.selected] = true;
        if self.selected == EMAIL || self.selected == NAME {
            self.authenticated = false;
            self.login_failed = false;
            if self.selected == EMAIL { self.source = None; }
        }
        self.error = false;
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
        let unknown_microsoft_login = microsoft && !self.edited[IMAP_LOGIN] && !self.edited[SMTP_LOGIN]
            && !self.authenticated;
        self.step = if unknown_microsoft_login {
            SetupStep::Authenticating
        } else { SetupStep::Settings };
        self.selected = PROVIDER;
        self.message = if unknown_microsoft_login {
            "Microsoft 365 found. Sign in to identify your actual mailbox login…".into()
        } else {
            String::new()
        };
        self.discovering = false;
        self.error = false;
    }

    /// Enter the dedicated browser sign-in stage for any OAuth provider.
    /// The completion handler returns to the editable settings review.
    pub fn begin_authentication(&mut self) {
        self.step = SetupStep::Authenticating;
        self.authenticating = true;
        self.login_failed = false;
        self.error = false;
        self.spinner_frame = 0;
        self.message = "Waiting for browser sign-in…".into();
    }

    /// OAuth may reveal a different UPN after the user signs in. Never replace
    /// a username that was edited by hand: show the suggestion for review.
    pub fn suggest_login(&mut self, login: Option<String>) {
        self.step = SetupStep::Settings;
        self.selected = IMAP_LOGIN;
        self.authenticated = true;
        self.authenticating = false;
        self.login_failed = false;
        if let Some(login) = login {
            for index in [IMAP_LOGIN, SMTP_LOGIN] {
                if !self.edited[index] { self.fields[index] = login.clone(); }
            }
            self.message = "Signed in. Review both suggested mailbox usernames, then Enter saves.".into();
        } else if self.provider_hint() == "microsoft" {
            self.message = "Signed in, but Microsoft did not return a username hint. Verify both logins before saving.".into();
        } else {
            self.message = "Signed in successfully. Review the detected settings, then Enter saves.".into();
        }
        self.error = false;
    }

    pub fn discovery_failed(&mut self, message: &str) {
        self.discovering = false;
        self.error = true;
        // Discovery failures should remain recoverable without a hidden
        // F2 shortcut: display editable manual settings in the review step.
        self.step = SetupStep::Settings;
        self.selected = PROVIDER;
        self.source = None;
        self.message = format!("Automatic discovery failed: {message}. Enter the server settings manually, or F4 to change the email address.");
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

/// Remove an account from the *local* config, never an IMAP folder or any
/// server-side mail. Preserve unrelated sections, comments and private mode.
/// Refuse ambiguous config layouts rather than risk deleting another account.
pub fn remove_account(path: &Path, name: &str) -> Result<()> {
    let destination = if path.exists() { path.canonicalize()? } else { path.to_owned() };
    let raw = fs::read_to_string(&destination)
        .with_context(|| format!("reading {}", destination.display()))?;
    let config: Config = toml::from_str(&raw).context("parsing account config before removal")?;
    ensure!(config.accounts.iter().filter(|account| account.name == name).count() == 1,
        "expected exactly one locally configured account with that name");
    let target = config.accounts.iter().position(|account| account.name == name)
        .ok_or_else(|| anyhow!("account not found"))?;

    let mut sections: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        let heading = line.trim();
        if heading.starts_with('[') && heading.ends_with(']') {
            sections.push((offset, heading));
        }
        offset += line.len();
    }
    let accounts: Vec<usize> = sections.iter()
        .filter_map(|(at, heading)| (*heading == "[[accounts]]").then_some(*at))
        .collect();
    ensure!(accounts.len() == config.accounts.len(),
        "account table layout is ambiguous; edit config.toml manually");
    let start = accounts[target];
    let end = sections.iter().filter(|(at, _)| *at > start)
        .find(|(_, heading)| *heading == "[[accounts]]"
            || (!heading.starts_with("[accounts.") && !heading.starts_with("[[accounts.")))
        .map(|(at, _)| *at).unwrap_or(raw.len());
    let mut result = raw.clone();
    result.replace_range(start..end, "");
    // Do not discard formatting or comments outside the removed account.
    // Validate the resulting TOML *before* touching disk.
    let parsed: Config = toml::from_str(&result).context("checking config after account removal")?;
    ensure!(parsed.accounts.len() + 1 == config.accounts.len()
        && parsed.accounts.iter().all(|account| account.name != name),
        "account removal would alter unexpected account entries");
    let parent = destination.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&destination)?.permissions().mode() & 0o600;
        tmp.as_file().set_permissions(fs::Permissions::from_mode(mode))?;
    }
    tmp.write_all(result.as_bytes())?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(&destination).map_err(|error| anyhow!("saving account removal: {}", error.error))?;
    Ok(())
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
    fn wizard_progress_matches_view_and_confirmation() {
        let mut wizard = SetupWizard::new(false);
        assert_eq!(wizard.progress_stage(), 0);
        wizard.step = SetupStep::Settings;
        assert_eq!(wizard.progress_stage(), 1);
        wizard.step = SetupStep::Authenticating;
        assert_eq!(wizard.progress_stage(), 2);
        wizard.step = SetupStep::Settings;
        wizard.saved = true;
        assert_eq!(wizard.progress_stage(), 3);
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)), WizardAction::Nothing);
    }

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
        wizard.fields[NAME] = "Example".into();
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)), WizardAction::Nothing);
        assert_eq!(wizard.selected, EMAIL);
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), WizardAction::Discover);
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
        wizard.step = SetupStep::Settings;
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)), WizardAction::Save);
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)), WizardAction::SaveOffline);
    }

    #[test]
    fn identity_first_then_discovered_settings_then_login() {
        let mut wizard = SetupWizard::new(true);
        assert_eq!(wizard.step, SetupStep::Identity);
        assert_eq!(wizard.visible_fields(), vec![NAME, EMAIL]);
        wizard.fields[NAME] = "Campus".into();
        wizard.fields[EMAIL] = "alias@example.edu".into();
        assert!(wizard.discovery_ready().is_ok());
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), WizardAction::Nothing);
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), WizardAction::Discover);
        let discovered = crate::discovery::discover_with_provider("alias@example.edu", "microsoft").unwrap();
        wizard.apply_discovery(discovered);
        assert_eq!(wizard.step, SetupStep::Authenticating);
        assert_eq!(wizard.fields[IMAP_HOST], "outlook.office365.com");
        wizard.suggest_login(Some("netid@example.edu".into()));
        assert_eq!(wizard.step, SetupStep::Settings);
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), WizardAction::Save);
        wizard.authenticating = true;
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)), WizardAction::Nothing);
        wizard.suggest_login(Some("netid@example.edu".into()));
        assert_eq!(wizard.fields[SMTP_LOGIN], "netid@example.edu");
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), WizardAction::Save);
    }

    #[test]
    fn google_sign_in_uses_its_own_stage_before_returning_to_review() {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[PROVIDER] = "Google".into();
        wizard.step = SetupStep::Settings;
        assert_eq!(wizard.progress_stage(), 1);
        wizard.begin_authentication();
        assert_eq!(wizard.progress_stage(), 2);
        assert_eq!(wizard.step, SetupStep::Authenticating);
        assert!(wizard.authenticating);
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), WizardAction::Nothing);
        wizard.suggest_login(None);
        assert_eq!(wizard.step, SetupStep::Settings);
        assert!(wizard.authenticated);
        assert!(!wizard.message.contains("Microsoft"));
    }

    #[test]
    fn invalid_address_is_rejected_and_discovery_failure_offers_manual_editing() {
        let mut wizard = SetupWizard::new(true);
        wizard.fields[NAME] = "Campus".into();
        wizard.fields[EMAIL] = "not-an-address".into();
        assert!(wizard.discovery_ready().is_err());
        assert_eq!(wizard.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)), WizardAction::Nothing);
        assert_eq!(wizard.step, SetupStep::Identity);
        wizard.discovery_failed("no autoconfig records");
        assert_eq!(wizard.step, SetupStep::Settings);
        assert_eq!(wizard.selected, PROVIDER);
        assert!(wizard.error);
    }

    #[test]
    fn discovered_settings_remain_editable_and_tab_skips_hidden_fields() {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[PROVIDER] = "Google".into();
        wizard.step = SetupStep::Settings;
        wizard.selected = CLIENT_ID;
        wizard.advanced = true;
        wizard.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(wizard.selected, GOOGLE_SECRET);
        assert!(!wizard.visible_fields().contains(&TENANT));
        wizard.key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
        assert_eq!(wizard.step, SetupStep::Identity);
        assert_eq!(wizard.selected, EMAIL);
    }

    #[test]
    fn spinner_cycles_while_waiting() {
        let mut wizard = SetupWizard::new(true);
        let initial = wizard.spinner();
        wizard.spin();
        assert_ne!(wizard.spinner(), initial);
        for _ in 0..7 { wizard.spin(); }
        assert_eq!(wizard.spinner(), initial);
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


    #[test]
    fn microsoft_discovery_authenticates_before_review_and_preserves_alias() {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[NAME] = "Campus".into();
        wizard.fields[EMAIL] = "alias@example.edu".into();
        wizard.apply_discovery(crate::discovery::discover_with_provider(
            "alias@example.edu", "microsoft").unwrap());
        assert_eq!(wizard.step, SetupStep::Authenticating);
        assert!(wizard.visible_fields().is_empty());
        wizard.suggest_login(Some("netid@example.edu".into()));
        assert_eq!(wizard.step, SetupStep::Settings);
        assert_eq!(wizard.fields[EMAIL], "alias@example.edu");
        assert_eq!(wizard.fields[IMAP_LOGIN], "netid@example.edu");
        assert_eq!(wizard.fields[SMTP_LOGIN], "netid@example.edu");
        assert!(!wizard.visible_fields().contains(&CLIENT_ID));
        wizard.key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
        assert!(wizard.visible_fields().contains(&CLIENT_ID));
        assert!(wizard.display_rows().iter().any(|row|
            matches!(row, SetupDisplayRow::BoxTop("Incoming · IMAP"))));
    }
    #[test]
    fn removing_only_local_account_preserves_other_accounts_and_theme() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        fs::write(&file, concat!(
            "# keep this header\n[ui]\ntheme = \"nord\"\n",
            "[[accounts]]\nname = \"First\"\nemail = \"a@example.com\"\n",
            "source = \"imap\"\n[accounts.oauth]\nprovider = \"microsoft\"\n",
            "[[accounts]]\nname = \"Second\"\nemail = \"b@example.com\"\n",
            "source = \"imap\"\n[accounts.oauth]\nprovider = \"google\"\n",
        )).unwrap();
        remove_account(&file, "First").unwrap();
        let saved = fs::read_to_string(&file).unwrap();
        assert!(saved.contains("# keep this header"));
        assert!(saved.contains("theme = \"nord\""));
        assert!(!saved.contains("name = \"First\""));
        assert!(saved.contains("name = \"Second\""));
        assert_eq!(Config::load_or_empty(Some(&file)).unwrap().accounts.len(), 1);
        remove_account(&file, "Second").unwrap();
        assert!(Config::load_or_empty(Some(&file)).unwrap().accounts.is_empty());
        assert!(fs::read_to_string(&file).unwrap().contains("theme = \"nord\""));
    }

}
