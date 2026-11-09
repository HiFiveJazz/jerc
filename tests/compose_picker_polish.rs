use jerc::model::OutgoingMessage;
use jerc::outgoing::{build_outgoing, validate_recipients};
use mail_parser::MessageParser;

fn draft() -> OutgoingMessage {
    OutgoingMessage {
        from: "sender@example.org".into(),
        to: "person@example.com".into(),
        cc: String::new(),
        bcc: String::new(),
        subject: "hello".into(),
        body: "test".into(),
        html_body: None,
        attachments: vec![],
        custom_headers: vec![],
        in_reply_to: None,
        references: vec![],
        forwarded_raw: None,
    }
}

#[test]
fn incomplete_recipient_domains_are_rejected_at_the_wire_boundary() {
    assert!(validate_recipients("jasmeet.bhaita.us@gmail", "", "").is_err());
    assert!(validate_recipients("ok@gmail.com", "other@example", "").is_err());
    assert!(validate_recipients("ok@gmail.com", "", "").is_ok());
    let mut outgoing = draft();
    outgoing.bcc = "not-complete@gmail".into();
    assert!(build_outgoing(&outgoing, "sender@example.org").is_err());
}

#[test]
fn generated_message_ids_use_sender_domain_and_remain_unique() {
    let outgoing = draft();
    let a = build_outgoing(&outgoing, "sender@example.org")
        .unwrap()
        .formatted();
    let b = build_outgoing(&outgoing, "sender@example.org")
        .unwrap()
        .formatted();
    let a = MessageParser::new().parse(&a).unwrap();
    let b = MessageParser::new().parse(&b).unwrap();
    let id_a = a.message_id().unwrap();
    assert!(id_a.ends_with("@example.org"));
    assert_ne!(Some(id_a), b.message_id());
}
