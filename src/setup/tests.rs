//! Tests for the parent module; kept alongside the implementation.
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
