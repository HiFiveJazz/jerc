//! Tests for the parent module; kept alongside the implementation.
#[test]
fn cached_oauth_token_expires_across_linux_suspend() {
    use super::CachedAccess;
    use std::time::{Duration, Instant, SystemTime};

    let now = Instant::now();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let access = CachedAccess {
        token: "test-only".into(),
        scope: None,
        provider: "google".into(),
        client_id: "test-client".into(),
        requested_scope: "mail".into(),
        expires_at: now + Duration::from_secs(900),
        expires_at_wall: wall + Duration::from_secs(900),
    };
    assert!(access.valid_at(
        now + Duration::from_secs(20),
        wall + Duration::from_secs(20)
    ));
    // Linux CLOCK_MONOTONIC barely advances during a long suspend.
    assert!(!access.valid_at(
        now + Duration::from_secs(21),
        wall + Duration::from_secs(3600)
    ));
    // A running process must still respect monotonic expiry if its wall
    // clock was moved backwards by NTP or a manual clock adjustment.
    assert!(!access.valid_at(
        now + Duration::from_secs(901),
        wall + Duration::from_secs(30)
    ));
    assert!(!access.valid_at(
        now + Duration::from_secs(900),
        wall + Duration::from_secs(899)
    ));
}

use super::*;

#[test]
fn cancellation_token_is_checked_without_network_access() {
    let cancelled = AtomicBool::new(false);
    assert!(ensure_not_cancelled(Some(&cancelled)).is_ok());
    cancelled.store(true, Ordering::Relaxed);
    assert!(ensure_not_cancelled(Some(&cancelled)).is_err());
    assert!(ensure_not_cancelled(None).is_ok());
    assert!(
        login_account_quiet_cancellable(&[], "unused", &cancelled)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}

#[test]
fn oauth_publisher_defaults_respect_explicit_nonempty_overrides() {
    assert_eq!(
        account_or_embedded(None, Some("embedded")),
        Some("embedded")
    );
    assert_eq!(
        account_or_embedded(Some(""), Some("embedded")),
        Some("embedded")
    );
    assert_eq!(
        account_or_embedded(Some("  "), Some("embedded")),
        Some("embedded")
    );
    assert_eq!(
        account_or_embedded(Some("override"), Some("embedded")),
        Some("override")
    );
    assert_eq!(
        account_or_embedded(Some("override"), None),
        Some("override")
    );
    assert_eq!(account_or_embedded(None, None), None);
    assert_eq!(account_or_embedded(Some(""), None), None);
}
#[test]
fn google_desktop_id_must_include_full_suffix() {
    assert!(valid_google_desktop_client_id(
        "123-abc.apps.googleusercontent.com"
    ));
    assert!(!valid_google_desktop_client_id("123-abc"));
    assert!(!valid_google_desktop_client_id(
        "oops\n.apps.googleusercontent.com"
    ));
}

#[test]
fn microsoft_login_hint_prefers_human_readable_login_and_rejects_unsafe_claims() {
    fn token(claims: serde_json::Value) -> String {
        let data = serde_json::to_vec(&claims).unwrap();
        format!("header.{}.signature", URL_SAFE_NO_PAD.encode(data))
    }
    let ok = token(serde_json::json!({
        "preferred_username": "f00851y@dartmouth.edu",
        "upn": "other@dartmouth.edu"
    }));
    assert_eq!(
        super::microsoft_login_username_hint(&ok).as_deref(),
        Some("f00851y@dartmouth.edu")
    );
    let fallback = token(serde_json::json!({
        "preferred_username": "User Name",
        "upn": "login@dartmouth.edu"
    }));
    assert_eq!(
        super::microsoft_login_username_hint(&fallback).as_deref(),
        Some("login@dartmouth.edu")
    );
    assert!(
        super::microsoft_login_username_hint(&token(serde_json::json!({
            "preferred_username": "login\n@dartmouth.edu"
        })))
        .is_none()
    );
    assert!(super::microsoft_login_username_hint("opaque-token").is_none());
}

#[test]
fn google_and_microsoft_use_distinct_imap_smtp_scopes() {
    let google = OAuthSettings::from_config(
        "Personal",
        &OAuthConfig {
            provider: "google".into(),
            client_id: Some("public-client".into()),
            tenant: None,
            client_secret: None,
            flow: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(google.scope, "https://mail.google.com/");
    let microsoft = OAuthSettings::from_config(
        "Work",
        &OAuthConfig {
            provider: "microsoft".into(),
            client_id: Some("public-client".into()),
            tenant: None,
            client_secret: None,
            flow: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(microsoft.scope.contains("IMAP.AccessAsUser.All"));
    assert!(microsoft.scope.contains("SMTP.Send"));
    assert!(microsoft.scope.contains("offline_access"));
    assert!(!format!("{google:?}").contains("public-client"));
}
#[test]
fn microsoft_redirect_flows_do_not_assume_localhost_path() {
    assert_eq!(
        LoginFlow::parse(None, "microsoft").unwrap(),
        LoginFlow::Browser
    );
    assert_eq!(
        LoginFlow::parse(Some("nativeclient"), "microsoft").unwrap(),
        LoginFlow::NativeClient
    );
    assert_eq!(
        LoginFlow::parse(Some("devicecode"), "microsoft").unwrap(),
        LoginFlow::DeviceCode
    );
    assert!(LoginFlow::parse(Some("nativeclient"), "google").is_err());
    assert!(LoginFlow::parse(Some("unsafe"), "microsoft").is_err());
}

#[test]
fn nativeclient_callback_requires_expected_origin_path_and_state() {
    let ok = format!("{MICROSOFT_NATIVE_REDIRECT}?code=abc%2B123&state=EXPECTED");
    assert_eq!(
        parse_nativeclient_callback(&ok, "EXPECTED").unwrap(),
        "abc+123"
    );
    assert!(parse_nativeclient_callback(&ok, "WRONG").is_err());
    assert!(
        parse_nativeclient_callback("https://evil.test/?code=abc&state=EXPECTED", "EXPECTED")
            .is_err()
    );
    assert!(
        parse_nativeclient_callback(
            "https://login.microsoftonline.com/other?code=abc&state=EXPECTED",
            "EXPECTED"
        )
        .is_err()
    );
    assert!(
        parse_nativeclient_callback(
            &format!("{MICROSOFT_NATIVE_REDIRECT}?state=EXPECTED"),
            "EXPECTED"
        )
        .is_err()
    );
}

#[test]
fn authorization_url_binds_pkce_to_configured_redirect() {
    let settings = OAuthSettings::from_config(
        "Work",
        &OAuthConfig {
            provider: "microsoft".into(),
            client_id: Some("test-id".into()),
            tenant: None,
            client_secret: None,
            flow: Some("nativeclient".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (url, verifier, state) = settings
        .authorization_url(MICROSOFT_NATIVE_REDIRECT)
        .unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        params.get("redirect_uri").unwrap(),
        MICROSOFT_NATIVE_REDIRECT
    );
    assert_eq!(params.get("state").unwrap(), &state);
    assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
    assert_eq!(
        params.get("code_challenge").unwrap(),
        &URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()).as_ref())
    );
}
#[test]
fn browser_redirect_and_consent_options_match_owned_registration() {
    let settings = OAuthSettings::from_config(
        "Work",
        &OAuthConfig {
            provider: "microsoft".into(),
            client_id: Some("registered-client".into()),
            callback_host: Some("localhost".into()),
            callback_path: Some("/".into()),
            prompt: Some("select_account".into()),
            login_hint: Some("person@example.net".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let redirect = "http://localhost:54321/";
    let (url, _, _) = settings.authorization_url(redirect).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        params.get("redirect_uri").map(String::as_str),
        Some(redirect)
    );
    assert_eq!(
        params.get("prompt").map(String::as_str),
        Some("select_account")
    );
    assert_eq!(
        params.get("login_hint").map(String::as_str),
        Some("person@example.net")
    );
    assert!(settings.scope.contains("offline_access"));
}

#[test]
fn rejects_unsafe_redirects_and_incompatible_flows() {
    for bad in ["evil.example", "0.0.0.0", "localhost.evil.test", "::1"] {
        assert!(
            OAuthSettings::from_config(
                "Work",
                &OAuthConfig {
                    provider: "microsoft".into(),
                    client_id: Some("owned".into()),
                    callback_host: Some(bad.into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
    for bad in [
        "not/a/path",
        "//attacker",
        "/foo?bar",
        "/foo#bar",
        "/foo%2fbar",
    ] {
        assert!(
            OAuthSettings::from_config(
                "Work",
                &OAuthConfig {
                    provider: "microsoft".into(),
                    client_id: Some("owned".into()),
                    callback_path: Some(bad.into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
    assert!(
        OAuthSettings::from_config(
            "Work",
            &OAuthConfig {
                provider: "microsoft".into(),
                client_id: Some("owned".into()),
                callback_path: Some("/".into()),
                flow: Some("devicecode".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        OAuthSettings::from_config(
            "Personal",
            &OAuthConfig {
                provider: "google".into(),
                client_id: Some("owned".into()),
                prompt: Some("login".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn custom_scopes_are_used_for_explicit_consent_tests() {
    let settings = OAuthSettings::from_config(
        "Work",
        &OAuthConfig {
            provider: "microsoft".into(),
            client_id: Some("owned".into()),
            scope: Some("offline_access https://outlook.office.com/IMAP.AccessAsUser.All".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (url, _, _) = settings
        .authorization_url(MICROSOFT_NATIVE_REDIRECT)
        .unwrap();
    assert_eq!(
        url.query_pairs().find(|(key, _)| key == "scope").unwrap().1,
        "offline_access https://outlook.office.com/IMAP.AccessAsUser.All"
    );
}

#[test]
fn experimental_credential_namespace_uses_a_separate_token_directory() {
    let ordinary = OAuthSettings::from_config(
        "University",
        &OAuthConfig {
            provider: "microsoft".into(),
            client_id: Some("local-public-client".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let experimental = OAuthSettings::from_config(
        "University",
        &OAuthConfig {
            provider: "microsoft".into(),
            client_id: Some("local-public-client".into()),
            credential_namespace: Some("oauth-lab".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_ne!(
        ordinary.token_file().unwrap(),
        experimental.token_file().unwrap()
    );
    assert_eq!(
        experimental
            .token_file()
            .unwrap()
            .parent()
            .unwrap()
            .file_name()
            .and_then(|name| name.to_str()),
        Some("oauth-lab")
    );
    for value in ["../escape", "a/b", "", "x.y", "a b"] {
        assert!(
            OAuthSettings::from_config(
                "University",
                &OAuthConfig {
                    provider: "microsoft".into(),
                    client_id: Some("local-public-client".into()),
                    credential_namespace: Some(value.into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}

#[test]
fn smtp_claim_diagnostics_do_not_expose_token_or_identifiers() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let payload = serde_json::json!({
        "aud": "https://outlook.office.com", "scp": "IMAP.AccessAsUser.All SMTP.Send",
        "azp": "client-identifier", "tid": "private-tenant",
        "preferred_username": "student@example.edu", "oid": "private-object"
    });
    let token = format!(
        "{}.{}.signature",
        URL_SAFE_NO_PAD.encode("{}"),
        URL_SAFE_NO_PAD.encode(payload.to_string())
    );
    let diagnostic =
        safe_smtp_token_diagnostics(&token, "client-identifier", "student@example.edu");
    assert!(diagnostic.contains("aud=Exchange Online"));
    assert!(diagnostic.contains("scp.SMTP.Send=yes"));
    assert!(diagnostic.contains("client_id_matches=yes"));
    assert!(diagnostic.contains("login_matches_SMTP=yes"));
    for secret in [
        "private-tenant",
        "private-object",
        "student@example.edu",
        "client-identifier",
        &token,
    ] {
        assert!(!diagnostic.contains(secret));
    }
    assert!(safe_smtp_token_diagnostics("opaque-token", "id", "u").contains("inconclusive"));
}

#[test]
fn protocol_permissions_are_read_only_from_token_response_scope_metadata() {
    assert_eq!(
        protocol_scope_flags(
            "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send"
        ),
        (true, true)
    );
    assert_eq!(
        protocol_scope_flags("https://outlook.office.com/IMAP.AccessAsUser.All"),
        (true, false)
    );
    assert_eq!(
        protocol_scope_flags("https://graph.microsoft.com/Mail.Send"),
        (false, false)
    );
    let parsed: TokenReply = serde_json::from_str(
            r#"{"access_token":"private-token","refresh_token":null,"scope":"https://outlook.office.com/SMTP.Send"}"#
        ).unwrap();
    assert_eq!(
        parsed.scope.as_deref().map(protocol_scope_flags),
        Some((false, true))
    );
    let no_scope: TokenReply =
        serde_json::from_str(r#"{"access_token":"private-token","refresh_token":null}"#).unwrap();
    assert!(no_scope.scope.is_none());
}

#[test]
fn safe_oauth_failure_does_not_include_descriptions_or_credentials() {
    let error = OAuthFailure {
        error: Some("invalid_grant".into()),
        error_codes: vec![65001],
    };
    let message = safe_oauth_failure(400, &error);
    assert!(message.contains("AADSTS65001"));
    assert!(message.contains("invalid_grant"));
    assert!(!message.contains("refresh_token"));
    let unsafe_error = OAuthFailure {
        error: Some("token=secret\n".into()),
        error_codes: vec![],
    };
    assert!(!safe_oauth_failure(400, &unsafe_error).contains("secret"));
}

#[test]
fn rejects_unexpected_providers_and_tenant_paths() {
    assert!(
        OAuthSettings::from_config(
            "Work",
            &OAuthConfig {
                provider: "example".into(),
                client_id: Some("id".into()),
                tenant: None,
                client_secret: None,
                flow: None,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        OAuthSettings::from_config(
            "Work",
            &OAuthConfig {
                provider: "microsoft".into(),
                client_id: Some("id".into()),
                tenant: Some("../x".into()),
                client_secret: None,
                flow: None,
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn open_oauth_settings_are_offline_until_browser_login() {
    let settings = OAuthSettings::from_config(
        "FiveJazz isolated test account",
        &OAuthConfig {
            provider: "open".into(),
            issuer: Some("https://mail.fivejazz.com".into()),
            credential_namespace: Some("stalwart-test-only".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(settings.provider, "open");
    assert_eq!(settings.scope, open::MAIL_SCOPE);
    assert_eq!(settings.callback_host, "127.0.0.1");
    assert!(settings.client_secret.is_none());
    assert_eq!(settings.flow, LoginFlow::Browser);
    assert!(!format!("{settings:?}").contains("mail.fivejazz.com"));
}

#[test]
fn open_oauth_cannot_use_google_secret_or_an_unsafe_redirect() {
    let base = OAuthConfig {
        provider: "open".into(),
        issuer: Some("https://mail.example.org".into()),
        client_secret: Some("should-not-be-here".into()),
        ..Default::default()
    };
    assert!(OAuthSettings::from_config("Open", &base).is_err());
    let mut bad = base.clone();
    bad.client_secret = None;
    bad.callback_host = Some("localhost".into());
    assert!(OAuthSettings::from_config("Open", &bad).is_err());
    bad.callback_host = Some("127.0.0.1".into());
    bad.callback_path = Some("/".into());
    assert!(OAuthSettings::from_config("Open", &bad).is_err());
}

#[test]
fn issuer_override_does_not_apply_to_google_or_microsoft() {
    for provider in ["google", "microsoft"] {
        let config = OAuthConfig {
            provider: provider.into(),
            client_id: Some("owned-app-id".into()),
            issuer: Some("https://mail.example.org".into()),
            ..Default::default()
        };
        assert!(OAuthSettings::from_config("Other", &config).is_err());
    }
}

#[test]
fn non_browser_flows_are_not_allowed_for_open_public_clients() {
    let config = OAuthConfig {
        provider: "open".into(),
        issuer: Some("https://mail.example.org".into()),
        flow: Some("devicecode".into()),
        ..Default::default()
    };
    assert!(OAuthSettings::from_config("Open", &config).is_err());
}

#[test]
fn google_revocation_allows_other_google_users_with_shared_client() {
    let mut removed = crate::config::Config::single_maildir(
        "Burner".into(),
        std::path::PathBuf::from("/tmp/unused-maildir"),
    )
    .accounts
    .remove(0);
    removed.email = Some("burner@gmail.com".into());
    removed.source = "imap".into();
    removed.oauth = Some(OAuthConfig {
        provider: "google".into(),
        client_id: Some("desktop-app".into()),
        ..Default::default()
    });
    let mut personal = removed.clone();
    personal.name = "Personal".into();
    personal.email = Some("personal@gmail.com".into());
    assert_eq!(
        super::google_mailbox_identity(&removed).as_deref(),
        Some("burner@gmail.com")
    );
    assert!(!super::google_revocation_conflicts(
        &removed,
        &[personal],
        "desktop-app"
    ));
}

#[test]
fn google_revocation_preserves_same_principal_or_unknown_identity() {
    let mut removed = crate::config::Config::single_maildir(
        "Burner".into(),
        std::path::PathBuf::from("/tmp/unused-maildir"),
    )
    .accounts
    .remove(0);
    removed.email = Some("burner@gmail.com".into());
    removed.source = "imap".into();
    removed.oauth = Some(OAuthConfig {
        provider: "google".into(),
        client_id: Some("desktop-app".into()),
        ..Default::default()
    });
    let mut alias = removed.clone();
    alias.name = "Alias".into();
    alias.email = Some("BURNER@gmail.com".into());
    assert!(super::google_revocation_conflicts(
        &removed,
        &[alias],
        "desktop-app"
    ));
    let mut unknown = removed.clone();
    unknown.email = None;
    assert!(super::google_revocation_conflicts(
        &removed,
        &[unknown],
        "desktop-app"
    ));
}

#[test]
fn credential_gate_is_shared_and_serializes_concurrent_refreshes() {
    use std::sync::atomic::AtomicUsize;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("account.json");
    let first = credential_slot(&path);
    let second = credential_slot(&path);
    assert!(Arc::ptr_eq(&first, &second));
    let in_critical_section = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for slot in [first, second] {
            let active = Arc::clone(&in_critical_section);
            scope.spawn(move || {
                for _ in 0..20 {
                    let _guard = slot.state.lock().unwrap();
                    assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0);
                    std::thread::yield_now();
                    assert_eq!(active.fetch_sub(1, Ordering::SeqCst), 1);
                }
            });
        }
    });
    assert_eq!(in_critical_section.load(Ordering::SeqCst), 0);
}

#[test]
fn retirement_allows_final_rotation_only_before_cleanup_and_not_after_new_login() {
    let slot = CredentialSlot::default();
    let old = slot.generation.load(Ordering::Acquire);
    assert!(refresh_may_persist(&slot, old));
    slot.retired.store(true, Ordering::Release);
    slot.generation.fetch_add(1, Ordering::AcqRel);
    assert!(refresh_may_persist(&slot, old));
    // New login supersedes the old refresh and its cleanup job.
    slot.generation.fetch_add(1, Ordering::AcqRel);
    slot.retired.store(false, Ordering::Release);
    assert!(!refresh_may_persist(&slot, old));
}

#[test]
fn cancelled_login_cannot_persist_new_refresh_token() {
    let config = OAuthConfig {
        provider: "google".into(),
        client_id: Some("test.apps.googleusercontent.com".into()),
        ..Default::default()
    };
    let mut settings = OAuthSettings::from_config("cancel-test", &config).unwrap();
    let slot = Arc::new(CredentialSlot::default());
    settings.credential_slot = slot;
    let cancel = AtomicBool::new(true);
    let reply = TokenReply {
        access_token: "never-save-access".into(),
        refresh_token: Some("never-save-refresh".into()),
        expires_in: Some(3600),
        scope: None,
    };
    assert!(
        settings
            .finish_login_with_cancel(reply, true, Some(&cancel))
            .is_err()
    );
}

#[test]
fn superseded_login_cannot_overwrite_a_new_generation() {
    let config = OAuthConfig {
        provider: "google".into(),
        client_id: Some("test.apps.googleusercontent.com".into()),
        ..Default::default()
    };
    let mut settings = OAuthSettings::from_config("superseded-test", &config).unwrap();
    let slot = Arc::new(CredentialSlot::default());
    slot.generation.store(1, Ordering::Release);
    settings.credential_slot = slot;
    settings.credential_generation = 0;
    let reply = TokenReply {
        access_token: "never-save-access".into(),
        refresh_token: Some("never-save-refresh".into()),
        expires_in: Some(3600),
        scope: None,
    };
    assert!(
        settings
            .finish_login_with_cancel(reply, true, None)
            .is_err()
    );
}

#[test]
fn wizard_cannot_rebind_grant_to_another_client_or_account() {
    let mut account = crate::config::Config::single_maildir(
        "original".into(),
        std::path::PathBuf::from("/tmp/oauth-unused-maildir"),
    )
    .accounts
    .remove(0);
    account.oauth = Some(OAuthConfig {
        provider: "google".into(),
        client_id: Some("first.apps.googleusercontent.com".into()),
        ..Default::default()
    });
    assert!(OAuthSettings::same_wizard_credential_identity(&account, &account).unwrap());
    let mut edited = account.clone();
    edited.name = "another".into();
    assert!(!OAuthSettings::same_wizard_credential_identity(&account, &edited).unwrap());
    edited = account.clone();
    edited.oauth.as_mut().unwrap().client_id = Some("second.apps.googleusercontent.com".into());
    assert!(!OAuthSettings::same_wizard_credential_identity(&account, &edited).unwrap());
}

#[test]
fn stale_cleanup_is_rejected_after_new_login() {
    let slot = CredentialSlot::default();
    let retired_generation = slot.generation.fetch_add(1, Ordering::AcqRel) + 1;
    slot.retired.store(true, Ordering::Release);
    assert!(retired_slot_is_current(&slot, retired_generation));
    slot.generation.fetch_add(1, Ordering::AcqRel);
    slot.retired.store(false, Ordering::Release);
    assert!(!retired_slot_is_current(&slot, retired_generation));
}

#[test]
fn old_oauth_settings_cannot_reuse_reauthenticated_session() {
    let config = OAuthConfig {
        provider: "google".into(),
        client_id: Some("test.apps.googleusercontent.com".into()),
        ..Default::default()
    };
    let mut settings = OAuthSettings::from_config("oauth-generation-test", &config).unwrap();
    let slot = Arc::new(CredentialSlot::default());
    settings.credential_slot = Arc::clone(&slot);
    settings.credential_generation = 0;
    assert!(settings.check_credential_active().is_ok());
    slot.retired.store(true, Ordering::Release);
    slot.generation.fetch_add(1, Ordering::AcqRel);
    assert!(settings.check_credential_active().is_err());
    slot.generation.fetch_add(1, Ordering::AcqRel);
    slot.retired.store(false, Ordering::Release);
    assert!(settings.check_credential_active().is_err());
    let mut new = settings.clone();
    new.credential_generation = 2;
    assert!(new.check_credential_active().is_ok());
}

#[test]
fn credential_slot_isolation_uses_entire_path() {
    let root = tempfile::tempdir().unwrap();
    let a = credential_slot(&root.path().join("a.json"));
    let b = credential_slot(&root.path().join("b.json"));
    assert!(!Arc::ptr_eq(&a, &b));
    a.retired.store(true, Ordering::Release);
    assert!(!b.retired.load(Ordering::Acquire));
}

#[test]
fn token_reply_expiry_is_optional_and_never_required_by_old_providers() {
    let reply: TokenReply =
        serde_json::from_str(r#"{"access_token":"test","refresh_token":null}"#).unwrap();
    assert_eq!(reply.expires_in, None);
    let reply: TokenReply =
        serde_json::from_str(r#"{"access_token":"test","expires_in":3600}"#).unwrap();
    assert_eq!(reply.expires_in, Some(3600));
}

#[test]
fn unsupported_provider_revocation_is_not_reported_as_success() {
    let config = OAuthConfig {
        provider: "microsoft".into(),
        client_id: Some("test-public-id".into()),
        ..Default::default()
    };
    let settings = OAuthSettings::from_config("revocation-test", &config).unwrap();
    let request = RevocationRequest {
        settings,
        token: "test".into(),
    };
    assert!(request.revoke().is_err());
}

#[test]
fn google_revocation_decodes_imap_logins_and_protects_other_clients() {
    let mut removed = crate::config::Config::single_maildir(
        "Burner".into(),
        std::path::PathBuf::from("/tmp/unused-maildir"),
    )
    .accounts
    .remove(0);
    removed.source = "imaps+xoauth2://burner%40gmail.com@imap.gmail.com:993".into();
    removed.oauth = Some(OAuthConfig {
        provider: "google".into(),
        client_id: Some("desktop-app".into()),
        ..Default::default()
    });
    assert_eq!(
        super::google_mailbox_identity(&removed).as_deref(),
        Some("burner@gmail.com")
    );
    let mut different_client = removed.clone();
    different_client.name = "Other".into();
    different_client.oauth.as_mut().unwrap().client_id = Some("other-app".into());
    assert!(super::google_revocation_conflicts(
        &removed,
        &[different_client],
        "desktop-app"
    ));
}
