use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use jerc::backend::MailBackend;
use jerc::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage, OutgoingMessage};
use jerc::outgoing::{MailSender, SendReceipt};
use jerc::worker::{ActionKind, RuntimeEvent, WorkerEvent, WorkerEventKind, WorkerHandle};

// Foreground actions carry a worker generation so the runtime can reject
// results from a replaced, stalled IMAP session. A freshly spawned worker
// always starts at generation zero.
fn recv_initial_foreground(rx: &Receiver<RuntimeEvent>) -> WorkerEvent {
    match rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker event")
    {
        RuntimeEvent::ForegroundWorker {
            generation: 0,
            event,
        } => event,
        other => panic!("expected generation-0 foreground event, got {other:?}"),
    }
}

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

// A deterministic stand-in for TCP/TLS/IMAP-greeting stalls. No internet,
// OAuth credentials, or slow sleeps are required to reproduce the original UI
// freeze. The worker is stuck in connect() until the test releases it.
struct PausedConnect {
    started: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl MailBackend for PausedConnect {
    fn capabilities(&self) -> Capabilities {
        Capabilities::IMAP_BASIC
    }
    fn connect(&mut self) -> Result<()> {
        let _ = self.started.send(());
        let _ = self.release.recv();
        Ok(())
    }
    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
        Ok(vec![])
    }
    fn mailbox_status(&mut self, _: &str) -> Result<(usize, usize)> {
        Ok((0, 0))
    }
    fn open_mailbox(&mut self, _: &str, offset: usize, _: usize) -> Result<MessagePage> {
        Ok(MessagePage {
            messages: vec![],
            offset,
            next_offset: offset,
            has_more: false,
        })
    }
    fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody> {
        FakeBackend.fetch_body(mailbox, id)
    }
    fn fetch_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        FakeBackend.fetch_raw(mailbox, id)
    }
    fn set_seen(&mut self, _: &str, _: &MessageId, _: bool) -> Result<()> {
        Ok(())
    }
    fn delete(&mut self, _: &str, _: &MessageId) -> Result<()> {
        Ok(())
    }
    fn move_to(&mut self, _: &str, _: &str, _: &MessageId) -> Result<()> {
        Ok(())
    }
}

// Run the reload on an isolated surrogate UI thread so a regression in
// shutdown cannot hang the entire test suite forever. Always unblock IMAP
// before reporting a failed assertion.
fn assert_reload_is_nonblocking(worker: WorkerHandle, release: std::sync::mpsc::SyncSender<()>) {
    let (done_tx, done_rx) = sync_channel(1);
    let reload = std::thread::spawn(move || {
        let mut worker = worker;
        let started = std::time::Instant::now();
        worker.retire_for_reload();
        drop(worker);
        let _ = done_tx.send(started.elapsed());
    });
    let elapsed = done_rx.recv_timeout(Duration::from_millis(750));
    release.send(()).unwrap();
    reload.join().unwrap();
    assert!(
        elapsed.is_ok_and(|elapsed| elapsed < Duration::from_millis(750)),
        "account reload waited for a blocked network worker",
    );
}

