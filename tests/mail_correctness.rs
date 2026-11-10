//! Wire-format regression tests. These are intentionally transport-free: the
//! exact bytes handed to SMTP and appended to Sent are verified before any I/O.
use jerc::model::{OutgoingAttachment, OutgoingMessage};
use jerc::outgoing::{ReplyAudience, build_outgoing, reply_recipients, reply_subject};
use mail_parser::MessageParser;

fn draft() -> OutgoingMessage {
    OutgoingMessage {
        from: "Sender <sender@example.org>".into(),
        to: "Recipient <recipient@example.org>".into(),
        cc: String::new(),
        bcc: String::new(),
        subject: "[PATCH v2 2/3] Introduce ünicode".into(),
        body: "héllo\n".into(),
        html_body: None,
        in_reply_to: None,
        references: vec![],
        forwarded_raw: None,
        attachments: vec![],
        custom_headers: vec![],
    }
}

fn wire(message: &OutgoingMessage) -> (lettre::Message, Vec<u8>) {
    let built = build_outgoing(message, "fallback@example.org").unwrap();
    let bytes = built.formatted();
    (built, bytes)
}

#[test]
fn outgoing_message_has_unique_id_and_utf8_headers() {
    let first = draft();
    let (_, bytes) = wire(&first);
    let parsed = MessageParser::new().parse(&bytes).unwrap();
    assert!(parsed.message_id().is_some());
    assert_eq!(
        parsed.subject().unwrap(),
        "[PATCH v2 2/3] Introduce ünicode"
    );
    assert!(String::from_utf8_lossy(&bytes).contains("Date:"));
    let (_, second) = wire(&first);
    let parsed2 = MessageParser::new().parse(&second).unwrap();
    assert_ne!(parsed.message_id(), parsed2.message_id());
}

#[test]
fn plain_mail_has_minimal_utf8_mime_without_html_or_client_branding() {
    let (_, raw) = wire(&draft());
    let headers = String::from_utf8_lossy(&raw);
    let headers = headers
        .split("\r\n\r\n")
        .next()
        .unwrap()
        .to_ascii_lowercase();
    assert!(headers.contains("mime-version: 1.0"));
    assert!(headers.contains("content-type: text/plain; charset=utf-8"));
    assert_eq!(headers.matches("mime-version:").count(), 1);
    assert_eq!(headers.matches("content-type:").count(), 1);
    assert!(headers.contains("content-transfer-encoding:"));
    assert!(!headers.contains("multipart/alternative"));
    assert!(!headers.contains("text/html"));
    assert!(!headers.contains("user-agent:"));
    assert!(!headers.contains("x-mailer:"));
    let parsed = MessageParser::new().parse(&raw).expect("valid MIME mail");
    assert_eq!(parsed.text_body().count(), 1);
    // mail-parser exposes a plain-text fallback through html_body() when an
    // actual HTML alternative is absent. Check the underlying part kind, not
    // the fallback iterator's length.
    assert!(
        parsed
            .html_body()
            .all(|part| !matches!(part.kind(), mail_parser::PartKind::Html))
    );
    assert!(
        parsed
            .text_body()
            .any(|part| part.text().is_some_and(|text| text.contains("héllo")))
    );
}

#[test]
fn reply_with_ascii_body_is_still_plain_utf8_mime() {
    let mut message = draft();
    message.subject = "Re: [PATCH] small fix".into();
    message.body = "On Mon, somebody wrote:\n> Test\n\nLooks good.\n".into();
    message.in_reply_to = Some("original@example.org".into());
    let (_, raw) = wire(&message);
    let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    assert!(text.contains("content-type: text/plain; charset=utf-8"));
    assert!(text.contains("in-reply-to: <original@example.org>"));
    assert!(!text.contains("text/html"));
}

#[test]
fn bcc_envelope_contains_secret_recipient_but_header_does_not() {
    let mut draft = draft();
    draft.bcc = "Secret <secret@example.net>".into();
    let (built, bytes) = wire(&draft);
    let recipients = built
        .envelope()
        .to()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert!(
        recipients
            .iter()
            .any(|x| x.eq_ignore_ascii_case("secret@example.net"))
    );
    assert!(
        !String::from_utf8_lossy(&bytes)
            .to_ascii_lowercase()
            .contains("secret@example.net")
    );
}

#[test]
fn reply_references_and_thread_parent_survive_serialization() {
    let mut draft = draft();
    draft.in_reply_to = Some("parent@example.org".into());
    draft.references = vec!["root@example.org".into(), "parent@example.org".into()];
    let (_, bytes) = wire(&draft);
    let headers = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    assert!(headers.contains("in-reply-to: <parent@example.org>"));
    assert!(headers.contains("references: <root@example.org> <parent@example.org>"));
}

