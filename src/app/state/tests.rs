use super::{
    ComposeField, ComposeMode, Composer, ReaderPosition, ReaderSelection, ReaderWordMotion,
    SMOOTH_SCROLL_ROWS, bounded_index, inner_text_object_range, message_view_offset,
    offset_index, preview_neighbor_positions, preview_travel_positions,
    reader_image_target_size, reader_scroll_for_visual_cursor, reader_selection_text,
    reader_word_target, reply_attribution, url_at_cursor, Focus, PendingDraft,
};
use crate::contacts::{ContactEdit, ContactsModal};
use crate::model::{MessageFlags, MessageId, MessageSummary};
use ratatui::layout::Size;
use ratatui::widgets::BorderType;
use std::path::PathBuf;

#[test]
fn quoted_diff_mode_toggles_without_changing_the_message() {
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    assert!(!app.quoted_diff_highlighting);
    app.handle_command(super::Command::ToggleQuotedDiff);
    assert!(app.quoted_diff_highlighting);
    app.handle_command(super::Command::ToggleQuotedDiff);
    assert!(!app.quoted_diff_highlighting);
}

#[test]
fn compose_shift_c_toggles_diff_mode_without_editing_text_or_insert_key() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut composer = Composer::new();
    composer.body = "> old text".into();
    composer.switch_field(ComposeField::Body);
    composer.mode = ComposeMode::Normal;
    app.composer = Some(composer);
    let shift_c = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT);
    app.handle_composer_key(shift_c);
    assert!(app.quoted_diff_highlighting);
    assert_eq!(app.composer.as_ref().unwrap().body, "> old text");
    app.handle_composer_key(shift_c);
    assert!(!app.quoted_diff_highlighting);
    app.composer.as_mut().unwrap().mode = ComposeMode::Insert;
    app.composer.as_mut().unwrap().cursor = 0;
    app.handle_composer_key(shift_c);
    assert!(!app.quoted_diff_highlighting);
    assert!(app.composer.as_ref().unwrap().body.starts_with('C'));
}

#[test]
fn unverified_cached_mail_never_appears_as_live_mail() {
    use std::sync::mpsc::sync_channel;
    use crate::worker::WorkerHandle;
    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = sync_channel(8);
    let worker = WorkerHandle::spawn(
        0,
        Box::new(crate::backend::MaildirBackend::new(root.path().into())),
        None,
        None,
        tx,
    );
    let config = crate::config::Config::single_maildir(
        "Local".into(), root.path().to_path_buf()
    );
    let mut app = super::App::new(&config, vec![worker]);
    let raw = b"From: Example <example@example.org>\r\nSubject: Already deleted\r\n\r\nold";
    let message = crate::mail::parse_summary(
        MessageId("old-uid".into()), raw, raw.len() as u64, MessageFlags::empty()
    ).unwrap();
    let account = app.account_mut().unwrap();
    account.current_mailbox = Some("INBOX".into());
    account.messages = vec![message];
    account.unverified_mailboxes.insert("INBOX".into());
    assert!(account.cached_mailbox_unverified());
    assert!(account.visible_message_indices().is_empty());
    assert!(account.selected_message().is_none());
    // A first-page server response marks this folder verified, after
    // which its real rows (not disk-only guesses) can be displayed.
    account.unverified_mailboxes.remove("INBOX");
    assert_eq!(account.visible_message_indices(), vec![0]);
    assert_eq!(account.selected_message().unwrap().subject, "Already deleted");
}

#[test]
fn ctrl_a_does_not_open_attachment_browser_for_known_empty_mime() {
    use std::sync::mpsc::sync_channel;
    use crate::worker::WorkerHandle;
    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = sync_channel(8);
    let worker = WorkerHandle::spawn(
        0,
        Box::new(crate::backend::MaildirBackend::new(root.path().into())),
        None,
        None,
        tx,
    );
    let config = crate::config::Config::single_maildir(
        "Local".into(), root.path().to_path_buf()
    );
    let mut app = super::App::new(&config, vec![worker]);
    let raw = b"From: Sender <sender@example.org>\r\nSubject: No files\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello";
    let id = MessageId("empty-attachment-test".into());
    let message = crate::mail::parse_summary(id.clone(), raw, raw.len() as u64, MessageFlags::empty()).unwrap();
    let body = crate::mail::parse_body(id.clone(), raw).unwrap();
    assert_eq!(body.attachment_count, 0);
    let account = app.account_mut().unwrap();
    account.current_mailbox = Some("INBOX".into());
    account.messages = vec![message];
    account.bodies.insert(id.clone(), body);
    account.unverified_mailboxes.remove("INBOX");
    app.open_attachment_picker();
    assert!(!app.attachment_picker_visible);
    assert!(app.attachment_pending.is_none());
    assert_eq!(app.status.text, "no attachments to preview");

    // If the MIME body is not cached, the worker checks it without
    // opening a blank popup. An empty result leaves the reader visible.
    app.account_mut().unwrap().bodies.clear();
    app.open_attachment_picker();
    assert!(!app.attachment_picker_visible);
    let request_id = app.attachment_pending_request_id.expect("attachment check queued");
    app.handle_worker_event(crate::worker::WorkerEvent {
        account: 0,
        request_id,
        kind: crate::worker::WorkerEventKind::AttachmentsLoaded {
            mailbox: "INBOX".into(),
            id,
            attachments: Vec::new(),
        },
    });
    assert!(!app.attachment_picker_visible);
    assert!(app.attachment_pending.is_none());
    assert_eq!(app.status.text, "no attachments to preview");
}

#[test]
fn shared_cut_writes_clipboard_and_keeps_register_on_clipboard_failure() {
    let mut register = None;
    let mut clipboard_text = String::new();
    let status = super::record_cut_with(&mut register, "😊text".into(), false, |text| {
        clipboard_text = text.to_owned();
        Ok(())
    });
    assert!(status.is_some());
    assert_eq!(clipboard_text, "😊text");
    assert_eq!(register.as_ref().unwrap().text, "😊text");

    let status = super::record_cut_with(&mut register, "other".into(), true, |_text| {
        Err(std::io::Error::other("clipboard unavailable"))
    });
    assert!(status.is_some());
    assert_eq!(register.as_ref().unwrap().text, "other");
    assert!(register.as_ref().unwrap().linewise);

    let mut clipboard_called = false;
    assert!(super::record_cut_with(&mut register, String::new(), false, |_text| {
        clipboard_called = true;
        Ok(())
    }).is_none());
    assert!(!clipboard_called);
    assert_eq!(register.as_ref().unwrap().text, "other");
}

