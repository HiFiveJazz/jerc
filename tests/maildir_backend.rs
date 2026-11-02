use std::fs;
use std::path::Path;

use aerc_rs::backend::{MailBackend, MaildirBackend};
use aerc_rs::model::MessageFlags;
use tempfile::TempDir;

fn make_maildir(root: &Path) {
    for dir in ["cur", "new", "tmp"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
}

fn sample_mail(subject: &str, body: &str) -> String {
    format!(
        "From: Jazz <jazz@example.com>\r\nSubject: {subject}\r\nDate: Sun, 1 Nov 2026 10:00:00 -0800\r\nMessage-ID: <{subject}@example.com>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    )
}

#[test]
fn maildir_is_usable_end_to_end() {
    let temp = TempDir::new().unwrap();
    make_maildir(temp.path());
    let archive = temp.path().join(".Archive");
    make_maildir(&archive);

    fs::write(
        temp.path().join("new").join("1001"),
        sample_mail("patch-one", "body one"),
    )
    .unwrap();
    let info_separator = if cfg!(windows) { '!' } else { ':' };
    fs::write(
        temp.path()
            .join("cur")
            .join(format!("1002{info_separator}2,S")),
        sample_mail("patch-two", "body two"),
    )
    .unwrap();

    let mut backend = MaildirBackend::new(temp.path().to_path_buf());
    backend.connect().unwrap();
    let folders = backend.list_mailboxes().unwrap();
    assert_eq!(folders[0].name, "INBOX");
    assert!(folders.iter().any(|m| m.name == "Archive"));

    let messages = backend.open_mailbox("INBOX").unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages.iter().filter(|m| m.unread()).count(), 1);

    let unread = messages.iter().find(|m| m.unread()).unwrap();
    let body = backend.fetch_body("INBOX", &unread.id).unwrap();
    assert!(body.text.contains("body one"));

    backend.set_seen("INBOX", &unread.id, true).unwrap();
    let messages = backend.open_mailbox("INBOX").unwrap();
    let updated = messages.iter().find(|m| m.subject == "patch-one").unwrap();
    assert!(updated.flags.contains(MessageFlags::SEEN));

    backend.move_to("INBOX", "Archive", &updated.id).unwrap();
    assert_eq!(backend.open_mailbox("INBOX").unwrap().len(), 1);
    assert_eq!(backend.open_mailbox("Archive").unwrap().len(), 1);
}
