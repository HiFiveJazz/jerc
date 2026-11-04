use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::backend::MailBackend;
use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage, OutgoingMessage, OutgoingAttachment};
use crate::outgoing::MailSender;
use crate::mail_image::MailImageEvent;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub enum ActionKind {
    Connect,
    ListMailboxes,
    MailboxStatus(String),
    OpenMailbox { name: String, offset: usize, limit: usize },
    FetchBody { mailbox: String, id: MessageId },
    FetchRawForCompose { mailbox: String, id: MessageId },
    FetchAttachments { mailbox: String, id: MessageId },
    ExportRaw { mailbox: String, id: MessageId, path: PathBuf },
    SetSeen { mailbox: String, id: MessageId, seen: bool },
    Delete { mailbox: String, id: MessageId },
    DeleteMany { mailbox: String, ids: Vec<MessageId> },
    Move { source: String, destination: String, id: MessageId },
    MoveMany { source: String, destination: String, ids: Vec<MessageId> },
    ArchiveMany { source: String, destination: String, ids: Vec<MessageId> },
    UnarchiveMany { source: String, destination: String, ids: Vec<MessageId> },
    Send {
        message: OutgoingMessage,
        sent_mailbox: Option<String>,
    },
    SaveDraft { message: OutgoingMessage, mailbox: String },
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub id: u64,
    pub kind: ActionKind,
    queued_at: Instant,
}

