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
    /// Optional JMAP host advertised during discovery (never credentials).
    jmap_hint: Option<String>,
    fallback_imap: Option<(String, u16)>,
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
            jmap_hint: None,
            fallback_imap: None,
        }
    }

    /// JMAP reuses the incoming-server form, but persists a jmaps:// source.
    pub fn is_jmap(&self) -> bool {
        self.fields[PROVIDER] == "JMAP (HTTPS)"
    }

    fn select_jmap_server(&mut self, was_jmap: bool) {
        if self.is_jmap() && !was_jmap {
            let domain = self.fields[EMAIL]
                .trim()
                .rsplit_once('@')
                .map(|(_, domain)| domain);
            self.fields[IMAP_HOST] = self
                .jmap_hint
                .clone()
                .or_else(|| domain.map(|domain| format!("mail.{domain}")))
                .unwrap_or_default();
            self.fields[IMAP_PORT] = "443".into();
            self.fields[IMAP_SECURITY] = "SSL/TLS".into();
            // Make a manually selected JMAP server testable without requiring
            // users to discover SMTP first, but never overwrite their choice.
            if self.fields[SMTP_HOST].is_empty() && !self.edited[SMTP_HOST] {
                self.fields[SMTP_HOST] = self.fields[IMAP_HOST].clone();
            }
            if self.fields[SMTP_PORT].is_empty() && !self.edited[SMTP_PORT] {
                self.fields[SMTP_PORT] = "465".into();
            }
            for index in [IMAP_LOGIN, SMTP_LOGIN] {
                if self.fields[index].is_empty() && !self.edited[index] {
                    self.fields[index] = self.fields[EMAIL].trim().to_owned();
                }
            }
        } else if !self.is_jmap() && was_jmap {
            if let Some((host, port)) = &self.fallback_imap {
                self.fields[IMAP_HOST] = host.clone();
                self.fields[IMAP_PORT] = port.to_string();
            } else {
                self.fields[IMAP_PORT] = "993".into();
            }
        }
    }

    pub fn provider_hint(&self) -> &'static str {
        match self.fields[PROVIDER].as_str() {
            "Microsoft 365" => "microsoft",
            "Google" => "google",
            "JMAP (HTTPS)" => "jmap",
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
            BoxTop(if self.is_jmap() {
                "Incoming · JMAP HTTPS"
            } else {
                "Incoming · IMAP"
            }),
            Field(IMAP_HOST),
            Field(IMAP_PORT),
        ]);
        if !self.is_jmap() {
            rows.push(Field(IMAP_SECURITY));
        }
        rows.extend([
            Field(IMAP_LOGIN),
            BoxBottom,
            BoxTop(if self.is_jmap() {
                "Outgoing · JMAP (blank SMTP server = native only)"
            } else {
                "Outgoing · SMTP"
            }),
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
            if matches!(self.provider_hint(), "auto" | "jmap") {
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
                let choices = [
                    "Auto",
                    "Microsoft 365",
                    "Google",
                    "Other IMAP/SMTP",
                    "JMAP (HTTPS)",
                ];
                let at = choices
                    .iter()
                    .position(|choice| *choice == self.fields[PROVIDER])
                    .unwrap_or(0);
                let next = if key.code == KeyCode::Left {
                    (at + choices.len() - 1) % choices.len()
                } else {
                    (at + 1) % choices.len()
                };
                let was_jmap = self.is_jmap();
                self.fields[PROVIDER] = choices[next].into();
                self.select_jmap_server(was_jmap);
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
        self.jmap_hint = proposal.jmap_host.clone();
        self.fallback_imap = Some((proposal.imap.host.clone(), proposal.imap.port));
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
            } else if proposal.jmap_host.is_some() {
                "JMAP (HTTPS)"
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
        if self.is_jmap() {
            self.fields[IMAP_HOST] = self.jmap_hint.clone().unwrap_or_else(|| {
                proposal
                    .email
                    .rsplit_once('@')
                    .map_or(String::new(), |(_, domain)| format!("mail.{domain}"))
            });
            self.fields[IMAP_PORT] = "443".into();
            self.fields[IMAP_SECURITY] = "SSL/TLS".into();
        }
        if !self.edited[PROVIDER] && !microsoft && !google && !self.is_jmap() {
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

    fn native_submission_only(&self) -> bool {
        self.is_jmap() && self.fields[SMTP_HOST].trim().is_empty()
    }

    fn validate(&self) -> Result<String> {
        let values = &self.fields;
        let native_only = self.native_submission_only();
        let email = values[EMAIL].trim();
        ensure!(valid_mailbox_name(email), "enter a complete email address");
        let name = values[NAME].trim();
        ensure!(
            !name.is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
            "account name must contain 1–80 printable characters"
        );
        for (idx, label) in [(IMAP_HOST, "IMAP"), (SMTP_HOST, "SMTP")] {
            if native_only && idx == SMTP_HOST {
                continue;
            }
            ensure!(
                valid_hostname(values[idx].trim()),
                "{label} hostname must be a valid hostname (no URI or credentials)"
            );
        }
        let imap_port: u16 = values[IMAP_PORT]
            .trim()
            .parse()
            .context("enter a valid IMAP port")?;
        let smtp_port: u16 = if native_only {
            465
        } else {
            values[SMTP_PORT]
                .trim()
                .parse()
                .context("enter a valid SMTP port")?
        };
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
            !self.is_jmap() || values[IMAP_SECURITY] == "SSL/TLS",
            "JMAP must use HTTPS, not IMAP STARTTLS"
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
            native_only
                || !self.uses_oauth()
                || matches!(values[SMTP_AUTH].as_str(), "Automatic" | "None"),
            "OAuth accounts cannot use SMTP PLAIN/LOGIN; select Automatic or None"
        );
        ensure!(
            native_only || !self.mixed_auth,
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
            if native_only && idx == SMTP_LOGIN {
                continue;
            }
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

        let incoming_source = if self.is_jmap() {
            // Build an encoded, credential-free JMAP URL. Passwords are kept
            // only in private credential files, never in the account source.
            let login = if values[IMAP_LOGIN].trim().is_empty() {
                common_login
            } else {
                values[IMAP_LOGIN].trim()
            };
            let mut source =
                url::Url::parse(&format!("jmaps://{}:{imap_port}", values[IMAP_HOST].trim()))?;
            source
                .set_username(login)
                .map_err(|()| anyhow!("invalid JMAP username"))?;
            source.to_string()
        } else if values[IMAP_SECURITY] == "SSL/TLS" {
            "imap".to_owned()
        } else {
            "imap+starttls".to_owned()
        };

        let mut text = format!(
            "[[accounts]]\nname = {}\nemail = {}\nsource = {}\noutgoing = {}\n",
            quoted(name),
            quoted(email),
            quoted(&incoming_source),
            quoted(if native_only {
                "jmap"
            } else {
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
            })
        );
        if !self.is_jmap() {
            text.push_str(&format!(
                "imap_host = {}\nimap_port = {imap_port}\n",
                quoted(values[IMAP_HOST].trim())
            ));
        }
        if !native_only {
            text.push_str(&format!(
                "smtp_host = {}\nsmtp_port = {smtp_port}\n",
                quoted(values[SMTP_HOST].trim())
            ));
        }
        for (key, index) in [("imap_username", IMAP_LOGIN), ("smtp_username", SMTP_LOGIN)] {
            if (self.is_jmap() && index == IMAP_LOGIN) || (native_only && index == SMTP_LOGIN) {
                continue;
            }
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
        self.message = if self.is_jmap() {
            "Testing JMAP Mail and outgoing submission; no message will be sent…".into()
        } else {
            "Testing IMAP and SMTP; no message will be sent…".into()
        };
        id
    }

    /// A stale network result must not overwrite a newer wizard or edited fields.
    pub fn finish_connection_test(&mut self, id: u64, report: probe::ProbeReport) {
        if !self.testing || self.test_request != id || self.saved {
            return;
        }
        self.testing = false;
        self.error = !report.passed();
        self.message = report.summary_for_incoming(if self.is_jmap() { "JMAP" } else { "IMAP" });
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
            if !self.native_submission_only() && self.fields[SMTP_AUTH] != "None" {
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
    if crate::cache::supports_mailbox_cache(&account.source) {
        let stem = crate::cache::account_cache_stem(account);
        let base = roots.cache.join("jerc").join("mail-index");
        for suffix in [
            ".json",
            ".mutations.json",
            ".navigation.json",
            ".index.lock",
        ] {
            files.push((base.join(format!("{stem}{suffix}")), roots.cache.clone()));
        }
        directories.push((base.join(format!("{stem}.bodies")), roots.cache.clone()));
        directories.push((base.join(format!("{stem}.raw")), roots.cache.clone()));
        directories.push((base.join(format!("{stem}.folders")), roots.cache.clone()));
    }
    if crate::backend::Pop3Source::supports(&account.source) {
        files.push((
            roots.data.join("jerc").join("pop3").join(format!(
                "{}.json",
                crate::cache::account_cache_stem(account)
            )),
            roots.data.clone(),
        ));
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
mod tests;

#[cfg(test)]
mod account_cleanup_tests;
