//! Tests for the parent module; kept alongside the implementation.
use super::{
    Action, ActionKind, RuntimeEvent, WorkerEventKind, WorkerHandle, compact_navigation_queue,
    pop_background_action, write_export_stream,
};
use crate::backend::MailBackend;
use crate::model::{CacheIdentity, Capabilities, Mailbox, MessageBody, MessageId, MessagePage};
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

#[test]
fn typed_diagnostics_survive_worker_boundary() {
    use crate::backend::{BackendError, ErrorKind};
    let error = anyhow::Error::new(BackendError::new(
        ErrorKind::PermissionDenied,
        "JMAP access denied",
    ))
    .context("connection reset in malformed server metadata");
    let diagnostic = super::WorkerError::from_error(error);
    assert_eq!(diagnostic.kind, ErrorKind::PermissionDenied);
    assert!(!diagnostic.kind.is_connection_failure());
    assert!(!diagnostic.background_status);
    let diagnostic = super::WorkerError::background_status(
        BackendError::new(ErrorKind::Timeout, "JMAP timeout").into(),
    );
    assert_eq!(diagnostic.kind, ErrorKind::Timeout);
    assert!(diagnostic.background_status);
}

/// A controllable IMAP watch: it emits a wake, stays in IDLE until
/// cancelled, and deliberately calls the callback once after cancellation
/// to verify stale notifications cannot enter the background queue.
struct WatchedBackend {
    visited: SyncSender<String>,
    account_wide: bool,
}

impl MailBackend for WatchedBackend {
    fn watch_is_account_wide(&self) -> bool {
        self.account_wide
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::IMAP_BASIC
    }
    fn connect(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn list_mailboxes(&mut self) -> anyhow::Result<Vec<Mailbox>> {
        Ok(Vec::new())
    }
    fn watch_mailbox(
        &mut self,
        name: &str,
        cancel: &std::sync::atomic::AtomicBool,
        changed: &mut dyn FnMut(),
    ) -> anyhow::Result<()> {
        self.visited.send(name.to_owned()).unwrap();
        changed();
        while !cancel.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        changed(); // must not send a stale wake after a switch
        Ok(())
    }
    fn mailbox_status(&mut self, _: &str) -> anyhow::Result<(usize, usize)> {
        Ok((0, 0))
    }
    fn open_mailbox(&mut self, _: &str, _: usize, _: usize) -> anyhow::Result<MessagePage> {
        unreachable!("IDLE backend never fetches messages")
    }
    fn fetch_body(&mut self, _: &str, _: &MessageId) -> anyhow::Result<MessageBody> {
        unreachable!("IDLE backend never fetches message bodies")
    }
    fn fetch_raw(&mut self, _: &str, _: &MessageId) -> anyhow::Result<Vec<u8>> {
        unreachable!("IDLE backend never fetches raw messages")
    }
    fn set_seen(&mut self, _: &str, _: &MessageId, _: bool) -> anyhow::Result<()> {
        unreachable!("IDLE backend never modifies messages")
    }
    fn delete(&mut self, _: &str, _: &MessageId) -> anyhow::Result<()> {
        unreachable!("IDLE backend never deletes messages")
    }
    fn move_to(&mut self, _: &str, _: &str, _: &MessageId) -> anyhow::Result<()> {
        unreachable!("IDLE backend never moves messages")
    }
}

#[test]
fn account_wide_watch_does_not_discover_or_select_an_inbox() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (visited_tx, visited_rx) = mpsc::sync_channel(8);
    let (wake_tx, wake_rx) = mpsc::sync_channel(8);
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let runner_stop = stop.clone();
    let join = std::thread::spawn(move || {
        super::run_idle_worker(
            Box::new(WatchedBackend {
                visited: visited_tx,
                account_wide: true,
            }),
            runner_stop,
            wake_tx,
        );
    });
    assert_eq!(visited_rx.recv_timeout(Duration::from_secs(1)).unwrap(), "");
    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)).unwrap().kind,
        ActionKind::WatchAccountChanged
    ));
    stop.store(true, Ordering::Release);
    join.join().unwrap();
    assert!(wake_rx.try_recv().is_err(), "no wake after shutdown");
}

