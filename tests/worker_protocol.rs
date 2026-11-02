use std::sync::mpsc::sync_channel;
use std::time::Duration;

use anyhow::Result;
use aerc_rs::backend::MailBackend;
use aerc_rs::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage};
use aerc_rs::worker::{ActionKind, RuntimeEvent, WorkerEventKind, WorkerHandle};

struct FakeBackend;

impl MailBackend for FakeBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities::MAILDIR
    }
    fn connect(&mut self) -> Result<()> {
        Ok(())
    }
    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
        Ok(vec![])
    }
    fn mailbox_status(&mut self, _name: &str) -> Result<(usize, usize)> {
        Ok((0, 0))
    }
    fn open_mailbox(&mut self, _name: &str, offset: usize, _limit: usize) -> Result<MessagePage> {
        Ok(MessagePage {
            messages: vec![],
            offset,
            next_offset: offset,
            has_more: false,
        })
    }
    fn fetch_body(&mut self, _mailbox: &str, id: &MessageId) -> Result<MessageBody> {
        Ok(MessageBody {
            id: id.clone(),
            plain_text: Some("body".into()),
            html_text: None,
            html_lines: None,
            html_renderer: None,
            attachment_count: 0,
        })
    }
    fn fetch_raw(&mut self, _mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        Ok(format!("Message-ID: <{id}>\r\n\r\nbody").into_bytes())
    }
    fn set_seen(&mut self, _mailbox: &str, _id: &MessageId, _seen: bool) -> Result<()> {
        Ok(())
    }
    fn delete(&mut self, _mailbox: &str, _id: &MessageId) -> Result<()> {
        Ok(())
    }
    fn move_to(&mut self, _source: &str, _destination: &str, _id: &MessageId) -> Result<()> {
        Ok(())
    }
}

#[test]
fn worker_preserves_request_correlation() {
    let (tx, rx) = sync_channel(8);
    let mut worker = WorkerHandle::spawn(3, Box::new(FakeBackend), None, tx);
    let request_id = worker.post(ActionKind::Connect).unwrap();
    let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("expected worker event");
    };
    assert_eq!(event.account, 3);
    assert_eq!(event.request_id, request_id);
    assert!(matches!(event.kind, WorkerEventKind::Connected { .. }));
    worker.shutdown();
}

#[test]
fn worker_preserves_pagination_metadata() {
    let (tx, rx) = sync_channel(8);
    let mut worker = WorkerHandle::spawn(0, Box::new(FakeBackend), None, tx);
    let request_id = worker
        .post(ActionKind::OpenMailbox {
            name: "INBOX".to_owned(),
            offset: 25,
            limit: 50,
        })
        .unwrap();
    let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("expected worker event");
    };
    assert_eq!(event.request_id, request_id);
    let WorkerEventKind::MailboxOpened { name, page, .. } = event.kind else {
        panic!("expected mailbox page");
    };
    assert_eq!(name, "INBOX");
    assert_eq!(page.offset, 25);
    assert_eq!(page.next_offset, 25);
    worker.shutdown();
}