#[test]
fn compose_normal_x_cuts_to_register_in_every_header_and_body() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);

    for field in [
        ComposeField::To,
        ComposeField::Cc,
        ComposeField::Bcc,
        ComposeField::Subject,
        ComposeField::Body,
    ] {
        let mut composer = Composer::new();
        composer.switch_field(field);
        *composer.field_mut() = "a😊bc".into();
        composer.mode = ComposeMode::Normal;
        composer.cursor = 1;
        app.composer = Some(composer);
        app.yank_register = None;

        // Counted x uses Unicode character offsets, not UTF-8 bytes.
        app.handle_composer_key(key('2'));
        app.handle_composer_key(key('x'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "ac", "{field:?}");
        assert_eq!(app.yank_register.as_ref().unwrap().text, "😊b", "{field:?}");
        assert!(!app.yank_register.as_ref().unwrap().linewise);

        app.handle_composer_key(key('u'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "a😊bc");
        app.composer.as_mut().unwrap().cursor = 1;
        app.handle_composer_key(key('x'));
        assert_eq!(app.yank_register.as_ref().unwrap().text, "😊");
        app.handle_composer_key(key('P'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "a😊bc");

        // Reaching end of a field must not erase the previous register.
        let len = app.composer.as_ref().unwrap().char_count();
        app.composer.as_mut().unwrap().cursor = len;
        app.handle_composer_key(key('x'));
        assert_eq!(app.yank_register.as_ref().unwrap().text, "😊");
    }
}

#[test]
fn compose_visual_x_cuts_selected_unicode_in_every_field_and_can_undo() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);

    for field in [
        ComposeField::To,
        ComposeField::Cc,
        ComposeField::Bcc,
        ComposeField::Subject,
        ComposeField::Body,
    ] {
        let mut composer = Composer::new();
        composer.switch_field(field);
        *composer.field_mut() = "a😊bc".into();
        composer.mode = ComposeMode::Normal;
        composer.cursor = 1;
        app.composer = Some(composer);
        app.yank_register = None;

        app.handle_composer_key(key('v'));
        app.handle_composer_key(key('l'));
        app.handle_composer_key(key('x'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "ac", "{field:?}");
        assert_eq!(app.yank_register.as_ref().unwrap().text, "😊b", "{field:?}");
        assert_eq!(app.composer.as_ref().unwrap().mode, ComposeMode::Normal);
        app.handle_composer_key(key('u'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "a😊bc", "{field:?}");
    }
}

#[test]
fn compose_linewise_visual_x_preserves_register_type_and_normal_delete_does_not_yank() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.mode = ComposeMode::Normal;
    composer.body = "first 😊\nsecond".into();
    composer.cursor = 0;
    app.composer = Some(composer);

    app.handle_composer_key(key('V'));
    app.handle_composer_key(key('x'));
    assert_eq!(app.yank_register.as_ref().unwrap().text, "first 😊\n");
    assert!(app.yank_register.as_ref().unwrap().linewise);
    assert_eq!(app.composer.as_ref().unwrap().body, "second");
    app.handle_composer_key(key('u'));
    assert_eq!(app.composer.as_ref().unwrap().body, "first 😊\nsecond");

    app.composer.as_mut().unwrap().cursor = 0;
    app.handle_composer_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
    assert_eq!(app.composer.as_ref().unwrap().body, "irst 😊\nsecond");
    assert_eq!(app.yank_register.as_ref().unwrap().text, "first 😊\n");

    // x on a newline does not accidentally concatenate adjacent lines.
    app.composer.as_mut().unwrap().cursor = 6;
    app.handle_composer_key(key('x'));
    assert_eq!(app.composer.as_ref().unwrap().body, "irst 😊\nsecond");
    assert_eq!(app.yank_register.as_ref().unwrap().text, "first 😊\n");
}

#[test]
fn compose_and_contacts_share_unicode_word_motions() {
    let mut compose = Composer::new();
    compose.to = "first 😊-second".into();
    compose.cursor = 0;
    compose.move_word_kind(false, true);
    let mut contact = ContactEdit::new(None);
    contact.name = compose.to.clone();
    contact.cursor = 0;
    contact.move_word(true);
    assert_eq!(compose.cursor, contact.cursor);
    compose.move_word_kind(true, true);
    contact.move_word(false);
    assert_eq!(compose.cursor, contact.cursor);
    assert_eq!(contact.cursor, 0);
}

#[test]
fn unrelated_worker_failure_cannot_unlock_or_discard_an_inflight_send() {
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;
    use crate::worker::{WorkerEvent, WorkerEventKind, RuntimeEvent, WorkerHandle};
    let root = tempfile::tempdir().unwrap();
    let (tx, rx) = sync_channel(16);
    let worker = WorkerHandle::spawn(
        0,
        Box::new(crate::backend::MaildirBackend::new(root.path().into())),
        None,
        None, // Fail SMTP deliberately: never reach a network relay.
        tx,
    );
    let config: crate::config::Config = toml::from_str(
        "[[accounts]]\nname = \"Local\"\nsource = \"maildir:///tmp/mail\"\noutgoing = \"sendmail:///usr/sbin/sendmail\"\nfrom = \"sender@example.org\""
    ).unwrap();
    let mut app = super::App::new(&config, vec![worker]);
    let mut composer = Composer::new();
    composer.to = "friend@example.net".into();
    composer.subject = "Don't lose this".into();
    composer.body = "Unsent private message".into();
    app.composer = Some(composer);
    app.send_composer();
    let (account_id, send_id) = app.pending_send.expect("send queued");
    assert_eq!(account_id, 0);
    assert!(app.composer.as_ref().unwrap().sending);
    app.handle_worker_event(WorkerEvent {
        account: 0,
        request_id: send_id.saturating_add(1000),
        kind: WorkerEventKind::Error("unrelated mailbox error".into()),
    });
    assert!(app.composer.as_ref().unwrap().sending, "unrelated error cannot end a send");
    assert_eq!(app.pending_send, Some((account_id, send_id)));
    let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else {
        panic!("expected worker response");
    };
    assert_eq!(event.request_id, send_id);
    app.handle_worker_event(event);
    assert!(app.pending_send.is_none());
    let composer = app.composer.as_ref().expect("unsent composer preserved");
    assert!(!composer.sending);
    assert_eq!(composer.body, "Unsent private message");
    assert_eq!(composer.to, "friend@example.net");
}

#[test]
fn draft_replacement_failure_only_clears_matching_append_request() {
    use std::sync::mpsc::sync_channel;
    use crate::worker::{WorkerEvent, WorkerEventKind, WorkerHandle};
    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = sync_channel(8);
    let worker = WorkerHandle::spawn(
        0, Box::new(crate::backend::MaildirBackend::new(root.path().into())),
        None, None, tx,
    );
    let config: crate::config::Config = toml::from_str(
        "[[accounts]]\nname = \"Local\"\nsource = \"maildir:///tmp/mail\""
    ).unwrap();
    let mut app = super::App::new(&config, vec![worker]);
    let source = super::DraftSource {
        mailbox: "Drafts".into(), id: MessageId("original".into()),
    };
    app.pending_draft_replacements.insert((0, 10), source.clone());
    app.pending_draft_replacements.insert((0, 11), source);
    app.handle_worker_event(WorkerEvent {
        account: 0,
        request_id: 10,
        kind: WorkerEventKind::DraftSaveFailed {
            mailbox: "Drafts".into(), error: "offline".into(),
        },
    });
    assert!(!app.pending_draft_replacements.contains_key(&(0, 10)));
    assert!(app.pending_draft_replacements.contains_key(&(0, 11)));
}

#[test]
fn rounded_borders_apply_to_every_theme() {
    let config: crate::config::Config =
        toml::from_str("[ui]\nborder_style = \"rounded\"").unwrap();
    let app = super::App::new(&config, vec![]);
    assert!(
        app.themes
            .iter()
            .all(|theme| theme.border_type == BorderType::Rounded)
    );
}

#[test]
fn space_marking_a_mail_range_is_idempotent_and_toggleable() {
    let ids = [
        MessageId("1".into()),
        MessageId("2".into()),
        MessageId("3".into()),
    ];
    let mut marked = std::collections::HashSet::new();
    super::toggle_message_marks(&mut marked, &ids[0..1]);
    assert!(marked.contains(&ids[0]));
    super::toggle_message_marks(&mut marked, &ids[0..2]);
    assert_eq!(marked.len(), 2);
    super::toggle_message_marks(&mut marked, &ids[0..2]);
    assert!(marked.is_empty());
    super::toggle_message_marks(&mut marked, &ids);
    assert_eq!(marked.len(), 3);
}

#[test]
fn escape_clears_mail_marks_and_visual_selection_before_closing_reader() {
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    app.message_open = true;
    app.message_marked.insert(MessageId("chosen".into()));
    app.visual_anchor = Some(0);
    app.reader_visual_anchor = Some(ReaderPosition { line: 0, column: 0 });
    app.handle_command(crate::app::Command::Escape);
    assert!(app.message_marked.is_empty());
    assert!(app.visual_anchor.is_none());
    assert!(app.reader_visual_anchor.is_none());
    assert!(app.message_open, "first Escape must keep the reader open");
}

#[test]
fn compose_wrapped_lines_use_visual_jk_and_preserve_draft() {
    use crossterm::event::KeyCode;
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.body = "abcdefghijklmnop\nshort\nabcdefghijklmnop".into();
    composer.body_view_width = 5;
    composer.cursor = 2;
    let original = composer.body.clone();
    assert_eq!(composer.body_rows().len(), 9);
    assert!(composer.apply_motion(KeyCode::Char('j'), 1));
    assert_eq!(composer.cursor, 7, "j moves one wrapped terminal row");
    assert!(composer.apply_motion(KeyCode::Char('j'), 1));
    assert_eq!(composer.cursor, 12);
    assert!(composer.apply_motion(KeyCode::Char('k'), 1));
    assert_eq!(composer.cursor, 7);
    assert_eq!(composer.body, original);
}

#[test]
fn compose_yy_p_p_and_undo_preserve_linewise_unicode() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.mode = ComposeMode::Normal;
    composer.body = "αβ\nsecond\nthird".into();
    composer.cursor = 0;
    app.composer = Some(composer);
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    app.handle_composer_key(key('y'));
    app.handle_composer_key(key('y'));
    assert_eq!(
        app.yank_register.as_ref().unwrap(),
        &super::YankRegister {
            text: "αβ\n".into(),
            linewise: true,
        }
    );
    app.handle_composer_key(key('p'));
    assert_eq!(app.composer.as_ref().unwrap().body, "αβ\nαβ\nsecond\nthird");
    app.handle_composer_key(key('P'));
    assert_eq!(app.composer.as_ref().unwrap().body, "αβ\nαβ\nαβ\nsecond\nthird");
    app.handle_composer_key(key('u'));
    assert_eq!(app.composer.as_ref().unwrap().body, "αβ\nαβ\nsecond\nthird");
    app.handle_composer_key(key('u'));
    assert_eq!(app.composer.as_ref().unwrap().body, "αβ\nsecond\nthird");
}

#[test]
fn compose_visual_yank_and_paste_work_without_system_clipboard() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.mode = ComposeMode::Normal;
    composer.body = "hello world".into();
    composer.cursor = 6;
    app.composer = Some(composer);
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    app.handle_composer_key(key('v'));
    for _ in 0..4 {
        app.handle_composer_key(key('l'));
    }
    app.handle_composer_key(key('y'));
    assert_eq!(app.yank_register.as_ref().unwrap().text, "world");
    assert_eq!(app.composer.as_ref().unwrap().mode, ComposeMode::Normal);
    app.composer.as_mut().unwrap().cursor = 0;
    app.handle_composer_key(key('p'));
    assert_eq!(app.composer.as_ref().unwrap().body, "hworldello world");
    app.composer.as_mut().unwrap().cursor = 1;
    app.handle_composer_key(key('v'));
    app.handle_composer_key(key('l'));
    app.handle_composer_key(key('p'));
    assert_eq!(app.composer.as_ref().unwrap().body, "hworldrldello world");
    assert_eq!(app.yank_register.as_ref().unwrap().text, "world");
    app.handle_composer_key(key('u'));
    assert_eq!(app.composer.as_ref().unwrap().body, "hworldello world");
}

#[test]
fn compose_linewise_paste_at_end_and_counted_yank() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.mode = ComposeMode::Normal;
    composer.body = "one\ntwo\nthree".into();
    composer.cursor = 0;
    app.composer = Some(composer);
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    app.handle_composer_key(key('2'));
    app.handle_composer_key(key('y'));
    app.handle_composer_key(key('y'));
    assert_eq!(app.yank_register.as_ref().unwrap().text, "one\ntwo\n");
    app.composer.as_mut().unwrap().cursor = 8; // 'three' at EOF
    app.handle_composer_key(key('p'));
    assert_eq!(app.composer.as_ref().unwrap().body, "one\ntwo\nthree\none\ntwo");
}

#[test]
fn compose_linewise_paste_into_empty_body_has_no_leading_blank_line() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.mode = ComposeMode::Normal;
    app.composer = Some(composer);
    app.yank_register = Some(super::YankRegister {
        text: "first\n".into(),
        linewise: true,
    });
    let key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
    app.handle_composer_key(key);
    assert_eq!(app.composer.as_ref().unwrap().body, "first");
}

#[test]
fn compose_paste_into_header_keeps_it_single_line_and_register_is_ephemeral() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Subject);
    composer.mode = ComposeMode::Normal;
    composer.subject = "x".into();
    composer.cursor = 0;
    app.composer = Some(composer);
    app.yank_register = Some(super::YankRegister {
        text: "first\nsecond".into(),
        linewise: false,
    });
    let key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
    app.handle_composer_key(key);
    assert_eq!(app.composer.as_ref().unwrap().subject, "xfirst second");
    let stored = serde_json::to_string(app.composer.as_ref().unwrap()).unwrap();
    assert!(!stored.contains("yank_register"));
}

