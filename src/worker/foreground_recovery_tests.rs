//! Tests for the parent module; kept alongside the implementation.
use super::*;
use crate::backend::MailBackend;
use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage};
use std::sync::mpsc::{self, Receiver, SyncSender};

struct StalledList {
    started: SyncSender<()>,
    release: Receiver<()>,
}

impl MailBackend for StalledList {
    fn capabilities(&self) -> Capabilities {
        Capabilities::IMAP_BASIC
    }
    fn connect(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn list_mailboxes(&mut self) -> anyhow::Result<Vec<Mailbox>> {
        let _ = self.started.send(());
        let _ = self.release.recv_timeout(Duration::from_secs(3));
        Ok(Vec::new())
    }
    fn mailbox_status(&mut self, _: &str) -> anyhow::Result<(usize, usize)> {
        Ok((0, 0))
    }
    fn open_mailbox(&mut self, _: &str, _: usize, _: usize) -> anyhow::Result<MessagePage> {
        Ok(MessagePage {
            messages: Vec::new(),
            offset: 0,
            next_offset: 0,
            has_more: false,
        })
    }
    fn fetch_body(&mut self, _: &str, _: &MessageId) -> anyhow::Result<MessageBody> {
        anyhow::bail!("not used")
    }
    fn fetch_raw(&mut self, _: &str, _: &MessageId) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("not used")
    }
    fn set_seen(&mut self, _: &str, _: &MessageId, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn delete(&mut self, _: &str, _: &MessageId) -> anyhow::Result<()> {
        Ok(())
    }
    fn move_to(&mut self, _: &str, _: &str, _: &MessageId) -> anyhow::Result<()> {
        Ok(())
    }
}

fn fixture() -> (
    tempfile::TempDir,
    WorkerHandle,
    Receiver<RuntimeEvent>,
    Receiver<()>,
    SyncSender<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    for folder in ["cur", "new", "tmp"] {
        std::fs::create_dir(dir.path().join(folder)).unwrap();
    }
    let (started, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release) = mpsc::sync_channel(1);
    let (events, rx) = mpsc::sync_channel(32);
    let mut worker = WorkerHandle::spawn(
        0,
        Box::new(StalledList { started, release }),
        None,
        None,
        events.clone(),
    );
    let config = crate::config::Config::single_maildir("Test".into(), dir.path().into());
    worker.enable_foreground_recovery(0, config.accounts[0].clone(), events);
    (dir, worker, rx, started_rx, release_tx)
}

#[test]
fn a_stalled_list_does_not_hold_the_replacement_connect_in_its_queue() {
    let (_dir, mut worker, rx, started, release) = fixture();
    worker.post(ActionKind::ListMailboxes).unwrap();
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    let before = Instant::now();
    assert!(worker.recover_stalled_foreground().unwrap());
    let request = worker.post(ActionKind::Connect).unwrap();
    let RuntimeEvent::ForegroundWorker { generation, event } = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("replacement must run during old LIST")
    else {
        panic!("expected foreground worker event")
    };
    assert_eq!(generation, 1);
    assert_eq!(event.request_id, request);
    assert!(matches!(event.kind, WorkerEventKind::Connected { .. }));
    assert!(before.elapsed() < Duration::from_secs(2));
    release.send(()).unwrap();
    // Even once the original request finishes, it cannot report an old
    // LIST snapshot or an old socket error into the recovered session.
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    worker.shutdown();
}

#[test]
fn write_queued_behind_list_prevents_worker_retirement() {
    let (_dir, mut worker, _rx, started, release) = fixture();
    worker.post(ActionKind::ListMailboxes).unwrap();
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    worker
        .post(ActionKind::Delete {
            mailbox: "INBOX".into(),
            id: MessageId("x".into()),
        })
        .unwrap();
    assert!(!worker.recover_stalled_foreground().unwrap());
    assert_eq!(worker.foreground_generation(), 0);
    release.send(()).unwrap();
    worker.shutdown();
}

#[test]
fn recovery_has_backoff_even_if_its_new_connection_is_stalled() {
    let (_dir, mut worker, _rx, started, release) = fixture();
    worker.post(ActionKind::ListMailboxes).unwrap();
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(worker.recover_stalled_foreground().unwrap());
    assert!(!worker.recover_stalled_foreground().unwrap());
    assert_eq!(worker.foreground_generation(), 1);
    release.send(()).unwrap();
    worker.shutdown();
}
