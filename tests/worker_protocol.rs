use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use aerc_rs::backend::MailBackend;
use aerc_rs::model::{
    Capabilities, Mailbox, MessageBody, MessageId, MessagePage, OutgoingMessage,
};
use aerc_rs::outgoing::{MailSender, SendReceipt};
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
            plain_fallback: None,
            html_source: None,
            html_text: None,
            html_lines: None,
            html_renderer: None,
            html_render_width: 0,
            attachment_count: 0,
            attachment_names: Vec::new(),
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

struct AppendingBackend {
    appended: Arc<Mutex<Vec<(String, Vec<u8>, bool)>>>,
    fail_append: bool,
}

impl MailBackend for AppendingBackend {
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
            plain_fallback: None,
            html_source: None,
            html_text: None,
            html_lines: None,
            html_renderer: None,
            html_render_width: 0,
            attachment_count: 0,
            attachment_names: Vec::new(),
        })
    }
    fn fetch_raw(&mut self, _mailbox: &str, _id: &MessageId) -> Result<Vec<u8>> {
        Ok(vec![])
    }
    fn append_message(&mut self, mailbox: &str, raw: &[u8], seen: bool) -> Result<()> {
        if self.fail_append { anyhow::bail!("simulated Sent append failure"); }
        self.appended
            .lock()
            .unwrap()
            .push((mailbox.to_owned(), raw.to_vec(), seen));
        Ok(())
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

struct FakeSender;

impl MailSender for FakeSender {
    fn send(&mut self, _message: &OutgoingMessage) -> Result<SendReceipt> {
        Ok(SendReceipt {
            raw_message: b"From: sender@example.com\r\nTo: receiver@example.com\r\n\r\nhello\r\n"
                .to_vec(),
            smtp_status: "2.0.0".to_owned(),
            server_files_sent_copy: false,
        })
    }
}

#[test]
fn worker_files_the_exact_smtp_bytes_in_sent() {
    let appended = Arc::new(Mutex::new(Vec::new()));
    let backend = AppendingBackend {
        appended: Arc::clone(&appended),
        fail_append: false,
    };
    let (tx, rx) = sync_channel(8);
    let mut worker = WorkerHandle::spawn(0, Box::new(backend), Some(Box::new(FakeSender)), tx);
    worker
        .post(ActionKind::Send {
            message: OutgoingMessage {
                from: "sender@example.com".to_owned(),
                to: "receiver@example.com".to_owned(),
                cc: String::new(),
                bcc: String::new(),
                html_body: None,
                attachments: Vec::new(),
                custom_headers: Vec::new(),
                subject: "hello".to_owned(),
                body: "body".to_owned(),
                in_reply_to: None,
                references: Vec::new(),
                forwarded_raw: None,
            },
            sent_mailbox: Some("Sent".to_owned()),
        })
        .unwrap();

    let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("expected worker event");
    };
    let WorkerEventKind::MessageSent {
        smtp_status,
        sent_mailbox,
        sent_copy_saved,
        sent_copy_warning,
    } = event.kind
    else {
        panic!("expected sent event");
    };
    assert_eq!(smtp_status, "2.0.0");
    assert_eq!(sent_mailbox.as_deref(), Some("Sent"));
    assert!(sent_copy_saved);
    assert_eq!(sent_copy_warning, None);

    let appended = appended.lock().unwrap();
    assert_eq!(appended.len(), 1);
    assert_eq!(appended[0].0, "Sent");
    assert!(appended[0].2);
    assert!(appended[0].1.windows(5).any(|window| window == b"hello"));
    worker.shutdown();
}


#[test]
fn sent_append_failure_is_reported_and_not_retried_as_smtp_send() {
    let appended = Arc::new(Mutex::new(Vec::new()));
    let backend = AppendingBackend { appended: Arc::clone(&appended), fail_append: true };
    let (tx, rx) = sync_channel(8);
    let mut worker = WorkerHandle::spawn(0, Box::new(backend), Some(Box::new(FakeSender)), tx);
    worker.post(ActionKind::Send {
        message: OutgoingMessage {
            from: "sender@example.com".into(), to: "receiver@example.com".into(),
            cc: String::new(), bcc: String::new(), subject: "failure test".into(),
            body: "test".into(), html_body: None, attachments: vec![], custom_headers: vec![],
            in_reply_to: None, references: vec![], forwarded_raw: None,
        },
        sent_mailbox: Some("Sent".into()),
    }).unwrap();
    let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("worker event expected");
    };
    let WorkerEventKind::MessageSent { sent_copy_saved, sent_copy_warning, .. } = event.kind else {
        panic!("SMTP accepted message but worker returned error");
    };
    assert!(!sent_copy_saved);
    assert!(sent_copy_warning.unwrap().contains("SMTP accepted"));
    assert!(appended.lock().unwrap().is_empty());
    worker.shutdown();
}