#[test]
fn compose_header_yank_and_paste_work_for_every_address_and_subject_field() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

    for field in [
        ComposeField::To,
        ComposeField::Cc,
        ComposeField::Bcc,
        ComposeField::Subject,
    ] {
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = super::App::new(&config, vec![]);
        app.contacts_autocomplete = true;
        app.contacts
            .upsert("Alex", "alex@example.org", true)
            .unwrap();
        let mut composer = Composer::new();
        composer.switch_field(field);
        *composer.field_mut() = "Alex".into();
        composer.cursor = composer.char_count();
        assert_eq!(composer.mode, ComposeMode::Insert);
        app.composer = Some(composer);

        let is_recipient = field != ComposeField::Subject;
        assert_eq!(
            !app.contact_suggestions().is_empty(),
            is_recipient,
            "autocomplete should be visible only in recipient fields: {field:?}"
        );
        app.handle_composer_key(esc);
        assert_eq!(app.composer.as_ref().unwrap().mode, ComposeMode::Normal);
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "Alex");

        // yy yanks all of a single-line header even when its cursor is
        // at the end after Tab/field navigation.
        app.handle_composer_key(key('y'));
        app.handle_composer_key(key('y'));
        assert_eq!(app.yank_register.as_ref().unwrap().text, "Alex", "{field:?}");
        assert!(!app.yank_register.as_ref().unwrap().linewise);

        // V should select the whole field, not a zero-width selection.
        app.handle_composer_key(key('V'));
        assert_eq!(app.composer.as_ref().unwrap().visual_range(), Some((0, 4)));
        app.handle_composer_key(key('y'));
        assert_eq!(app.yank_register.as_ref().unwrap().text, "Alex", "{field:?}");
        assert_eq!(app.composer.as_ref().unwrap().mode, ComposeMode::Normal);

        // v at the end of a header must select the final character.
        app.handle_composer_key(key('v'));
        app.handle_composer_key(key('y'));
        assert_eq!(app.yank_register.as_ref().unwrap().text, "x", "{field:?}");
        app.handle_composer_key(key('Y'));
        assert_eq!(app.yank_register.as_ref().unwrap().text, "Alex");

        // Paste and undo work in the same field, without changing the
        // register or any of the other Compose fields.
        app.handle_composer_key(key('P'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "AleAlexx", "{field:?}");
        app.handle_composer_key(key('u'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "Alex", "{field:?}");
        app.composer.as_mut().unwrap().cursor = 0;
        app.handle_composer_key(key('p'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "AAlexlex", "{field:?}");
        app.handle_composer_key(key('u'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "Alex");
    }
}

#[test]
fn compose_yank_register_pastes_between_recipient_fields_and_subject() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut composer = Composer::new();
    composer.to = "person@example.org".into();
    composer.mode = ComposeMode::Normal;
    composer.cursor = composer.to.chars().count();
    app.composer = Some(composer);
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);

    app.handle_composer_key(key('Y'));
    assert_eq!(app.yank_register.as_ref().unwrap().text, "person@example.org");
    for field in [ComposeField::Cc, ComposeField::Bcc, ComposeField::Subject] {
        app.composer.as_mut().unwrap().switch_field(field);
        app.handle_composer_key(key('p'));
        assert_eq!(
            app.composer.as_ref().unwrap().field_value(),
            "person@example.org",
            "{field:?}"
        );
    }
    assert_eq!(app.composer.as_ref().unwrap().to, "person@example.org");
    assert_eq!(app.yank_register.as_ref().unwrap().text, "person@example.org");
}

#[test]
fn compose_header_yank_motion_visual_replace_and_multiline_paste_are_safe() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    for field in [
        ComposeField::To,
        ComposeField::Cc,
        ComposeField::Bcc,
        ComposeField::Subject,
    ] {
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = super::App::new(&config, vec![]);
        let mut composer = Composer::new();
        composer.switch_field(field);
        *composer.field_mut() = "Aébc".into();
        composer.cursor = 1;
        composer.mode = ComposeMode::Normal;
        app.composer = Some(composer);

        // y$ uses Unicode character positions, not UTF-8 byte offsets.
        app.handle_composer_key(key('y'));
        app.handle_composer_key(key('$'));
        assert_eq!(app.yank_register.as_ref().unwrap().text, "ébc");
        app.composer.as_mut().unwrap().cursor = 0;
        app.handle_composer_key(key('v'));
        app.handle_composer_key(key('l'));
        app.handle_composer_key(key('p'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "ébcbc", "{field:?}");
        assert_eq!(app.yank_register.as_ref().unwrap().text, "ébc");
        app.handle_composer_key(key('u'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "Aébc");

        // Reader/body yanks may contain newlines. A header paste must
        // normalize those into spaces rather than inject header breaks.
        app.yank_register = Some(super::YankRegister {
            text: "one\ntwo".into(),
            linewise: true,
        });
        app.composer.as_mut().unwrap().cursor = 0;
        app.handle_composer_key(key('P'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "one twoAébc");
        assert!(!app.composer.as_ref().unwrap().field_value().contains('\n'));
        app.handle_composer_key(key('u'));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "Aébc");
    }
}

#[test]
fn compose_neoscroll_advances_one_display_row_per_tick() {
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.body = "x\n".repeat(70);
    composer.body_view_width = 20;
    composer.cursor = 0;
    composer.queue_body_smooth_scroll(SMOOTH_SCROLL_ROWS);
    assert_eq!(composer.body_smooth_target, Some(22));
    composer.advance_body_smooth_scroll();
    assert_eq!(composer.cursor, 2);
    assert!(composer.body_smooth_target.is_some());
    // Repeated Ctrl-j extends the destination from the existing target.
    composer.queue_body_smooth_scroll(SMOOTH_SCROLL_ROWS);
    assert_eq!(composer.body_smooth_target, Some(44));
    for _ in 0..43 {
        composer.advance_body_smooth_scroll();
    }
    assert!(composer.body_smooth_target.is_none());
    assert_eq!(composer.cursor, 88);
    composer.queue_body_smooth_scroll(-SMOOTH_SCROLL_ROWS);
    for _ in 0..22 {
        composer.advance_body_smooth_scroll();
    }
    assert_eq!(composer.cursor, 44);
}

#[test]
fn compose_soft_wrap_neoscroll_does_not_skip_boundaries() {
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.body = "abcdefghijklmnopqrstuvwxyz".into();
    composer.body_view_width = 5;
    composer.cursor = 0;
    composer.queue_body_smooth_scroll(SMOOTH_SCROLL_ROWS);
    for _ in 0..10 {
        composer.advance_body_smooth_scroll();
    }
    assert!(composer.body_smooth_target.is_none());
    assert_eq!(
        super::compose_body_cursor_row(&composer.body_rows(), composer.cursor),
        5
    );
}

#[test]
fn compose_contact_suggestions_accept_control_jk() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    app.contacts_autocomplete = true;
    app.contacts
        .upsert("Alex One", "alex1@example.org", true)
        .unwrap();
    app.contacts
        .upsert("Alex Two", "alex2@example.org", true)
        .unwrap();
    let mut composer = Composer::new();
    composer.to = "Alex".into();
    composer.cursor = 4;
    app.composer = Some(composer);
    assert!(app.contact_suggestions().len() >= 2);
    let control = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    app.handle_composer_key(control('j'));
    assert_eq!(app.composer.as_ref().unwrap().contact_suggestion, 1);
    app.handle_composer_key(control('k'));
    assert_eq!(app.composer.as_ref().unwrap().contact_suggestion, 0);
}

#[test]
fn escape_clears_contacts_marks_without_closing_manager() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut modal = crate::contacts::ContactsModal {
        visual_anchor: Some(0),
        ..Default::default()
    };
    modal.marked.insert("person@example.com".into());
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.handle_contacts_modal_key(&mut modal, escape));
    assert!(modal.marked.is_empty());
    assert!(modal.visual_anchor.is_none());
}

#[test]
fn contacts_help_is_searchable_and_navigable_without_closing_contacts() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut modal = crate::contacts::ContactsModal::default();
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    app.handle_contacts_modal_key(&mut modal, key('?'));
    assert!(modal.help);
    for ch in "favorite".chars() {
        app.handle_contacts_modal_key(&mut modal, key(ch));
    }
    assert_eq!(modal.help_query, "favorite");
    let matches =
        crate::app::ranked_help_matches(crate::app::CONTACT_HELP_ENTRIES, &modal.help_query);
    assert!(!matches.is_empty());
    assert!(
        crate::app::CONTACT_HELP_ENTRIES[matches[0]]
            .description
            .contains("favorite")
    );
    app.handle_contacts_modal_key(&mut modal, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!modal.help);
    assert!(app.contacts_modal.is_none());
}

#[test]
fn contacts_visual_and_space_marks_support_bulk_actions() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    app.contacts_writable = true;
    app.contacts.entries.clear();
    for index in 0..30 {
        app.contacts
            .upsert(
                &format!("Name{index:02}"),
                &format!("name{index:02}@example.org"),
                true,
            )
            .unwrap();
    }
    let mut modal = crate::contacts::ContactsModal {
        matches: app.contacts.filtered(""),
        view_height: 10,
        ..Default::default()
    };
    let key = |ch, modifiers| KeyEvent::new(KeyCode::Char(ch), modifiers);
    app.handle_contacts_modal_key(&mut modal, key('V', KeyModifiers::SHIFT));
    app.handle_contacts_modal_key(&mut modal, key('3', KeyModifiers::NONE));
    app.handle_contacts_modal_key(&mut modal, key('j', KeyModifiers::NONE));
    assert_eq!(modal.visual_range(), Some((0, 3)));
    app.handle_contacts_modal_key(&mut modal, key(' ', KeyModifiers::NONE));
    assert!(modal.visual_range().is_none());
    assert_eq!(modal.marked.len(), 4);
    app.handle_contacts_modal_key(&mut modal, key('p', KeyModifiers::NONE));
    assert_eq!(
        app.contacts.entries.iter().filter(|c| c.favorite).count(),
        4
    );
    assert!(modal.marked.is_empty());
    app.handle_contacts_modal_key(&mut modal, key('v', KeyModifiers::NONE));
    app.handle_contacts_modal_key(&mut modal, key('2', KeyModifiers::NONE));
    app.handle_contacts_modal_key(&mut modal, key('j', KeyModifiers::NONE));
    assert_eq!(modal.action_ids(&app.contacts).len(), 3);
    app.handle_contacts_modal_key(&mut modal, key('d', KeyModifiers::NONE));
    assert!(modal.confirm_delete);
    app.handle_contacts_modal_key(&mut modal, key('y', KeyModifiers::NONE));
    assert_eq!(app.contacts.entries.len(), 27);
    assert!(modal.visual_anchor.is_none());
}

#[test]
fn editing_recipient_resets_autocomplete_dismissal() {
    let mut composer = Composer::new();
    composer.dismissed_contact_query = Some("Ja".into());
    composer.insert_char('J');
    assert!(composer.dismissed_contact_query.is_none());
    composer.dismissed_contact_query = Some("J".into());
    composer.switch_field(ComposeField::Cc);
    assert!(composer.dismissed_contact_query.is_none());
    assert_eq!(composer.contact_suggestion, 0);
    composer.insert_char('J');
    composer.dismissed_contact_query = Some("J".into());
    composer.backspace();
    assert!(composer.dismissed_contact_query.is_none());
}

#[test]
fn mail_search_matches_typos_as_subsequences_but_ranks_contiguous_first() {
    let query = "calinv";
    assert!(super::mail_fuzzy_score("Calendar invitation", query).is_some());
    assert!(super::mail_fuzzy_score("Invoice", query).is_none());
    assert!(
        super::mail_fuzzy_score("calendar", "cal")
            > super::mail_fuzzy_score("c a l service", "cal")
    );
    assert!(super::mail_fuzzy_score("Patch from Takashi", "pft").is_some());
}