#[test]
fn selected_idle_switch_cancels_old_watcher_and_drops_stale_wakes() {
    use super::{SelectedIdleTarget, run_selected_idle_worker};
    use std::sync::Arc;
    let (visited_tx, visited_rx) = mpsc::sync_channel(8);
    let (wake_tx, wake_rx) = mpsc::sync_channel(8);
    let target = Arc::new(SelectedIdleTarget::new());
    let runner_target = Arc::clone(&target);
    let join = std::thread::spawn(move || {
        run_selected_idle_worker(
            Box::new(WatchedBackend {
                visited: visited_tx,
                account_wide: false,
            }),
            runner_target,
            wake_tx,
        );
    });
    // The additional connection remains completely dormant on Inbox.
    assert!(visited_rx.recv_timeout(Duration::from_millis(50)).is_err());
    target.set_mailbox(Some("Drafts"));
    assert_eq!(
        visited_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        "Drafts"
    );
    assert!(
        matches!(wake_rx.recv_timeout(Duration::from_secs(1)).unwrap().kind,
            ActionKind::WatchMailboxChanged(name) if name == "Drafts")
    );

    target.set_mailbox(Some("Sent"));
    assert_eq!(
        visited_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        "Sent"
    );
    assert!(
        matches!(wake_rx.recv_timeout(Duration::from_secs(1)).unwrap().kind,
            ActionKind::WatchMailboxChanged(name) if name == "Sent")
    );
    assert!(
        wake_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "retired watcher must not deliver a stale change event"
    );

    target.set_mailbox(None);
    target.set_mailbox(Some("Trash"));
    assert_eq!(
        visited_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        "Trash"
    );
    target.stop();
    join.join().unwrap();
    // Shutdown is permanent: a late event cannot restart the watcher.
    target.set_mailbox(Some("Drafts"));
    assert!(target.wait_for_mailbox().is_none());
}

#[test]
fn idle_target_is_idempotent_and_cancels_on_account_switch() {
    use super::SelectedIdleTarget;
    use std::sync::atomic::Ordering;
    let target = SelectedIdleTarget::new();
    target.set_mailbox(Some("Drafts"));
    let (folder, revision, cancel) = target.wait_for_mailbox().unwrap();
    assert_eq!(folder, "Drafts");
    target.set_mailbox(Some("Drafts"));
    let (_, unchanged, _) = target.wait_for_mailbox().unwrap();
    assert_eq!(revision, unchanged);
    assert!(!cancel.load(Ordering::Acquire));
    target.set_mailbox(None);
    assert!(cancel.load(Ordering::Acquire));
    target.stop();
    assert!(target.wait_for_mailbox().is_none());
}

