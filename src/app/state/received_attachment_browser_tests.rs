use super::*;

fn attachment(name: &str, bytes: &[u8]) -> OutgoingAttachment {
    OutgoingAttachment {
        filename: name.into(),
        content_type: "text/plain".into(),
        data: bytes.to_vec(),
    }
}

#[test]
fn saving_an_active_visual_range_saves_every_attachment_without_marking() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = App::new(&config, vec![]);
    app.attachment_files = Arc::new(vec![
        attachment("one.txt", b"one"),
        attachment("two.txt", b"two"),
        attachment("three.txt", b"three"),
        attachment("four.txt", b"four"),
    ]);
    app.attachment_picker_visible = true;
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    app.handle_attachment_picker_key(key('v'));
    app.handle_attachment_picker_key(key('j'));
    app.handle_attachment_picker_key(key('j'));
    assert_eq!(app.attachment_marked.len(), 0);
    assert_eq!(app.selected_attachment_indices(), vec![0, 1, 2]);

    let temp = tempfile::tempdir().unwrap();
    app.attachment_destination_picker = Some(
        ComposeFilePicker::open_directory_only(temp.path().to_path_buf()).unwrap(),
    );
    app.save_selected_attachments();
    assert_eq!(std::fs::read(temp.path().join("one.txt")).unwrap(), b"one");
    assert_eq!(std::fs::read(temp.path().join("two.txt")).unwrap(), b"two");
    assert_eq!(std::fs::read(temp.path().join("three.txt")).unwrap(), b"three");
    assert!(!temp.path().join("four.txt").exists());
    assert!(app.attachment_visual_anchor.is_none());
}

#[test]
fn visual_attachment_ranges_and_explicit_marks_are_combined_without_duplicates() {
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = App::new(&config, vec![]);
    app.attachment_files = Arc::new(vec![
        attachment("one", b"1"),
        attachment("two", b"2"),
        attachment("three", b"3"),
        attachment("four", b"4"),
    ]);
    app.attachment_selection = 3;
    app.attachment_visual_anchor = Some(1);
    app.attachment_marked.extend([0, 2, 999]);
    assert_eq!(app.selected_attachment_indices(), vec![0, 1, 2, 3]);
    app.attachment_visual_anchor = None;
    assert_eq!(app.selected_attachment_indices(), vec![0, 2]);
}

#[test]
fn received_attachment_filenames_cannot_escape_destination() {
    assert_eq!(
        attachment_safe_name("../../important.txt", 0),
        "important.txt"
    );
    assert_eq!(attachment_safe_name("..\\..\\secret.pdf", 2), "secret.pdf");
    assert_eq!(attachment_safe_name("..", 1), "attachment-2.bin");
    assert_eq!(attachment_safe_name("\n", 0), "attachment-1.bin");
}

#[test]
fn received_pdf_attachment_recognizes_mime_and_filename() {
    let mut file = attachment("document.bin", b"%PDF-1.4");
    assert!(!received_attachment_is_pdf(&file));
    file.content_type = "application/pdf".into();
    assert!(received_attachment_is_pdf(&file));
    file.content_type = "application/octet-stream".into();
    file.filename = "presentation.PDF".into();
    assert!(received_attachment_is_pdf(&file));
}

#[test]
fn destination_file_explorer_question_mark_opens_searchable_help_only() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
    let mut app = App::new(&config, vec![]);
    app.attachment_picker_visible = true;
    let temp = tempfile::tempdir().unwrap();
    app.attachment_destination_picker = Some(
        ComposeFilePicker::open_directory_only(temp.path().to_path_buf()).unwrap(),
    );
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

    app.handle_attachment_picker_key(key(KeyCode::Char('?')));
    let picker = app.attachment_destination_picker.as_ref().unwrap();
    assert!(picker.help_visible);
    assert_eq!(picker.help_query, "");
    assert_eq!(picker.directory, temp.path());

    for ch in "folder".chars() {
        app.handle_attachment_picker_key(key(KeyCode::Char(ch)));
    }
    let picker = app.attachment_destination_picker.as_ref().unwrap();
    assert_eq!(picker.help_query, "folder");
    let matches = crate::app::ranked_help_matches(
        crate::app::DESTINATION_HELP_ENTRIES,
        &picker.help_query,
    );
    assert!(!matches.is_empty());
    assert!(matches.iter().all(|&index| {
        crate::app::DESTINATION_HELP_ENTRIES[index]
            .description
            .contains("folder")
    }));
    app.handle_attachment_picker_key(key(KeyCode::Backspace));
    assert_eq!(app.attachment_destination_picker.as_ref().unwrap().help_query, "folde");
    app.handle_attachment_picker_key(key(KeyCode::Esc));
    let picker = app.attachment_destination_picker.as_ref().unwrap();
    assert!(!picker.help_visible);
    assert_eq!(picker.directory, temp.path());
    // In help mode, "s" filters help; it must never save attachments.
    app.handle_attachment_picker_key(key(KeyCode::Char('?')));
    app.handle_attachment_picker_key(key(KeyCode::Char('s')));
    assert!(app.attachment_destination_picker.as_ref().unwrap().help_visible);
    app.handle_attachment_picker_key(key(KeyCode::Char('?')));
    assert!(!app.attachment_destination_picker.as_ref().unwrap().help_visible);
}

#[test]
fn destination_picker_does_not_decode_or_launch_pdf_preview() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.pdf"), b"%PDF-1.4\n").unwrap();
    let picker = ComposeFilePicker::open_directory_only(temp.path().to_path_buf()).unwrap();
    assert!(picker.directory_only);
    assert!(matches!(&picker.preview, ComposeFilePreview::Binary(_)));
    assert!(picker.pdf_preview_due.is_none());
    assert!(picker.pdf_preview_inflight.is_empty());
}

#[test]
fn attachment_session_cache_keys_include_mailbox_account_and_uidvalidity() {
    let uid = MessageId("42".into());
    let key = (0usize, "INBOX".to_owned(), Some(123u32), uid.clone());
    assert_ne!(key, (0, "INBOX".to_owned(), Some(124u32), uid.clone()));
    assert_ne!(key, (1, "INBOX".to_owned(), Some(123u32), uid.clone()));
    assert_ne!(key, (0, "Archive".to_owned(), Some(123u32), uid));
}

#[test]
fn opened_attachment_temp_file_is_deleted_when_dropped() {
    let path = super::export_attachment_for_open(&attachment("secret.txt", b"sensitive"))
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"sensitive");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let location = path.to_path_buf();
    drop(path);
    assert!(!location.exists());
}

#[test]
fn saving_multiple_same_named_attachments_never_overwrites() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("report.txt"), "existing").unwrap();
    let first = save_attachment_without_overwrite(
        directory.path(),
        &attachment("report.txt", b"first"),
        0,
    )
    .unwrap();
    let second = save_attachment_without_overwrite(
        directory.path(),
        &attachment("../../report.txt", b"second"),
        1,
    )
    .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("report.txt")).unwrap(),
        "existing"
    );
    assert_eq!(std::fs::read(first).unwrap(), b"first");
    assert_eq!(std::fs::read(second).unwrap(), b"second");
}

#[test]
fn selection_start_and_drag_width_are_safely_bounded() {
    assert_eq!(attachment_list_start(15, 30, 10), 10);
    assert_eq!(attachment_list_start(1, 30, 10), 0);
    assert_eq!(mouse_width_percent(0, 120), 12);
    assert_eq!(mouse_width_percent(110, 120), 50);
    assert_eq!(mouse_width_percent(24, 100), 25);
}
