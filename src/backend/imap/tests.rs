//! Tests for the parent module; kept alongside the implementation.
#[test]
fn password_backend_reads_wizard_private_credential_file() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), "generated account password").unwrap();
    let command = format!("@file:{}", file.path().display());
    assert_eq!(
        super::run_credential_command(&command).unwrap(),
        "generated account password"
    );
}
use super::{ImapAuth, ImapSource, role_for_name};

#[test]
fn starttls_uri_builds_required_upgrade_options_before_authentication() {
    let source = ImapSource::parse("imap+starttls://me%40example.com@imap.example.com").unwrap();
    assert_eq!(source.port, 143);
    assert_eq!(source.auth, ImapAuth::Password);
    let (url, options) = source.connection_settings().unwrap();
    assert_eq!(url.scheme(), "imap");
    assert_eq!(url.port(), Some(143));
    assert!(options.session.starttls);
}

#[test]
fn implicit_tls_still_does_not_issue_starttls() {
    let source = ImapSource::parse("imaps://me%40example.com@imap.example.com").unwrap();
    let (url, options) = source.connection_settings().unwrap();
    assert_eq!(url.scheme(), "imaps");
    assert_eq!(url.port(), Some(993));
    assert!(!options.session.starttls);
}

#[test]
fn all_imap_starttls_auth_variants_are_accepted_without_plaintext_uri_support() {
    for (scheme, expected) in [
        ("imap+starttls", ImapAuth::Password),
        ("imap+starttls+login", ImapAuth::Login),
        ("imap+starttls+xoauth2", ImapAuth::XOAuth2),
        ("imap+starttls+oauthbearer", ImapAuth::OAuthBearer),
    ] {
        let uri = format!("{scheme}://me%40example.com@mail.example.com:1143");
        assert!(ImapSource::supports(&uri));
        let source = ImapSource::parse(&uri).unwrap();
        assert_eq!(source.auth, expected);
        assert!(source.connection_settings().unwrap().1.session.starttls);
    }
    assert!(!ImapSource::supports("imap://user@mail.example.com"));
    assert!(ImapSource::parse("imap://user@mail.example.com").is_err());
    assert!(ImapSource::parse("imap+starttls+garbage://user@mail.example.com").is_err());
}

#[test]
fn starttls_connection_failure_does_not_send_password() {
    use crate::backend::MailBackend;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(b"* OK [CAPABILITY IMAP4rev1] Fake IMAP server\r\n")
            .unwrap();
        let mut buffer = [0_u8; 1024];
        let count = stream.read(&mut buffer).unwrap_or(0);
        let commands = String::from_utf8_lossy(&buffer[..count]);
        assert!(!commands.to_ascii_uppercase().contains("LOGIN"));
        assert!(!commands.contains("sensitive-password"));
        // No STARTTLS capability; deliberately close the connection.
    });
    let source = ImapSource::parse(&format!(
        "imap+starttls://user%40example.com:sensitive-password@127.0.0.1:{port}"
    ))
    .unwrap();
    let mut backend = super::ImapBackend::new(source, None, 10);
    assert!(backend.connect().is_err());
    server.join().unwrap();
}
#[test]
fn parses_password_source() {
    let source =
        ImapSource::parse("imaps://person%40example.com:abcd%20efgh@imap.example.com:993").unwrap();
    assert_eq!(source.username, "person@example.com");
    assert_eq!(source.host, "imap.example.com");
    assert_eq!(source.port, 993);
    assert_eq!(source.auth, ImapAuth::Password);
    assert_eq!(source.secret.as_deref(), Some("abcd efgh"));
}

#[test]
fn parses_oauth_refresh_source() {
    let source = ImapSource::parse(
            "imaps+oauthbearer://person%40example.edu:refresh-token@imap.example.com:993?client_id=client&client_secret=secret&token_endpoint=https%3A%2F%2Foauth.example.com%2Ftoken",
        )
        .unwrap();
    assert_eq!(source.auth, ImapAuth::OAuthBearer);
    assert_eq!(source.secret.as_deref(), Some("refresh-token"));
    assert_eq!(
        source.token_endpoint.as_deref(),
        Some("https://oauth.example.com/token")
    );
}

#[test]
fn uri_credential_takes_precedence_over_external_command() {
    let source =
        ImapSource::parse("imaps://person%40example.com:uri-secret@imap.example.com:993").unwrap();
    assert_eq!(
        source
            .credential(Some("this command must never run"))
            .unwrap(),
        "uri-secret"
    );
}