#[cfg(unix)]
#[test]
fn raw_export_does_not_follow_existing_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"untouched").unwrap();
    let export = dir.path().join("message.eml");
    std::os::unix::fs::symlink(&victim, &export).unwrap();
    write_export_stream(&export, |writer| {
        std::io::Write::write_all(writer, b"confidential")?;
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
    assert_eq!(std::fs::read(&export).unwrap(), b"confidential");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&export).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn failed_streaming_export_keeps_existing_target_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("message.eml");
    std::fs::write(&path, b"original").unwrap();
    let result = write_export_stream(&path, |out| {
        std::io::Write::write_all(out, b"partial")?;
        anyhow::bail!("simulated disconnect")
    });
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

fn open(name: &str) -> Action {
    Action::new(ActionKind::OpenMailbox {
        name: name.into(),
        offset: 0,
        limit: 50,
        hint: None,
    })
}

fn body(mailbox: &str) -> Action {
    Action::new(ActionKind::FetchBody {
        mailbox: mailbox.into(),
        id: MessageId("imap-uid:1".into()),
    })
}

#[test]
fn coalescing_preserves_only_latest_navigation_and_keeps_mutations() {
    let mut pending = VecDeque::from([
        open("A"),
        body("A"),
        Action::new(ActionKind::SetSeen {
            mailbox: "A".into(),
            id: MessageId("imap-uid:1".into()),
            seen: true,
        }),
        open("B"),
        body("B"),
        open("C"),
        body("C"),
    ]);
    compact_navigation_queue(&mut pending);
    assert_eq!(pending.len(), 3);
    assert!(matches!(&pending[0].kind, ActionKind::SetSeen { .. }));
    assert!(matches!(&pending[1].kind, ActionKind::OpenMailbox { name, .. } if name == "C"));
    assert!(matches!(&pending[2].kind, ActionKind::FetchBody { mailbox, .. } if mailbox == "C"));
}

#[test]
fn coalescing_discards_body_for_previous_folder_even_if_queued_after_open() {
    let mut pending = VecDeque::from([open("new"), body("old")]);
    compact_navigation_queue(&mut pending);
    assert_eq!(pending.len(), 1);
    assert!(matches!(&pending[0].kind, ActionKind::OpenMailbox { name, .. } if name == "new"));
}

#[test]
fn warm_folder_never_overtakes_interactive_compose_or_status() {
    let mut pending = VecDeque::from([
        Action::new(ActionKind::WarmMailbox {
            name: "Drafts".into(),
            limit: 40,
            hint: None,
        }),
        Action::new(ActionKind::MailboxStatus("INBOX".into())),
        Action::new(ActionKind::PrepareCompose {
            mailbox: "INBOX".into(),
            id: MessageId("imap-uid:9:4".into()),
            summary: crate::mail::parse_summary(
                MessageId("imap-uid:9:4".into()),
                b"From: a@example.org\r\nSubject: sample\r\n\r\nbody",
                50,
                crate::model::MessageFlags::empty(),
            )
            .unwrap(),
            intent: crate::app::ComposeIntent::Forward,
            own_addresses: vec![],
            cache: None,
        }),
    ]);
    assert!(matches!(
        pop_background_action(&mut pending, 0).unwrap().kind,
        ActionKind::PrepareCompose { .. }
    ));
    assert!(matches!(
        pop_background_action(&mut pending, 0).unwrap().kind,
        ActionKind::MailboxStatus(_)
    ));
    assert!(
        matches!(pop_background_action(&mut pending, 0).unwrap().kind,
            ActionKind::WarmMailbox { name, .. } if name == "Drafts")
    );
}

#[test]
fn folder_preload_uses_background_worker() {
    let (tx, rx) = mpsc::sync_channel(8);
    let mut worker = WorkerHandle::spawn(
        0,
        Box::new(TestBackend {
            status_started: None,
            status_release: None,
        }),
        Some(Box::new(TestBackend {
            status_started: None,
            status_release: None,
        })),
        None,
        tx,
    );
    let request = worker
        .post(ActionKind::WarmMailbox {
            name: "Drafts".into(),
            limit: 40,
            hint: None,
        })
        .unwrap();
    let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("expected background event");
    };
    assert_eq!(event.request_id, request);
    assert!(
        matches!(event.kind, WorkerEventKind::MailboxWarmed { name, .. }
            if name == "Drafts")
    );
    worker.shutdown();
}

