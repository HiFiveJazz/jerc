//! Editable, non-destructive first-run and add-account wizard.
//! Discovery is a suggestion; only an explicit Save persists settings.
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub mod probe;

static NEXT_TEST_REQUEST: AtomicU64 = AtomicU64::new(1);
static NEXT_OAUTH_REQUEST: AtomicU64 = AtomicU64::new(1);
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{AccountConfig, Config};
use crate::discovery::MailConfiguration;

pub const FIELD_COUNT: usize = 21;
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
pub const USERNAME: usize = 12;
pub const SECURITY: usize = 13;
pub const AUTH_METHOD: usize = 14;
pub const PASSWORD: usize = 15;
pub const SMTP_SECURITY: usize = 16;
pub const IMAP_SECURITY: usize = 17;
pub const SMTP_AUTH: usize = 18;
pub const OAUTH_ISSUER: usize = 19;
pub const OAUTH_SCOPE: usize = 20;

pub const LABELS: [&str; FIELD_COUNT] = [
    "Email address",
    "Account name",
    "Mail provider",
    "IMAP server",
    "IMAP port",
    "IMAP username",
    "SMTP server",
    "SMTP port",
    "SMTP username",
    "OAuth application ID",
    "Microsoft tenant",
    "Google desktop secret",
    "Username (both)",
    "Security (both)",
    "Authentication",
    "Password",
    "SMTP security",
    "IMAP security",
    "SMTP authentication",
    "OAuth issuer (HTTPS)",
    "OAuth scopes (optional)",
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
    TestConfiguration,
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
    pub oauth_request_id: u64,
    oauth_cancel: Option<Arc<AtomicBool>>,
    /// A newly authenticated wizard grant is provisional until config save.
    pub(crate) provisional_oauth_account: Option<AccountConfig>,
    pub source: Option<String>,
    pub first_run: bool,
    pub mixed_auth: bool,
    pub testing: bool,
    pub test_request: u64,
    pub last_test_result: Option<probe::ProbeReport>,
}

