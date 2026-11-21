//! Setup wizard and account configuration interactions
use super::*;

impl App {
    pub fn attach_setup_sender(&mut self, sender: SyncSender<RuntimeEvent>) {
        self.setup_sender = Some(sender);
    }

    /// Keep the already-negotiated terminal graphics protocol across an
    /// in-place account reload. Re-querying after the input thread starts
    /// would consume terminal escape sequences as keypresses.
    pub fn inherit_image_runtime(&mut self, previous: &mut Self) {
        self.image_picker = previous.image_picker.take();
        self.image_loader = previous.image_loader.take();
    }

    pub fn setup_discovered(
        &mut self,
        email: String,
        result: Result<crate::discovery::MailConfiguration, String>,
    ) {
        let Some(wizard) = self.setup.as_mut() else {
            return;
        };
        if !wizard.discovering
            || wizard.authenticating
            || wizard.saved
            || !wizard.fields[crate::setup::EMAIL]
                .trim()
                .eq_ignore_ascii_case(&email)
        {
            return;
        }
        match result {
            Ok(config) => wizard.apply_discovery(config),
            Err(error) => wizard.discovery_failed(&error),
        }
        // An alias doesn't establish which Exchange mailbox identity to use
        // for SMTP. Authenticate first, then offer the login UPN for review.
        if self
            .setup
            .as_ref()
            .is_some_and(|wizard| wizard.step == crate::setup::SetupStep::Authenticating)
        {
            self.start_setup_oauth();
        }
    }

    pub fn setup_login_finished(
        &mut self,
        request_id: u64,
        result: Result<Option<String>, String>,
    ) {
        let Some(wizard) = self.setup.as_mut() else {
            return;
        };
        if !wizard.accepts_oauth_result(request_id) {
            return; // A cancelled or superseded browser attempt completed late.
        }
        match result {
            Ok(login) => {
                if let Err(error) = wizard.complete_authentication(login, &self.setup_config_path) {
                    wizard.error = true;
                    wizard.message = format!(
                        "Signed in, but saving failed: {error:#}. Review settings and save again."
                    );
                }
            }
            Err(error) => {
                wizard.cancel_authentication();
                wizard.authentication_failed(&error);
            }
        }
    }

    /// Start a browser login after validating generated server settings.
    /// Used both for a Microsoft alias (before settings review) and for the
    /// explicit Continue action in the settings step.
    pub(super) fn start_setup_oauth(&mut self) {
        let Some(wizard) = self.setup.as_mut() else {
            return;
        };
        if wizard.authenticating || wizard.authenticated || !wizard.uses_oauth() {
            return;
        }
        match wizard.preview_account() {
            Ok(account) => {
                // OAuth tokens are keyed by account name, so never let wizard
                // setup overwrite a configured account's credentials.
                let duplicate = self
                    .accounts
                    .iter()
                    .any(|existing| existing.name == account.name)
                    || crate::config::Config::load_or_empty(Some(&self.setup_config_path))
                        .is_ok_and(|config| {
                            config
                                .accounts
                                .iter()
                                .any(|existing| existing.name == account.name)
                        });
                if duplicate {
                    wizard.step = crate::setup::SetupStep::Settings;
                    wizard.error = true;
                    wizard.message =
                        "This account name is already in use. Choose another before signing in."
                            .into();
                    return;
                }
                let Some(sender) = self.setup_sender.clone() else {
                    wizard.step = crate::setup::SetupStep::Settings;
                    wizard.error = true;
                    wizard.message = "OAuth runtime unavailable".into();
                    return;
                };
                // The login is provisional until the account is saved.
                wizard.provisional_oauth_account = Some(account.clone());
                let (request_id, cancel) = wizard.begin_authentication();
                std::thread::spawn(move || {
                    let name = account.name.clone();
                    let result = crate::oauth::login_account_quiet_cancellable(
                        std::slice::from_ref(&account),
                        &name,
                        &cancel,
                    )
                    .and_then(|()| {
                        let settings = crate::oauth::OAuthSettings::for_account(&account)?
                            .ok_or_else(|| anyhow::anyhow!("OAuth is not configured"))?;
                        if settings.provider == "microsoft" {
                            settings.suggested_login_username()
                        } else {
                            Ok(None)
                        }
                    })
                    .map_err(|error| format!("{}", error.root_cause()));
                    let _ = sender.send(RuntimeEvent::SetupLoginFinished { request_id, result });
                });
            }
            Err(error) => {
                wizard.step = crate::setup::SetupStep::Settings;
                wizard.selected = crate::setup::PROVIDER;
                wizard.error = true;
                wizard.message =
                    format!("Check settings: {error:#}. F3 opens advanced OAuth settings.");
            }
        }
    }