impl Action {
    #[must_use]
    pub fn new(kind: ActionKind) -> Self {
        Self {
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
            kind,
            queued_at: Instant::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum WorkerEventKind {
    Connected { capabilities: Capabilities },
    Mailboxes(Vec<Mailbox>),
    MailboxStatus {
        name: String,
        total: usize,
        unread: usize,
    },
    MailboxOpened {
        name: String,
        page: MessagePage,
        uid_validity: Option<u32>,
        elapsed_micros: u128,
    },
    MailboxOpenFailed { name: String, error: String },
    BodyLoaded(MessageBody),
    BodyLoadFailed { error: String },
    RawLoaded { id: MessageId, raw: Vec<u8> },
    AttachmentsLoaded { id: MessageId, attachments: Vec<OutgoingAttachment> },
    MessageExported { path: PathBuf },
    SeenChanged { id: MessageId, seen: bool },
    MutationComplete,
    DraftSaved { mailbox: String },
    DraftSaveFailed { mailbox: String, error: String },
    MessageSent {
        smtp_status: String,
        sent_mailbox: Option<String>,
        sent_copy_saved: bool,
        sent_copy_warning: Option<String>,
    },
    Error(String),
    Stopped,
}

#[derive(Debug, Clone)]
pub struct WorkerEvent {
    pub account: usize,
    pub request_id: u64,
    pub kind: WorkerEventKind,
}

#[derive(Debug)]
pub enum RuntimeEvent {
    Input(crossterm::event::Event),
    Worker(WorkerEvent),
    MailImage(MailImageEvent),
    InputStopped,
}

pub struct WorkerHandle {
    sender: SyncSender<Action>,
    // STATUS runs on a separate IMAP session. A stalled unread-count lookup
    // must never get ahead of an interactive SELECT or FETCH.
    status_sender: Option<SyncSender<Action>>,
    join: Option<JoinHandle<()>>,
    status_join: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn spawn(
        account: usize,
        mut backend: Box<dyn MailBackend>,
        status_backend: Option<Box<dyn MailBackend>>,
        mut outgoing: Option<Box<dyn MailSender>>,
        runtime_tx: SyncSender<RuntimeEvent>,
    ) -> Self {
        let (sender, receiver) = sync_channel::<Action>(64);
        let status_events = runtime_tx.clone();
        let join = thread::Builder::new()
            .name(format!("mail-worker-{account}"))
            .spawn(move || {
                run_worker(
                    account,
                    &mut *backend,
                    &mut outgoing,
                    receiver,
                    runtime_tx,
                );
            })
            .expect("failed to spawn mail worker");
        let (status_sender, status_join) = if let Some(status_backend) = status_backend {
            let (tx, rx) = sync_channel::<Action>(16);
            let join = thread::Builder::new()
                .name(format!("mail-status-{account}"))
                .spawn(move || run_status_worker(account, status_backend, rx, status_events))
                .expect("failed to spawn mailbox status worker");
            (Some(tx), Some(join))
        } else {
            (None, None)
        };
        Self {
            sender,
            status_sender,
            join: Some(join),
            status_join,
        }
    }

    pub fn post(&self, kind: ActionKind) -> Result<u64, TrySendError<Action>> {
        let background = matches!(&kind, ActionKind::MailboxStatus(_));
        let action = Action::new(kind);
        let id = action.id;
        if background {
            self.status_sender.as_ref().unwrap_or(&self.sender).try_send(action)?;
        } else {
            self.sender.try_send(action)?;
        }
        Ok(id)
    }

    pub fn shutdown(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        // Shutdown is allowed to wait for the bounded queue to drain. Ordinary
        // UI actions use try_send() so backend pressure can never freeze input.
        // The status worker is independent: shut it down as well, but never
        // wait for an idle-count request before signalling the foreground one.
        if let Some(sender) = self.status_sender.take() {
            let _ = sender.send(Action::new(ActionKind::Shutdown));
        }
        let _ = self.sender.send(Action::new(ActionKind::Shutdown));
        let _ = join.join();
        if let Some(status_join) = self.status_join.take() {
            let _ = status_join.join();
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_worker(
    account: usize,
    backend: &mut dyn MailBackend,
    outgoing: &mut Option<Box<dyn MailSender>>,
    receiver: Receiver<Action>,
    runtime_tx: SyncSender<RuntimeEvent>,
) {
    let mut pending = VecDeque::<Action>::new();

    loop {
        // Drain the current queue BEFORE choosing an action. Previously a
        // stale OpenMailbox already in `pending` could execute before newer
        // folder requests that arrived in the channel during a slow SELECT.
        if pending.is_empty() {
            match receiver.recv() {
                Ok(action) => pending.push_back(action),
                Err(_) => break,
            }
        }
        while let Ok(action) = receiver.try_recv() {
            pending.push_back(action);
        }
        compact_navigation_queue(&mut pending);
        let Some(action) = pending.pop_front() else { continue; };
        if matches!(&action.kind, ActionKind::Shutdown) {
            break;
        }

        // A blocked IMAP operation can delay subsequent queued work even while
        // the UI is responsive. Record queue time separately from execution.
        let queue_wait = action.queued_at.elapsed();
        let operation = match &action.kind {
            ActionKind::Connect => "worker_connect",
            ActionKind::ListMailboxes => "worker_list_mailboxes",
            ActionKind::MailboxStatus(_) => "worker_mailbox_status",
            ActionKind::OpenMailbox { .. } => "worker_open_mailbox",
            ActionKind::FetchBody { .. } => "worker_fetch_body",
            ActionKind::FetchRawForCompose { .. } => "worker_fetch_compose",
            ActionKind::FetchAttachments { .. } => "worker_fetch_attachments",
            ActionKind::ExportRaw { .. } => "worker_export_raw",
            ActionKind::SetSeen { .. } => "worker_set_seen",
            ActionKind::Delete { .. } | ActionKind::DeleteMany { .. } => "worker_delete",
            ActionKind::Move { .. } | ActionKind::MoveMany { .. } => "worker_move",
            ActionKind::ArchiveMany { .. } | ActionKind::UnarchiveMany { .. } => "worker_archive",
            ActionKind::Send { .. } => "worker_send",
            ActionKind::SaveDraft { .. } => "worker_save_draft",
            ActionKind::Shutdown => "worker_shutdown",
        };
        let started = Instant::now();
        let request_id = action.id;
        if operation == "worker_open_mailbox" || operation == "worker_fetch_body" {
            crate::perf::record_request("worker_start", request_id, Some(queue_wait), started.elapsed(), true);
        }
        let event = match action.kind {
            ActionKind::Connect => backend
                .connect()
                .map(|()| WorkerEventKind::Connected {
                    capabilities: backend.capabilities(),
                }),
            ActionKind::ListMailboxes => backend.list_mailboxes().map(WorkerEventKind::Mailboxes),
            ActionKind::MailboxStatus(name) => backend.mailbox_status(&name).map(|(total, unread)| {
                WorkerEventKind::MailboxStatus {
                    name,
                    total,
                    unread,
                }
            }),
            ActionKind::OpenMailbox { name, offset, limit } => {
                let start = Instant::now();
                Ok(match backend.open_mailbox(&name, offset, limit) {
                    Ok(page) => WorkerEventKind::MailboxOpened {
                        uid_validity: backend.mailbox_uid_validity(&name),
                        name, page, elapsed_micros: start.elapsed().as_micros(),
                    },
                    Err(error) => WorkerEventKind::MailboxOpenFailed {
                        name, error: format!("{error:#}"),
                    },
                })
            }
            ActionKind::FetchBody { mailbox, id } => Ok(match backend.fetch_body(&mailbox, &id) {
                Ok(body) => WorkerEventKind::BodyLoaded(body),
                Err(error) => WorkerEventKind::BodyLoadFailed { error: format!("{error:#}") },
            }),
            ActionKind::FetchRawForCompose { mailbox, id } => backend
                .fetch_raw(&mailbox, &id)
                .map(|raw| WorkerEventKind::RawLoaded { id, raw }),
            ActionKind::FetchAttachments { mailbox, id } => backend
                .fetch_raw(&mailbox, &id)
                .and_then(|raw| crate::mail::extract_attachments(&raw))
                .map(|attachments| WorkerEventKind::AttachmentsLoaded { id, attachments }),
            ActionKind::ExportRaw { mailbox, id, path } => backend
                .fetch_raw(&mailbox, &id)
                .and_then(|raw| write_export(&path, &raw).map(|()| path))
                .map(|path| WorkerEventKind::MessageExported { path }),
            ActionKind::SetSeen { mailbox, id, seen } => backend
                .set_seen(&mailbox, &id, seen)
                .map(|()| WorkerEventKind::SeenChanged { id, seen }),
            ActionKind::Delete { mailbox, id } => backend
                .delete(&mailbox, &id)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::DeleteMany { mailbox, ids } => backend
                .delete_many(&mailbox, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::Move { source, destination, id } => backend
                .move_to(&source, &destination, &id)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::MoveMany { source, destination, ids } => backend
                .move_many(&source, &destination, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::ArchiveMany { source, destination, ids } => backend
                .archive_many(&source, &destination, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::UnarchiveMany { source, destination, ids } => backend
                .unarchive_many(&source, &destination, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::SaveDraft { message, mailbox } => {
                // Save failures must not invalidate unrelated mailbox state.
                // A local recovery copy is already on disk at this point.
                Ok(match crate::outgoing::build_draft(&message, &message.from)
                    .and_then(|raw| backend.append_draft(&mailbox, &raw)) {
                    Ok(()) => WorkerEventKind::DraftSaved { mailbox },
                    Err(error) => WorkerEventKind::DraftSaveFailed {
                        mailbox, error: format!("{error:#}"),
                    },
                })
            }
            ActionKind::Send {
                message,
                sent_mailbox,
            } => match outgoing.as_deref_mut() {
                Some(sender) => sender.send(&message).map(|receipt| {
                    let mut sent_copy_saved = false;
                    let mut sent_copy_warning = None;
                    if !receipt.server_files_sent_copy {
                        if let Some(mailbox) = sent_mailbox.as_deref() {
                            match backend.append_message(mailbox, &receipt.raw_message, true) {
                                Ok(()) => sent_copy_saved = true,
                                Err(error) => {
                                    sent_copy_warning = Some(format!(
                                        "SMTP accepted the message, but saving a Sent copy to {mailbox:?} failed: {error:#}"
                                    ));
                                }
                            }
                        } else {
                            sent_copy_warning = Some(
                                "SMTP accepted the message, but the account advertises no Sent mailbox"
                                    .to_owned(),
                            );
                        }
                    }
                    WorkerEventKind::MessageSent {
                        smtp_status: receipt.smtp_status,
                        sent_mailbox,
                        sent_copy_saved,
                        sent_copy_warning,
                    }
                }),
                None => Err(anyhow::anyhow!(
                    "no outgoing transport configured for this account"
                )),
            },
            ActionKind::Shutdown => unreachable!(),
        }
        .unwrap_or_else(|err| WorkerEventKind::Error(format!("{err:#}")));

        crate::perf::record(
            operation,
            Some(queue_wait),
            started.elapsed(),
            !matches!(&event, WorkerEventKind::Error(_) | WorkerEventKind::DraftSaveFailed { .. }
                | WorkerEventKind::MailboxOpenFailed { .. } | WorkerEventKind::BodyLoadFailed { .. }),
        );

        if operation == "worker_open_mailbox" || operation == "worker_fetch_body" {
            crate::perf::record_request(
                "worker_complete", request_id, Some(queue_wait), started.elapsed(),
                !matches!(&event, WorkerEventKind::Error(_) | WorkerEventKind::MailboxOpenFailed { .. }
                    | WorkerEventKind::BodyLoadFailed { .. }),
            );
        }
        if runtime_tx
            .send(RuntimeEvent::Worker(WorkerEvent {
                account,
                request_id,
                kind: event,
            }))
            .is_err()
        {
            break;
        }
    }
}

/// Drop obsolete navigation, preserving the relative order of mutations and
/// other side effects. The newest mailbox open wins; a newer open invalidates
/// earlier body fetches as well. An in-flight IMAP request can't be canceled.
fn compact_navigation_queue(pending: &mut VecDeque<Action>) {
    let latest_mailbox = pending.iter().rev().find_map(|action| match &action.kind {
        ActionKind::OpenMailbox { name, .. } => Some(name.as_str()),
        _ => None,
    }).map(str::to_owned);
    let mut seen_open = false;
    let mut seen_body = false;
    let mut kept = VecDeque::with_capacity(pending.len());
    while let Some(action) = pending.pop_back() {
        match &action.kind {
            ActionKind::OpenMailbox { .. } if seen_open => continue,
            ActionKind::OpenMailbox { .. } => {
                seen_open = true;
                seen_body = true;
            }
            ActionKind::FetchBody { mailbox, .. }
                if seen_body || latest_mailbox.as_deref().is_some_and(|name| name != mailbox) => continue,
            ActionKind::FetchBody { .. } => seen_body = true,
            _ => {}
        }
        kept.push_front(action);
    }
    *pending = kept;
}

/// Background count checks use their own IMAP session, which is connected
/// lazily. A slow STATUS cannot delay interactive SELECT/FETCH, and a failed
/// background connection never tears down the foreground session.
fn run_status_worker(
    account: usize,
    mut backend: Box<dyn MailBackend>,
    receiver: Receiver<Action>,
    runtime_tx: SyncSender<RuntimeEvent>,
) {
    let mut connected = false;
    while let Ok(action) = receiver.recv() {
        let name = match action.kind {
            ActionKind::MailboxStatus(name) => name,
            ActionKind::Shutdown => break,
            _ => continue,
        };
        let queue_wait = action.queued_at.elapsed();
        let started = Instant::now();
        let result = (|| {
            if !connected {
                backend.connect()?;
                connected = true;
            }
            backend.mailbox_status(&name)
        })();
        if result.is_err() {
            // The next request attempts a fresh connection. A stale session
            // must not poison subsequent count updates indefinitely.
            connected = false;
        }
        let ok = result.is_ok();
        crate::perf::record("background_status", Some(queue_wait), started.elapsed(), ok);
        let kind = match result {
            Ok((total, unread)) => WorkerEventKind::MailboxStatus { name, total, unread },
            Err(error) => WorkerEventKind::Error(format!("background mailbox count: {error:#}")),
        };
        if runtime_tx.send(RuntimeEvent::Worker(WorkerEvent {
            account, request_id: action.id, kind,
        })).is_err() { break; }
    }
}

fn write_export(path: &std::path::Path, raw: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, raw)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{compact_navigation_queue, write_export, Action, ActionKind, WorkerHandle, RuntimeEvent, WorkerEventKind};
    use crate::backend::MailBackend;
    use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage};
    use std::collections::VecDeque;
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::time::Duration;

    fn open(name: &str) -> Action {
        Action::new(ActionKind::OpenMailbox { name: name.into(), offset: 0, limit: 50 })
    }

    fn body(mailbox: &str) -> Action {
        Action::new(ActionKind::FetchBody { mailbox: mailbox.into(), id: MessageId("imap-uid:1".into()) })
    }

    #[test]
    fn coalescing_preserves_only_latest_navigation_and_keeps_mutations() {
        let mut pending = VecDeque::from([
            open("A"), body("A"),
            Action::new(ActionKind::SetSeen { mailbox: "A".into(), id: MessageId("imap-uid:1".into()), seen: true }),
            open("B"), body("B"), open("C"), body("C"),
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

    struct TestBackend {
        status_started: Option<SyncSender<()>>,
        status_release: Option<Receiver<()>>,
    }

    impl MailBackend for TestBackend {
        fn capabilities(&self) -> Capabilities { Capabilities::IMAP_BASIC }
        fn connect(&mut self) -> anyhow::Result<()> { Ok(()) }
        fn list_mailboxes(&mut self) -> anyhow::Result<Vec<Mailbox>> { Ok(vec![]) }
        fn mailbox_status(&mut self, _name: &str) -> anyhow::Result<(usize, usize)> {
            if let Some(started) = &self.status_started { let _ = started.send(()); }
            if let Some(release) = &self.status_release { let _ = release.recv_timeout(Duration::from_secs(2)); }
            Ok((5, 2))
        }
        fn open_mailbox(&mut self, _name: &str, offset: usize, _limit: usize) -> anyhow::Result<MessagePage> {
            Ok(MessagePage { messages: vec![], offset, next_offset: offset, has_more: false })
        }
        fn fetch_body(&mut self, _mailbox: &str, _id: &MessageId) -> anyhow::Result<MessageBody> {
            anyhow::bail!("not needed for this test")
        }
        fn fetch_raw(&mut self, _mailbox: &str, _id: &MessageId) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("not needed for this test")
        }
        fn set_seen(&mut self, _mailbox: &str, _id: &MessageId, _seen: bool) -> anyhow::Result<()> { Ok(()) }
        fn delete(&mut self, _mailbox: &str, _id: &MessageId) -> anyhow::Result<()> { Ok(()) }
        fn move_to(&mut self, _source: &str, _destination: &str, _id: &MessageId) -> anyhow::Result<()> { Ok(()) }
    }

    #[test]
    fn stalled_status_lookup_does_not_block_interactive_open() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let foreground = Box::new(TestBackend { status_started: None, status_release: None });
        let background = Box::new(TestBackend { status_started: Some(started_tx), status_release: Some(release_rx) });
        let (events_tx, events_rx) = mpsc::sync_channel(16);
        let mut worker = WorkerHandle::spawn(0, foreground, Some(background), None, events_tx);
        let _ = worker.post(ActionKind::MailboxStatus("INBOX".into())).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).expect("background status should start");
        let requested = worker.post(ActionKind::OpenMailbox { name: "All Mail".into(), offset: 0, limit: 50 }).unwrap();
        let event = events_rx.recv_timeout(Duration::from_millis(500)).expect("interactive open must not wait for STATUS");
        match event {
            RuntimeEvent::Worker(ev) => {
                assert_eq!(ev.request_id, requested);
                assert!(matches!(ev.kind, WorkerEventKind::MailboxOpened { .. }));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        release_tx.send(()).unwrap();
        worker.shutdown();
    }

    #[test]
    fn raw_export_writes_exact_message_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("message.eml");
        let raw = b"From: test@example.com\r\nSubject: debug\r\n\r\nbody\r\n";
        write_export(&path, raw).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), raw);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