#[test]
fn reload_retirement_does_not_join_a_blocked_initial_imap_connect() {
    let (started_tx, started_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let (events_tx, _events_rx) = sync_channel(8);
    let backend = PausedConnect {
        started: started_tx,
        release: release_rx,
    };
    let worker = WorkerHandle::spawn(0, Box::new(backend), None, None, events_tx);
    worker.post(ActionKind::Connect).unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("connect was entered");
    assert_reload_is_nonblocking(worker, release_tx);
}

#[test]
fn reload_retirement_does_not_join_a_stalled_status_connection() {
    let (started_tx, started_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let (events_tx, _events_rx) = sync_channel(8);
    let backend = PausedConnect {
        started: started_tx,
        release: release_rx,
    };
    let worker = WorkerHandle::spawn(
        0,
        Box::new(FakeBackend),
        Some(Box::new(backend)),
        None,
        events_tx,
    );
    worker
        .post(ActionKind::MailboxStatus("INBOX".into()))
        .unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("STATUS connected");
    assert_reload_is_nonblocking(worker, release_tx);
}

#[test]
fn reload_retirement_does_not_wait_for_full_action_queue() {
    let (started_tx, started_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let (events_tx, _events_rx) = sync_channel(128);
    let backend = PausedConnect {
        started: started_tx,
        release: release_rx,
    };
    let worker = WorkerHandle::spawn(0, Box::new(backend), None, None, events_tx);
    worker.post(ActionKind::Connect).unwrap();
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    for _ in 0..64 {
        worker.post(ActionKind::ListMailboxes).unwrap();
    }
    assert_reload_is_nonblocking(worker, release_tx);
}

#[test]
fn account_reload_waits_for_write_ack_not_merely_backend_completion() {
    let (events_tx, events_rx) = sync_channel(8);
    let mut worker = WorkerHandle::spawn(0, Box::new(FakeBackend), None, None, events_tx);
    let id = worker
        .post(ActionKind::Delete {
            mailbox: "INBOX".into(),
            id: MessageId("old-message".into()),
        })
        .unwrap();
    assert!(worker.has_unacknowledged_writes());
    let event = recv_initial_foreground(&events_rx);
    assert_eq!(event.request_id, id);
    assert!(
        worker.has_unacknowledged_writes(),
        "queued ACK is not yet applied"
    );
    worker.acknowledge_foreground_event(id);
    assert!(!worker.has_unacknowledged_writes());
    worker.retire_for_reload();
}

#[test]
fn a_failed_connect_can_be_retried_without_rebuilding_the_worker() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct FlakyConnect(Arc<AtomicUsize>);
    impl MailBackend for FlakyConnect {
        fn capabilities(&self) -> Capabilities {
            Capabilities::IMAP_BASIC
        }
        fn connect(&mut self) -> Result<()> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("simulated TCP failure")
            }
            Ok(())
        }
        fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
            Ok(vec![])
        }
        fn mailbox_status(&mut self, _: &str) -> Result<(usize, usize)> {
            Ok((0, 0))
        }
        fn open_mailbox(&mut self, _: &str, offset: usize, _: usize) -> Result<MessagePage> {
            Ok(MessagePage {
                messages: vec![],
                offset,
                next_offset: offset,
                has_more: false,
            })
        }
        fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody> {
            FakeBackend.fetch_body(mailbox, id)
        }
        fn fetch_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
            FakeBackend.fetch_raw(mailbox, id)
        }
        fn set_seen(&mut self, _: &str, _: &MessageId, _: bool) -> Result<()> {
            Ok(())
        }
        fn delete(&mut self, _: &str, _: &MessageId) -> Result<()> {
            Ok(())
        }
        fn move_to(&mut self, _: &str, _: &str, _: &MessageId) -> Result<()> {
            Ok(())
        }
    }
    let attempts = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = sync_channel(8);
    let mut worker = WorkerHandle::spawn(
        0,
        Box::new(FlakyConnect(Arc::clone(&attempts))),
        None,
        None,
        tx,
    );
    let first = worker.post(ActionKind::Connect).unwrap();
    let error = recv_initial_foreground(&rx);
    assert_eq!(first, error.request_id);
    assert!(matches!(error.kind, WorkerEventKind::Error(_)));
    let second = worker.post(ActionKind::Connect).unwrap();
    let connected = recv_initial_foreground(&rx);
    assert_eq!(second, connected.request_id);
    assert!(matches!(connected.kind, WorkerEventKind::Connected { .. }));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    worker.shutdown();
}

#[test]
fn worker_preserves_request_correlation() {
    let (tx, rx) = sync_channel(8);
    let mut worker = WorkerHandle::spawn(3, Box::new(FakeBackend), None, None, tx);
    let request_id = worker.post(ActionKind::Connect).unwrap();
    let event = recv_initial_foreground(&rx);
    assert_eq!(event.account, 3);
    assert_eq!(event.request_id, request_id);
    assert!(matches!(event.kind, WorkerEventKind::Connected { .. }));
    worker.shutdown();
}

#[test]
fn worker_preserves_pagination_metadata() {
    let (tx, rx) = sync_channel(8);
    let mut worker = WorkerHandle::spawn(0, Box::new(FakeBackend), None, None, tx);
    let request_id = worker
        .post(ActionKind::OpenMailbox {
            name: "INBOX".to_owned(),
            offset: 25,
            limit: 50,
            hint: None,
        })
        .unwrap();
    let event = recv_initial_foreground(&rx);
    assert_eq!(event.request_id, request_id);
    let WorkerEventKind::MailboxOpened { name, page, .. } = event.kind else {
        panic!("expected mailbox page");
    };
    assert_eq!(name, "INBOX");
    assert_eq!(page.offset, 25);
    assert_eq!(page.next_offset, 25);
    worker.shutdown();
}

// Keep the recorded mailbox, serialized message, and seen flag together.
type AppendedMessage = (String, Vec<u8>, bool);
type AppendLog = Arc<Mutex<Vec<AppendedMessage>>>;