    pub(super) fn handle_setup_key(&mut self, key: KeyEvent) {
        let action = if let Some(wizard) = self.setup.as_mut() {
            if wizard.saved && !wizard.authenticating && key.code == KeyCode::Enter {
                self.setup_reload_requested = true;
                return;
            }
            wizard.key(key)
        } else {
            return;
        };
        match action {
            WizardAction::Nothing => {}
            WizardAction::Close => {
                if self.setup.as_ref().is_some_and(|wizard| !wizard.first_run) {
                    self.setup = None;
                } else {
                    self.should_quit = true;
                }
            }
            WizardAction::Discover => {
                let Some(sender) = self.setup_sender.clone() else {
                    return;
                };
                let Some(wizard) = self.setup.as_mut() else {
                    return;
                };
                if wizard.discovering {
                    return;
                }
                if let Err(error) = wizard.discovery_ready() {
                    wizard.error = true;
                    wizard.message = format!("Cannot discover settings: {error}");
                    return;
                }
                let email = wizard.fields[crate::setup::EMAIL].trim().to_owned();
                let provider = wizard.provider_hint().to_owned();
                wizard.discovering = true;
                wizard.error = false;
                wizard.spinner_frame = 0;
                wizard.message = "Finding account settings…".into();
                std::thread::spawn(move || {
                    let result = crate::discovery::discover_with_provider(&email, &provider)
                        .map_err(|error| error.to_string());
                    let _ = sender.send(RuntimeEvent::SetupDiscovered { email, result });
                });
            }
            WizardAction::TestConfiguration => {
                let Some(sender) = self.setup_sender.clone() else {
                    return;
                };
                let Some(wizard) = self.setup.as_mut() else {
                    return;
                };
                if wizard.uses_oauth() && !wizard.authenticated {
                    wizard.error = true;
                    wizard.message =
                        "Sign in with OAuth first (Ctrl-S), then press F5 to test IMAP and SMTP."
                            .into();
                    return;
                }
                match wizard.preview_account() {
                    Ok(account) => {
                        let password = (!wizard.uses_oauth())
                            .then(|| wizard.fields[crate::setup::PASSWORD].clone());
                        let request_id = wizard.begin_connection_test();
                        std::thread::spawn(move || {
                            let report = crate::setup::probe::test_account(account, password);
                            let _ =
                                sender.send(RuntimeEvent::SetupTestFinished { request_id, report });
                        });
                    }
                    Err(error) => {
                        wizard.error = true;
                        wizard.message = format!("Check settings before testing: {error}");
                    }
                }
            }
            WizardAction::SaveOffline => {
                let Some(wizard) = self.setup.as_mut() else {
                    return;
                };
                match wizard.save(&self.setup_config_path) {
                    Ok(_) => {
                        wizard.saved = true;
                        wizard.error = false;
                        // Confirmation already explains the offline account state.
                        wizard.message.clear();
                    }
                    Err(error) => {
                        wizard.error = true;
                        wizard.message = format!("Cannot save: {error:#}");
                    }
                }
            }
            WizardAction::Save => {
                if self.setup.as_ref().is_some_and(|wizard| wizard.saved) {
                    return;
                }
                if self
                    .setup
                    .as_ref()
                    .is_some_and(|wizard| wizard.uses_oauth() && !wizard.authenticated)
                {
                    self.start_setup_oauth();
                    return;
                }
                let Some(wizard) = self.setup.as_mut() else {
                    return;
                };
                match wizard.save(&self.setup_config_path) {
                    Ok(_) => {
                        wizard.saved = true;
                        wizard.error = false;
                        // The confirmation page and its centered action provide
                        // the success feedback without a duplicate status line.
                        wizard.message.clear();
                    }
                    Err(error) => {
                        wizard.error = true;
                        wizard.message = format!("Cannot save: {error:#}");
                    }
                }
            }
        }
    }
}