#[test]
fn reader_scroll_follows_wrapped_visual_rows_not_logical_lines() {
    // Forty logical lines can occupy 140 physical rows after wrapping.
    // Each j motion must keep the resulting terminal row on screen.
    let view_height = 15;
    let max_scroll = 160;
    let mut scroll = 0;
    for visual_cursor_row in 12..145 {
        scroll =
            reader_scroll_for_visual_cursor(scroll, visual_cursor_row, view_height, max_scroll);
        assert!(
            visual_cursor_row >= scroll,
            "cursor scrolled above viewport at row {visual_cursor_row}"
        );
        assert!(
            visual_cursor_row < scroll + view_height,
            "cursor disappeared below viewport at row {visual_cursor_row}"
        );
    }
    for visual_cursor_row in (12..145).rev() {
        scroll =
            reader_scroll_for_visual_cursor(scroll, visual_cursor_row, view_height, max_scroll);
        assert!(visual_cursor_row >= scroll && visual_cursor_row < scroll + view_height);
    }
}

#[test]
fn reader_scroll_respects_short_viewports_and_end_of_document() {
    assert_eq!(reader_scroll_for_visual_cursor(0, 5, 1, 20), 5);
    assert_eq!(reader_scroll_for_visual_cursor(0, 19, 10, 10), 10);
    assert_eq!(reader_scroll_for_visual_cursor(8, 4, 10, 20), 1);
}

#[test]
fn alt_a_opens_account_setup_without_stealing_compose_ctrl_a() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config =
        toml::from_str("[[accounts]]\nname = \"Test\"\nsource = \"maildir:///tmp/mail\"")
            .unwrap();
    let mut app = super::App::new(&config, vec![]);
    assert!(app.setup.is_none());
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(app.setup.is_none());
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
    assert!(app.setup.is_some());
}

#[test]
fn received_attachment_help_is_searchable_and_keeps_viewer_open() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config =
        toml::from_str("[[accounts]]\nname = \"Test\"\nsource = \"maildir:///tmp/mail\"")
            .unwrap();
    let mut app = super::App::new(&config, vec![]);
    app.attachment_picker_visible = true;
    app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));
    assert!(app.help_visible);
    assert!(app.attachment_picker_visible);
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    assert_eq!(app.help_query, "s");
    assert!(app.help_matches().iter().any(|&index| {
        super::ATTACHMENT_HELP_ENTRIES[index]
            .description
            .contains("save")
    }));
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.help_visible);
    assert!(app.attachment_picker_visible);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.attachment_picker_visible);
}

#[test]
fn mute_toggle_drops_queued_alerts_and_restores_desktop_notifications() {
    let config: crate::config::Config =
        toml::from_str("[ui]\nnew_mail_notifications = true\nnew_mail_bell = true")
            .expect("valid config");
    let mut app = super::App::new(&config, vec![]);
    app.new_mail_notices.push_back(super::NewMailNotice {
        account: "University".into(),
        count: 1,
        sender: String::new(),
        subject: "test".into(),
        excerpt: String::new(),
    });
    app.new_mail_bell_pending = true;
    app.new_mail_since_launch = 3;
    app.handle_command(super::Command::ToggleNotifications);
    assert!(!app.notifications_enabled());
    assert!(app.take_new_mail_notice().is_none());
    assert!(!app.take_new_mail_bell());
    assert_eq!(app.new_mail_count(), 0);
    app.handle_command(super::Command::ToggleNotifications);
    assert!(app.notifications_enabled());
}

#[test]
fn nearby_previews_favor_direction_with_bounded_normal_and_fast_windows() {
    assert_eq!(
        preview_neighbor_positions(5, 10, 1, false),
        vec![6, 7, 8, 9, 4, 3, 2, 1, 0]
    );
    assert_eq!(
        preview_neighbor_positions(5, 10, -1, false),
        vec![4, 3, 2, 1, 0, 6, 7, 8, 9]
    );
    let normal = preview_neighbor_positions(50, 200, 1, false);
    assert_eq!(&normal[..12], &(51..=62).collect::<Vec<_>>());
    assert_eq!(&normal[12..], &(44..=49).rev().collect::<Vec<_>>());
    assert_eq!(normal.len(), 18);
    let fast = preview_neighbor_positions(50, 200, -1, true);
    assert_eq!(&fast[..22], &(28..=49).rev().collect::<Vec<_>>());
    assert_eq!(&fast[22..], &(51..=58).collect::<Vec<_>>());
    assert_eq!(fast.len(), 30);
    assert_eq!(preview_neighbor_positions(0, 3, 1, false), vec![1, 2]);
    assert_eq!(preview_neighbor_positions(2, 3, -1, false), vec![1, 0]);
    assert!(preview_neighbor_positions(3, 3, 1, false).is_empty());
    assert!(preview_neighbor_positions(0, 0, -1, true).is_empty());
}

#[test]
fn smooth_scroll_prefetches_immediate_rows_then_destination_without_duplicates() {
    let positions = preview_travel_positions(5, 27, 100);
    assert_eq!(&positions[..7], &[5, 6, 7, 8, 27, 26, 28]);
    assert_eq!(
        positions.len(),
        positions
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len()
    );
    let backwards = preview_travel_positions(30, 8, 100);
    assert_eq!(&backwards[..7], &[30, 29, 28, 27, 8, 9, 7]);
    assert!(preview_travel_positions(100, 0, 100).is_empty());
    assert!(preview_travel_positions(0, 100, 100).is_empty());
    assert_eq!(preview_travel_positions(0, 0, 1), vec![0]);
}

#[test]
fn preview_size_budget_accounts_for_decoded_html_and_plain_text() {
    use super::preview_body_bytes;
    use crate::model::{HtmlLine, HtmlSpan, HtmlSpanStyle, MessageBody};
    let body = MessageBody {
        id: MessageId("test".into()),
        plain_text: Some("plain".into()),
        plain_fallback: None,
        html_source: Some("<p>html</p>".into()),
        html_text: Some("html".into()),
        html_lines: Some(vec![HtmlLine {
            spans: vec![HtmlSpan {
                text: "rich".into(),
                style: HtmlSpanStyle {
                    link: Some("https://example.org".into()),
                    ..Default::default()
                },
            }],
        }]),
        html_renderer: None,
        html_render_width: 80,
        attachment_count: 0,
        attachment_names: Vec::new(),
    };
    assert_eq!(preview_body_bytes(&body), 5 + 11 + 4 + 4 + 19);
}

#[test]
fn list_navigation_is_bounded() {
    assert_eq!(bounded_index(0, 5, -1), 0);
    assert_eq!(bounded_index(3, 5, 10), 4);
    assert_eq!(bounded_index(0, 0, 1), 0);
}

#[test]
fn message_view_keeps_neovim_style_scrolloff() {
    // At the bottom of a 100-message list in a 20-row viewport, moving up
    // should let the cursor rise inside the viewport before the viewport moves.
    assert_eq!(message_view_offset(80, 99, 100, 20), 80);
    assert_eq!(message_view_offset(80, 95, 100, 20), 80);
    assert_eq!(message_view_offset(80, 90, 100, 20), 80);
    assert_eq!(message_view_offset(80, 83, 100, 20), 79);

    // Likewise, moving down through the middle does not pin the cursor to
    // the bottom row; scrolling begins only inside the scrolloff guard.
    assert_eq!(message_view_offset(20, 30, 100, 20), 20);
    assert_eq!(message_view_offset(20, 36, 100, 20), 21);
}

#[test]
fn contact_editor_normal_motions_and_backspace_work_in_both_fields() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut modal = ContactsModal {
        edit: Some(ContactEdit::new(None)),
        ..ContactsModal::default()
    };
    let send = |app: &mut super::App, modal: &mut ContactsModal, code| {
        app.handle_contacts_modal_key(modal, KeyEvent::new(code, KeyModifiers::NONE));
    };
    let editor = modal.edit.as_mut().unwrap();
    editor.name = "Ada Lovelace".into();
    editor.email = "ada@example.org".into();
    editor.cursor = editor.name.chars().count();

    // Esc from Insert must not cancel the editor; Normal motions apply.
    send(&mut app, &mut modal, KeyCode::Esc);
    assert_eq!(modal.edit.as_ref().unwrap().mode, ComposeMode::Normal);
    for (is_email, original) in [(false, "Ada Lovelace"), (true, "ada@example.org")] {
        if is_email {
            send(&mut app, &mut modal, KeyCode::Tab);
        }
        send(&mut app, &mut modal, KeyCode::Char('H'));
        assert_eq!(modal.edit.as_ref().unwrap().cursor, 0);
        send(&mut app, &mut modal, KeyCode::Backspace);
        assert_eq!(modal.edit.as_ref().unwrap().cursor, 0);
        send(&mut app, &mut modal, KeyCode::Char('L'));
        assert_eq!(modal.edit.as_ref().unwrap().cursor, original.chars().count());
        send(&mut app, &mut modal, KeyCode::Backspace);
        assert_eq!(modal.edit.as_ref().unwrap().cursor, original.chars().count() - 1);
        assert_eq!(modal.edit.as_ref().unwrap().value(), original);
        send(&mut app, &mut modal, KeyCode::Char('v'));
        send(&mut app, &mut modal, KeyCode::Char('H'));
        assert_eq!(modal.edit.as_ref().unwrap().cursor, 0);
        assert_eq!(modal.edit.as_ref().unwrap().mode, ComposeMode::Visual);
        send(&mut app, &mut modal, KeyCode::Esc);
        assert_eq!(modal.edit.as_ref().unwrap().mode, ComposeMode::Normal);
    }
}

#[test]
fn contact_editor_x_cuts_into_register_for_name_and_email_and_visual_ranges() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut modal = ContactsModal {
        edit: Some(ContactEdit::new(None)),
        ..ContactsModal::default()
    };
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    {
        let editor = modal.edit.as_mut().unwrap();
        editor.name = "Jazz".into();
        editor.email = "jazz@example.org".into();
        editor.cursor = 0;
    }
    app.handle_contacts_modal_key(&mut modal, esc);
    app.handle_contacts_modal_key(&mut modal, key('x'));
    assert_eq!(app.yank_register.as_ref().unwrap().text, "J");
    assert_eq!(modal.edit.as_ref().unwrap().name, "azz");
    app.handle_contacts_modal_key(&mut modal, key('p'));
    assert_eq!(modal.edit.as_ref().unwrap().name, "aJzz");

    app.handle_contacts_modal_key(&mut modal, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.handle_contacts_modal_key(&mut modal, key('0'));
    app.handle_contacts_modal_key(&mut modal, key('v'));
    app.handle_contacts_modal_key(&mut modal, key('l'));
    app.handle_contacts_modal_key(&mut modal, key('x'));
    assert_eq!(app.yank_register.as_ref().unwrap().text, "ja");
    assert_eq!(modal.edit.as_ref().unwrap().email, "zz@example.org");
    assert_eq!(modal.edit.as_ref().unwrap().mode, ComposeMode::Normal);
}

#[test]
fn compose_backspace_in_normal_mode_moves_without_deleting() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut composer = Composer::new();
    composer.to = "test@example.org".into();
    composer.cursor = composer.to.chars().count();
    composer.mode = ComposeMode::Normal;
    app.composer = Some(composer);
    app.handle_composer_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    let composer = app.composer.as_ref().unwrap();
    assert_eq!(composer.cursor, "test@example.org".chars().count() - 1);
    assert_eq!(composer.to, "test@example.org");
}