#[test]
fn forwarding_raw_cache_hit_prepares_without_network_or_attachment_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let cache = crate::cache::RawStore {
        dir: dir.path().join("raw"),
    };
    let id = MessageId("imap-uid:42:7".into());
    let raw = b"From: sender@example.org\r\nSubject: kernel thread\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nforwarded text\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=kernel.bin\r\n\r\n\x01\x02\x03\r\n--x--\r\n";
    cache
        .write("INBOX", &CacheIdentity::Imap(42), &id, raw)
        .unwrap();
    let summary = crate::mail::parse_summary(
        id.clone(),
        raw,
        raw.len() as u64,
        crate::model::MessageFlags::empty(),
    )
    .unwrap();
    struct Offline;
    impl MailBackend for Offline {
        fn capabilities(&self) -> Capabilities {
            Capabilities::IMAP_BASIC
        }
        fn connect(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
        fn list_mailboxes(&mut self) -> anyhow::Result<Vec<Mailbox>> {
            anyhow::bail!("offline")
        }
        fn mailbox_status(&mut self, _: &str) -> anyhow::Result<(usize, usize)> {
            anyhow::bail!("offline")
        }
        fn open_mailbox(&mut self, _: &str, _: usize, _: usize) -> anyhow::Result<MessagePage> {
            anyhow::bail!("offline")
        }
        fn fetch_body(&mut self, _: &str, _: &MessageId) -> anyhow::Result<MessageBody> {
            anyhow::bail!("offline")
        }
        fn fetch_raw(&mut self, _: &str, _: &MessageId) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("offline")
        }
        fn set_seen(&mut self, _: &str, _: &MessageId, _: bool) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
        fn delete(&mut self, _: &str, _: &MessageId) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
        fn move_to(&mut self, _: &str, _: &str, _: &MessageId) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
    }
    let (tx, rx) = mpsc::sync_channel(8);
    let mut worker = WorkerHandle::spawn(0, Box::new(Offline), Some(Box::new(Offline)), None, tx);
    let request = worker
        .post(ActionKind::PrepareCompose {
            mailbox: "INBOX".into(),
            id,
            summary,
            intent: crate::app::ComposeIntent::Forward,
            own_addresses: vec![],
            cache: Some((cache.dir, CacheIdentity::Imap(42))),
        })
        .unwrap();
    let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("expected prepared compose");
    };
    assert_eq!(event.request_id, request);
    match event.kind {
        WorkerEventKind::ComposePrepared {
            result: Ok(composer),
            ..
        } => {
            assert!(composer.body.contains("forwarded text"));
            assert_eq!(composer.forwarded_raw.as_deref(), Some(&raw[..]));
            assert!(composer.attachment_paths.is_empty());
        }
        other => panic!("unexpected event {other:?}"),
    }
    worker.shutdown();
}

#[test]
fn background_scheduler_yields_to_status_after_four_bodies() {
    let mut pending = VecDeque::from([
        Action::new(ActionKind::MailboxStatus("INBOX".into())),
        Action::new(ActionKind::PrefetchBodies {
            mailbox: "A".into(),
            ids: vec![],
        }),
        Action::new(ActionKind::PrefetchBodies {
            mailbox: "B".into(),
            ids: vec![],
        }),
    ]);
    let next = pop_background_action(&mut pending, 0).unwrap();
    assert!(matches!(next.kind, ActionKind::PrefetchBodies { mailbox, .. } if mailbox == "B"));
    assert_eq!(pending.len(), 1); // Older preview window discarded.
    assert!(matches!(&pending[0].kind, ActionKind::MailboxStatus(_)));

    pending.push_back(Action::new(ActionKind::PrefetchBodies {
        mailbox: "C".into(),
        ids: vec![],
    }));
    let next = pop_background_action(&mut pending, 4).unwrap();
    assert!(matches!(next.kind, ActionKind::MailboxStatus(_)));
    let next = pop_background_action(&mut pending, 0).unwrap();
    assert!(matches!(next.kind, ActionKind::PrefetchBodies { mailbox, .. } if mailbox == "C"));
}