impl SetupWizard {
    pub fn new(first_run: bool) -> Self {
        let mut fields: [String; FIELD_COUNT] = std::array::from_fn(|_| String::new());
        fields[PROVIDER] = "Auto".into();
        fields[TENANT] = "common".into();
        fields[SECURITY] = "TLS required".into();
        fields[AUTH_METHOD] = "Normal password".into();
        fields[SMTP_SECURITY] = "SSL/TLS".into();
        fields[IMAP_SECURITY] = "SSL/TLS".into();
        fields[SMTP_AUTH] = "Automatic".into();
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
            oauth_request_id: 0,
            oauth_cancel: None,
            provisional_oauth_account: None,
            source: None,
            first_run,
            mixed_auth: false,
            testing: false,
            test_request: 0,
            last_test_result: None,
        }
    }

    pub fn provider_hint(&self) -> &'static str {
        match self.fields[PROVIDER].as_str() {
            "Microsoft 365" => "microsoft",
            "Google" => "google",
            _ => "auto",
        }
    }

    /// A password mailbox never enters the browser OAuth flow.
    pub fn uses_oauth(&self) -> bool {
        self.fields[AUTH_METHOD] == "OAuth2"
    }

    /// Display the actual identity provider, never assume a custom OAuth server
    /// belongs to Microsoft simply because it is not Google.
    pub fn oauth_sign_in_label(&self) -> String {
        match self.provider_hint() {
            "google" => "Google".into(),
            "microsoft" => "Microsoft 365".into(),
            _ => self.fields[OAUTH_ISSUER]
                .trim()
                .parse::<url::Url>()
                .ok()
                .and_then(|issuer| issuer.host_str().map(str::to_owned))
                .or_else(|| {
                    let host = self.fields[IMAP_HOST].trim();
                    (!host.is_empty()).then(|| host.to_owned())
                })
                .unwrap_or_else(|| "your mail provider".into()),
        }
    }

    /// The wizard always exposes the *effective* IMAP and SMTP usernames
    /// independently. The saved-account display can still group common settings.
    pub fn visible_fields(&self) -> Vec<usize> {
        self.display_rows()
            .into_iter()
            .filter_map(|row| match row {
                SetupDisplayRow::Field(index) => Some(index),
                _ => None,
            })
            .collect()
    }

    pub fn display_rows(&self) -> Vec<SetupDisplayRow> {
        use SetupDisplayRow::{BoxBottom, BoxTop, Field};
        match self.step {
            SetupStep::Identity => return vec![Field(NAME), Field(EMAIL)],
            SetupStep::Authenticating => return Vec::new(),
            SetupStep::Settings => {}
        }
        let mut rows = vec![
            BoxTop("All Servers"),
            Field(PROVIDER),
            Field(SECURITY),
            Field(AUTH_METHOD),
        ];
        if !self.uses_oauth() {
            rows.push(Field(PASSWORD));
        }
        rows.extend([
            BoxBottom,
            BoxTop("Incoming · IMAP"),
            Field(IMAP_HOST),
            Field(IMAP_PORT),
            Field(IMAP_SECURITY),
            Field(IMAP_LOGIN),
            BoxBottom,
            BoxTop("Outgoing · SMTP"),
            Field(SMTP_HOST),
            Field(SMTP_PORT),
            Field(SMTP_SECURITY),
            Field(SMTP_AUTH),
            Field(SMTP_LOGIN),
        ]);
        rows.push(BoxBottom);
        if self.advanced && self.uses_oauth() {
            rows.push(BoxTop("Advanced OAuth settings"));
            rows.push(Field(CLIENT_ID));
            if self.provider_hint() == "auto" {
                rows.push(Field(OAUTH_ISSUER));
                rows.push(Field(OAUTH_SCOPE));
            }
            match self.provider_hint() {
                "microsoft" => rows.push(Field(TENANT)),
                "google" => rows.push(Field(GOOGLE_SECRET)),
                _ => {
                    rows.push(Field(TENANT));
                    rows.push(Field(GOOGLE_SECRET));
                }
            }
            rows.push(BoxBottom);
        }
        rows
    }

    /// Keep mouse hit-testing aligned with the review heading. Discovery
    /// provenance is not rendered as an extra row above the settings.
    pub fn header_rows(&self) -> u16 {
        if self.step == SetupStep::Identity {
            1
        } else {
            2
        }
    }

    pub fn popup_size(&self, screen_width: u16, screen_height: u16) -> (u16, u16) {
        let max_height = match self.step {
            SetupStep::Identity | SetupStep::Authenticating => 13,
            SetupStep::Settings if self.advanced => 32,
            SetupStep::Settings => 28,
        };
        (
            screen_width.saturating_sub(2).clamp(1, 96),
            screen_height.saturating_sub(2).clamp(1, max_height),
        )
    }

    /// The active stage on the fixed four-part setup progress header.
    /// Microsoft can sign in before showing the editable settings, so the
    /// indicator represents the actual screen rather than a monotonic counter.
    pub fn progress_stage(&self) -> usize {
        if self.saved {
            3
        } else {
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
        ensure!(
            valid_mailbox_name(self.fields[EMAIL].trim()),
            "enter a complete email address"
        );
        let name = self.fields[NAME].trim();
        ensure!(
            !name.is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
            "enter an account name (1–80 characters)"
        );
        Ok(())
    }

    pub fn key(&mut self, key: KeyEvent) -> WizardAction {
        // The confirmation page is read-only; Enter is handled by App to
        // reload the newly configured account.
        if self.saved {
            return WizardAction::Nothing;
        }
        // Do not allow an in-flight browser sign-in to mutate the identity or
        // settings from which its OAuth state was created.
        if self.authenticating {
            if key.code == KeyCode::Esc {
                self.cancel_authentication();
            }
            return WizardAction::Nothing;
        }
        // A connection test runs off-thread. Esc cancels the UI operation;
        // late results from the old request are ignored by request ID.
        if self.testing {
            if key.code == KeyCode::Esc {
                self.testing = false;
                self.test_request = 0;
                self.message =
                    "Test cancelled; background connections will close on timeout.".into();
            }
            return WizardAction::Nothing;
        }
        if self.discovering {
            if key.code == KeyCode::Esc {
                self.discovering = false;
                self.message =
                    "Discovery cancelled. Edit the address or press Enter to retry.".into();
            }
            return WizardAction::Nothing;
        }
        if self.step == SetupStep::Authenticating {
            return WizardAction::Nothing;
        }
        if self.step == SetupStep::Settings && key.code == KeyCode::F(5) {
            return WizardAction::TestConfiguration;
        }
        if self.step == SetupStep::Settings && key.code == KeyCode::F(3) {
            self.advanced = !self.advanced;
            if !self.visible_fields().contains(&self.selected) {
                self.selected = PROVIDER;
            }
            return WizardAction::Nothing;
        }
        if self.step == SetupStep::Settings && self.login_failed {
            if key.code == KeyCode::F(7) {
                return WizardAction::SaveOffline;
            }
            if key.code == KeyCode::F(6) {
                return WizardAction::Save;
            }
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
                KeyCode::Backspace => {
                    self.edit_field(None);
                    WizardAction::Nothing
                }
                KeyCode::Char(ch) if !ch.is_control() => {
                    self.edit_field(Some(ch));
                    WizardAction::Nothing
                }
                _ => WizardAction::Nothing,
            };
        }

        let identity_before = [
            EMAIL,
            NAME,
            PROVIDER,
            CLIENT_ID,
            TENANT,
            GOOGLE_SECRET,
            AUTH_METHOD,
            USERNAME,
            OAUTH_ISSUER,
            OAUTH_SCOPE,
        ]
        .map(|index| self.fields[index].clone());
        let previous_fields = self.fields.clone();
        let action = match key.code {
            KeyCode::Esc | KeyCode::F(4) => {
                self.discard_provisional_oauth();
                self.authenticated = false;
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
                let at = rows
                    .iter()
                    .position(|field| *field == self.selected)
                    .unwrap_or(0);
                self.selected = rows[at.saturating_sub(1)];
                WizardAction::Nothing
            }
            KeyCode::Down | KeyCode::Tab => {
                let rows = self.visible_fields();
                let at = rows
                    .iter()
                    .position(|field| *field == self.selected)
                    .unwrap_or(0);
                self.selected = rows[(at + 1).min(rows.len() - 1)];
                WizardAction::Nothing
            }
            KeyCode::Left | KeyCode::Right if self.selected == PROVIDER => {
                let choices = ["Auto", "Microsoft 365", "Google", "Other IMAP/SMTP"];
                let at = choices
                    .iter()
                    .position(|choice| *choice == self.fields[PROVIDER])
                    .unwrap_or(0);
                let next = if key.code == KeyCode::Left {
                    (at + choices.len() - 1) % choices.len()
                } else {
                    (at + 1) % choices.len()
                };
                self.fields[PROVIDER] = choices[next].into();
                self.edited[PROVIDER] = true;
                self.fields[AUTH_METHOD] =
                    if choices[next] == "Microsoft 365" || choices[next] == "Google" {
                        "OAuth2"
                    } else {
                        "Normal password"
                    }
                    .into();
                if self.uses_oauth()
                    && !matches!(self.fields[SMTP_AUTH].as_str(), "Automatic" | "None")
                {
                    self.fields[SMTP_AUTH] = "Automatic".into();
                }
                self.authenticated = false;
                if !self.visible_fields().contains(&self.selected) {
                    self.selected = AUTH_METHOD;
                }
                WizardAction::Nothing
            }
            KeyCode::Left | KeyCode::Right if self.selected == AUTH_METHOD => {
                self.fields[AUTH_METHOD] = if self.uses_oauth() {
                    "Normal password"
                } else {
                    "OAuth2"
                }
                .into();
                self.authenticated = false;
                self.login_failed = false;
                self.mixed_auth = false;
                if self.uses_oauth()
                    && !matches!(self.fields[SMTP_AUTH].as_str(), "Automatic" | "None")
                {
                    self.fields[SMTP_AUTH] = "Automatic".into();
                }
                self.edited[AUTH_METHOD] = true;
                if !self.visible_fields().contains(&self.selected) {
                    self.selected = AUTH_METHOD;
                }
                WizardAction::Nothing
            }
            KeyCode::Left | KeyCode::Right if self.selected == IMAP_SECURITY => {
                let previous = self.fields[IMAP_SECURITY].clone();
                self.fields[IMAP_SECURITY] = if previous == "SSL/TLS" {
                    "STARTTLS"
                } else {
                    "SSL/TLS"
                }
                .into();
                let old_default = if previous == "SSL/TLS" { "993" } else { "143" };
                if !self.edited[IMAP_PORT] || self.fields[IMAP_PORT] == old_default {
                    self.fields[IMAP_PORT] = if self.fields[IMAP_SECURITY] == "SSL/TLS" {
                        "993"
                    } else {
                        "143"
                    }
                    .into();
                }
                self.edited[IMAP_SECURITY] = true;
                WizardAction::Nothing
            }
            KeyCode::Left | KeyCode::Right if self.selected == SMTP_AUTH => {
                const CHOICES: [&str; 4] = ["Automatic", "PLAIN", "LOGIN", "None"];
                const OAUTH_CHOICES: [&str; 2] = ["Automatic", "None"];
                let choices: &[&str] = if self.uses_oauth() {
                    &OAUTH_CHOICES
                } else {
                    &CHOICES
                };
                let at = choices
                    .iter()
                    .position(|choice| *choice == self.fields[SMTP_AUTH])
                    .unwrap_or(0);
                let next = if key.code == KeyCode::Left {
                    (at + choices.len() - 1) % choices.len()
                } else {
                    (at + 1) % choices.len()
                };
                self.fields[SMTP_AUTH] = choices[next].into();
                if self.fields[SMTP_AUTH] == "None" {
                    self.mixed_auth = false;
                }
                self.edited[SMTP_AUTH] = true;
                WizardAction::Nothing
            }
            KeyCode::Left | KeyCode::Right if self.selected == SMTP_SECURITY => {
                let previous = self.fields[SMTP_SECURITY].clone();
                self.fields[SMTP_SECURITY] = if previous == "SSL/TLS" {
                    "STARTTLS"
                } else {
                    "SSL/TLS"
                }
                .into();
                let old_default = if previous == "SSL/TLS" { "465" } else { "587" };
                if !self.edited[SMTP_PORT] || self.fields[SMTP_PORT] == old_default {
                    self.fields[SMTP_PORT] = if self.fields[SMTP_SECURITY] == "SSL/TLS" {
                        "465"
                    } else {
                        "587"
                    }
                    .into();
                }
                self.edited[SMTP_SECURITY] = true;
                WizardAction::Nothing
            }
            KeyCode::Backspace
                if !matches!(
                    self.selected,
                    PROVIDER | AUTH_METHOD | SMTP_SECURITY | IMAP_SECURITY | SMTP_AUTH | SECURITY
                ) =>
            {
                self.edit_field(None);
                WizardAction::Nothing
            }
            KeyCode::Char(ch)
                if !matches!(
                    self.selected,
                    PROVIDER | AUTH_METHOD | SMTP_SECURITY | IMAP_SECURITY | SMTP_AUTH | SECURITY
                ) && !ch.is_control() =>
            {
                self.edit_field(Some(ch));
                WizardAction::Nothing
            }
            _ => WizardAction::Nothing,
        };
        if (self.authenticated || self.provisional_oauth_account.is_some())
            && [
                EMAIL,
                NAME,
                PROVIDER,
                CLIENT_ID,
                TENANT,
                GOOGLE_SECRET,
                AUTH_METHOD,
                USERNAME,
                OAUTH_ISSUER,
                OAUTH_SCOPE,
            ]
            .iter()
            .enumerate()
            .any(|(i, index)| self.fields[*index] != identity_before[i])
        {
            self.discard_provisional_oauth();
            self.authenticated = false;
            self.login_failed = false;
            self.error = false;
            self.message = "Account identity changed. Sign in again before saving.".into();
        }
        if self.fields != previous_fields {
            self.last_test_result = None;
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
        self.last_test_result = None;
        if self.selected == EMAIL || self.selected == NAME || self.selected == USERNAME {
            self.authenticated = false;
            self.login_failed = false;
            if self.selected == EMAIL {
                self.source = None;
            }
        }
        self.error = false;
    }

    pub fn apply_discovery(&mut self, proposal: MailConfiguration) {
        self.last_test_result = None;
        let microsoft = proposal
            .imap
            .host
            .eq_ignore_ascii_case("outlook.office365.com");
        let google = proposal.imap.host.eq_ignore_ascii_case("imap.gmail.com");
        if !self.edited[PROVIDER] {
            self.fields[PROVIDER] = if microsoft {
                "Microsoft 365"
            } else if google {
                "Google"
            } else {
                "Auto"
            }
            .into();
        }
        let discovered_oauth_issuer = proposal.oauth_issuer.clone();
        self.mixed_auth = !proposal.smtp.auth.eq_ignore_ascii_case("none")
            && (proposal.imap.auth.eq_ignore_ascii_case("OAuth2")
                != proposal.smtp.auth.eq_ignore_ascii_case("OAuth2"));
        let imap_implicit_tls = proposal.imap_implicit_tls();
        let smtp_implicit_tls = proposal.smtp_implicit_tls();
        if !self.edited[USERNAME] {
            self.fields[USERNAME] = proposal.imap.username.clone();
        }
        if !self.edited[OAUTH_ISSUER]
            && let Some(issuer) = discovered_oauth_issuer
        {
            self.fields[OAUTH_ISSUER] = issuer;
        }
        if !self.edited[AUTH_METHOD] {
            if (microsoft || google)
                && !proposal.imap.auth.eq_ignore_ascii_case("OAuth2")
                && !proposal.smtp.auth.eq_ignore_ascii_case("OAuth2")
            {
                self.edited[AUTH_METHOD] = true;
            }
            self.fields[AUTH_METHOD] = if proposal.imap.auth.eq_ignore_ascii_case("OAuth2")
                || proposal.smtp.auth.eq_ignore_ascii_case("OAuth2")
            {
                "OAuth2"
            } else {
                "Normal password"
            }
            .into();
        }
        if !self.edited[IMAP_SECURITY] {
            self.fields[IMAP_SECURITY] = if imap_implicit_tls {
                "SSL/TLS"
            } else {
                "STARTTLS"
            }
            .into();
        }
        if !self.edited[SMTP_AUTH] {
            self.fields[SMTP_AUTH] = if proposal.smtp.auth.eq_ignore_ascii_case("none") {
                "None"
            } else {
                "Automatic"
            }
            .into();
        }
        if !self.edited[SMTP_SECURITY] {
            self.fields[SMTP_SECURITY] = if smtp_implicit_tls {
                "SSL/TLS"
            } else {
                "STARTTLS"
            }
            .into();
        }
        for (index, value) in [
            (IMAP_HOST, proposal.imap.host),
            (IMAP_PORT, proposal.imap.port.to_string()),
            (
                IMAP_LOGIN,
                if proposal.imap.username.is_empty() {
                    self.fields[EMAIL].trim().to_owned()
                } else {
                    proposal.imap.username
                },
            ),
            (SMTP_HOST, proposal.smtp.host),
            (SMTP_PORT, proposal.smtp.port.to_string()),
            (
                SMTP_LOGIN,
                if proposal.smtp.username.is_empty() {
                    self.fields[EMAIL].trim().to_owned()
                } else {
                    proposal.smtp.username
                },
            ),
        ] {
            if !self.edited[index] {
                self.fields[index] = value;
            }
        }
        if !self.edited[PROVIDER] && !microsoft && !google {
            self.fields[PROVIDER] = "Other IMAP/SMTP".into();
        }
        self.source = Some(format!("{} ({})", proposal.provider, proposal.source));
        let unknown_microsoft_login = microsoft
            && !self.mixed_auth
            && self.uses_oauth()
            && !self.edited[IMAP_LOGIN]
            && !self.edited[SMTP_LOGIN]
            && !self.authenticated;
        self.step = if unknown_microsoft_login {
            SetupStep::Authenticating
        } else {
            SetupStep::Settings
        };
        self.selected = PROVIDER;
        self.message = if self.mixed_auth {
            self.error = true;
            "IMAP and SMTP advertise different authentication methods. Review both before continuing.".into()
        } else if unknown_microsoft_login {
            "Microsoft 365 found. Sign in to identify your actual mailbox login…".into()
        } else {
            String::new()
        };
        self.discovering = false;
        self.error = self.mixed_auth;
    }

    /// Start a cancellable OAuth attempt. A unique request ID prevents a late
    /// callback from a cancelled attempt from changing another wizard session.
    pub fn begin_authentication(&mut self) -> (u64, Arc<AtomicBool>) {
        self.last_test_result = None;
        self.step = SetupStep::Authenticating;
        self.authenticating = true;
        self.login_failed = false;
        self.error = false;
        self.spinner_frame = 0;
        self.oauth_request_id = NEXT_OAUTH_REQUEST.fetch_add(1, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.oauth_cancel = Some(Arc::clone(&cancel));
        self.message = "Waiting for browser sign-in… (Esc cancels)".into();
        (self.oauth_request_id, cancel)
    }

    /// Closing a browser tab is not observable through the loopback callback.
    /// Esc cancels the listener wait; any late completion is discarded.
    pub fn cancel_authentication(&mut self) {
        if let Some(cancel) = self.oauth_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.discard_provisional_oauth();
        self.oauth_request_id = 0;
        self.authenticating = false;
        self.step = SetupStep::Settings;
        self.selected = PROVIDER;
        self.error = false;
        self.message = "Sign-in cancelled. Review settings and retry when ready.".into();
    }

    fn discard_provisional_oauth(&mut self) {
        if !self.saved
            && let Some(account) = self.provisional_oauth_account.take()
        {
            crate::oauth::discard_provisional_login(&account);
        }
    }

    pub fn accepts_oauth_result(&self, request_id: u64) -> bool {
        self.authenticating && request_id != 0 && self.oauth_request_id == request_id
    }

    fn finish_oauth_request(&mut self) {
        self.oauth_request_id = 0;
        self.oauth_cancel = None;
    }

    pub fn authentication_failed(&mut self, error: &str) {
        self.finish_oauth_request();
        self.authenticating = false;
        self.step = SetupStep::Settings;
        self.selected = PROVIDER;
        self.login_failed = true;
        self.error = true;
        self.message = format!("OAuth failed: {error}. Ctrl-S/F6 retry; F7 saves offline.");
    }

    /// Keep Microsoft's post-login UPN review, but finish normal OAuth setup
    /// with the same Confirmation page used by password accounts.
    pub fn complete_authentication(&mut self, login: Option<String>, path: &Path) -> Result<()> {
        self.suggest_login(login);
        if self.provider_hint() != "microsoft" {
            self.save(path)?;
            self.saved = true;
            self.message.clear();
        }
        Ok(())
    }

    /// OAuth may reveal a different UPN after the user signs in. Never replace
    /// a username that was edited by hand: show the suggestion for review.
    pub fn suggest_login(&mut self, login: Option<String>) {
        self.finish_oauth_request();
        self.last_test_result = None;
        self.step = SetupStep::Settings;
        self.selected = IMAP_LOGIN;
        self.authenticated = true;
        self.authenticating = false;
        self.login_failed = false;
        if let Some(login) = login {
            for index in [IMAP_LOGIN, SMTP_LOGIN] {
                if !self.edited[index] {
                    self.fields[index] = login.clone();
                }
            }
            self.message =
                "Signed in. Review both suggested mailbox usernames, then Enter saves.".into();
        } else if self.provider_hint() == "microsoft" {
            self.message = "Signed in, but Microsoft did not return a username hint. Verify both logins before saving.".into();
        } else {
            self.message =
                "Signed in successfully. Review the detected settings, then Enter saves.".into();
        }
        self.error = false;
    }

    pub fn discovery_failed(&mut self, message: &str) {
        self.last_test_result = None;
        self.discovering = false;
        self.error = true;
        // Discovery failures should remain recoverable without a hidden
        // F2 shortcut: display editable manual settings in the review step.
        self.step = SetupStep::Settings;
        self.selected = PROVIDER;
        self.source = None;
        if !self.edited[PROVIDER] {
            self.fields[PROVIDER] = "Other IMAP/SMTP".into();
        }
        if !self.edited[AUTH_METHOD] {
            self.fields[AUTH_METHOD] = "Normal password".into();
        }
        if !self.edited[USERNAME] {
            self.fields[USERNAME] = self.fields[EMAIL].trim().to_owned();
        }
        for index in [IMAP_LOGIN, SMTP_LOGIN] {
            if !self.edited[index] {
                self.fields[index] = self.fields[EMAIL].trim().to_owned();
            }
        }
        if self.fields[IMAP_PORT].is_empty() {
            self.fields[IMAP_PORT] = "993".into();
        }
        if self.fields[SMTP_PORT].is_empty() {
            self.fields[SMTP_PORT] = "465".into();
        }
        self.message = format!(
            "Automatic discovery failed: {message}. Enter the server settings manually, or F4 to change the email address."
        );
    }

    fn validate(&self) -> Result<String> {
        let values = &self.fields;
        let email = values[EMAIL].trim();
        ensure!(valid_mailbox_name(email), "enter a complete email address");
        let name = values[NAME].trim();
        ensure!(
            !name.is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
            "account name must contain 1–80 printable characters"
        );
        for (idx, label) in [(IMAP_HOST, "IMAP"), (SMTP_HOST, "SMTP")] {
            ensure!(
                valid_hostname(values[idx].trim()),
                "{label} hostname must be a valid hostname (no URI or credentials)"
            );
        }
        let imap_port: u16 = values[IMAP_PORT]
            .trim()
            .parse()
            .context("enter a valid IMAP port")?;
        let smtp_port: u16 = values[SMTP_PORT]
            .trim()
            .parse()
            .context("enter a valid SMTP port")?;
        ensure!(
            imap_port != 0,
            "IMAP port must be nonzero (TLS is required)"
        );
        ensure!(
            smtp_port != 0,
            "SMTP port must be nonzero (TLS is required)"
        );
        ensure!(
            values[SECURITY] == "TLS required",
            "unencrypted mail connections are not supported"
        );
        ensure!(
            matches!(values[IMAP_SECURITY].as_str(), "SSL/TLS" | "STARTTLS"),
            "choose IMAP SSL/TLS or STARTTLS"
        );
        ensure!(
            matches!(values[SMTP_SECURITY].as_str(), "SSL/TLS" | "STARTTLS"),
            "choose SMTP SSL/TLS or STARTTLS"
        );
        ensure!(
            matches!(values[AUTH_METHOD].as_str(), "Normal password" | "OAuth2"),
            "choose password or OAuth2 authentication"
        );
        ensure!(
            matches!(
                values[SMTP_AUTH].as_str(),
                "Automatic" | "PLAIN" | "LOGIN" | "None"
            ),
            "choose a supported SMTP authentication mechanism"
        );
        ensure!(
            !self.uses_oauth() || matches!(values[SMTP_AUTH].as_str(), "Automatic" | "None"),
            "OAuth accounts cannot use SMTP PLAIN/LOGIN; select Automatic or None"
        );
        ensure!(
            !self.mixed_auth,
            "IMAP and SMTP advertise different authentication; choose a consistent supported account configuration"
        );
        let common_login = if values[USERNAME].trim().is_empty() {
            email
        } else {
            values[USERNAME].trim()
        };
        ensure!(
            !common_login.chars().any(char::is_control)
                && !common_login.contains(char::is_whitespace),
            "shared username must be a single-line login"
        );
        for (idx, label) in [(IMAP_LOGIN, "IMAP"), (SMTP_LOGIN, "SMTP")] {
            let value = values[idx].trim();
            // Clearing a visible username must not silently restore the hidden
            // legacy shared login when saving the account.
            ensure!(
                !self.edited[idx] || !value.is_empty(),
                "enter {label} username"
            );
            ensure!(
                value.is_empty()
                    || (!value.chars().any(char::is_control)
                        && !value.contains(char::is_whitespace)),
                "{label} username must be a single-line login"
            );
        }
        let provider = if self.uses_oauth() {
            match self.provider_hint() {
                "microsoft" => "microsoft",
                "google" => "google",
                _ if values[IMAP_HOST].eq_ignore_ascii_case("outlook.office365.com") => "microsoft",
                _ if values[IMAP_HOST].eq_ignore_ascii_case("imap.gmail.com") => "google",
                _ => "open",
            }
        } else {
            "password"
        };
        let client_id = values[CLIENT_ID].trim();
        ensure!(
            !client_id
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control()),
            "OAuth application ID must be one line"
        );
        // The Google OAuth client ID includes its DNS suffix. It is not an
        // optional decoration: removing it produces a different client ID.
        // Also validate the publisher's build-time default, if one exists.
        let effective_client_id = if !client_id.is_empty() {
            Some(client_id)
        } else {
            crate::oauth::embedded_client_id(provider)
        };
        if provider == "google"
            && let Some(id) = effective_client_id
        {
            ensure!(
                crate::oauth::valid_google_desktop_client_id(id),
                "Google OAuth client ID is incomplete: keep the full .apps.googleusercontent.com suffix"
            );
        }
        let google_secret = values[GOOGLE_SECRET].trim();
        ensure!(
            !google_secret.chars().any(char::is_control),
            "Google desktop client secret must be a single line"
        );
        let tenant = values[TENANT].trim();
        ensure!(
            !tenant.chars().any(|c| c.is_control() || c.is_whitespace()),
            "invalid tenant"
        );

        let mut text = format!(
            "[[accounts]]\nname = {}\nemail = {}\nsource = {}\noutgoing = {}\n",
            quoted(name),
            quoted(email),
            quoted(if values[IMAP_SECURITY] == "SSL/TLS" {
                "imap"
            } else {
                "imap+starttls"
            }),
            quoted(
                match (values[SMTP_SECURITY].as_str(), values[SMTP_AUTH].as_str()) {
                    ("SSL/TLS", "Automatic") => "smtps",
                    ("STARTTLS", "Automatic") => "starttls",
                    ("SSL/TLS", "PLAIN") => "smtps+plain",
                    ("STARTTLS", "PLAIN") => "smtp+plain",
                    ("SSL/TLS", "LOGIN") => "smtps+login",
                    ("STARTTLS", "LOGIN") => "smtp+login",
                    ("SSL/TLS", "None") => "smtps+none",
                    ("STARTTLS", "None") => "smtp+none",
                    _ => unreachable!("transport and authentication choices validated above"),
                }
            )
        );
        for (key, index) in [("imap_host", IMAP_HOST), ("smtp_host", SMTP_HOST)] {
            text.push_str(&format!("{key} = {}\n", quoted(values[index].trim())));
        }
        text.push_str(&format!(
            "imap_port = {imap_port}\nsmtp_port = {smtp_port}\n"
        ));
        for (key, index) in [("imap_username", IMAP_LOGIN), ("smtp_username", SMTP_LOGIN)] {
            let login = if values[index].trim().is_empty() {
                common_login
            } else {
                values[index].trim()
            };
            if login != email {
                text.push_str(&format!("{key} = {}\n", quoted(login)));
            }
        }
        if !self.uses_oauth() {
            ensure!(
                !values[PASSWORD].is_empty(),
                "enter an account password (kept out of config.toml)"
            );
            ensure!(
                !values[PASSWORD]
                    .chars()
                    .any(|c| matches!(c, '\n' | '\r' | '\0')),
                "password contains a forbidden control character"
            );
            let parsed: Config =
                toml::from_str(&text).context("validating password account TOML")?;
            ensure!(
                parsed.accounts.len() == 1 && parsed.accounts[0].oauth.is_none(),
                "password account cannot have OAuth settings"
            );
            return Ok(text);
        }
        text.push_str(&format!("[accounts.oauth]\nprovider = {}\nflow = \"browser\"\ncallback_host = {}\ncallback_path = {}\n",
            quoted(provider),
            quoted(if provider == "open" { "127.0.0.1" } else { "localhost" }),
            quoted(if provider == "open" { "/oauth/callback" } else { "/" }),
        ));
        if provider == "open" {
            let issuer = if values[OAUTH_ISSUER].trim().is_empty() {
                format!("https://{}", values[IMAP_HOST].trim())
            } else {
                values[OAUTH_ISSUER].trim().to_owned()
            };
            crate::oauth::validate_open_issuer(&issuer)?;
            text.push_str(&format!("issuer = {}\n", quoted(&issuer)));
            if !values[OAUTH_SCOPE].trim().is_empty() {
                text.push_str(&format!("scope = {}\n", quoted(values[OAUTH_SCOPE].trim())));
            }
        }
        if !client_id.is_empty() {
            text.push_str(&format!("client_id = {}\n", quoted(client_id)));
        }
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
        let account = parsed
            .accounts
            .first()
            .ok_or_else(|| anyhow!("no generated account"))?;
        crate::oauth::OAuthSettings::for_account(account)?
            .ok_or_else(|| anyhow!("an OAuth provider is required"))?;
        Ok(text)
    }

    /// Mark a new non-sending test. IDs are unique across wizard instances so
    /// events from a closed/reopened wizard can never be mistaken as current.
    pub fn begin_connection_test(&mut self) -> u64 {
        let id = NEXT_TEST_REQUEST.fetch_add(1, Ordering::Relaxed);
        self.test_request = id;
        self.testing = true;
        self.spinner_frame = 0;
        self.error = false;
        self.last_test_result = None;
        self.message = "Testing IMAP and SMTP connections; no message will be sent…".into();
        id
    }

    /// A stale network result must not overwrite a newer wizard or edited fields.
    pub fn finish_connection_test(&mut self, id: u64, report: probe::ProbeReport) {
        if !self.testing || self.test_request != id || self.saved {
            return;
        }
        self.testing = false;
        self.error = !report.passed();
        self.message = report.summary();
        self.last_test_result = Some(report);
    }

    pub fn preview_account(&self) -> Result<AccountConfig> {
        let fragment = self.validate()?;
        let parsed: Config = toml::from_str(&fragment)?;
        parsed
            .accounts
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("missing new account"))
    }

    /// Preserve all existing settings/comments, then atomically replace the file.
    /// Password credentials go into a separate private file (never config.toml).
    pub fn save(&self, path: &Path) -> Result<AccountConfig> {
        let fragment = self.validate()?;
        if let Some(original) = &self.provisional_oauth_account {
            let edited = self.preview_account()?;
            ensure!(
                crate::oauth::OAuthSettings::same_wizard_credential_identity(original, &edited)?,
                "OAuth account identity changed since sign-in; sign in again before saving"
            );
        }
        // Preserve an existing dotfiles symlink rather than replacing it.
        let destination = if path.exists() {
            path.canonicalize()?
        } else {
            path.to_owned()
        };
        let destination = if destination.is_absolute() {
            destination
        } else {
            std::env::current_dir()?.join(destination)
        };
        let path = destination.as_path();
        let old = match fs::read_to_string(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let mut existing: Config = if old.trim().is_empty() {
            toml::from_str("[ui]\n")?
        } else {
            toml::from_str(&old).context("parsing existing config before updating")?
        };
        ensure!(
            !existing
                .accounts
                .iter()
                .any(|account| account.name == self.fields[NAME].trim()),
            "an account with this name already exists; choose a unique account name"
        );
        // A password mailbox gets its own private credential file, never a
        // password in config.toml, a URI, a shell command, or a process argument.
        let secret_file = if !self.uses_oauth() {
            // Never write plaintext secrets alongside potentially Git-managed
            // config files. Use OS per-user data storage instead.
            let folder = dirs::data_local_dir()
                .ok_or_else(|| anyhow!("cannot locate private user data directory"))?
                .join("jerc")
                .join("credentials");
            if !folder.exists() {
                fs::create_dir_all(&folder)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Reject a preexisting group/world-accessible credential store.
                let meta = fs::symlink_metadata(&folder)?;
                ensure!(
                    meta.file_type().is_dir() && meta.permissions().mode() & 0o077 == 0,
                    "credential directory must be private (chmod 700)"
                );
            }
            let mut file = tempfile::Builder::new()
                .prefix("jerc-")
                .suffix(".secret")
                .tempfile_in(&folder)?;
            file.write_all(self.fields[PASSWORD].as_bytes())?;
            file.as_file().sync_all()?;
            let (_, credential_path) = file.keep()?;
            Some(credential_path)
        } else {
            None
        };
        struct CleanupSecret(Option<std::path::PathBuf>);
        impl Drop for CleanupSecret {
            fn drop(&mut self) {
                if let Some(path) = &self.0 {
                    let _ = fs::remove_file(path);
                }
            }
        }
        let mut pending_secret = CleanupSecret(secret_file);
        let mut fragment = fragment;
        if let Some(secret) = &pending_secret.0 {
            let command = format!("@file:{}", secret.display());
            fragment.push_str(&format!("source_cred_cmd = {}\n", quoted(&command)));
            if self.fields[SMTP_AUTH] != "None" {
                fragment.push_str(&format!("outgoing_cred_cmd = {}\n", quoted(&command)));
            }
        }
        let mut merged = old;
        if !merged.is_empty() && !merged.ends_with('\n') {
            merged.push('\n');
        }
        merged.push('\n');
        merged.push_str(&fragment);
        existing = toml::from_str(&merged).context("validating merged config")?;
        let new_account = existing
            .accounts
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("missing saved account"))?;
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Never loosen the permissions of a private existing config.
            let mode = fs::metadata(path)
                .ok()
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o600);
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(mode & 0o600))?;
        }
        tmp.write_all(merged.as_bytes())?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist(path)
            .map_err(|err| anyhow!("persisting account config: {}", err.error))?;
        pending_secret.0.take(); // Saved config now references this private file.
        Ok(new_account)
    }
}