#[test]
fn debug_output_never_contains_secret_material() {
    let source = ImapSource::parse(
            "imaps+oauthbearer://person%40example.com:refresh-secret@imap.example.com:993?client_id=id&client_secret=client-secret&token_endpoint=https%3A%2F%2Foauth.example.com%2Ftoken",
        )
        .unwrap();
    let debug = format!("{source:?}");
    assert!(!debug.contains("refresh-secret"));
    assert!(!debug.contains("client-secret"));
    assert!(!debug.contains("oauth.example.com"));
}

#[test]
fn tls_connection_closed_before_imap_greeting_fails_without_oauth() {
    // Local-only regression test for the actual IMAP transport. Keep
    // ImapClientStd::connect unchanged, and never depend on Microsoft,
    // Google, DNS, or OAuth availability in CI.
    use super::ImapBackend;
    use crate::backend::MailBackend;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    drop(stream); // TLS peer closes before the IMAP greeting
                    break;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("loopback test server did not accept: {error}"),
            }
        }
    });
    let source = ImapSource::parse(&format!(
        "imaps://user%40example.test:dummy@127.0.0.1:{port}"
    ))
    .unwrap();
    let mut backend = ImapBackend::new(source, None, 10);
    let started = Instant::now();
    let err = backend.connect().expect_err("closed TLS peer must fail");
    assert!(err.to_string().contains("connecting to IMAP server"));
    assert!(started.elapsed() < Duration::from_secs(5));
    server.join().unwrap();
}

#[test]
fn cached_uid_generation_is_checked_before_mutations() {
    use super::ImapBackend;
    use crate::model::MessageId;
    let source = ImapSource::parse("imaps://user%40example.com@imap.example.com").unwrap();
    let mut backend = ImapBackend::new(source, None, 100);
    backend.selected_uid_validity = Some(42);
    let valid = MessageId("imap-uid:42:1234".into());
    let stale = MessageId("imap-uid:41:1234".into());
    assert_eq!(ImapBackend::uid_from_id(&valid).unwrap(), 1234);
    assert!(backend.check_uid_generation(&valid).is_ok());
    assert!(backend.check_uid_generation(&stale).is_err());
    backend.selected_uid_validity = None;
    assert!(backend.check_uid_generation(&valid).is_err());
}

fn cached_summary(validity: u32, uid: u32) -> crate::model::MessageSummary {
    use crate::model::{MessageFlags, MessageId, MessageSummary};
    MessageSummary {
        headers: std::sync::Arc::new(crate::model::MessageHeaders {
            id: MessageId(format!("imap-uid:{validity}:{uid}")),
            thread_id: None,
            message_id: None,
            in_reply_to: None,
            references: vec![].into(),
            from_name: String::new(),
            from_address: String::new(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: format!("subject-{uid}"),
            timestamp: i64::from(uid),
            date_label: String::new().into(),
            size: 17,
            has_attachment_hint: true,
            attachment_names_hint: vec!["cached.pdf".into()].into(),
        }),
        flags: MessageFlags::empty(),
    }
}

#[test]
fn incremental_merge_reuses_headers_but_refreshes_flags() {
    use super::merge_uid_page;
    use crate::model::MessageFlags;
    use std::collections::HashMap;
    let cached = vec![cached_summary(41, 7), cached_summary(41, 8)];
    let mut seen = MessageFlags::empty();
    seen.set(MessageFlags::SEEN, true);
    let page = merge_uid_page(41, &cached, &[(7, seen), (8, seen)], HashMap::new()).unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].subject, "subject-8");
    assert!(page[0].flags.contains(MessageFlags::SEEN));
    assert!(std::sync::Arc::ptr_eq(&page[0].headers, &cached[1].headers));
    assert!(!cached[1].flags.contains(MessageFlags::SEEN));
    assert_eq!(
        page[0].attachment_names_hint.as_ref(),
        &["cached.pdf".to_string()]
    );
}

#[test]
fn incremental_merge_adds_new_uids_and_omits_expunged_uids() {
    use super::merge_uid_page;
    use crate::model::MessageFlags;
    use std::collections::HashMap;
    let cached = vec![cached_summary(41, 7), cached_summary(41, 8)];
    let fresh = HashMap::from([(9, cached_summary(41, 9))]);
    let page = merge_uid_page(
        41,
        &cached,
        &[(8, MessageFlags::empty()), (9, MessageFlags::empty())],
        fresh,
    )
    .unwrap();
    assert_eq!(
        page.iter().map(|m| m.id.0.as_str()).collect::<Vec<_>>(),
        vec!["imap-uid:41:9", "imap-uid:41:8"]
    );
}

