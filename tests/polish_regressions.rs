//! Mail/UI correctness tests for forwarding and attachment handling.
use jerc::mail::{extract_attachments, parse_body, parse_summary};
use jerc::model::{MessageFlags, MessageId};
use jerc::outgoing::{build_outgoing, inline_forward_parts};

const MIME_FIXTURE: &[u8] = b"From: Maintainer <maintainer@example.org>\r\nTo: dev@example.org\r\nSubject: [PATCH 1/2] Test\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n--outer\r\nContent-Type: multipart/alternative; boundary=alt\r\n\r\n--alt\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nPlain patch body\r\n--alt\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><strong>Rich patch body</strong></body></html>\r\n--alt--\r\n--outer\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQK\r\n--outer--\r\n";

#[test]
fn inline_forward_keeps_html_and_original_file_but_never_attaches_eml() {
    let parts = inline_forward_parts(MIME_FIXTURE).unwrap();
    assert!(parts.plain.contains("Plain patch body"));
    assert!(parts.html.as_deref().unwrap().contains("Rich patch body"));
    assert_eq!(parts.attachments.len(), 1);
    assert_eq!(parts.attachments[0].filename, "report.pdf");
    let message = jerc::model::OutgoingMessage {
        from: "dev@example.org".into(),
        to: "recipient@example.org".into(),
        cc: String::new(),
        bcc: String::new(),
        subject: "Fwd: [PATCH 1/2] Test".into(),
        body: parts.plain,
        html_body: parts.html,
        in_reply_to: None,
        references: vec![],
        forwarded_raw: None,
        attachments: parts.attachments,
        custom_headers: vec![],
    };
    let wire = build_outgoing(&message, "dev@example.org")
        .unwrap()
        .formatted();
    let decoded = mail_parser::MessageParser::new().parse(&wire).unwrap();
    assert!(String::from_utf8_lossy(&wire).contains("multipart/alternative"));
    assert!(!String::from_utf8_lossy(&wire).contains("message/rfc822"));
    assert_eq!(decoded.attachments().count(), 1);
    assert_eq!(
        decoded.attachments().next().unwrap().attachment_name(),
        Some("report.pdf")
    );
}

#[test]
fn attachment_extraction_preserves_decoded_bytes_and_safe_names() {
    let list = extract_attachments(MIME_FIXTURE).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].filename, "report.pdf");
    assert_eq!(list[0].data, b"%PDF-1.4\n");
}

#[test]
fn mixed_and_related_html_without_files_does_not_claim_an_attachment() {
    // HEYTEA-style mail: multipart/mixed contains only multipart/related
    // with a text/html body and remote <img> URLs, no MIME file parts.
    let raw = b"From: A <a@example.org>\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n--outer\r\nContent-Type: multipart/related; boundary=inner\r\n\r\n--inner\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><img src=\"https://example.org/product.png\"></html>\r\n--inner--\r\n--outer--\r\n";
    let id = MessageId("html-no-files".into());
    let summary = parse_summary(id.clone(), raw, raw.len() as u64, MessageFlags::empty()).unwrap();
    assert!(
        !summary.has_attachment_hint,
        "multipart/mixed does not prove a file is attached"
    );
    assert!(!summary.has_attachment(None));
    let body = parse_body(id, raw).unwrap();
    assert_eq!(body.attachment_count, 0);
    assert!(body.attachment_names.is_empty());
    assert!(!summary.has_attachment(Some(&body)));
}

#[test]
fn actual_attachment_count_overrides_stale_header_hint() {
    let raw = b"From: A <a@example.org>\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n";
    let mut summary = parse_summary(
        MessageId("stale-hint".into()),
        raw,
        123,
        MessageFlags::empty(),
    )
    .unwrap();
    // Regression: old cached summaries may still have a speculative hint.
    summary.has_attachment_hint = true;
    let body = parse_body(
        MessageId("stale-hint".into()),
        b"From: A <a@example.org>\r\nContent-Type: text/html\r\n\r\n<html>Just text</html>",
    )
    .unwrap();
    assert!(!summary.has_attachment(Some(&body)));
    assert!(summary.has_attachment(None));
}

#[test]
fn real_mime_attachment_is_shown_after_body_load() {
    let id = MessageId("with-file".into());
    let summary = parse_summary(
        id.clone(),
        MIME_FIXTURE,
        MIME_FIXTURE.len() as u64,
        MessageFlags::empty(),
    )
    .unwrap();
    assert!(
        !summary.has_attachment_hint,
        "top-level mixed is not a reliable indicator"
    );
    let body = parse_body(id, MIME_FIXTURE).unwrap();
    assert_eq!(body.attachment_count, 1);
    assert!(summary.has_attachment(Some(&body)));
}