/// Remove an account from the *local* config, never an IMAP folder or any
/// server-side mail. Preserve unrelated sections, comments and private mode.
/// Refuse ambiguous config layouts rather than risk deleting another account.
pub fn remove_account(path: &Path, name: &str) -> Result<AccountConfig> {
    let destination = if path.exists() {
        path.canonicalize()?
    } else {
        path.to_owned()
    };
    let raw = fs::read_to_string(&destination)
        .with_context(|| format!("reading {}", destination.display()))?;
    let config: Config = toml::from_str(&raw).context("parsing account config before removal")?;
    ensure!(
        config
            .accounts
            .iter()
            .filter(|account| account.name == name)
            .count()
            == 1,
        "expected exactly one locally configured account with that name"
    );
    let target = config
        .accounts
        .iter()
        .position(|account| account.name == name)
        .ok_or_else(|| anyhow!("account not found"))?;
    let mut removed = config.accounts[target].clone();
    // Config::load_or_empty() uses normalized IMAP URIs for disk cache keys.
    // Preserve that same identity for deferred cleanup. Invalid accounts
    // remain removable even if their shorthand cannot be resolved.
    let _ = removed.normalize_shorthand();

    let mut sections: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        let heading = line.trim();
        if heading.starts_with('[') && heading.ends_with(']') {
            sections.push((offset, heading));
        }
        offset += line.len();
    }
    let accounts: Vec<usize> = sections
        .iter()
        .filter_map(|(at, heading)| (*heading == "[[accounts]]").then_some(*at))
        .collect();
    ensure!(
        accounts.len() == config.accounts.len(),
        "account table layout is ambiguous; edit config.toml manually"
    );
    let start = accounts[target];
    let end = sections
        .iter()
        .filter(|(at, _)| *at > start)
        .find(|(_, heading)| {
            *heading == "[[accounts]]"
                || (!heading.starts_with("[accounts.") && !heading.starts_with("[[accounts."))
        })
        .map(|(at, _)| *at)
        .unwrap_or(raw.len());
    let mut result = raw.clone();
    result.replace_range(start..end, "");
    // Do not discard formatting or comments outside the removed account.
    // Validate the resulting TOML *before* touching disk.
    let parsed: Config =
        toml::from_str(&result).context("checking config after account removal")?;
    ensure!(
        parsed.accounts.len() + 1 == config.accounts.len()
            && parsed.accounts.iter().all(|account| account.name != name),
        "account removal would alter unexpected account entries"
    );
    let parent = destination.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&destination)?.permissions().mode() & 0o600;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    tmp.write_all(result.as_bytes())?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(&destination)
        .map_err(|error| anyhow!("saving account removal: {}", error.error))?;
    Ok(removed)
}