#[test]
fn contact_editor_uses_compose_modal_keys_for_both_fields() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut modal = ContactsModal { edit: Some(ContactEdit::new(None)), ..ContactsModal::default() };
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
    for ch in "Jazz".chars() { app.handle_contacts_modal_key(&mut modal, key(ch)); }
    assert_eq!(modal.edit.as_ref().unwrap().name, "Jazz");
    app.handle_contacts_modal_key(&mut modal, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(modal.edit.as_ref().unwrap().mode, ComposeMode::Normal);
    app.handle_contacts_modal_key(&mut modal, key('y'));
    app.handle_contacts_modal_key(&mut modal, key('y'));
    assert_eq!(app.yank_register.as_ref().unwrap().text, "Jazz");
    app.handle_contacts_modal_key(&mut modal, ctrl_w);
    assert_eq!(modal.edit.as_ref().unwrap().name, "");
    app.handle_contacts_modal_key(&mut modal, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_contacts_modal_key(&mut modal, key('u'));
    assert_eq!(modal.edit.as_ref().unwrap().name, "Jazz");
    app.handle_contacts_modal_key(&mut modal, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(modal.edit.as_ref().unwrap().active_email);
    app.handle_contacts_modal_key(&mut modal, key('p'));
    assert_eq!(modal.edit.as_ref().unwrap().email, "Jazz");
    app.handle_contacts_modal_key(&mut modal, ctrl_w);
    assert!(modal.edit.as_ref().unwrap().email.is_empty());
    app.handle_contacts_modal_key(&mut modal, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_contacts_modal_key(&mut modal, key('u'));
    assert_eq!(modal.edit.as_ref().unwrap().email, "Jazz");
    // A first Escape exits Insert; a second from Normal closes editing.
    app.handle_contacts_modal_key(&mut modal, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(modal.edit.is_none());
}

#[test]
fn draft_resume_decline_keeps_reader_and_accept_opens_composer() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    app.setup = None;
    let make_pending = || PendingDraft {
        account: 0, mailbox: "Drafts".into(),
        message_id: MessageId("draft:42".into()),
        composer: Some(Composer::new()),
    };
    app.message_open = true;
    app.focus = Focus::Preview;
    app.pending_draft = Some(make_pending());
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.message_open);
    assert!(app.composer.is_none());
    assert!(app.pending_draft.is_none());
    app.pending_draft = Some(make_pending());
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(!app.message_open);
    assert_eq!(app.composer.as_ref().unwrap().draft_source.as_ref().unwrap().mailbox, "Drafts");
    assert_eq!(app.composer.as_ref().unwrap().draft_source.as_ref().unwrap().id.0, "draft:42");
}

#[test]
fn compose_ctrl_w_clears_each_field_or_one_body_line_and_is_undoable() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
    for field in [ComposeField::To, ComposeField::Cc, ComposeField::Bcc, ComposeField::Subject] {
        let mut editor = Composer::new();
        editor.switch_field(field);
        *editor.field_mut() = "Jasmeet (Jazz) Bhatia".into();
        editor.cursor = 4;
        app.composer = Some(editor);
        app.handle_composer_key(ctrl_w);
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "", "{field:?}");
        app.handle_composer_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_composer_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        assert_eq!(app.composer.as_ref().unwrap().field_value(), "Jasmeet (Jazz) Bhatia");
    }
    let mut editor = Composer::new();
    editor.switch_field(ComposeField::Body);
    editor.body = "first line\nsecond line\nthird".into();
    editor.cursor = "first line\nsec".chars().count();
    app.composer = Some(editor);
    app.handle_composer_key(ctrl_w);
    assert_eq!(app.composer.as_ref().unwrap().body, "first line\n\nthird");
}

#[test]
fn draft_parser_restores_headers_and_body_without_html() {
    let original = crate::model::OutgoingMessage {
        from: "Sender <sender@example.org>".into(),
        to: "\"Jasmeet (Jazz) Bhatia\" <jazz@example.org>".into(),
        cc: "Another <cc@example.org>".into(),
        bcc: "Blind <bcc@example.org>".into(),
        subject: "Continue later".into(),
        body: "Hello draft 🌎".into(),
        html_body: None,
        attachments: vec![],
        forwarded_raw: None,
        custom_headers: vec![],
        in_reply_to: None,
        references: vec![],
    };
    let raw = crate::outgoing::build_draft(&original, &original.from).unwrap();
    let draft = Composer::from_server_draft(crate::model::MessageId("uid:1".into()), raw).unwrap();
    assert_eq!(draft.subject, "Continue later");
    assert_eq!(draft.body.trim(), "Hello draft 🌎");
    assert!(crate::outgoing::validate_recipients(&draft.to, &draft.cc, &draft.bcc).is_ok());
    assert!(draft.draft_raw.is_none());
}

#[test]
fn composer_field_switch_keeps_labels_modal_and_exits_visual() {
    let mut composer = Composer::new();
    composer.to = "person@example.com".to_owned();
    composer.switch_field(ComposeField::To);
    composer.mode = ComposeMode::Visual;
    composer.visual_anchor = Some(0);

    composer.switch_field(ComposeField::Cc);
    assert_eq!(composer.field, ComposeField::Cc);
    assert_eq!(composer.mode, ComposeMode::Normal);
    assert_eq!(composer.visual_anchor, None);
    assert_eq!(composer.cursor, 0);
}

#[test]
fn normal_header_jk_switch_fields_without_changing_body_motions() {
    let mut composer = Composer::new();
    composer.mode = ComposeMode::Normal;
    assert!(composer.move_header_field(true, 1));
    assert_eq!(composer.field, ComposeField::Cc);
    assert!(composer.move_header_field(true, 3));
    assert_eq!(composer.field, ComposeField::Body);
    assert!(!composer.move_header_field(false, 1));
    composer.switch_field(ComposeField::To);
    assert!(composer.move_header_field(false, 3));
    assert_eq!(composer.field, ComposeField::To);
    composer.mode = ComposeMode::Insert;
    assert!(!composer.move_header_field(true, 1));
    assert_eq!(composer.field, ComposeField::To);
}

#[test]
fn delete_review_scroll_advances_one_row_per_frame() {
    let mut review = super::ReviewScroll::default();
    review.queue(80, super::SMOOTH_SCROLL_ROWS);
    assert!(review.active());
    assert_eq!(review.position, 0);
    review.advance();
    assert_eq!(review.position, 1);
    for _ in 1..22 {
        review.advance();
    }
    assert_eq!(review.position, 22);
    assert!(!review.active());
    review.queue(80, -super::SMOOTH_SCROLL_ROWS);
    for _ in 0..22 {
        review.advance();
    }
    assert_eq!(review.position, 0);
    assert!(!review.active());
}

#[test]
fn detach_confirmation_can_restore_multiple_attachments() {
    use std::path::PathBuf;
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = super::App::new(&config, vec![]);
    let mut composer = super::Composer::new();
    composer.attachment_paths = ["first.png", "second.pdf", "third.txt"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    composer.attachment_manager_open = true;
    app.composer = Some(composer);
    app.detach_confirmation = Some(super::DetachConfirmation {
        paths: vec![PathBuf::from("first.png"), PathBuf::from("third.txt")],
        review: super::ReviewScroll::default(),
    });
    app.handle_detach_confirmation_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('y'),
        crossterm::event::KeyModifiers::NONE,
    ));
    let composer = app.composer.as_mut().unwrap();
    assert_eq!(composer.attachment_paths, vec![PathBuf::from("second.pdf")]);
    composer.undo_managed_attachment();
    composer.undo_managed_attachment();
    assert_eq!(
        composer.attachment_paths,
        vec![
            PathBuf::from("first.png"),
            PathBuf::from("second.pdf"),
            PathBuf::from("third.txt")
        ]
    );
}

#[test]
fn attachment_manager_detach_can_be_undone_in_original_order() {
    let mut composer = Composer::new();
    let a = PathBuf::from("/tmp/a.txt");
    let b = PathBuf::from("/other/place/b.txt");
    let c = PathBuf::from("/tmp/c.txt");
    composer.attachment_paths = vec![a.clone(), b.clone(), c.clone()];
    composer.attachment_manager_selected = 1;
    assert_eq!(composer.remove_managed_attachment(), Some(b.clone()));
    assert_eq!(composer.attachment_paths, vec![a.clone(), c.clone()]);
    assert_eq!(composer.undo_managed_attachment(), Some(b.clone()));
    assert_eq!(composer.attachment_paths, vec![a, b, c]);
    assert_eq!(composer.attachment_manager_selected, 1);
    assert_eq!(composer.undo_managed_attachment(), None);
}

#[test]
fn legacy_attachment_manager_detach_can_be_undone() {
    let mut composer = Composer::new();
    composer.attachments = "/tmp/old-a.txt;/tmp/old-b.txt".to_owned();
    composer.attachment_manager_selected = 0;
    assert_eq!(
        composer.remove_managed_attachment(),
        Some(PathBuf::from("/tmp/old-a.txt"))
    );
    assert_eq!(composer.attachment_count(), 1);
    assert_eq!(
        composer.undo_managed_attachment(),
        Some(PathBuf::from("/tmp/old-a.txt"))
    );
    assert_eq!(composer.attachments, "/tmp/old-a.txt;/tmp/old-b.txt");
}

#[test]
fn closing_filled_composer_requires_explicit_draft_choice() {
    let mut composer = Composer::new();
    assert!(!composer.has_unsent_content());
    composer.to = "someone@example.org".to_owned();
    assert!(composer.has_unsent_content());
    composer.to.clear();
    composer
        .attachment_paths
        .push("/tmp/some-attachment.pdf".into());
    assert!(composer.has_unsent_content());
    composer.exit_prompt = true;
    let restored: Composer =
        serde_json::from_slice(&serde_json::to_vec(&composer).unwrap()).unwrap();
    assert!(!restored.exit_prompt);
}

#[test]
fn smooth_picker_scroll_only_refreshes_preview_at_destination() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..30 {
        std::fs::write(
            temp.path().join(format!("file-{index:02}.txt")),
            format!("file content {index}"),
        )
        .unwrap();
    }
    let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
    let first = match &picker.preview {
        super::ComposeFilePreview::Text { lines, .. } => lines.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    picker.queue_smooth_scroll(SMOOTH_SCROLL_ROWS);
    assert_eq!(picker.smooth_target, Some(SMOOTH_SCROLL_ROWS as usize));
    // Preview I/O is deferred through every intermediate animation frame.
    for _ in 0..(SMOOTH_SCROLL_ROWS as usize - 1) {
        picker.advance_smooth_scroll();
        assert!(picker.smooth_target.is_some());
        assert!(matches!(&picker.preview,
            super::ComposeFilePreview::Text { lines, .. } if lines == &first));
    }
    picker.advance_smooth_scroll();
    assert_eq!(picker.selected, SMOOTH_SCROLL_ROWS as usize);
    assert!(picker.smooth_target.is_none());
    assert!(matches!(&picker.preview,
        super::ComposeFilePreview::Text { lines, .. } if lines != &first));
}