#[test]
fn explicit_top_level_attachment_disposition_is_a_header_hint() {
    let raw = b"From: A <a@example.org>\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\n\r\n";
    let summary = parse_summary(MessageId("pdf".into()), raw, 123, MessageFlags::empty()).unwrap();
    assert!(summary.has_attachment_hint);
    assert!(summary.has_attachment(None));
}

#[test]
fn attachment_hint_ignores_nested_mime_disposition_in_full_message() {
    // Some mail backends pass full RFC 5322 bytes to parse_summary rather
    // than the header block alone. Only the outer headers are list hints.
    let raw = b"From: A <a@example.org>\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n--outer\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\n\r\n%PDF\r\n--outer--\r\n";
    let summary = parse_summary(
        MessageId("nested-file".into()),
        raw,
        raw.len() as u64,
        MessageFlags::empty(),
    )
    .unwrap();
    assert!(!summary.has_attachment_hint);
    let body = parse_body(MessageId("nested-file".into()), raw).unwrap();
    assert_eq!(body.attachment_count, 1);
    assert!(summary.has_attachment(Some(&body)));
}

#[test]
fn attachment_hint_accepts_lf_separated_top_level_disposition_only() {
    let raw = b"From: A <a@example.org>\nContent-Disposition: attachment; filename=report.pdf\n\nContent-Disposition: inline\n";
    let summary = parse_summary(
        MessageId("top-level-file".into()),
        raw,
        raw.len() as u64,
        MessageFlags::empty(),
    )
    .unwrap();
    assert!(summary.has_attachment_hint);
}

#[test]
fn forwarded_html_inserts_note_into_body_without_dropping_original_style() {
    let html = "<html><head><style>strong{color:red}</style></head><body><strong>Hi</strong></body></html>";
    let result = jerc::outgoing::forward_html(html, "From: Dev <dev@example.org>", "Please review");
    assert!(result.contains("<style>strong{color:red}</style>"));
    assert!(result.contains("<strong>Hi</strong>"));
    assert!(result.contains("&lt;dev@example.org&gt;"));
    assert!(result.find("Please review").unwrap() < result.find("<strong>Hi</strong>").unwrap());
}

#[test]
fn gmail_style_forward_attribution_preserves_origin_headers() {
    let forward = jerc::outgoing::forward_attribution(
        b"From: Yuzu Keycaps <contact@example.test>\r\nTo: James <james@example.test>\r\nCc: Team <team@example.test>\r\nDate: Fri, 23 Oct 2026 14:02:00 -0700\r\nSubject: 10% off ends Saturday\r\n\r\nHello",
    ).unwrap();
    assert!(forward.starts_with("---------- Forwarded message ---------\n"));
    assert!(forward.contains("From: Yuzu Keycaps <contact@example.test>\n"));
    assert!(forward.contains("Date: "));
    assert!(forward.contains("Subject: 10% off ends Saturday\n"));
    assert!(forward.contains("To: James <james@example.test>\n"));
    assert!(forward.contains("Cc: Team <team@example.test>\n"));
    assert!(!forward.contains("Bcc:"));
}

const REACTION_WITHOUT_FILES: &[u8] = b"From: Someone <someone@example.org>\r\nSubject: Reaction\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=react\r\n\r\n--react\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nLiked your email\r\n--react\r\nContent-Type: text/vnd.google.email-reaction+json; charset=utf-8\r\n\r\n{\"emoji\":\"smile\",\"version\":1}\r\n--react\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Liked your email</p>\r\n--react--\r\n";

#[test]
fn gmail_reaction_metadata_is_not_a_downloadable_attachment() {
    let files = extract_attachments(REACTION_WITHOUT_FILES).unwrap();
    assert!(
        files.is_empty(),
        "reaction JSON is MIME metadata, not a user file"
    );
    let body = parse_body(MessageId("reaction".into()), REACTION_WITHOUT_FILES).unwrap();
    assert_eq!(body.attachment_count, 0);
    assert!(body.attachment_names.is_empty());
}

#[test]
fn gmail_reaction_does_not_hide_real_attachments() {
    let raw = b"From: Someone <someone@example.org>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nHello\r\n--x\r\nContent-Type: text/vnd.google.email-reaction+json\r\n\r\n{\"emoji\":\"smile\"}\r\n--x\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQK\r\n--x--\r\n";
    let files = extract_attachments(raw).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].filename, "report.pdf");
    let body = parse_body(MessageId("reaction-and-attachment".into()), raw).unwrap();
    assert_eq!(body.attachment_count, 1);
}

#[test]
fn explicitly_attached_reaction_json_is_still_available() {
    let raw = b"From: Someone <someone@example.org>\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nHello\r\n--x\r\nContent-Type: text/vnd.google.email-reaction+json\r\nContent-Disposition: attachment; filename=reaction.json\r\n\r\n{\"emoji\":\"smile\"}\r\n--x--\r\n";
    let files = extract_attachments(raw).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].filename, "reaction.json");
}
