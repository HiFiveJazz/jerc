//! Tests for the parent module; kept alongside the implementation.
use super::*;
use crate::model::OutgoingAttachment;

#[test]
fn preview_text_is_bounded_and_does_not_display_binary() {
    let file = OutgoingAttachment {
        filename: "notes.txt".into(),
        content_type: "text/plain".into(),
        data: vec![b'a'; 64 * 1024],
    };
    let preview = attachment_text_preview(&file).unwrap();
    assert!(preview.len() < file.data.len());
    assert!(preview.contains("truncated"));
    let binary = OutgoingAttachment {
        filename: "file.bin".into(),
        content_type: "application/octet-stream".into(),
        data: vec![0, 1, 2],
    };
    assert!(attachment_text_preview(&binary).is_none());
}