#[test]
fn picker_has_parent_and_file_preview_and_count_navigation() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("alpha")).unwrap();
    std::fs::write(temp.path().join("hello.txt"), b"hello from jerc").unwrap();
    let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
    let index = picker
        .entries
        .iter()
        .position(|entry| entry.name == "hello.txt")
        .unwrap();
    picker.move_selection(index as isize);
    assert!(matches!(
        picker.preview,
        super::ComposeFilePreview::Text { .. }
    ));
    assert!(!picker.parent_entries.is_empty());
    let mut composer = Composer::new();
    composer.file_picker = Some(picker);
    composer
        .file_picker_key(crossterm::event::KeyCode::Char('g'))
        .unwrap();
    composer
        .file_picker_key(crossterm::event::KeyCode::Char('g'))
        .unwrap();
    assert_eq!(composer.file_picker.as_ref().unwrap().selected, 0);
    composer
        .file_picker_key(crossterm::event::KeyCode::Char('G'))
        .unwrap();
    assert_eq!(composer.file_picker.as_ref().unwrap().selected, index);
}

#[test]
fn send_review_can_prepare_rich_preview_without_an_html_alternative() {
    let mut composer = Composer::new();
    composer.to = "friend@example.com".to_owned();
    composer.body = "Hello!".to_owned();
    composer.prepare_send_preview().unwrap();
    assert!(composer.confirm_send);
    assert!(composer.preview_rich_lines.is_none());
    assert_eq!(composer.preview_scroll, 0);
}

#[test]
fn compose_tabs_skip_file_paths_and_advanced_headers() {
    assert_eq!(ComposeField::Subject.next(), ComposeField::Body);
    assert_eq!(ComposeField::Body.previous(), ComposeField::Subject);
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Subject);
    composer.toggle_advanced_headers();
    assert_eq!(composer.field, ComposeField::Headers);
    composer.headers = "Reply-To: team@example.com".to_owned();
    composer.toggle_advanced_headers();
    assert_eq!(composer.field, ComposeField::Subject);
    assert_eq!(composer.headers, "Reply-To: team@example.com");
}

#[test]
fn picker_handles_semicolon_names_and_restores_selected_files() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("report;v2.txt"), b"attachment test").unwrap();
    std::fs::create_dir(temp.path().join("folder")).unwrap();
    let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
    assert!(picker.entries[0].is_directory);
    let file_index = picker
        .entries
        .iter()
        .position(|entry| entry.name == "report;v2.txt")
        .unwrap();
    picker.selected = file_index;
    let mut composer = Composer::new();
    composer.file_picker = Some(picker);
    composer
        .file_picker_key(crossterm::event::KeyCode::Char(' '))
        .unwrap();
    assert_eq!(composer.attachment_count(), 1);
    let loaded = super::load_compose_attachments("", &composer.attachment_paths).unwrap();
    assert_eq!(loaded[0].filename, "report;v2.txt");
    assert_eq!(loaded[0].data.as_slice(), b"attachment test");
    // Space on the final file does not jump back to the previous item.
    composer.file_picker.as_mut().unwrap().selected = file_index;
    composer
        .file_picker_key(crossterm::event::KeyCode::Char(' '))
        .unwrap();
    assert_eq!(composer.attachment_count(), 0);
}

#[test]
fn picker_visual_range_attaches_files_after_count_motion() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..7 {
        std::fs::write(temp.path().join(format!("{index}.txt")), b"hi").unwrap();
    }
    let mut composer = Composer::new();
    composer.file_picker =
        Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
    let key = |ch| crossterm::event::KeyCode::Char(ch);
    composer.file_picker_key(key('v')).unwrap();
    composer.file_picker_key(key('5')).unwrap();
    composer.file_picker_key(key('j')).unwrap();
    assert_eq!(composer.file_picker.as_ref().unwrap().selected, 5);
    assert_eq!(
        composer.file_picker.as_ref().unwrap().visual_anchor,
        Some(0)
    );
    composer
        .file_picker_key(crossterm::event::KeyCode::Enter)
        .unwrap();
    assert_eq!(composer.attachment_count(), 6);
    assert!(
        composer
            .file_picker
            .as_ref()
            .unwrap()
            .visual_anchor
            .is_none()
    );
    // Re-selecting the same range does not add duplicates.
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('v')).unwrap();
    composer.file_picker_key(key('5')).unwrap();
    composer.file_picker_key(key('j')).unwrap();
    composer
        .file_picker_key(crossterm::event::KeyCode::Enter)
        .unwrap();
    assert_eq!(composer.attachment_count(), 6);
}

#[test]
fn picker_space_advances_and_stays_at_end_and_search_help_is_local() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(temp.path().join(name), b"ok").unwrap();
    }
    let mut composer = Composer::new();
    composer.file_picker =
        Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
    let key = |ch| crossterm::event::KeyCode::Char(ch);
    composer.file_picker_key(key(' ')).unwrap();
    assert_eq!(composer.file_picker.as_ref().unwrap().selected, 1);
    composer.file_picker_key(key(' ')).unwrap();
    assert_eq!(composer.file_picker.as_ref().unwrap().selected, 2);
    composer.file_picker_key(key(' ')).unwrap();
    assert_eq!(composer.file_picker.as_ref().unwrap().selected, 2);
    assert_eq!(composer.attachment_count(), 3);
    composer.file_picker_key(key(' ')).unwrap();
    assert_eq!(composer.file_picker.as_ref().unwrap().selected, 2);
    assert_eq!(composer.attachment_count(), 2);
    composer.file_picker_key(key('?')).unwrap();
    composer.file_picker_key(key('v')).unwrap();
    assert!(composer.file_picker.as_ref().unwrap().help_visible);
    assert_eq!(composer.file_picker.as_ref().unwrap().help_query, "v");
    composer
        .file_picker_key(crossterm::event::KeyCode::Esc)
        .unwrap();
    assert!(!composer.file_picker.as_ref().unwrap().help_visible);
    composer.file_picker_key(key('v')).unwrap();
    composer
        .file_picker_key(crossterm::event::KeyCode::Esc)
        .unwrap();
    assert!(
        composer
            .file_picker
            .as_ref()
            .unwrap()
            .visual_anchor
            .is_none()
    );
}

#[test]
fn picker_slash_search_accepts_current_match_and_tab_opens_folder() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["photos-2025", "photos-2026", "notes"] {
        std::fs::create_dir(temp.path().join(name)).unwrap();
    }
    std::fs::write(temp.path().join("photos.txt"), b"hello").unwrap();
    let mut composer = Composer::new();
    composer.file_picker =
        Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
    let key = |ch| crossterm::event::KeyCode::Char(ch);
    composer.file_picker_key(key('/')).unwrap();
    for ch in "photos".chars() {
        composer.file_picker_key(key(ch)).unwrap();
    }
    let picker = composer.file_picker.as_ref().unwrap();
    assert_eq!(picker.search_matches().len(), 2); // folder results take precedence
    assert_eq!(picker.entries[picker.selected].name, "photos-2025");
    composer
        .file_picker_key(crossterm::event::KeyCode::Down)
        .unwrap();
    let picker = composer.file_picker.as_ref().unwrap();
    assert_eq!(picker.entries[picker.selected].name, "photos-2026");
    composer
        .file_picker_key(crossterm::event::KeyCode::Enter)
        .unwrap();
    let picker = composer.file_picker.as_ref().unwrap();
    assert_eq!(picker.entries[picker.selected].name, "photos-2026");
    assert!(picker.search_query.is_none());
    // Enter should return to normal motion, so l opens the highlighted folder.
    composer.file_picker_key(key('l')).unwrap();
    let picker = composer.file_picker.as_ref().unwrap();
    assert!(picker.directory.ends_with("photos-2026"));
    composer.file_picker_key(key('/')).unwrap();
    composer
        .file_picker_key(crossterm::event::KeyCode::Esc)
        .unwrap();
    assert!(
        composer
            .file_picker
            .as_ref()
            .unwrap()
            .search_query
            .is_none()
    );
    assert!(composer.file_picker.is_some());
}

#[test]
fn picker_visual_x_detaches_multiple_files_without_touching_others() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(temp.path().join(name), b"ok").unwrap();
    }
    let mut composer = Composer::new();
    composer.file_picker =
        Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
    let key = |ch| crossterm::event::KeyCode::Char(ch);
    composer.file_picker_key(key('v')).unwrap();
    composer.file_picker_key(key('2')).unwrap();
    composer.file_picker_key(key('j')).unwrap();
    composer
        .file_picker_key(crossterm::event::KeyCode::Enter)
        .unwrap();
    assert_eq!(composer.attachment_count(), 3);
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('v')).unwrap();
    composer.file_picker_key(key('j')).unwrap();
    composer.file_picker_key(key('x')).unwrap();
    assert_eq!(composer.attachment_count(), 1);
    assert_eq!(composer.attachment_paths[0].file_name().unwrap(), "c.txt");
    assert!(
        composer
            .file_picker
            .as_ref()
            .unwrap()
            .visual_anchor
            .is_none()
    );
}

#[test]
fn picker_visual_space_detaches_only_when_every_file_is_attached() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("0folder")).unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(temp.path().join(name), b"hello").unwrap();
    }
    let mut composer = Composer::new();
    composer.file_picker =
        Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
    let key = |ch| crossterm::event::KeyCode::Char(ch);
    assert!(composer.file_picker.as_ref().unwrap().entries[0].is_directory);
    // Attach a and b while selecting a directory too (which must be ignored).
    composer.file_picker_key(key('v')).unwrap();
    composer.file_picker_key(key('2')).unwrap();
    composer.file_picker_key(key('j')).unwrap();
    composer
        .file_picker_key(crossterm::event::KeyCode::Enter)
        .unwrap();
    assert_eq!(composer.attachment_count(), 2);
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('v')).unwrap();
    composer.file_picker_key(key('2')).unwrap();
    composer.file_picker_key(key('j')).unwrap();
    composer.file_picker_key(key(' ')).unwrap();
    assert_eq!(composer.attachment_count(), 0); // folder is ignored
    assert!(
        composer
            .file_picker
            .as_ref()
            .unwrap()
            .visual_anchor
            .is_none()
    );

    // A mixed range should attach the missing file without detaching an
    // already attached file.
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('v')).unwrap();
    composer.file_picker_key(key('j')).unwrap();
    composer.file_picker_key(key(' ')).unwrap();
    assert_eq!(composer.attachment_count(), 1); // a.txt only
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('g')).unwrap();
    composer.file_picker_key(key('v')).unwrap();
    composer.file_picker_key(key('2')).unwrap();
    composer.file_picker_key(key('j')).unwrap();
    composer.file_picker_key(key(' ')).unwrap();
    assert_eq!(composer.attachment_count(), 2); // a.txt and b.txt
    assert!(composer.attachment_paths.iter().all(|path| path.is_file()));
}

