use std::sync::mpsc::sync_channel;
use std::time::Duration;

use anyhow::Result;
use aerc_rs::backend::MailBackend;
use aerc_rs::model::{Capabilities, Mailbox, MessageBody, MessageId, MessageSummary};
use aerc_rs::worker::{ActionKind, RuntimeEvent, WorkerEventKind, WorkerHandle};

struct FakeBackend;

impl MailBackend for FakeBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities::MAILDIR
    }
    fn connect(&mut self) -> Result<()> { Ok(()) }
    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> { Ok(vec![]) }
    fn open_mailbox(&mut self, _name: &str) -> Result<Vec<MessageSummary>> { Ok(vec![]) }
    fn fetch_body(&mut self, _mailbox: &str, id: &MessageId) -> Result<MessageBody> {
        Ok(MessageBody { id: id.clone(), text: "body".into(), attachment_count: 0 })
    }
    fn set_seen(&mut self, _mailbox: &str, _id: &MessageId, _seen: bool) -> Result<()> { Ok(()) }
    fn delete(&mut self, _mailbox: &str, _id: &MessageId) -> Result<()> { Ok(()) }
    fn move_to(&mut self, _source: &str, _destination: &str, _id: &MessageId) -> Result<()> { Ok(()) }
}

#[test]
fn worker_preserves_request_correlation() {
    let (tx, rx) = sync_channel(8);
    let mut worker = WorkerHandle::spawn(3, Box::new(FakeBackend), tx);
    let request_id = worker.post(ActionKind::Connect).unwrap();
    let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("expected worker event");
    };
    assert_eq!(event.account, 3);
    assert_eq!(event.request_id, request_id);
    assert!(matches!(event.kind, WorkerEventKind::Connected { .. }));
    worker.shutdown();
}