/// Private per-user roots; never search the filesystem or delete by account label.
struct LocalDataRoots {
    config: std::path::PathBuf,
    cache: std::path::PathBuf,
    data: std::path::PathBuf,
}

/// Remove only files whose names are derived from the deleted account's
/// identity. Called after the retired App and its cache writer have stopped.
/// Remote IMAP/Maildir data, shared contacts and external secrets are untouched.
pub(crate) fn purge_local_account_data(
    account: &AccountConfig,
    remaining: &[AccountConfig],
) -> Result<()> {
    let roots = LocalDataRoots {
        config: dirs::config_dir().context("finding OAuth configuration directory")?,
        cache: dirs::cache_dir().context("finding mail cache directory")?,
        data: dirs::data_local_dir().context("finding local data directory")?,
    };
    purge_local_account_data_in(account, remaining, &roots)
}

fn purge_local_account_data_in(
    account: &AccountConfig,
    remaining: &[AccountConfig],
    roots: &LocalDataRoots,
) -> Result<()> {
    // The only possible path sharing is a hand-edited credential command.
    // Other configured accounts must never lose a password they reference.
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut errors = Vec::new();

    if let Some(oauth) = &account.oauth {
        match crate::oauth::token_path_for_account(
            &roots.config,
            &account.name,
            oauth.credential_namespace.as_deref(),
        ) {
            Ok(path) => {
                // Open-provider dynamic client registrations live alongside
                // the refresh token and also belong to this account.
                files.push((
                    path.with_extension("public-client.json"),
                    roots.config.clone(),
                ));
                files.push((path, roots.config.clone()));
            }
            Err(error) => errors.push(format!("OAuth token: {error:#}")),
        }
    }
    if crate::backend::ImapSource::supports(&account.source) {
        let stem = crate::cache::account_cache_stem(account);
        let base = roots.cache.join("jerc").join("mail-index");
        for suffix in [".json", ".mutations.json"] {
            files.push((base.join(format!("{stem}{suffix}")), roots.cache.clone()));
        }
        directories.push((base.join(format!("{stem}.bodies")), roots.cache.clone()));
        directories.push((base.join(format!("{stem}.raw")), roots.cache.clone()));
    }
    files.push((
        crate::outgoing::outbox::path_for_account(account, &roots.data),
        roots.data.clone(),
    ));
    files.push((
        crate::outgoing::drafts::path_for(
            &roots.data.join("jerc").join("postponed"),
            &account.name,
        ),
        roots.data.clone(),
    ));
    // Only secrets created by Jerc's own account wizard may be removed.
    // Never unlink arbitrary @file: commands or shell-managed credentials.
    for command in [
        account.source_cred_cmd.as_deref(),
        account.outgoing_cred_cmd.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let Some(path) = wizard_secret_path(command, &roots.data) else {
            continue;
        };
        let used_elsewhere = remaining.iter().any(|other| {
            [
                other.source_cred_cmd.as_deref(),
                other.outgoing_cred_cmd.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|value| value == command)
        });
        if !used_elsewhere && !files.iter().any(|(candidate, _)| candidate == &path) {
            files.push((path, roots.data.clone()));
        }
    }

    // Try every artifact even if a previous removal failed. In particular,
    // never leave a saved OAuth refresh token merely because a body failed.
    for (file, root) in files {
        if let Err(error) = remove_private_artifact(&file, &root, false) {
            errors.push(format!("{}: {error:#}", file.display()));
        }
    }
    for (directory, root) in directories {
        if let Err(error) = remove_private_artifact(&directory, &root, true) {
            errors.push(format!("{}: {error:#}", directory.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("{}", errors.join("; ")))
    }
}

fn wizard_secret_path(command: &str, data_root: &Path) -> Option<std::path::PathBuf> {
    let value = command.strip_prefix("@file:")?;
    let path = Path::new(value);
    let credentials_dir = data_root.join("jerc").join("credentials");
    if !path.is_absolute() || path.parent()? != credentials_dir.as_path() {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    (name.starts_with("jerc-") && name.ends_with(".secret")).then(|| path.to_owned())
}

/// Refuse symbolic links in managed directories and at the artifact path.
/// A body cache must be a real directory; everything else a regular file.
fn remove_private_artifact(path: &Path, root: &Path, directory: bool) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .context("path outside managed root")?;
    let mut current = root.to_owned();
    for component in relative
        .components()
        .take(relative.components().count().saturating_sub(1))
    {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_dir() => {
                return Err(anyhow!("unsafe managed directory"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if directory && meta.file_type().is_dir() => {
            fs::remove_dir_all(path).context("removing cache body directory")
        }
        Ok(meta) if !directory && meta.file_type().is_file() => {
            fs::remove_file(path).context("removing local account file")
        }
        Ok(_) => Err(anyhow!(
            "refusing non-regular or symlinked account artifact"
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn quoted(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}
fn valid_mailbox_name(text: &str) -> bool {
    let Some((local, domain)) = text.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !local.contains('@')
        && valid_hostname(domain)
        && !text.chars().any(|ch| ch.is_control() || ch.is_whitespace())
}
fn valid_hostname(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && !host.contains('/')
        && host.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        })
}

impl Drop for SetupWizard {
    fn drop(&mut self) {
        if let Some(cancel) = &self.oauth_cancel {
            cancel.store(true, Ordering::Release);
        }
        self.discard_provisional_oauth();
    }
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
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            WizardAction::Nothing
        );
    }

    #[test]
    fn generated_config_keeps_alias_separate_from_login() {
        let mut wizard = SetupWizard::new(true);
        wizard.fields[EMAIL] = "alias@example.edu".into();
        wizard.fields[NAME] = "University".into();
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        wizard.fields[AUTH_METHOD] = "OAuth2".into();
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
        assert_eq!(
            config.accounts[0].smtp_username.as_deref(),
            Some("netid@example.edu")
        );
    }

    #[test]
    fn duplicate_account_never_modifies_config() {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[EMAIL] = "alias@example.edu".into();
        wizard.fields[NAME] = "University".into();
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        wizard.fields[AUTH_METHOD] = "OAuth2".into();
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
        wizard.fields[AUTH_METHOD] = "OAuth2".into();
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
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            WizardAction::Nothing
        );
        assert_eq!(wizard.selected, EMAIL);
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WizardAction::Discover
        );
        let config =
            crate::discovery::discover_with_provider("alias@example.edu", "microsoft").unwrap();
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
        wizard.fields[AUTH_METHOD] = "OAuth2".into();
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
        assert_eq!(
            account.oauth.unwrap().client_secret.as_deref(),
            Some("desktop-public-secret")
        );
    }

    #[test]
    fn failed_login_does_not_silently_save() {
        let mut wizard = SetupWizard::new(false);
        wizard.login_failed = true;
        wizard.step = SetupStep::Settings;
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            WizardAction::Save
        );
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)),
            WizardAction::SaveOffline
        );
    }

    #[test]
    fn identity_first_then_discovered_settings_then_login() {
        let mut wizard = SetupWizard::new(true);
        assert_eq!(wizard.step, SetupStep::Identity);
        assert_eq!(wizard.visible_fields(), vec![NAME, EMAIL]);
        wizard.fields[NAME] = "Campus".into();
        wizard.fields[EMAIL] = "alias@example.edu".into();
        assert!(wizard.discovery_ready().is_ok());
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WizardAction::Nothing
        );
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WizardAction::Discover
        );
        let discovered =
            crate::discovery::discover_with_provider("alias@example.edu", "microsoft").unwrap();
        wizard.apply_discovery(discovered);
        assert_eq!(wizard.step, SetupStep::Authenticating);
        assert_eq!(wizard.fields[IMAP_HOST], "outlook.office365.com");
        wizard.suggest_login(Some("netid@example.edu".into()));
        assert_eq!(wizard.step, SetupStep::Settings);
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WizardAction::Save
        );
        wizard.authenticating = true;
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            WizardAction::Nothing
        );
        wizard.suggest_login(Some("netid@example.edu".into()));
        assert_eq!(wizard.fields[SMTP_LOGIN], "netid@example.edu");
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WizardAction::Save
        );
    }

    #[test]
    fn google_sign_in_uses_its_own_stage_before_returning_to_review() {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[PROVIDER] = "Google".into();
        wizard.fields[AUTH_METHOD] = "OAuth2".into();
        wizard.step = SetupStep::Settings;
        assert_eq!(wizard.progress_stage(), 1);
        let _ = wizard.begin_authentication();
        assert_eq!(wizard.progress_stage(), 2);
        assert_eq!(wizard.step, SetupStep::Authenticating);
        assert!(wizard.authenticating);
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WizardAction::Nothing
        );
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
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)),
            WizardAction::Nothing
        );
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
        wizard.fields[AUTH_METHOD] = "OAuth2".into();
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
        for _ in 0..7 {
            wizard.spin();
        }
        assert_eq!(wizard.spinner(), initial);
    }

    #[test]
    fn missing_or_empty_config_starts_with_zero_accounts() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("missing.toml");
        let config = Config::load_or_empty(Some(&path)).unwrap();
        assert!(config.accounts.is_empty());
        fs::write(
            &path,
            "# Only theme settings so far\n[ui]\ntheme = \"nord\"\n",
        )
        .unwrap();
        let config = Config::load_or_empty(Some(&path)).unwrap();
        assert!(config.accounts.is_empty());
        assert_eq!(config.ui.theme, "nord");
    }

    #[test]
    fn microsoft_discovery_authenticates_before_review_and_preserves_alias() {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[NAME] = "Campus".into();
        wizard.fields[EMAIL] = "alias@example.edu".into();
        wizard.apply_discovery(
            crate::discovery::discover_with_provider("alias@example.edu", "microsoft").unwrap(),
        );
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
        assert!(
            wizard
                .display_rows()
                .iter()
                .any(|row| matches!(row, SetupDisplayRow::BoxTop("Incoming · IMAP")))
        );
    }
    fn fivejazz_password_wizard() -> SetupWizard {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[NAME] = "FiveJazz".into();
        wizard.fields[EMAIL] = "hi@fivejazz.com".into();
        wizard.fields[PROVIDER] = "Other IMAP/SMTP".into();
        wizard.fields[AUTH_METHOD] = "Normal password".into();
        wizard.fields[USERNAME] = "hi@fivejazz.com".into();
        wizard.fields[PASSWORD] = "independent mailbox secret!".into();
        wizard.fields[IMAP_HOST] = "mail.fivejazz.com".into();
        wizard.fields[IMAP_PORT] = "993".into();
        wizard.fields[SMTP_HOST] = "mail.fivejazz.com".into();
        wizard.fields[SMTP_PORT] = "465".into();
        wizard.fields[SMTP_SECURITY] = "SSL/TLS".into();
        wizard.step = SetupStep::Settings;
        wizard
    }

    #[test]
    fn fivejazz_password_account_does_not_require_or_emit_oauth() {
        let wizard = fivejazz_password_wizard();
        assert!(!wizard.uses_oauth());
        let account = wizard.preview_account().unwrap();
        assert!(account.oauth.is_none());
        assert_eq!(account.imap_host.as_deref(), Some("mail.fivejazz.com"));
        assert_eq!(account.outgoing.as_deref(), Some("smtps"));
    }

    #[test]
    fn password_account_saves_private_secret_never_inline_credentials() {
        let wizard = fivejazz_password_wizard();
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("config.toml");
        let account = wizard.save(&path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("independent mailbox secret!"));
        assert!(!raw.contains("[accounts.oauth]"));
        assert_eq!(account.source_cred_cmd, account.outgoing_cred_cmd);
        let command = account.source_cred_cmd.unwrap();
        assert!(command.starts_with("@file:"));
        let secret_path = std::path::Path::new(command.strip_prefix("@file:").unwrap());
        assert!(secret_path.is_absolute());
        assert!(
            !secret_path.starts_with(folder.path()),
            "secrets must not live in a Git-managed config path"
        );
        assert_eq!(
            crate::credentials::private_file_credential(&command)
                .unwrap()
                .as_deref(),
            Some("independent mailbox secret!")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(secret_path).unwrap().permissions().mode() & 0o077,
                0
            );
            assert_eq!(
                fs::metadata(secret_path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
        let _ = fs::remove_file(secret_path);
    }

    #[test]
    fn custom_smtp_starttls_port_is_respected() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[SMTP_SECURITY] = "STARTTLS".into();
        wizard.fields[SMTP_PORT] = "2525".into();
        let account = wizard.preview_account().unwrap();
        assert_eq!(account.outgoing.as_deref(), Some("starttls"));
        assert_eq!(account.smtp_port, Some(2525));
    }

    #[test]
    fn ssl_tls_on_nonstandard_secure_ports_supported() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[IMAP_PORT] = "1993".into();
        wizard.fields[SMTP_PORT] = "8465".into();
        let account = wizard.preview_account().unwrap();
        assert_eq!(account.imap_port, Some(1993));
        assert_eq!(account.smtp_port, Some(8465));
    }

    #[test]
    fn shared_username_populates_both_identities() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[USERNAME] = "unique-user".into();
        let account = wizard.preview_account().unwrap();
        assert_eq!(account.imap_username.as_deref(), Some("unique-user"));
        assert_eq!(account.smtp_username.as_deref(), Some("unique-user"));
    }

    #[test]
    fn different_imap_and_smtp_usernames_are_preserved() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[IMAP_LOGIN] = "inbound-user".into();
        wizard.fields[SMTP_LOGIN] = "outbound-user".into();
        assert!(wizard.visible_fields().contains(&IMAP_LOGIN));
        assert!(wizard.visible_fields().contains(&SMTP_LOGIN));
        let account = wizard.preview_account().unwrap();
        assert_eq!(account.imap_username.as_deref(), Some("inbound-user"));
        assert_eq!(account.smtp_username.as_deref(), Some("outbound-user"));
    }

    #[test]
    fn wizard_always_shows_both_usernames_but_never_the_legacy_shared_input() {
        for advanced in [false, true] {
            let mut wizard = fivejazz_password_wizard();
            wizard.advanced = advanced;
            let rows = wizard.visible_fields();
            assert!(rows.contains(&IMAP_LOGIN));
            assert!(rows.contains(&SMTP_LOGIN));
            assert!(!rows.contains(&USERNAME));
            assert!(
                rows.iter().position(|i| *i == IMAP_SECURITY).unwrap()
                    < rows.iter().position(|i| *i == IMAP_LOGIN).unwrap()
            );
            assert!(
                rows.iter().position(|i| *i == SMTP_AUTH).unwrap()
                    < rows.iter().position(|i| *i == SMTP_LOGIN).unwrap()
            );
        }
    }

    #[test]
    fn separate_usernames_can_be_edited_without_touching_the_other_server() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[IMAP_LOGIN] = "incoming@example.edu".into();
        wizard.fields[SMTP_LOGIN] = "outgoing@example.edu".into();
        wizard.selected = IMAP_LOGIN;
        wizard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let account = wizard.preview_account().unwrap();
        assert_eq!(
            account.imap_username.as_deref(),
            Some("incoming@example.edux")
        );
        assert_eq!(
            account.smtp_username.as_deref(),
            Some("outgoing@example.edu")
        );
    }

    #[test]
    fn clearing_a_visible_username_is_a_validation_error_not_a_hidden_fallback() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[IMAP_LOGIN].clear();
        wizard.edited[IMAP_LOGIN] = true;
        assert!(
            wizard
                .preview_account()
                .unwrap_err()
                .to_string()
                .contains("IMAP username")
        );
        wizard.fields[IMAP_LOGIN] = "hi@fivejazz.com".into();
        wizard.fields[SMTP_LOGIN].clear();
        wizard.edited[SMTP_LOGIN] = true;
        assert!(
            wizard
                .preview_account()
                .unwrap_err()
                .to_string()
                .contains("SMTP username")
        );
    }

    #[test]
    fn discovery_failure_prefills_both_editable_login_fields() {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[EMAIL] = "hi@fivejazz.com".into();
        wizard.discovery_failed("not published");
        assert_eq!(wizard.fields[IMAP_LOGIN], "hi@fivejazz.com");
        assert_eq!(wizard.fields[SMTP_LOGIN], "hi@fivejazz.com");
        assert!(wizard.visible_fields().contains(&IMAP_LOGIN));
        assert!(wizard.visible_fields().contains(&SMTP_LOGIN));
        assert_eq!(wizard.header_rows(), 2);
        wizard.step = SetupStep::Identity;
        assert_eq!(wizard.header_rows(), 1);
    }

    #[test]
    fn changing_smtp_security_updates_only_its_default_port() {
        let mut wizard = fivejazz_password_wizard();
        wizard.selected = SMTP_SECURITY;
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(wizard.fields[SMTP_SECURITY], "STARTTLS");
        assert_eq!(wizard.fields[SMTP_PORT], "587");
        wizard.fields[SMTP_PORT] = "2525".into();
        wizard.edited[SMTP_PORT] = true;
        wizard.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(wizard.fields[SMTP_SECURITY], "SSL/TLS");
        assert_eq!(wizard.fields[SMTP_PORT], "2525");
    }

    #[test]
    fn security_field_cannot_disable_tls() {
        let mut wizard = fivejazz_password_wizard();
        wizard.selected = SECURITY;
        wizard.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        wizard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(wizard.fields[SECURITY], "TLS required");
        wizard.fields[SECURITY] = "None".into();
        assert!(wizard.preview_account().is_err());
    }

    #[test]
    fn missing_password_cannot_be_saved_as_a_working_account() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[PASSWORD].clear();
        assert!(
            wizard
                .preview_account()
                .unwrap_err()
                .to_string()
                .contains("password")
        );
    }

    #[test]
    fn authentication_choice_is_editable_without_activating_browser() {
        let mut wizard = fivejazz_password_wizard();
        wizard.selected = AUTH_METHOD;
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(wizard.uses_oauth());
        assert!(!wizard.visible_fields().contains(&PASSWORD));
        wizard.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(!wizard.uses_oauth());
        assert!(wizard.visible_fields().contains(&PASSWORD));
    }

    #[test]
    fn rejects_invalid_password_account_host_or_port() {
        for host in [
            "http://mail.fivejazz.com",
            "mail/evil.com",
            "mail..fivejazz.com",
        ] {
            let mut wizard = fivejazz_password_wizard();
            wizard.fields[IMAP_HOST] = host.into();
            assert!(wizard.preview_account().is_err(), "{host}");
        }
        for port in ["0", "65536", "nope"] {
            let mut wizard = fivejazz_password_wizard();
            wizard.fields[SMTP_PORT] = port.into();
            assert!(wizard.preview_account().is_err(), "{port}");
        }
    }

    #[test]
    fn password_mode_rejects_line_breaks_in_secret() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[PASSWORD] = "first\nsecond".into();
        assert!(wizard.preview_account().is_err());
    }

    #[test]
    fn duplicate_password_account_does_not_rewrite_existing_config() {
        let wizard = fivejazz_password_wizard();
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        let account = wizard.save(&file).unwrap();
        let original = fs::read(&file).unwrap();
        assert!(wizard.save(&file).is_err());
        assert_eq!(fs::read(&file).unwrap(), original);
        if let Some(path) = account.source_cred_cmd.unwrap().strip_prefix("@file:") {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn manual_discovery_failure_has_a_usable_password_path() {
        let mut wizard = SetupWizard::new(false);
        wizard.fields[NAME] = "Private".into();
        wizard.fields[EMAIL] = "hi@fivejazz.com".into();
        wizard.discovery_failed("no autoconfig");
        assert_eq!(wizard.fields[PROVIDER], "Other IMAP/SMTP");
        assert_eq!(wizard.fields[AUTH_METHOD], "Normal password");
        assert_eq!(wizard.fields[USERNAME], "hi@fivejazz.com");
        assert!(!wizard.uses_oauth());
    }

    fn custom_xml(imap_auth: &str, smtp_auth: &str) -> String {
        format!(
            r#"<clientConfig version="1.1"><emailProvider id="fivejazz.com">
          <domain>fivejazz.com</domain>
          <incomingServer type="imap"><hostname>mail.fivejazz.com</hostname>
            <port>993</port><socketType>SSL</socketType><authentication>{imap_auth}</authentication>
            <username>%EMAILADDRESS%</username></incomingServer>
          <outgoingServer type="smtp"><hostname>mail.fivejazz.com</hostname>
            <port>587</port><socketType>STARTTLS</socketType><authentication>{smtp_auth}</authentication>
            <username>%EMAILADDRESS%</username></outgoingServer>
        </emailProvider></clientConfig>"#
        )
    }

    #[test]
    fn thunderbird_password_xml_populates_common_fields_and_tls() {
        let config = crate::discovery::parse_autoconfig(
            "hi@fivejazz.com",
            &custom_xml("password-cleartext", "password-cleartext"),
            "fixture",
        )
        .unwrap();
        let mut wizard = SetupWizard::new(false);
        wizard.apply_discovery(config);
        assert_eq!(wizard.fields[USERNAME], "hi@fivejazz.com");
        assert_eq!(wizard.fields[AUTH_METHOD], "Normal password");
        assert_eq!(wizard.fields[SMTP_SECURITY], "STARTTLS");
        assert!(!wizard.uses_oauth());
        assert!(wizard.visible_fields().contains(&IMAP_LOGIN));
        assert!(wizard.visible_fields().contains(&SMTP_LOGIN));
        assert!(!wizard.visible_fields().contains(&USERNAME));
        assert_eq!(wizard.fields[IMAP_LOGIN], "hi@fivejazz.com");
        assert_eq!(wizard.fields[SMTP_LOGIN], "hi@fivejazz.com");
        assert!(
            wizard
                .display_rows()
                .iter()
                .any(|r| matches!(r, SetupDisplayRow::BoxTop("All Servers")))
        );
    }

    #[test]
    fn mixed_oauth_and_password_autoconfig_requires_manual_resolution() {
        let config = crate::discovery::parse_autoconfig(
            "hi@fivejazz.com",
            &custom_xml("OAuth2", "password-cleartext"),
            "fixture",
        )
        .unwrap();
        let mut wizard = SetupWizard::new(false);
        wizard.apply_discovery(config);
        assert!(wizard.mixed_auth);
        assert!(wizard.validate().is_err());
    }

    #[test]
    fn imap_starttls_choice_changes_source_not_smtp() {
        let mut wizard = fivejazz_password_wizard();
        wizard.selected = IMAP_SECURITY;
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(wizard.fields[IMAP_SECURITY], "STARTTLS");
        assert_eq!(wizard.fields[IMAP_PORT], "143");
        let account = wizard.preview_account().unwrap();
        assert_eq!(account.source, "imap+starttls");
        assert_eq!(account.outgoing.as_deref(), Some("smtps"));
    }

    #[test]
    fn imap_tls_choice_keeps_explicit_nonstandard_port() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[IMAP_PORT] = "2143".into();
        wizard.edited[IMAP_PORT] = true;
        wizard.selected = IMAP_SECURITY;
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(wizard.fields[IMAP_PORT], "2143");
        assert_eq!(wizard.preview_account().unwrap().imap_port, Some(2143));
    }

    #[test]
    fn incoming_security_is_a_noneditable_selection() {
        let mut wizard = fivejazz_password_wizard();
        wizard.selected = IMAP_SECURITY;
        wizard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        wizard.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(wizard.fields[IMAP_SECURITY], "SSL/TLS");
        wizard.fields[IMAP_SECURITY] = "None".into();
        assert!(wizard.preview_account().is_err());
    }

    #[test]
    fn tls_required_while_smtp_authentication_is_none() {
        for (security, expected) in [("SSL/TLS", "smtps+none"), ("STARTTLS", "smtp+none")] {
            let mut wizard = fivejazz_password_wizard();
            wizard.fields[SMTP_SECURITY] = security.into();
            wizard.fields[SMTP_AUTH] = "None".into();
            let account = wizard.preview_account().unwrap();
            assert_eq!(account.outgoing.as_deref(), Some(expected));
            assert_eq!(wizard.fields[SECURITY], "TLS required");
        }
    }

    #[test]
    fn outgoing_authentication_explicit_plain_or_login() {
        for (method, ssl, starttls) in [
            ("PLAIN", "smtps+plain", "smtp+plain"),
            ("LOGIN", "smtps+login", "smtp+login"),
        ] {
            let mut wizard = fivejazz_password_wizard();
            wizard.fields[SMTP_AUTH] = method.into();
            assert_eq!(
                wizard.preview_account().unwrap().outgoing.as_deref(),
                Some(ssl)
            );
            wizard.fields[SMTP_SECURITY] = "STARTTLS".into();
            assert_eq!(
                wizard.preview_account().unwrap().outgoing.as_deref(),
                Some(starttls)
            );
        }
    }

    #[test]
    fn outgoing_authentication_selector_cycles_without_editing_text() {
        let mut wizard = fivejazz_password_wizard();
        wizard.selected = SMTP_AUTH;
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(wizard.fields[SMTP_AUTH], "PLAIN");
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(wizard.fields[SMTP_AUTH], "LOGIN");
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(wizard.fields[SMTP_AUTH], "None");
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(wizard.fields[SMTP_AUTH], "Automatic");
        wizard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(wizard.fields[SMTP_AUTH], "Automatic");
    }

    #[test]
    fn smtp_noauth_writes_no_outgoing_secret_command() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[SMTP_AUTH] = "None".into();
        let tmp = tempfile::tempdir().unwrap();
        let account = wizard.save(&tmp.path().join("jerc.toml")).unwrap();
        assert!(account.source_cred_cmd.is_some());
        assert!(account.outgoing_cred_cmd.is_none());
        assert!(
            !fs::read_to_string(tmp.path().join("jerc.toml"))
                .unwrap()
                .contains("independent mailbox secret!")
        );
        if let Some(path) = account.source_cred_cmd.unwrap().strip_prefix("@file:") {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn oauth_imap_can_use_tlssmtp_without_authentication() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        wizard.fields[AUTH_METHOD] = "OAuth2".into();
        wizard.fields[IMAP_HOST] = "outlook.office365.com".into();
        wizard.fields[SMTP_AUTH] = "None".into();
        wizard.fields[CLIENT_ID] = "example-public-client".into();
        assert_eq!(
            wizard.preview_account().unwrap().outgoing.as_deref(),
            Some("smtps+none")
        );
    }

    #[test]
    fn oauth_imap_never_silently_switches_to_password_smtp() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        wizard.fields[AUTH_METHOD] = "OAuth2".into();
        wizard.fields[IMAP_HOST] = "outlook.office365.com".into();
        wizard.fields[CLIENT_ID] = "example-public-client".into();
        for method in ["PLAIN", "LOGIN"] {
            wizard.fields[SMTP_AUTH] = method.into();
            assert!(
                wizard
                    .preview_account()
                    .unwrap_err()
                    .to_string()
                    .contains("OAuth")
            );
        }
    }

    #[test]
    fn switching_to_oauth_clears_incompatible_smtp_sasl_choice() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[SMTP_AUTH] = "LOGIN".into();
        wizard.selected = AUTH_METHOD;
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(wizard.fields[SMTP_AUTH], "Automatic");
        assert!(wizard.uses_oauth());
    }

    #[test]
    fn discovered_pacc_oauth_populates_issuer_and_browser_auth() {
        let json = r#"{"protocols":{"imap":{"host":"mail.fivejazz.com"},"submit":{"host":"mail.fivejazz.com"}},"authentication":{"oauth-public":{"issuer":"https://mail.fivejazz.com"},"password":true}}"#;
        let config = crate::discovery::parse_pacc("hi@fivejazz.com", json).unwrap();
        let mut wizard = SetupWizard::new(false);
        wizard.fields[EMAIL] = "hi@fivejazz.com".into();
        wizard.fields[NAME] = "FiveJazz".into();
        wizard.apply_discovery(config);
        assert_eq!(wizard.fields[AUTH_METHOD], "OAuth2");
        assert_eq!(wizard.fields[OAUTH_ISSUER], "https://mail.fivejazz.com");
        let account = wizard.preview_account().unwrap();
        assert_eq!(account.oauth.as_ref().unwrap().provider, "open");
        assert_eq!(
            account.oauth.as_ref().unwrap().issuer.as_deref(),
            Some("https://mail.fivejazz.com")
        );
    }

    #[test]
    fn discovered_imap_starttls_and_smtp_none_are_preserved() {
        let xml = r#"<clientConfig version="1.1"><emailProvider id="fivejazz.com">
            <domain>fivejazz.com</domain>
            <incomingServer type="imap"><hostname>mail.fivejazz.com</hostname>
            <port>143</port><socketType>STARTTLS</socketType><authentication>password-cleartext</authentication>
            <username>%EMAILADDRESS%</username></incomingServer>
            <outgoingServer type="smtp"><hostname>relay.fivejazz.com</hostname>
            <port>587</port><socketType>STARTTLS</socketType><authentication>none</authentication></outgoingServer>
            </emailProvider></clientConfig>"#;
        let config = crate::discovery::parse_autoconfig("hi@fivejazz.com", xml, "fixture").unwrap();
        let mut wizard = SetupWizard::new(false);
        wizard.fields[EMAIL] = "hi@fivejazz.com".into();
        wizard.apply_discovery(config);
        assert_eq!(wizard.fields[IMAP_SECURITY], "STARTTLS");
        assert_eq!(wizard.fields[SMTP_SECURITY], "STARTTLS");
        assert_eq!(wizard.fields[SMTP_AUTH], "None");
        wizard.fields[NAME] = "FiveJazz".into();
        wizard.fields[PASSWORD] = "test-credential".into();
        let account = wizard.preview_account().unwrap();
        assert_eq!(account.source, "imap+starttls");
        assert_eq!(account.outgoing.as_deref(), Some("smtp+none"));
    }

    #[test]
    fn unsupported_smtp_authentication_is_rejected() {
        let mut wizard = fivejazz_password_wizard();
        wizard.fields[SMTP_AUTH] = "CRAM-MD5".into();
        assert!(wizard.preview_account().is_err());
    }
    #[test]
    fn f5_tests_review_settings_not_identity_discovery() {
        let mut wizard = fivejazz_password_wizard();
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            WizardAction::TestConfiguration
        );
        wizard.step = SetupStep::Identity;
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            WizardAction::Discover
        );
    }

    #[test]
    fn connection_test_freezes_edits_and_save_until_finished() {
        let mut wizard = fivejazz_password_wizard();
        let original = wizard.fields[SMTP_HOST].clone();
        let id = wizard.begin_connection_test();
        wizard.selected = SMTP_HOST;
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            WizardAction::Nothing
        );
        assert_eq!(wizard.fields[SMTP_HOST], original);
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            WizardAction::Nothing
        );
        wizard.finish_connection_test(
            id,
            probe::ProbeReport {
                imap: Ok(()),
                smtp: Ok(()),
            },
        );
        assert!(!wizard.testing);
        assert!(!wizard.error);
        assert!(wizard.last_test_result.as_ref().unwrap().passed());
    }

    #[test]
    fn stale_test_results_are_discarded_after_cancel_and_new_request() {
        let mut wizard = fivejazz_password_wizard();
        let first = wizard.begin_connection_test();
        wizard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!wizard.testing);
        let second = wizard.begin_connection_test();
        assert_ne!(first, second);
        wizard.finish_connection_test(
            first,
            probe::ProbeReport {
                imap: Ok(()),
                smtp: Ok(()),
            },
        );
        assert!(wizard.testing);
        assert!(wizard.last_test_result.is_none());
        wizard.finish_connection_test(
            second,
            probe::ProbeReport {
                imap: Ok(()),
                smtp: Err("test failure".into()),
            },
        );
        assert!(wizard.error);
        assert!(wizard.message.contains("SMTP ✗"));
    }

    #[test]
    fn closed_wizards_cannot_receive_results_from_old_instances() {
        let mut old = fivejazz_password_wizard();
        let old_request = old.begin_connection_test();
        let mut reopened = fivejazz_password_wizard();
        let new_request = reopened.begin_connection_test();
        assert_ne!(old_request, new_request);
        reopened.finish_connection_test(
            old_request,
            probe::ProbeReport {
                imap: Ok(()),
                smtp: Ok(()),
            },
        );
        assert!(reopened.testing);
        assert!(reopened.last_test_result.is_none());
    }

    #[test]
    fn completed_test_does_not_overwrite_saved_wizard() {
        let mut wizard = fivejazz_password_wizard();
        let id = wizard.begin_connection_test();
        wizard.saved = true;
        wizard.finish_connection_test(
            id,
            probe::ProbeReport {
                imap: Ok(()),
                smtp: Err("rejected".into()),
            },
        );
        assert!(!wizard.error);
        assert!(wizard.last_test_result.is_none());
    }

    #[test]
    fn editing_the_password_clears_a_previous_success() {
        let mut wizard = fivejazz_password_wizard();
        let id = wizard.begin_connection_test();
        wizard.finish_connection_test(
            id,
            probe::ProbeReport {
                imap: Ok(()),
                smtp: Ok(()),
            },
        );
        wizard.selected = PASSWORD;
        wizard.key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        assert!(wizard.last_test_result.is_none());
    }

    #[test]
    fn changing_any_server_setting_invalidates_earlier_pass() {
        let mut wizard = fivejazz_password_wizard();
        let id = wizard.begin_connection_test();
        wizard.finish_connection_test(
            id,
            probe::ProbeReport {
                imap: Ok(()),
                smtp: Ok(()),
            },
        );
        wizard.selected = SMTP_SECURITY;
        wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(wizard.last_test_result.is_none());
        assert_eq!(wizard.fields[SMTP_SECURITY], "STARTTLS");
    }

    #[test]
    fn test_does_not_write_password_to_config() {
        let wizard = fivejazz_password_wizard();
        let _account = wizard.preview_account().unwrap();
        // Preview/test has no persistence side effects. Save remains separate.
        assert!(!wizard.saved);
        assert!(wizard.last_test_result.is_none());
    }

    #[test]
    fn removing_only_local_account_preserves_other_accounts_and_theme() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        fs::write(
            &file,
            concat!(
                "# keep this header\n[ui]\ntheme = \"nord\"\n",
                "[[accounts]]\nname = \"First\"\nemail = \"a@example.com\"\n",
                "source = \"imap\"\n[accounts.oauth]\nprovider = \"microsoft\"\n",
                "[[accounts]]\nname = \"Second\"\nemail = \"b@example.com\"\n",
                "source = \"imap\"\n[accounts.oauth]\nprovider = \"google\"\n",
            ),
        )
        .unwrap();
        remove_account(&file, "First").unwrap();
        let saved = fs::read_to_string(&file).unwrap();
        assert!(saved.contains("# keep this header"));
        assert!(saved.contains("theme = \"nord\""));
        assert!(!saved.contains("name = \"First\""));
        assert!(saved.contains("name = \"Second\""));
        assert_eq!(
            Config::load_or_empty(Some(&file)).unwrap().accounts.len(),
            1
        );
        remove_account(&file, "Second").unwrap();
        assert!(
            Config::load_or_empty(Some(&file))
                .unwrap()
                .accounts
                .is_empty()
        );
        assert!(
            fs::read_to_string(&file)
                .unwrap()
                .contains("theme = \"nord\"")
        );
    }

    fn open_mail_wizard() -> SetupWizard {
        let mut wizard = SetupWizard::new(false);
        wizard.step = SetupStep::Settings;
        wizard.fields[EMAIL] = "hi@fivejazz.com".into();
        wizard.fields[NAME] = "FiveJazz OAuth test".into();
        wizard.fields[PROVIDER] = "Other IMAP/SMTP".into();
        wizard.fields[AUTH_METHOD] = "OAuth2".into();
        wizard.fields[IMAP_HOST] = "mail.fivejazz.com".into();
        wizard.fields[SMTP_HOST] = "mail.fivejazz.com".into();
        wizard.fields[IMAP_PORT] = "993".into();
        wizard.fields[SMTP_PORT] = "465".into();
        wizard
    }

    #[test]
    fn custom_oauth_label_never_says_microsoft_for_stalwart() {
        let mut wizard = open_mail_wizard();
        wizard.fields[OAUTH_ISSUER] = "https://mail.fivejazz.com".into();
        assert_eq!(wizard.oauth_sign_in_label(), "mail.fivejazz.com");
        wizard.fields[OAUTH_ISSUER].clear();
        assert_eq!(wizard.oauth_sign_in_label(), "mail.fivejazz.com");
        wizard.fields[IMAP_HOST].clear();
        assert_eq!(wizard.oauth_sign_in_label(), "your mail provider");
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        assert_eq!(wizard.oauth_sign_in_label(), "Microsoft 365");
        wizard.fields[PROVIDER] = "Google".into();
        assert_eq!(wizard.oauth_sign_in_label(), "Google");
    }

    #[test]
    fn escape_cancels_browser_oauth_and_disregards_stale_completion() {
        let mut wizard = open_mail_wizard();
        let (first, cancel) = wizard.begin_authentication();
        assert!(wizard.accepts_oauth_result(first));
        assert!(!cancel.load(Ordering::Relaxed));
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WizardAction::Nothing
        );
        assert!(wizard.authenticating);
        assert_eq!(
            wizard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            WizardAction::Nothing
        );
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!wizard.authenticating);
        assert!(!wizard.authenticated);
        assert_eq!(wizard.step, SetupStep::Settings);
        assert!(!wizard.accepts_oauth_result(first));
        let (second, second_cancel) = wizard.begin_authentication();
        assert_ne!(first, second);
        assert!(!second_cancel.load(Ordering::Relaxed));
        assert!(!wizard.accepts_oauth_result(first));
        assert!(wizard.accepts_oauth_result(second));
        wizard.authentication_failed("provider declined authorization");
        assert!(!wizard.authenticating);
        assert_eq!(wizard.oauth_request_id, 0);
        assert!(wizard.login_failed);
        assert_eq!(wizard.step, SetupStep::Settings);
    }

    #[test]
    fn successful_stalwart_browser_login_advances_to_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("jerc.toml");
        let mut wizard = open_mail_wizard();
        let (request, cancel) = wizard.begin_authentication();
        assert!(wizard.accepts_oauth_result(request));
        wizard.complete_authentication(None, &path).unwrap();
        assert!(wizard.saved);
        assert!(wizard.authenticated);
        assert!(!wizard.authenticating);
        assert_eq!(wizard.progress_stage(), 3);
        assert!(!wizard.accepts_oauth_result(request));
        assert!(!cancel.load(Ordering::Relaxed));
        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(saved.accounts.len(), 1);
        assert_eq!(saved.accounts[0].oauth.as_ref().unwrap().provider, "open");
    }

    #[test]
    fn microsoft_oauth_still_reviews_actual_mailbox_username_before_save() {
        let mut wizard = open_mail_wizard();
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        let _ = wizard.begin_authentication();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("jerc.toml");
        wizard
            .complete_authentication(Some("netid@example.edu".into()), &path)
            .unwrap();
        assert!(wizard.authenticated);
        assert!(!wizard.saved);
        assert_eq!(wizard.step, SetupStep::Settings);
        assert_eq!(wizard.fields[SMTP_LOGIN], "netid@example.edu");
        assert!(!path.exists());
    }

    #[test]
    fn oauth_save_failure_keeps_authenticated_review_editable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut wizard = open_mail_wizard();
        let _ = wizard.begin_authentication();
        // A directory is not a writable config file.
        assert!(wizard.complete_authentication(None, tmp.path()).is_err());
        assert!(wizard.authenticated);
        assert!(!wizard.saved);
        assert_eq!(wizard.step, SetupStep::Settings);
    }

    #[test]
    fn fivejazz_oauth_wizard_uses_public_client_without_password_or_client_id() {
        let wizard = open_mail_wizard();
        let account = wizard.preview_account().unwrap();
        let oauth = account.oauth.unwrap();
        assert_eq!(oauth.provider, "open");
        assert_eq!(oauth.issuer.as_deref(), Some("https://mail.fivejazz.com"));
        assert_eq!(oauth.callback_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(oauth.callback_path.as_deref(), Some("/oauth/callback"));
        assert!(oauth.client_id.is_none());
        assert_eq!(oauth.flow.as_deref(), Some("browser"));
        assert!(!wizard.validate().unwrap().contains("password ="));
    }

    #[test]
    fn self_hosted_oauth_can_use_separate_identity_provider() {
        let mut wizard = open_mail_wizard();
        wizard.fields[OAUTH_ISSUER] = "https://id.example.org/realms/mail".into();
        wizard.fields[OAUTH_SCOPE] = "email offline_access".into();
        wizard.fields[CLIENT_ID] = "registered-public-client".into();
        let account = wizard.preview_account().unwrap();
        let oauth = account.oauth.unwrap();
        assert_eq!(
            oauth.issuer.as_deref(),
            Some("https://id.example.org/realms/mail")
        );
        assert_eq!(oauth.scope.as_deref(), Some("email offline_access"));
        assert_eq!(oauth.client_id.as_deref(), Some("registered-public-client"));
    }

    #[test]
    fn wizard_rejects_insecure_custom_oauth_issuer() {
        let mut wizard = open_mail_wizard();
        wizard.fields[OAUTH_ISSUER] = "http://mail.fivejazz.com".into();
        assert!(wizard.preview_account().is_err());
        wizard.fields[OAUTH_ISSUER] = "https://user:pass@mail.fivejazz.com".into();
        assert!(wizard.preview_account().is_err());
        wizard.fields[OAUTH_ISSUER] = "https://mail.fivejazz.com/ok#frag".into();
        assert!(wizard.preview_account().is_err());
    }

    #[test]
    fn oauth_provider_switch_keeps_existing_google_and_microsoft_logic() {
        let mut wizard = open_mail_wizard();
        wizard.fields[PROVIDER] = "Google".into();
        wizard.fields[CLIENT_ID] = "abc.apps.googleusercontent.com".into();
        let google = wizard.preview_account().unwrap();
        assert_eq!(google.oauth.unwrap().provider, "google");
        wizard.fields[PROVIDER] = "Microsoft 365".into();
        wizard.fields[CLIENT_ID] = "owned-public-client".into();
        let microsoft = wizard.preview_account().unwrap();
        assert_eq!(microsoft.oauth.unwrap().provider, "microsoft");
    }

    #[test]
    fn open_oauth_never_accepts_unencrypted_imap_smtp() {
        let mut wizard = open_mail_wizard();
        wizard.fields[SECURITY] = "None".into();
        assert!(wizard.preview_account().is_err());
        wizard.fields[SECURITY] = "TLS required".into();
        wizard.fields[IMAP_SECURITY] = "None".into();
        assert!(wizard.preview_account().is_err());
        wizard.fields[IMAP_SECURITY] = "STARTTLS".into();
        assert!(wizard.preview_account().is_ok());
    }

    #[test]
    fn open_oauth_advanced_fields_are_present_when_selected() {
        let mut wizard = open_mail_wizard();
        wizard.advanced = true;
        let fields = wizard.visible_fields();
        assert!(fields.contains(&OAUTH_ISSUER));
        assert!(fields.contains(&OAUTH_SCOPE));
        wizard.fields[AUTH_METHOD] = "Normal password".into();
        assert!(!wizard.visible_fields().contains(&OAUTH_ISSUER));
    }
}