struct AppendingBackend {
    appended: AppendLog,
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
        if self.fail_append {
            anyhow::bail!("simulated Sent append failure");
        }
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
    let mut worker =
        WorkerHandle::spawn(0, Box::new(backend), None, Some(Box::new(FakeSender)), tx);
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
            copy_to: None,
        })
        .unwrap();

    let event = recv_initial_foreground(&rx);
    let WorkerEventKind::MessageSent {
        smtp_status,
        sent_mailbox,
        sent_copy_saved,
        sent_copy_warning,
        sent_copy_disabled,
    } = event.kind
    else {
        panic!("expected sent event");
    };
    assert_eq!(smtp_status, "2.0.0");
    assert_eq!(sent_mailbox.as_deref(), Some("Sent"));
    assert!(sent_copy_saved);
    assert!(!sent_copy_disabled);
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
    let backend = AppendingBackend {
        appended: Arc::clone(&appended),
        fail_append: true,
    };
    let (tx, rx) = sync_channel(8);
    let mut worker =
        WorkerHandle::spawn(0, Box::new(backend), None, Some(Box::new(FakeSender)), tx);
    worker
        .post(ActionKind::Send {
            message: OutgoingMessage {
                from: "sender@example.com".into(),
                to: "receiver@example.com".into(),
                cc: String::new(),
                bcc: String::new(),
                subject: "failure test".into(),
                body: "test".into(),
                html_body: None,
                attachments: vec![],
                custom_headers: vec![],
                in_reply_to: None,
                references: vec![],
                forwarded_raw: None,
            },
            sent_mailbox: Some("Sent".into()),
            copy_to: None,
        })
        .unwrap();
    let event = recv_initial_foreground(&rx);
    let WorkerEventKind::MessageSent {
        sent_copy_saved,
        sent_copy_warning,
        ..
    } = event.kind
    else {
        panic!("SMTP accepted message but worker returned error");
    };
    assert!(!sent_copy_saved);
    assert!(sent_copy_warning.unwrap().contains("SMTP accepted"));
    assert!(appended.lock().unwrap().is_empty());
    worker.shutdown();
}

#[test]
fn sent_copy_can_be_disabled_without_appending_to_server() {
    let appended = Arc::new(Mutex::new(Vec::new()));
    let backend = AppendingBackend {
        appended: Arc::clone(&appended),
        fail_append: false,
    };
    let (tx, rx) = sync_channel(8);
    let mut worker =
        WorkerHandle::spawn(0, Box::new(backend), None, Some(Box::new(FakeSender)), tx);
    worker
        .post(ActionKind::Send {
            message: OutgoingMessage {
                from: "sender@example.com".into(),
                to: "receiver@example.com".into(),
                cc: String::new(),
                bcc: String::new(),
                subject: "no copy".into(),
                body: "test".into(),
                html_body: None,
                attachments: vec![],
                custom_headers: vec![],
                in_reply_to: None,
                references: vec![],
                forwarded_raw: None,
            },
            sent_mailbox: Some("Sent".into()),
            copy_to: Some(Vec::new()),
        })
        .unwrap();
    let event = recv_initial_foreground(&rx);
    let WorkerEventKind::MessageSent {
        sent_copy_saved,
        sent_copy_disabled,
        ..
    } = event.kind
    else {
        panic!("SMTP accepted message but worker returned error");
    };
    assert!(!sent_copy_saved);
    assert!(sent_copy_disabled);
    assert!(appended.lock().unwrap().is_empty());
    worker.shutdown();
}

#[test]
fn sent_copy_explicit_multiple_folders_appends_both() {
    let appended = Arc::new(Mutex::new(Vec::new()));
    let backend = AppendingBackend {
        appended: Arc::clone(&appended),
        fail_append: false,
    };
    let (tx, rx) = sync_channel(8);
    let mut worker =
        WorkerHandle::spawn(0, Box::new(backend), None, Some(Box::new(FakeSender)), tx);
    worker
        .post(ActionKind::Send {
            message: OutgoingMessage {
                from: "sender@example.com".into(),
                to: "receiver@example.com".into(),
                cc: String::new(),
                bcc: String::new(),
                subject: "multiple".into(),
                body: "test".into(),
                html_body: None,
                attachments: vec![],
                custom_headers: vec![],
                in_reply_to: None,
                references: vec![],
                forwarded_raw: None,
            },
            sent_mailbox: Some("Sent".into()),
            copy_to: Some(vec!["Sent".into(), "Project Archive".into()]),
        })
        .unwrap();
    let event = recv_initial_foreground(&rx);
    let WorkerEventKind::MessageSent {
        sent_copy_saved,
        sent_copy_warning,
        ..
    } = event.kind
    else {
        panic!("SMTP accepted message but worker returned error");
    };
    assert!(sent_copy_saved);
    assert!(sent_copy_warning.is_none());
    let appended = appended.lock().unwrap();
    assert_eq!(
        appended
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["Sent", "Project Archive"]
    );
    drop(appended);
    worker.shutdown();
}
