use aerc_rs::mail::parse_summary;
use aerc_rs::model::{MessageFlags, MessageId};

#[test]
fn recipient_headers_are_retained_for_reader_without_inventing_bcc() {
    let raw = b"From: Sender <sender@example.test>\r\nTo: James <james@example.test>\r\nCc: Jazz <jazz@example.test>\r\nDate: Fri, 23 Oct 2026 14:02:00 -0700\r\nSubject: Check\r\n\r\nHello";
    let summary = parse_summary(MessageId("1".into()), raw, raw.len() as u64, MessageFlags::empty()).unwrap();
    assert!(summary.to.contains("james@example.test"));
    assert!(summary.cc.contains("jazz@example.test"));
    assert!(summary.bcc.is_empty(), "Bcc must not be fabricated in received mail");
    assert!(summary.timestamp > 0);
}

#[test]
fn original_bcc_header_is_displayable_when_actually_present() {
    let raw = b"From: Sender <sender@example.test>\r\nTo: James <james@example.test>\r\nBcc: Archive <archive@example.test>\r\n\r\nHello";
    let summary = parse_summary(MessageId("2".into()), raw, raw.len() as u64, MessageFlags::empty()).unwrap();
    assert!(summary.bcc.contains("archive@example.test"));
}
