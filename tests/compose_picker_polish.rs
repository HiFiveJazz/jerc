use jerc::contacts::{Contact, replace_recipient};
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
fn autocomplete_to_single_self_contact_can_be_previewed_and_sent() {
    let self_contact = Contact {
        name: "Jasmeet (Japan) Bhatia".into(),
        email: "burner@example.org".into(),
        saved: true,
        favorite: false,
    };
    let mut recipient = "burner".to_owned();
    let cursor = recipient.chars().count();
    replace_recipient(&mut recipient, cursor, &self_contact);
    assert_eq!(
        recipient,
        "\"Jasmeet (Japan) Bhatia\" <burner@example.org>, "
    );
    validate_recipients(&recipient, "", "").expect("autocomplete To should validate");
    let mut outgoing = draft();
    outgoing.to = recipient;
    let built = build_outgoing(&outgoing, "sender@example.org").expect("outgoing To");
    assert_eq!(built.envelope().to().len(), 1);
    assert_eq!(built.envelope().to()[0].to_string(), "burner@example.org");
}

#[test]
fn autocomplete_multiple_self_addresses_and_cc_bcc_are_supported() {
    let contacts = [
        Contact {
            name: "Jasmeet (Jazz) Bhatia".into(),
            email: "main@example.org".into(),
            saved: true,
            favorite: false,
        },
        Contact {
            name: "Jasmeet (Japan) Bhatia".into(),
            email: "burner@example.org".into(),
            saved: true,
            favorite: false,
        },
        Contact {
            name: "Doe, Jane".into(),
            email: "copy@example.net".into(),
            saved: true,
            favorite: false,
        },
    ];
    let mut to = String::new();
    for contact in &contacts[..2] {
        let cursor = to.chars().count();
        replace_recipient(&mut to, cursor, contact);
    }
    let mut cc = String::new();
    replace_recipient(&mut cc, 0, &contacts[2]);
    let bcc = "hidden@example.com, ";
    validate_recipients(&to, &cc, bcc).expect("all autocomplete recipients valid");
    let mut outgoing = draft();
    outgoing.to = to;
    outgoing.cc = cc;
    outgoing.bcc = bcc.to_owned();
    let built = build_outgoing(&outgoing, "sender@example.org").expect("message builder");
    assert_eq!(built.envelope().to().len(), 4);
    let wire = String::from_utf8_lossy(&built.formatted()).to_ascii_lowercase();
    assert!(
        !wire.contains("hidden@example.com"),
        "Bcc must not leak into headers"
    );
}

#[test]
fn only_one_unfinished_autocomplete_separator_is_ignored() {
    for valid in ["one@example.org, ", "one@example.org, two@example.net, "] {
        validate_recipients(valid, "", "").expect("one terminal autocomplete separator");
    }
    for invalid in [", ", "one@example.org,, ", "one@example"] {
        assert!(validate_recipients(invalid, "", "").is_err(), "{invalid:?}");
    }
}

#[test]
fn generated_message_ids_use_sender_domain_and_remain_unique() {
    let outgoing = draft();
    let raw_a = build_outgoing(&outgoing, "sender@example.org")
        .unwrap()
        .formatted();
    let raw_b = build_outgoing(&outgoing, "sender@example.org")
        .unwrap()
        .formatted();
    let a = MessageParser::new().parse(&raw_a).unwrap();
    let b = MessageParser::new().parse(&raw_b).unwrap();
    let id_a = a.message_id().unwrap();
    assert!(id_a.ends_with("@example.org"));
    let token = id_a.split('@').next().unwrap();
    assert_eq!(token.len(), 32, "128-bit opaque random token");
    assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(!token.contains("jerc"));
    assert_ne!(Some(id_a), b.message_id());
    for raw in [&raw_a, &raw_b] {
        let headers = String::from_utf8_lossy(raw);
        assert!(!headers.to_ascii_lowercase().contains("user-agent:"));
        assert!(!headers.to_ascii_lowercase().contains("x-mailer:"));
    }
}