#[cfg(test)]
mod account_cleanup_tests {
    use super::*;

    fn fixture() -> (
        tempfile::TempDir,
        LocalDataRoots,
        AccountConfig,
        AccountConfig,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let roots = LocalDataRoots {
            config: directory.path().join("config"),
            cache: directory.path().join("cache"),
            data: directory.path().join("data"),
        };
        let parsed: Config = toml::from_str(concat!(
            "[[accounts]]\nname = 'Deleted'\nsource = 'imaps://deleted@imap.example.com'\n",
            "[accounts.oauth]\nprovider = 'google'\ncredential_namespace = 'work'\n",
            "[[accounts]]\nname = 'Retained'\nsource = 'imaps://retained@imap.example.com'\n",
            "[accounts.oauth]\nprovider = 'google'\ncredential_namespace = 'work'\n",
        ))
        .unwrap();
        (
            directory,
            roots,
            parsed.accounts[0].clone(),
            parsed.accounts[1].clone(),
        )
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"test local data").unwrap();
    }

    fn artifacts(account: &AccountConfig, roots: &LocalDataRoots) -> Vec<std::path::PathBuf> {
        let stem = crate::cache::account_cache_stem(account);
        vec![
            crate::oauth::token_path_for_account(
                &roots.config,
                &account.name,
                account
                    .oauth
                    .as_ref()
                    .unwrap()
                    .credential_namespace
                    .as_deref(),
            )
            .unwrap(),
            crate::oauth::token_path_for_account(
                &roots.config,
                &account.name,
                account
                    .oauth
                    .as_ref()
                    .unwrap()
                    .credential_namespace
                    .as_deref(),
            )
            .unwrap()
            .with_extension("public-client.json"),
            roots
                .cache
                .join("jerc/mail-index")
                .join(format!("{stem}.json")),
            roots
                .cache
                .join("jerc/mail-index")
                .join(format!("{stem}.mutations.json")),
            crate::outgoing::outbox::path_for_account(account, &roots.data),
            crate::outgoing::drafts::path_for(&roots.data.join("jerc/postponed"), &account.name),
        ]
    }

    #[test]
    fn remove_account_returns_exact_identity_for_deferred_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "[[accounts]]\nname='Removed'\nemail='removed@example.com'\nsource='imap'\nimap_host='imap.example.com'\n[[accounts]]\nname='Keep'\nsource='imaps://keep@imap.example.com'\n",
        )
        .unwrap();
        let removed = remove_account(&path, "Removed").unwrap();
        assert_eq!(removed.name, "Removed");
        assert!(removed.source.starts_with("imaps://"));
        assert_eq!(
            Config::load_or_empty(Some(&path)).unwrap().accounts[0].name,
            "Keep"
        );
    }

    #[test]
    fn purges_only_deleted_account_oauth_cache_outbox_drafts_and_bodies() {
        let (_directory, roots, deleted, retained) = fixture();
        let deleted_paths = artifacts(&deleted, &roots);
        let retained_paths = artifacts(&retained, &roots);
        for path in deleted_paths.iter().chain(retained_paths.iter()) {
            touch(path);
        }
        let deleted_bodies = roots.cache.join("jerc/mail-index").join(format!(
            "{}.bodies",
            crate::cache::account_cache_stem(&deleted)
        ));
        let retained_bodies = roots.cache.join("jerc/mail-index").join(format!(
            "{}.bodies",
            crate::cache::account_cache_stem(&retained)
        ));
        touch(&deleted_bodies.join("INBOX/message.body"));
        touch(&retained_bodies.join("INBOX/message.body"));
        let deleted_raw = deleted_bodies.with_extension("raw");
        let retained_raw = retained_bodies.with_extension("raw");
        touch(&deleted_raw.join("message.eml"));
        touch(&retained_raw.join("message.eml"));

        purge_local_account_data_in(&deleted, &[retained], &roots).unwrap();
        for path in deleted_paths {
            assert!(
                !path.exists(),
                "deleted account artifact left: {}",
                path.display()
            );
        }
        assert!(!deleted_bodies.exists());
        assert!(!deleted_raw.exists());
        for path in retained_paths {
            assert!(
                path.exists(),
                "unrelated account artifact removed: {}",
                path.display()
            );
        }
        assert!(retained_bodies.join("INBOX/message.body").exists());
        assert!(retained_raw.join("message.eml").exists());
        // Removing twice is harmless and does not affect other accounts.
        purge_local_account_data_in(&deleted, &[], &roots).unwrap();
    }

    #[test]
    fn removes_only_own_wizard_passwords_never_shared_or_external_files() {
        let (_directory, roots, mut deleted, mut retained) = fixture();
        let own = roots.data.join("jerc/credentials/jerc-owned.secret");
        let shared = roots.data.join("jerc/credentials/jerc-shared.secret");
        let external = roots.data.join("external-owned.secret");
        for path in [&own, &shared, &external] {
            touch(path);
        }
        deleted.source_cred_cmd = Some(format!("@file:{}", own.display()));
        deleted.outgoing_cred_cmd = Some(format!("@file:{}", shared.display()));
        retained.source_cred_cmd = Some(format!("@file:{}", shared.display()));
        purge_local_account_data_in(&deleted, &[retained], &roots).unwrap();
        assert!(!own.exists());
        assert!(shared.exists());
        assert!(external.exists());

        // A configured @file: outside the app-managed credential directory
        // is always user-managed, even if it has a Jerc-looking name.
        deleted.source_cred_cmd = Some(format!("@file:{}", external.display()));
        deleted.outgoing_cred_cmd = None;
        purge_local_account_data_in(&deleted, &[], &roots).unwrap();
        assert!(external.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_without_following_or_deleting_target() {
        use std::os::unix::fs::symlink;
        let (_directory, roots, deleted, _retained) = fixture();
        let target = roots.data.join("unrelated-mail.json");
        touch(&target);
        let outbox = crate::outgoing::outbox::path_for_account(&deleted, &roots.data);
        fs::create_dir_all(outbox.parent().unwrap()).unwrap();
        symlink(&target, &outbox).unwrap();
        assert!(purge_local_account_data_in(&deleted, &[], &roots).is_err());
        assert!(target.exists());
        assert!(
            fs::symlink_metadata(outbox)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn invalid_oauth_namespace_cannot_escape_managed_directory() {
        let (_directory, roots, mut deleted, _retained) = fixture();
        deleted.oauth.as_mut().unwrap().credential_namespace = Some("../../escape".into());
        assert!(purge_local_account_data_in(&deleted, &[], &roots).is_err());
        assert!(
            crate::oauth::token_path_for_account(
                &roots.config,
                &deleted.name,
                Some("../../escape"),
            )
            .is_err()
        );
    }
}