#[test]
fn picker_pdf_previews_are_deferred_and_stale_results_are_cached_not_displayed() {
    let temp = tempfile::tempdir().unwrap();
    let pdf = temp.path().join("a.pdf");
    let text = temp.path().join("b.txt");
    std::fs::write(&pdf, b"%PDF-1.4\n").unwrap();
    std::fs::write(&text, b"text file").unwrap();
    let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
    assert!(matches!(
        picker.preview,
        super::ComposeFilePreview::Loading(_)
    ));
    assert!(picker.pdf_preview_due.is_some());
    // Navigate away before the debounce expires: no converter was started.
    picker.move_selection(1);
    assert!(picker.pdf_preview_due.is_none());
    assert!(picker.pdf_preview_inflight.is_empty());
    assert!(matches!(
        picker.preview,
        super::ComposeFilePreview::Text { .. }
    ));
    // Pretend an earlier conversion completed while the selection changed.
    picker.pdf_preview_inflight.insert(pdf.clone());
    picker
        .pdf_preview_results
        .lock()
        .unwrap()
        .push_back((pdf.clone(), Ok(vec![7, 8, 9])));
    picker.advance_pdf_preview();
    assert!(matches!(
        picker.preview,
        super::ComposeFilePreview::Text { .. }
    ));
    assert!(picker.pdf_preview_inflight.is_empty());
    picker.move_selection(-1);
    assert!(
        matches!(picker.preview, super::ComposeFilePreview::Image(ref bytes) if bytes == &[7, 8, 9])
    );
    assert!(picker.pdf_preview_due.is_none());
}

#[test]
fn manage_attachments_removes_files_from_unrelated_directories() {
    let mut composer = Composer::new();
    let first = std::path::PathBuf::from("/tmp/one/first.txt");
    let second = std::path::PathBuf::from("/home/example/elsewhere/second.txt");
    composer.attachment_paths = vec![first, second.clone()];
    composer.attachment_manager_selected = 0;
    assert_eq!(
        composer
            .remove_managed_attachment()
            .unwrap()
            .file_name()
            .unwrap(),
        "first.txt"
    );
    assert_eq!(composer.attachment_paths, vec![second]);
    assert_eq!(composer.attachment_manager_selected, 0);
    composer.attachments = "/tmp/legacy/old.txt".to_owned();
    composer.attachment_manager_selected = 1;
    assert_eq!(
        composer
            .remove_managed_attachment()
            .unwrap()
            .file_name()
            .unwrap(),
        "old.txt"
    );
    assert_eq!(composer.attachment_count(), 1);
}

#[test]
fn picker_recognizes_image_files_for_terminal_preview() {
    let temp = tempfile::tempdir().unwrap();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    std::fs::write(temp.path().join("photo.png"), png.into_inner()).unwrap();
    let picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
    match &picker.preview {
        super::ComposeFilePreview::Image(bytes) => {
            assert!(image::load_from_memory(bytes).is_ok())
        }
        preview => panic!("expected a decodable image, got {preview:?}"),
    }
    assert!(super::compose_image_extension(&temp.path().join("a.WEBP")));
}

#[test]
fn picker_detects_known_source_file_mime_types() {
    let temp = tempfile::tempdir().unwrap();
    for (name, expected) in [
        ("Cargo.toml", "application/toml"),
        ("Cargo.lock", "text/plain"),
        ("lib.rs", "text/plain"),
        ("sample.json", "application/json"),
        ("LICENSE", "text/plain"),
    ] {
        let path = temp.path().join(name);
        std::fs::write(&path, "content").unwrap();
        assert_eq!(
            super::load_compose_file(&path).unwrap().content_type,
            expected
        );
    }
}

#[test]
fn picker_attached_files_are_encoded_as_mime_parts() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("hello.txt");
    std::fs::write(&file, b"hello attachment content").unwrap();
    let files = super::load_compose_attachments("", &[file]).unwrap();
    let message = crate::model::OutgoingMessage {
        from: "sender@example.com".to_owned(),
        to: "receiver@example.com".to_owned(),
        cc: String::new(),
        bcc: String::new(),
        subject: "attachment smoke test".to_owned(),
        body: "See the attachment".to_owned(),
        html_body: None,
        attachments: files,
        custom_headers: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        forwarded_raw: None,
    };
    let raw = crate::outgoing::build_outgoing(&message, "sender@example.com")
        .unwrap()
        .formatted();
    let wire = String::from_utf8_lossy(&raw);
    assert!(wire.contains("multipart/mixed"));
    assert!(wire.contains("hello.txt"));
    assert!(wire.contains("Content-Disposition: attachment"));
}

#[test]
fn compose_selected_file_paths_survive_postponed_draft_encoding() {
    let mut composer = Composer::new();
    composer
        .attachment_paths
        .push(std::path::PathBuf::from("/tmp/a file.pdf"));
    let bytes = serde_json::to_vec(&composer).unwrap();
    let restored: Composer = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored.attachment_paths, composer.attachment_paths);
    assert!(restored.file_picker.is_none());
    assert!(!restored.advanced_headers_open);
}

#[test]
fn composer_normal_line_motions_follow_body_lines() {
    let mut composer = Composer::new();
    composer.body = "one\ntwo\nthree".to_owned();
    composer.switch_field(ComposeField::Body);
    composer.cursor = 6;
    assert_eq!(composer.line_start(), 4);
    assert_eq!(composer.line_end(), 7);
    composer.open_line_below();
    assert_eq!(composer.body, "one\ntwo\n\nthree");
    assert_eq!(composer.cursor, 8);
}

#[test]
fn composer_visual_range_tracks_character_indices() {
    let mut composer = Composer::new();
    composer.body = "héllo".to_owned();
    composer.switch_field(ComposeField::Body);
    composer.mode = ComposeMode::Visual;
    composer.visual_anchor = Some(1);
    composer.cursor = 4;
    assert_eq!(composer.visual_range(), Some((1, 5)));
    composer.delete_range(1, 5);
    assert_eq!(composer.body, "h");
}

#[test]
fn compose_vim_word_motions_and_undo_redo_are_consistent() {
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.body = "first second".to_owned();
    composer.cursor = 0;
    assert!(composer.apply_motion(crossterm::event::KeyCode::Char('w'), 1));
    assert_eq!(composer.cursor, 6);
    composer.insert_char('X');
    assert_eq!(composer.body, "first Xsecond");
    composer.undo_edit(false);
    assert_eq!(composer.body, "first second");
    composer.undo_edit(true);
    assert_eq!(composer.body, "first Xsecond");
}

#[test]
fn compose_inner_text_objects_choose_the_inner_enclosing_delimiter() {
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.body = "call(foo[bar])".to_owned();
    composer.cursor = 10;
    composer.select_inner_object('[');
    assert_eq!(composer.mode, ComposeMode::Visual);
    assert_eq!(composer.visual_range(), Some((9, 12)));
    composer.mode = ComposeMode::Normal;
    composer.cursor = 10;
    composer.select_inner_object('(');
    assert_eq!(composer.visual_range(), Some((5, 13)));
}

#[test]
fn compose_find_char_never_crosses_into_the_next_line() {
    let mut composer = Composer::new();
    composer.switch_field(ComposeField::Body);
    composer.body = "first line\nsecond line x".to_owned();
    composer.cursor = 0;
    composer.find_char('x', true, 1);
    assert_eq!(composer.cursor, 0);
    composer.cursor = composer.body.chars().count();
    composer.find_char('s', false, 1);
    assert_eq!(composer.cursor, "first line\n".chars().count());
}

#[test]
fn compose_visual_linewise_range_covers_the_newline() {
    let mut composer = Composer::new();
    composer.body = "one\ntwo\nthree".to_owned();
    composer.switch_field(ComposeField::Body);
    composer.cursor = 5;
    composer.visual_anchor = Some(5);
    composer.visual_linewise = true;
    composer.mode = ComposeMode::Visual;
    assert_eq!(composer.visual_range(), Some((4, 8)));
}

#[test]
fn compose_help_search_uses_its_own_keymap() {
    let mut composer = Composer::new();
    composer.help_query = "attachment".to_owned();
    let matches = composer.compose_help_matches();
    assert!(!matches.is_empty());
    assert!(matches.iter().all(|&index| {
        super::super::input::COMPOSE_HELP_ENTRIES[index]
            .description
            .contains("attachment")
    }));
}