#[test]
fn selected_message_beats_status_and_obsolete_body_requests() {
    let mut pending = VecDeque::from([
        Action::new(ActionKind::MailboxStatus("INBOX".into())),
        Action::new(ActionKind::PrefetchBodies {
            mailbox: "INBOX".into(),
            ids: vec![],
        }),
        body("INBOX"),
        body("INBOX"),
    ]);
    let expected = pending.back().unwrap().id;
    let next = pop_background_action(&mut pending, 4).unwrap();
    assert_eq!(next.id, expected);
    assert_eq!(pending.len(), 2);
    assert!(
        pending
            .iter()
            .all(|a| !matches!(&a.kind, ActionKind::FetchBody { .. }))
    );
    assert!(matches!(
        pop_background_action(&mut pending, 4).unwrap().kind,
        ActionKind::MailboxStatus(_)
    ));
}

struct TestBackend {
    status_started: Option<SyncSender<()>>,
    status_release: Option<Receiver<()>>,
}

impl MailBackend for TestBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities::IMAP_BASIC
    }
    fn connect(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn list_mailboxes(&mut self) -> anyhow::Result<Vec<Mailbox>> {
        Ok(vec![])
    }
    fn mailbox_status(&mut self, _name: &str) -> anyhow::Result<(usize, usize)> {
        if let Some(started) = &self.status_started {
            let _ = started.send(());
        }
        if let Some(release) = &self.status_release {
            let _ = release.recv_timeout(Duration::from_secs(2));
        }
        Ok((5, 2))
    }
    fn open_mailbox(
        &mut self,
        _name: &str,
        offset: usize,
        _limit: usize,
    ) -> anyhow::Result<MessagePage> {
        if let Some(started) = &self.status_started {
            let _ = started.send(());
        }
        if let Some(release) = &self.status_release {
            let _ = release.recv_timeout(Duration::from_secs(2));
        }
        Ok(MessagePage {
            messages: vec![],
            offset,
            next_offset: offset,
            has_more: false,
        })
    }
    fn fetch_body(&mut self, _mailbox: &str, id: &MessageId) -> anyhow::Result<MessageBody> {
        if let Some(started) = &self.status_started {
            let _ = started.send(());
        }
        if let Some(release) = &self.status_release {
            let _ = release.recv_timeout(Duration::from_secs(2));
        }
        Ok(MessageBody {
            id: id.clone(),
            plain_text: Some("prefetched".into()),
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
    fn fetch_raw(&mut self, _mailbox: &str, _id: &MessageId) -> anyhow::Result<Vec<u8>> {
        Ok(b"From: sender@example.org\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nbody\r\n--x\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=test.txt\r\n\r\nfile contents\r\n--x--\r\n".to_vec())
    }
    fn download_native_attachment_into(
        &mut self,
        _mailbox: &str,
        part: &crate::backend::RemoteAttachment,
        writer: &mut dyn std::io::Write,
    ) -> anyhow::Result<()> {
        writer.write_all(b"attachment bytes")?;
        anyhow::ensure!(part.blob_id != "fail", "simulated interrupted download");
        Ok(())
    }
    fn set_seen(&mut self, _mailbox: &str, _id: &MessageId, _seen: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn delete(&mut self, _mailbox: &str, _id: &MessageId) -> anyhow::Result<()> {
        Ok(())
    }
    fn move_to(
        &mut self,
        _source: &str,
        _destination: &str,
        _id: &MessageId,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn remote_attachment_save_never_overwrites_or_publishes_partial_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("report.txt"), b"existing").unwrap();
    let part = crate::backend::RemoteAttachment {
        identity: CacheIdentity::Jmap {
            account_id: "a".into(),
            mailbox_id: "m".into(),
            email_state: None,
            query_state: None,
        },
        blob_id: "ok".into(),
        filename: "../report.txt".into(),
        content_type: "text/plain".into(),
        size: 16,
    };
    let mut failed = part.clone();
    failed.blob_id = "fail".into();
    failed.filename = "partial.txt".into();
    let mut backend = TestBackend {
        status_started: None,
        status_release: None,
    };
    let result = super::download_attachment_job(
        &mut backend,
        "INBOX",
        super::RemoteAttachmentJob::Save {
            directory: dir.path().to_owned(),
            files: vec![(0, part), (1, failed)],
        },
    )
    .unwrap();
    assert!(matches!(
        result,
        super::RemoteAttachmentResult::Saved {
            count: 1,
            error: Some(_),
            ..
        }
    ));
    assert_eq!(
        std::fs::read(dir.path().join("report.txt")).unwrap(),
        b"existing"
    );
    assert_eq!(
        std::fs::read(dir.path().join("report (1).txt")).unwrap(),
        b"attachment bytes"
    );
    assert!(!dir.path().join("partial.txt").exists());
}

#[test]
fn stalled_status_lookup_does_not_block_interactive_open() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let foreground = Box::new(TestBackend {
        status_started: None,
        status_release: None,
    });
    let background = Box::new(TestBackend {
        status_started: Some(started_tx),
        status_release: Some(release_rx),
    });
    let (events_tx, events_rx) = mpsc::sync_channel(16);
    let mut worker = WorkerHandle::spawn(0, foreground, Some(background), None, events_tx);
    let _ = worker
        .post(ActionKind::MailboxStatus("INBOX".into()))
        .unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("background status should start");
    let requested = worker
        .post(ActionKind::OpenMailbox {
            name: "All Mail".into(),
            offset: 0,
            limit: 50,
            hint: None,
        })
        .unwrap();
    let event = events_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("interactive open must not wait for STATUS");
    match event {
        RuntimeEvent::ForegroundWorker { event: ev, .. } => {
            assert_eq!(ev.request_id, requested);
            assert!(matches!(ev.kind, WorkerEventKind::MailboxOpened { .. }));
        }
        other => panic!("unexpected event: {other:?}"),
    }
    release_tx.send(()).unwrap();
    worker.shutdown();
}

#[test]
fn interactive_body_can_finish_while_foreground_headers_are_blocked() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let foreground = Box::new(TestBackend {
        status_started: Some(started_tx),
        status_release: Some(release_rx),
    });
    let background = Box::new(TestBackend {
        status_started: None,
        status_release: None,
    });
    let (events_tx, events_rx) = mpsc::sync_channel(8);
    let mut worker = WorkerHandle::spawn(0, foreground, Some(background), None, events_tx);
    worker
        .post(ActionKind::OpenMailbox {
            name: "INBOX".into(),
            offset: 0,
            limit: 100,
            hint: None,
        })
        .unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("header fetch should be in flight");
    let id = MessageId("imap-uid:9:123".into());
    let requested = worker
        .post(ActionKind::FetchBody {
            mailbox: "INBOX".into(),
            id: id.clone(),
        })
        .unwrap();
    let RuntimeEvent::Worker(event) = events_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("selected body must not wait for foreground headers")
    else {
        panic!("expected selected-message body event");
    };
    assert_eq!(event.request_id, requested);
    assert!(matches!(event.kind, WorkerEventKind::BodyLoaded(body) if body.id == id));
    release_tx.send(()).unwrap();
    worker.shutdown();
}

#[test]
fn background_prefetch_delivers_body_without_changing_read_state() {
    let (tx, rx) = mpsc::sync_channel(16);
    let foreground = Box::new(TestBackend {
        status_started: None,
        status_release: None,
    });
    let background = Box::new(TestBackend {
        status_started: None,
        status_release: None,
    });
    let mut worker = WorkerHandle::spawn(0, foreground, Some(background), None, tx);
    let id = MessageId("imap-uid:9:123".into());
    let requested = worker
        .post(ActionKind::PrefetchBodies {
            mailbox: "INBOX".into(),
            ids: vec![id.clone()],
        })
        .unwrap();
    let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("expected background prefetch event");
    };
    assert_eq!(event.request_id, requested);
    match event.kind {
        WorkerEventKind::BodyPrefetched { mailbox, body } => {
            assert_eq!(mailbox, "INBOX");
            assert_eq!(body.id, id);
        }
        other => panic!("unexpected prefetch event: {other:?}"),
    }
    worker.shutdown();
}

#[test]
fn attachment_request_does_not_wait_for_foreground_header_fetch() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let foreground = Box::new(TestBackend {
        status_started: Some(started_tx),
        status_release: Some(release_rx),
    });
    let background = Box::new(TestBackend {
        status_started: None,
        status_release: None,
    });
    let (tx, rx) = mpsc::sync_channel(8);
    let mut worker = WorkerHandle::spawn(0, foreground, Some(background), None, tx);
    worker
        .post(ActionKind::OpenMailbox {
            name: "INBOX".into(),
            offset: 0,
            limit: 100,
            hint: None,
        })
        .unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("foreground headers should block");
    let requested = worker
        .post(ActionKind::FetchAttachments {
            mailbox: "INBOX".into(),
            id: MessageId("imap-uid:42:7".into()),
            cache: None,
        })
        .unwrap();
    let RuntimeEvent::Worker(event) = rx
        .recv_timeout(Duration::from_millis(500))
        .expect("attachment extraction must not wait for foreground header FETCH")
    else {
        panic!("expected attachment event");
    };
    assert_eq!(event.request_id, requested);
    assert!(
        matches!(event.kind, WorkerEventKind::AttachmentsLoaded { attachments, .. }
            if attachments.len() == 1 && attachments[0].filename == "test.txt")
    );
    release_tx.send(()).unwrap();
    worker.shutdown();
}