#[test]
fn incremental_merge_refuses_unverified_or_incomplete_uids() {
    use super::merge_uid_page;
    use crate::model::MessageFlags;
    use std::collections::HashMap;
    let cached = vec![cached_summary(40, 7)];
    let requested = &[(7, MessageFlags::empty())];
    assert!(merge_uid_page(41, &cached, requested, HashMap::new()).is_none());
    assert!(merge_uid_page(41, &[], requested, HashMap::new()).is_none());
    assert!(
        merge_uid_page(
            41,
            &[],
            requested,
            HashMap::from([(7, cached_summary(40, 7))])
        )
        .is_none()
    );
    assert!(
        merge_uid_page(
            41,
            &[],
            &[(7, MessageFlags::empty()), (7, MessageFlags::empty())],
            HashMap::from([(7, cached_summary(41, 7))])
        )
        .is_none()
    );
}

#[test]
fn common_mailbox_names_map_to_roles() {
    assert_eq!(role_for_name("INBOX"), crate::model::MailboxRole::Inbox);
    assert_eq!(
        role_for_name("[Gmail]/Sent Mail"),
        crate::model::MailboxRole::Sent
    );
    assert_eq!(
        role_for_name("[Gmail]/All Mail"),
        crate::model::MailboxRole::Archive
    );
    assert_eq!(
        role_for_name("[Gmail]/Trash"),
        crate::model::MailboxRole::Trash
    );
}
#[test]
fn imap_literal_cannot_exceed_memory_budget() {
    use super::LimitedMessageWriter;
    use std::io::Write;
    let mut writer = LimitedMessageWriter::new(5);
    writer.write_all(b"123").unwrap();
    assert!(writer.write_all(b"456").is_err());
    assert_eq!(writer.bytes, b"123");
    writer.write_all(b"45").unwrap();
    assert_eq!(writer.bytes, b"12345");
}

#[test]
fn io_imap_partial_fetch_is_never_treated_as_complete() {
    use io_imap::send::{ImapFetchPartial, ImapOutput};
    use std::num::NonZeroU32;
    let output = ImapOutput {
        data: vec![42],
        partial: vec![ImapFetchPartial {
            seq: NonZeroU32::new(1).unwrap(),
            uid: None,
            failed: vec![],
        }],
        skipped: vec![],
        alerts: vec![],
    };
    let error = super::complete_imap_output(output, "FETCH").unwrap_err();
    assert!(error.to_string().contains("incomplete IMAP FETCH"));
    let good = ImapOutput {
        data: vec![42],
        partial: vec![],
        skipped: vec![],
        alerts: vec![],
    };
    assert_eq!(
        super::complete_imap_output(good, "FETCH").unwrap(),
        vec![42]
    );
}

#[test]
fn idle_resume_checkpoint_is_bound_to_mailbox_and_size_limit() {
    use io_imap::watch::ImapMailboxWatchState;
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;
    let checkpoint = ImapMailboxWatchState {
        uid_validity: NonZeroU32::new(42).unwrap(),
        highest_mod_seq: 100,
        flags: BTreeMap::new(),
    };
    let mut stored = Some(("INBOX".to_owned(), checkpoint.clone()));
    assert!(super::take_idle_checkpoint(&mut stored, "Other").is_none());
    assert!(stored.is_none());
    let mut stored = Some(("INBOX".to_owned(), checkpoint.clone()));
    assert_eq!(
        super::take_idle_checkpoint(&mut stored, "INBOX"),
        Some(checkpoint.clone())
    );
    assert!(stored.is_none());
    let mut oversized = checkpoint;
    for uid in 1..=super::MAX_IDLE_SHADOW_MESSAGES as u32 + 1 {
        oversized
            .flags
            .insert(NonZeroU32::new(uid).unwrap(), vec![]);
    }
    let mut stored = Some(("INBOX".to_owned(), oversized));
    assert!(super::take_idle_checkpoint(&mut stored, "INBOX").is_none());
}

#[test]
fn io_imap_connection_error_is_distinguished_from_uid_validity_change() {
    use io_imap::client::ImapClientError;
    use io_imap::watch::ImapMailboxWatchError;
    use std::num::NonZeroU32;
    let lost = ImapClientError::Io(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
    assert_eq!(super::watch_error_label(&lost), "connection lost");
    let changed = ImapClientError::MailboxWatch(ImapMailboxWatchError::UidValidityChanged {
        known: NonZeroU32::new(4).unwrap(),
        seen: NonZeroU32::new(5).unwrap(),
    });
    assert_eq!(
        super::watch_error_label(&changed),
        "mailbox UIDVALIDITY changed"
    );
}