#[test]
fn reader_inner_text_objects_support_quotes_and_brackets() {
    assert_eq!(
        inner_text_object_range(r#"say "hello" now"#, 6, '"'),
        Some((5, 9))
    );
    assert_eq!(
        inner_text_object_range("call(foo[bar])", 9, ']'),
        Some((9, 11))
    );
    assert_eq!(
        inner_text_object_range("call(foo[bar])", 9, ')'),
        Some((5, 12))
    );
    assert_eq!(inner_text_object_range("'{value}'", 4, '\''), Some((1, 7)));
}

#[test]
fn reader_selection_yanks_exact_character_range() {
    let selection = ReaderSelection {
        start: ReaderPosition { line: 0, column: 2 },
        end: ReaderPosition { line: 1, column: 2 },
    };
    assert_eq!(
        reader_selection_text("abcdef\nghijkl", selection),
        "cdef\nghi"
    );
}

#[test]
fn reader_linewise_selection_yanks_complete_lines() {
    let selection = ReaderSelection {
        start: ReaderPosition { line: 0, column: 0 },
        end: ReaderPosition {
            line: 1,
            column: usize::MAX,
        },
    };
    assert_eq!(
        reader_selection_text("abcdef\nghijkl\nmnop", selection),
        "abcdef\nghijkl"
    );
    assert_eq!(selection.columns_for_line(0, 6), Some((0, 5)));
    assert_eq!(selection.columns_for_line(1, 6), Some((0, 5)));
}

#[test]
fn reader_word_motions_cross_lines() {
    let text = "one two\n\nthree";
    assert_eq!(
        reader_word_target(text, 0, 0, ReaderWordMotion::NextStart),
        Some(ReaderPosition { line: 0, column: 4 })
    );
    assert_eq!(
        reader_word_target(text, 0, 4, ReaderWordMotion::NextStart),
        Some(ReaderPosition { line: 2, column: 0 })
    );
    assert_eq!(
        reader_word_target(text, 2, 2, ReaderWordMotion::PreviousStart),
        Some(ReaderPosition { line: 2, column: 0 })
    );
}

#[test]
fn reader_images_treat_html_dimensions_as_css_pixels_for_hidpi_assets() {
    // A typical 2x email logo should use its full backing bitmap instead of
    // being rendered at half-size just because HTML declares the CSS size.
    assert_eq!(
        reader_image_target_size((148, 48), (Some(74), Some(24)), (8, 16), (120, 40),),
        Size::new(19, 3)
    );

    // A genuine 1x asset is still capped at native size and never upscaled.
    assert_eq!(
        reader_image_target_size((74, 24), (Some(300), Some(100)), (8, 16), (120, 40),),
        Size::new(10, 2)
    );
}

#[test]
fn reader_images_use_full_reader_bounds_instead_of_legacy_caps() {
    assert_eq!(
        reader_image_target_size((1200, 600), (None, None), (8, 16), (120, 40),),
        Size::new(120, 38)
    );
    assert_eq!(
        reader_image_target_size((2400, 1600), (None, None), (8, 16), (90, 30),),
        Size::new(90, 30)
    );
}

#[test]
fn reader_url_lookup_uses_the_token_under_the_cursor() {
    let line = "first https://example.com/a then https://example.net/b.";
    assert_eq!(
        url_at_cursor(line, 9).as_deref(),
        Some("https://example.com/a")
    );
    assert_eq!(
        url_at_cursor(line, 40).as_deref(),
        Some("https://example.net/b")
    );
    assert_eq!(url_at_cursor(line, 0), None);
}

#[test]
fn quoted_reply_attribution_matches_expected_shape() {
    let message = MessageSummary {
        id: MessageId("reply".into()),
        message_id: Some("message@example.com".into()),
        in_reply_to: None,
        references: Vec::new(),
        from_name: "Takashi Iwai".into(),
        from_address: "tiwai@suse.de".into(),
        to: String::new(),
        cc: String::new(),
        bcc: String::new(),
        subject: "Re: test".into(),
        timestamp: 1_792_662_534,
        date_label: String::new(),
        size: 0,
        flags: MessageFlags::empty(),
        has_attachment_hint: false,
        attachment_names_hint: Vec::new(),
    };
    let attribution = reply_attribution(&message);
    assert!(attribution.starts_with("On "));
    assert!(attribution.contains(", Takashi Iwai wrote:"));
    assert!(!attribution.contains("previous message"));
}

#[test]
fn forwarded_message_contains_gmail_attribution_and_original_recipient() {
    let raw = b"From: Yuzu Keycaps <contact@example.org>\r\nTo: Person <person@example.net>\r\nCc: Friend <friend@example.net>\r\nDate: Fri, 23 Oct 2026 14:02:00 -0700\r\nSubject: Ends Saturday\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nA great deal";
    let message = crate::mail::parse_summary(
        MessageId("forward".into()),
        raw,
        raw.len() as u64,
        MessageFlags::empty(),
    )
    .unwrap();
    let composer = Composer::forward(&message, raw.to_vec()).unwrap();
    assert!(
        composer
            .body
            .contains("---------- Forwarded message ---------")
    );
    assert!(composer.body.contains("To: Person <person@example.net>"));
    assert!(composer.body.contains("Cc: Friend <friend@example.net>"));
    assert!(composer.body.contains("A great deal"));
    assert!(!composer.body.contains("Bcc:"));
}

#[test]
fn account_navigation_wraps() {
    assert_eq!(offset_index(0, 3, -1), 2);
    assert_eq!(offset_index(2, 3, 1), 0);
}

#[test]
fn unrelated_worker_error_preserves_other_mutations_and_seen_requests() {
    use crate::worker::{WorkerEvent, WorkerEventKind, WorkerHandle};
    use std::sync::mpsc::sync_channel;

    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = sync_channel(8);
    let worker = WorkerHandle::spawn(
        0,
        Box::new(crate::backend::MaildirBackend::new(root.path().into())),
        None,
        None,
        tx,
    );
    let config = crate::config::Config::single_maildir("Local".into(), root.path().into());
    let mut app = super::App::new(&config, vec![worker]);
    let seen_id = MessageId("test-seen".into());
    let account = app.account_mut().unwrap();
    account.pending_mutation_labels.insert(101, "archived");
    account.pending_mutation_labels.insert(102, "deleted");
    account.pending_seen.insert(seen_id.clone(), 103);

    app.handle_worker_event(WorkerEvent {
        account: 0,
        request_id: 101,
        kind: WorkerEventKind::Error("archive failed".into()),
    });
    let account = app.account().unwrap();
    assert!(!account.pending_mutation_labels.contains_key(&101));
    assert_eq!(account.pending_mutation_labels.get(&102), Some(&"deleted"));
    assert_eq!(account.pending_seen.get(&seen_id), Some(&103));

    // A late success for the failed request must not release another pending
    // request or incorrectly overwrite the failure status.
    app.handle_worker_event(WorkerEvent {
        account: 0,
        request_id: 101,
        kind: WorkerEventKind::MutationComplete,
    });
    assert_eq!(app.account().unwrap().pending_mutation_labels.len(), 1);
    app.handle_worker_event(WorkerEvent {
        account: 0,
        request_id: 102,
        kind: WorkerEventKind::MutationComplete,
    });
    assert!(app.account().unwrap().pending_mutation_labels.is_empty());
}

#[test]
fn seen_response_must_match_the_pending_request_id() {
    use crate::worker::{WorkerEvent, WorkerEventKind, WorkerHandle};
    use std::sync::mpsc::sync_channel;

    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = sync_channel(8);
    let worker = WorkerHandle::spawn(
        0,
        Box::new(crate::backend::MaildirBackend::new(root.path().into())),
        None,
        None,
        tx,
    );
    let config = crate::config::Config::single_maildir("Local".into(), root.path().into());
    let mut app = super::App::new(&config, vec![worker]);
    let id = MessageId("test-seen".into());
    app.account_mut().unwrap().pending_seen.insert(id.clone(), 203);

    app.handle_worker_event(WorkerEvent {
        account: 0,
        request_id: 202,
        kind: WorkerEventKind::SeenChanged { id: id.clone(), seen: true },
    });
    assert_eq!(app.account().unwrap().pending_seen.get(&id), Some(&203));
    app.handle_worker_event(WorkerEvent {
        account: 0,
        request_id: 203,
        kind: WorkerEventKind::SeenChanged { id: id.clone(), seen: true },
    });
    assert!(!app.account().unwrap().pending_seen.contains_key(&id));
}

// Pass 5: exercise the shared state paths without a live IMAP account.
fn local_account_fixture() -> (tempfile::TempDir, super::App) {
    use crate::worker::WorkerHandle;
    use std::sync::mpsc::sync_channel;

    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = sync_channel(8);
    let worker = WorkerHandle::spawn(
        0,
        Box::new(crate::backend::MaildirBackend::new(root.path().into())),
        None,
        None,
        tx,
    );
    let config = crate::config::Config::single_maildir("Local".into(), root.path().into());
    (root, super::App::new(&config, vec![worker]))
}

#[test]
fn seen_transition_is_idempotent_and_does_not_guess_unknown_counts() {
    let (_root, mut app) = local_account_fixture();
    let account = app.account_mut().unwrap();
    let id = MessageId("seen-transition".into());
    let raw = b"From: Example <a@example.net>\r\nSubject: Test\r\n\r\nbody";
    account.messages.push(
        crate::mail::parse_summary(id.clone(), raw, raw.len() as u64, MessageFlags::empty())
            .unwrap(),
    );
    account.current_mailbox = Some("INBOX".into());
    let mut folder = crate::model::Mailbox::new("INBOX", crate::model::MailboxRole::Inbox);
    folder.unread = 1;
    folder.total = 1;
    folder.counts_known = true;
    account.mailboxes = vec![folder];

    account.apply_seen_flag(&id, true);
    assert!(!account.messages[0].unread());
    assert_eq!(account.mailboxes[0].unread, 0);
    account.apply_seen_flag(&id, true); // Server ACK must not double-decrement.
    assert_eq!(account.mailboxes[0].unread, 0);
    account.apply_seen_flag(&id, false);
    assert!(account.messages[0].unread());
    assert_eq!(account.mailboxes[0].unread, 1);

    account.mailboxes[0].counts_known = false;
    account.apply_seen_flag(&id, true);
    assert!(!account.messages[0].unread());
    assert_eq!(account.mailboxes[0].unread, 1); // Next STATUS owns reconciliation.
}

#[test]
fn cache_invalidation_forgets_lru_and_explicitly_stales_folder_counts() {
    let (_root, mut app) = local_account_fixture();
    let account = app.account_mut().unwrap();
    let empty = crate::cache::CachedMailbox {
        messages: Vec::new(),
        bodies: std::collections::HashMap::new(),
        message_index: 0,
        message_view_offset: 0,
        next_page_offset: 0,
        has_more: false,
        uid_validity: Some(9),
        uid_next: Some(10),
    };
    account.remember_mailbox("Drafts".into(), empty.clone());
    account.remember_mailbox("Sent".into(), empty);
    assert_eq!(account.warm_folder_order.len(), 2);
    account.current_mailbox = Some("INBOX".into());
    let mut draft = crate::model::Mailbox::new("Drafts", crate::model::MailboxRole::Drafts);
    draft.counts_known = true;
    account.mailboxes.push(draft);

    account.forget_cached_folder("Drafts");
    assert!(!account.mailbox_cache.contains_key("Drafts"));
    assert_eq!(account.warm_folder_order.iter().map(String::as_str).collect::<Vec<_>>(), vec!["Sent"]);
    assert!(account.mailboxes.last().unwrap().counts_known);
    account.invalidate_remote_folder("Drafts");
    assert!(!account.mailboxes.last().unwrap().counts_known);
    assert_eq!(account.current_mailbox.as_deref(), Some("INBOX"));
}

#[test]
fn persistent_and_in_memory_snapshots_share_bounded_cursor_rules() {
    let (_root, mut app) = local_account_fixture();
    let account = app.account_mut().unwrap();
    let raw = b"From: Example <a@example.net>\r\nSubject: Test\r\n\r\nbody";
    let example = crate::mail::parse_summary(
        MessageId("one".into()), raw, raw.len() as u64, MessageFlags::empty(),
    ).unwrap();
    account.current_mailbox = Some("INBOX".into());
    account.uid_validities.insert("INBOX".into(), 15);
    account.uid_nexts.insert("INBOX".into(), 600);
    account.messages = (0..405).map(|i| {
        let mut item = example.clone();
        item.id = MessageId(format!("message-{i}"));
        item
    }).collect();
    account.message_index = 404;
    account.next_page_offset = 405;
    let snapshot = account.current_cache_snapshot().unwrap();
    assert_eq!(snapshot.messages.len(), crate::cache::MAX_MESSAGES_PER_FOLDER);
    assert_eq!(snapshot.message_index, crate::cache::MAX_MESSAGES_PER_FOLDER - 1);
    assert_eq!(snapshot.next_page_offset, crate::cache::MAX_MESSAGES_PER_FOLDER);
    assert!(snapshot.has_more);
    assert_eq!(snapshot.uid_validity, Some(15));
    assert_eq!(snapshot.uid_next, Some(600));
}