#[test]
fn attachment_preempts_prefetch_and_survives_status_queue_compaction() {
    let attachment_id = MessageId("imap-uid:22:3".into());
    let mut pending = VecDeque::from([
        Action::new(ActionKind::MailboxStatus("INBOX".into())),
        Action::new(ActionKind::PrefetchBodies {
            mailbox: "INBOX".into(),
            ids: vec![],
        }),
        Action::new(ActionKind::FetchAttachments {
            mailbox: "INBOX".into(),
            id: attachment_id.clone(),
            cache: None,
        }),
        Action::new(ActionKind::PrefetchBodies {
            mailbox: "Sent".into(),
            ids: vec![],
        }),
    ]);
    let next = pop_background_action(&mut pending, 0).unwrap();
    assert!(matches!(next.kind, ActionKind::FetchAttachments { id, .. } if id == attachment_id));
    assert!(
        pending
            .iter()
            .all(|action| !matches!(&action.kind, ActionKind::FetchAttachments { .. }))
    );
}

#[test]
fn raw_mime_disk_hit_does_not_use_imap() {
    struct Offline;
    impl MailBackend for Offline {
        fn capabilities(&self) -> Capabilities {
            Capabilities::IMAP_BASIC
        }
        fn connect(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
        fn list_mailboxes(&mut self) -> anyhow::Result<Vec<Mailbox>> {
            anyhow::bail!("offline")
        }
        fn mailbox_status(&mut self, _: &str) -> anyhow::Result<(usize, usize)> {
            anyhow::bail!("offline")
        }
        fn open_mailbox(&mut self, _: &str, _: usize, _: usize) -> anyhow::Result<MessagePage> {
            anyhow::bail!("offline")
        }
        fn fetch_body(&mut self, _: &str, _: &MessageId) -> anyhow::Result<MessageBody> {
            anyhow::bail!("offline")
        }
        fn fetch_raw(&mut self, _: &str, _: &MessageId) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("offline")
        }
        fn set_seen(&mut self, _: &str, _: &MessageId, _: bool) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
        fn delete(&mut self, _: &str, _: &MessageId) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
        fn move_to(&mut self, _: &str, _: &str, _: &MessageId) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = crate::cache::RawStore {
        dir: dir.path().join("raw"),
    };
    let id = MessageId("imap-uid:42:7".into());
    let mut backend = TestBackend {
        status_started: None,
        status_release: None,
    };
    let initial = super::fetch_attachments(
        &mut backend,
        "INBOX",
        &id,
        Some(&(store.dir.clone(), CacheIdentity::Imap(42))),
    )
    .unwrap();
    assert_eq!(initial.len(), 1);
    let offline = super::fetch_attachments(
        &mut Offline,
        "INBOX",
        &id,
        Some(&(store.dir.clone(), CacheIdentity::Imap(42))),
    )
    .unwrap();
    assert_eq!(offline[0].filename, "test.txt");

    // Exercise the real priority worker path: a cache hit must complete
    // without even attempting connect() on either offline backend.
    let (tx, rx) = mpsc::sync_channel(8);
    let mut worker = WorkerHandle::spawn(0, Box::new(Offline), Some(Box::new(Offline)), None, tx);
    let requested = worker
        .post(ActionKind::FetchAttachments {
            mailbox: "INBOX".into(),
            id,
            cache: Some((store.dir, CacheIdentity::Imap(42))),
        })
        .unwrap();
    let RuntimeEvent::Worker(event) = rx
        .recv_timeout(Duration::from_millis(500))
        .expect("cached attachment must load offline")
    else {
        panic!("worker event expected")
    };
    assert_eq!(event.request_id, requested);
    assert!(
        matches!(event.kind, WorkerEventKind::AttachmentsLoaded { attachments, .. }
            if attachments.len() == 1)
    );
    worker.shutdown();
}

#[test]
fn slow_background_prefetch_never_blocks_interactive_open() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let foreground = Box::new(TestBackend {
        status_started: None,
        status_release: None,
    });
    let background = Box::new(TestBackend {
        status_started: Some(started_tx),
        status_release: Some(release_rx),
    });
    let (tx, rx) = mpsc::sync_channel(16);
    let mut worker = WorkerHandle::spawn(0, foreground, Some(background), None, tx);
    worker
        .post(ActionKind::PrefetchBodies {
            mailbox: "INBOX".into(),
            ids: vec![MessageId("imap-uid:1:1".into())],
        })
        .unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("prefetch should start");
    let requested = worker
        .post(ActionKind::OpenMailbox {
            name: "INBOX".into(),
            offset: 0,
            limit: 10,
            hint: None,
        })
        .unwrap();
    let RuntimeEvent::ForegroundWorker { event, .. } = rx
        .recv_timeout(Duration::from_millis(500))
        .expect("interactive mailbox open must not await prefetch")
    else {
        panic!("expected foreground event");
    };
    assert_eq!(event.request_id, requested);
    assert!(matches!(event.kind, WorkerEventKind::MailboxOpened { .. }));
    release_tx.send(()).unwrap();
    worker.shutdown();
}

