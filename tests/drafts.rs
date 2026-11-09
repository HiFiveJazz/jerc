use jerc::app::Composer;
use jerc::outgoing::drafts::{delete_draft, load_draft, save_draft};

#[test]
fn postponed_draft_round_trip_preserves_reply_and_forward_context() {
    let temporary = tempfile::tempdir().unwrap();
    let mut draft = Composer::new();
    draft.to = "list@example.org".into();
    draft.bcc = "secret@example.org".into();
    draft.body = "quoted\n> patch\n".into();
    draft.in_reply_to = Some("parent@example.org".into());
    draft.references = vec!["root@example.org".into(), "parent@example.org".into()];
    draft.forwarded_raw = Some(b"From: a@example.org\r\n\r\n<b>original</b>".to_vec());
    save_draft(temporary.path(), "work", &draft).unwrap();
    let restored = load_draft(temporary.path(), "work").unwrap().unwrap();
    assert_eq!(restored.to, draft.to);
    assert_eq!(restored.bcc, draft.bcc);
    assert_eq!(restored.in_reply_to, draft.in_reply_to);
    assert_eq!(restored.references, draft.references);
    assert_eq!(restored.forwarded_raw, draft.forwarded_raw);
    delete_draft(temporary.path(), "work").unwrap();
    assert!(load_draft(temporary.path(), "work").unwrap().is_none());
}

#[test]
fn postponement_is_isolated_by_account_and_replaces_old_version() {
    let temporary = tempfile::tempdir().unwrap();
    let mut first = Composer::new();
    first.subject = "first".into();
    save_draft(temporary.path(), "personal@example.org", &first).unwrap();
    assert!(
        load_draft(temporary.path(), "work@example.org")
            .unwrap()
            .is_none()
    );
    first.subject = "second".into();
    save_draft(temporary.path(), "personal@example.org", &first).unwrap();
    assert_eq!(
        load_draft(temporary.path(), "personal@example.org")
            .unwrap()
            .unwrap()
            .subject,
        "second"
    );
}