#[test]
fn mime_alternative_and_multiple_attachments_have_correct_hierarchy() {
    let mut draft = draft();
    draft.html_body = Some("<p>héllo</p>".into());
    draft.attachments = vec![
        OutgoingAttachment {
            filename: "résumé.txt".into(),
            content_type: "text/plain".into(),
            data: b"alpha".to_vec(),
        },
        OutgoingAttachment {
            filename: "second.bin".into(),
            content_type: "application/octet-stream".into(),
            data: vec![0, 1, 2, 255],
        },
    ];
    let (_, bytes) = wire(&draft);
    let raw = String::from_utf8_lossy(&bytes);
    assert!(raw.contains("multipart/mixed"));
    assert!(raw.contains("multipart/alternative"));
    let parsed = MessageParser::new().parse(&bytes).unwrap();
    let parts = parsed.attachments().collect::<Vec<_>>();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].attachment_name(), Some("résumé.txt"));
    assert_eq!(parts[0].decoded().as_ref(), b"alpha");
    assert_eq!(parts[1].decoded().as_ref(), &[0, 1, 2, 255]);
}

#[test]
fn attached_forward_preserves_original_bytes_not_renderer_text() {
    let mut draft = draft();
    let original =
        b"Message-ID: <original@kernel.org>\r\nContent-Type: text/html\r\n\r\n<b>RAW HTML</b>\r\n";
    draft.forwarded_raw = Some(original.to_vec());
    let (_, bytes) = wire(&draft);
    let parsed = MessageParser::new().parse(&bytes).unwrap();
    let raw = String::from_utf8_lossy(&bytes);
    assert!(raw.contains("message/rfc822"));
    let attached = parsed.attachments().collect::<Vec<_>>();
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].decoded().as_ref(), original);
    assert_eq!(
        attached[0]
            .nested()
            .expect("nested original MIME message")
            .message_id(),
        Some("original@kernel.org")
    );
    // MIME encapsulation includes original body, even when the user sees a rich rendered projection.
    assert!(raw.contains("forwarded.eml"));
}

#[test]
fn no_duplicate_reply_prefix_and_patch_series_preserved() {
    assert_eq!(
        reply_subject("Re: [PATCH v2 2/3] foo"),
        "Re: [PATCH v2 2/3] foo"
    );
    assert_eq!(
        reply_subject("[PATCH v2 2/3] foo"),
        "Re: [PATCH v2 2/3] foo"
    );
}

#[test]
fn reply_to_reply_all_and_list_reply_are_distinct() {
    let raw = b"From: Developer <dev@example.org>\r\nReply-To: Maintainer <maintainer@example.org>\r\nTo: Me <me@example.org>, Team <team@example.org>\r\nCc: Reviewer <reviewer@example.org>, Team <TEAM@example.org>\r\nList-Post: <mailto:linux-kernel@vger.kernel.org>\r\n\r\nbody";
    let own = ["me@example.org".to_string()];
    let reply = reply_recipients(raw, ReplyAudience::Sender, &own).unwrap();
    assert!(reply.to.contains("maintainer@example.org"));
    assert!(!reply.to.contains("dev@example.org"));
    let all = reply_recipients(raw, ReplyAudience::All, &own).unwrap();
    assert!(all.to.contains("maintainer@example.org"));
    assert!(all.cc.contains("team@example.org"));
    assert!(all.cc.contains("reviewer@example.org"));
    assert!(!all.cc.to_lowercase().contains("me@example.org"));
    assert_eq!(all.cc.to_lowercase().matches("team@example.org").count(), 1);
    let list = reply_recipients(raw, ReplyAudience::List, &own).unwrap();
    assert_eq!(list.to, "linux-kernel@vger.kernel.org");
    assert!(list.cc.is_empty());
}

#[test]
fn custom_headers_cannot_override_delivery_or_inject_newlines() {
    let mut message = draft();
    message.custom_headers = vec![("X-Patchwork-Id".into(), "1234".into())];
    let (_, raw) = wire(&message);
    assert!(String::from_utf8_lossy(&raw).contains("X-Patchwork-Id: 1234"));
    message.custom_headers = vec![("Bcc".into(), "leak@example.org".into())];
    assert!(build_outgoing(&message, "fallback@example.org").is_err());
    message.custom_headers = vec![("X-Test".into(), "ok\r\nBcc: leak@example.org".into())];
    assert!(build_outgoing(&message, "fallback@example.org").is_err());
}

#[test]
fn list_post_no_does_not_fall_back_to_reply_all() {
    let raw = b"From: author@example.org\r\nList-Post: NO\r\n\r\nbody";
    assert!(reply_recipients(raw, ReplyAudience::List, &[]).is_err());
}

#[test]
fn missing_subject_and_missing_attachment_are_send_warnings() {
    let mut message = draft();
    message.subject.clear();
    message.body = "Please find the attachment included".into();
    let warnings = jerc::outgoing::send_warnings(&message);
    assert!(warnings.contains(&"Subject is empty"));
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("attachment"))
    );
}

#[test]
fn sending_a_reply_to_yourself_is_supported_but_reply_all_excludes_self() {
    let raw = b"From: Me <me@example.org>\r\nTo: Friend <friend@example.org>\r\n\r\nhello";
    let own = ["me@example.org".to_string()];
    let reply = reply_recipients(raw, ReplyAudience::Sender, &own).unwrap();
    assert_eq!(reply.to, "me@example.org");
    let all = reply_recipients(raw, ReplyAudience::All, &own).unwrap();
    assert_eq!(all.to, "friend@example.org");
    assert!(all.cc.is_empty());
}