#[test]
fn final_exit_does_not_wait_for_busy_worker_or_full_queue() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let backend = Box::new(TestBackend {
        status_started: Some(started_tx),
        status_release: Some(release_rx),
    });
    let (events_tx, events_rx) = mpsc::sync_channel(128);
    let mut worker = WorkerHandle::spawn(0, backend, None, None, events_tx);
    let open = || ActionKind::OpenMailbox {
        name: "INBOX".into(),
        offset: 0,
        limit: 10,
        hint: None,
    };
    worker.post(open()).unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("foreground request must have started");
    // The first request is waiting for the test to release it. Fill the
    // worker's bounded queue, which used to block shutdown's send().
    for _ in 0..64 {
        worker.post(open()).unwrap();
    }
    let started = Instant::now();
    worker.detach_for_exit();
    drop(worker);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "final exit should not wait on the IMAP worker"
    );
    // Release the blocked read and disconnect the event channel. This
    // lets the detached test thread stop rather than processing 64 reads.
    drop(started_rx);
    drop(events_rx);
    release_tx.send(()).unwrap();
    drop(release_tx);
}

#[test]
fn raw_export_writes_exact_message_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("message.eml");
    let raw = b"From: test@example.com\r\nSubject: debug\r\n\r\nbody\r\n";
    write_export_stream(&path, |writer| {
        std::io::Write::write_all(writer, raw)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), raw);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
