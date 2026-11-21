//! Tests for the parent module; kept alongside the implementation.
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
        crate::oauth::token_path_for_account(&roots.config, &deleted.name, Some("../../escape"),)
            .is_err()
    );
}
#[test]
fn discovered_jmap_can_be_tested_and_saved_without_plaintext_password() {
    let mut wizard = SetupWizard::new(false);
    wizard.fields[NAME] = "Personal".into();
    wizard.fields[EMAIL] = "hi@fivejazz.com".into();
    wizard.apply_discovery(
        crate::discovery::discover_with_provider("hi@fivejazz.com", "jmap").unwrap(),
    );
    assert!(wizard.is_jmap());
    assert_eq!(wizard.fields[IMAP_HOST], "mail.fivejazz.com");
    assert_eq!(wizard.fields[IMAP_PORT], "443");
    assert!(!wizard.visible_fields().contains(&IMAP_SECURITY));
    wizard.fields[PASSWORD] = "private-password".into();
    let account = wizard.preview_account().unwrap();
    assert!(crate::backend::JmapSource::supports(&account.source));
    assert!(account.source.contains("hi%40fivejazz.com"));
    assert!(!account.source.contains("private-password"));
    assert!(account.source_cred_cmd.is_none());
    assert!(wizard.validate().unwrap().contains("source = \"jmaps://"));
    assert!(!wizard.validate().unwrap().contains("private-password"));
}

#[test]
fn jmap_wizard_supports_native_only_without_smtp_settings() {
    let mut wizard = SetupWizard::new(false);
    wizard.fields[NAME] = "Native".into();
    wizard.fields[EMAIL] = "hi@fivejazz.com".into();
    wizard.apply_discovery(
        crate::discovery::discover_with_provider("hi@fivejazz.com", "jmap").unwrap(),
    );
    wizard.fields[PASSWORD] = "private-password".into();
    wizard.fields[SMTP_HOST].clear();
    wizard.fields[SMTP_PORT].clear();
    wizard.fields[SMTP_LOGIN].clear();
    let account = wizard.preview_account().unwrap();
    assert_eq!(account.outgoing.as_deref(), Some("jmap"));
    assert!(account.smtp_host.is_none());
    assert!(crate::outgoing::from_account(&account).unwrap().is_none());
}

#[test]
fn wizard_can_switch_between_discovered_jmap_and_original_imap_server() {
    let mut wizard = SetupWizard::new(false);
    wizard.fields[NAME] = "Personal".into();
    wizard.fields[EMAIL] = "hi@fivejazz.com".into();
    wizard.apply_discovery(
        crate::discovery::discover_with_provider("hi@fivejazz.com", "jmap").unwrap(),
    );
    wizard.selected = PROVIDER;
    wizard.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert!(!wizard.is_jmap());
    assert_eq!(wizard.fields[IMAP_PORT], "993");
    assert!(wizard.visible_fields().contains(&IMAP_SECURITY));
    wizard.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(wizard.is_jmap());
    assert_eq!(wizard.fields[IMAP_PORT], "443");
}
