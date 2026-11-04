use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::backend::{MailBackend, MailboxSyncHint};
use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage, OutgoingMessage, OutgoingAttachment};
use crate::outgoing::MailSender;
use crate::mail_image::MailImageEvent;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub enum ActionKind {
    Connect,
    ListMailboxes,
    MailboxStatus(String),
    /// A single newest message preview after a known Inbox count increase.
    NewMailPreview { mailbox: String, count: usize },
    OpenMailbox { name: String, offset: usize, limit: usize, hint: Option<MailboxSyncHint> },
    FetchBody { mailbox: String, id: MessageId },
    /// Read-only, opportunistic preview fetches on the background session.
    PrefetchBodies { mailbox: String, ids: Vec<MessageId> },
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
        /// None = legacy auto; Some(empty) = disable; Some(names) = APPEND to
        /// every explicit folder, even when the SMTP provider files Sent.
        copy_to: Option<Vec<String>>,
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
    NewMailPreview {
        count: usize,
        sender: String,
        subject: String,
        excerpt: String,
    },
    MailboxOpened {
        name: String,
        page: MessagePage,
        uid_validity: Option<u32>,
        uid_next: Option<u32>,
        elapsed_micros: u128,
    },
    MailboxOpenFailed { name: String, error: String },
    BodyLoaded(MessageBody),
    BodyPrefetched { mailbox: String, body: MessageBody },
    BodyLoadFailed { error: String },
    RawLoaded { id: MessageId, raw: Vec<u8> },
    AttachmentsLoaded { mailbox: String, id: MessageId, attachments: Vec<OutgoingAttachment> },
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
        sent_copy_disabled: bool,
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
        let action = Action::new(kind);
        let id = action.id;
        if matches!(&action.kind, ActionKind::FetchBody { .. }) {
            // Prioritize explicit reads on the separate session while a
            // foreground header fetch is in flight. If that queue is full or
            // gone, the foreground worker is a safe fallback.
            if let Some(sender) = &self.status_sender {
                match sender.try_send(action) {
                    Ok(()) => {}
                    Err(TrySendError::Full(action) | TrySendError::Disconnected(action)) => {
                        self.sender.try_send(action)?;
                    }
                }
            } else {
                self.sender.try_send(action)?;
            }
        } else if matches!(&action.kind, ActionKind::PrefetchBodies { .. }) {
            // Speculative downloads must not compete with foreground work.
            match &self.status_sender {
                Some(sender) => sender.try_send(action)?,
                None => return Err(TrySendError::Disconnected(action)),
            }
        } else if matches!(&action.kind, ActionKind::MailboxStatus(_) | ActionKind::NewMailPreview { .. }) {
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
            ActionKind::NewMailPreview { .. } => "worker_new_mail_preview",
            ActionKind::OpenMailbox { .. } => "worker_open_mailbox",
            ActionKind::FetchBody { .. } => "worker_fetch_body",
            ActionKind::PrefetchBodies { .. } => "worker_prefetch_bodies",
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
            ActionKind::NewMailPreview { mailbox, count } => {
                Ok(new_mail_preview(backend, &mailbox, count))
            }
            ActionKind::MailboxStatus(name) => backend.mailbox_status(&name).map(|(total, unread)| {
                WorkerEventKind::MailboxStatus {
                    name,
                    total,
                    unread,
                }
            }),
            ActionKind::OpenMailbox { name, offset, limit, hint } => {
                let start = Instant::now();
                Ok(match backend.open_mailbox_with_hint(&name, offset, limit, hint) {
                    Ok(page) => WorkerEventKind::MailboxOpened {
                        uid_validity: backend.mailbox_uid_validity(&name),
                        uid_next: backend.mailbox_uid_next(&name),
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
            ActionKind::PrefetchBodies { .. } => unreachable!("prefetch requires background worker"),
            ActionKind::FetchRawForCompose { mailbox, id } => backend
                .fetch_raw(&mailbox, &id)
                .map(|raw| WorkerEventKind::RawLoaded { id, raw }),
            ActionKind::FetchAttachments { mailbox, id } => backend
                .fetch_raw(&mailbox, &id)
                .and_then(|raw| crate::mail::extract_attachments(&raw))
                .map(|attachments| WorkerEventKind::AttachmentsLoaded { mailbox, id, attachments }),
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
                copy_to,
            } => match outgoing.as_deref_mut() {
                Some(sender) => sender.send(&message).map(|receipt| {
                    let mut sent_copy_saved = false;
                    let mut sent_copy_warning = None;
                    let targets = match copy_to.as_ref() {
                        Some(folders) => folders.clone(),
                        None if receipt.server_files_sent_copy => Vec::new(),
                        None => sent_mailbox.clone().into_iter().collect(),
                    };
                    for mailbox in &targets {
                        match backend.append_message(mailbox, &receipt.raw_message, true) {
                            Ok(()) => sent_copy_saved = true,
                            Err(_error) => {
                                // SMTP already succeeded; never retry the send
                                // merely because a server-side Sent APPEND fails.
                                sent_copy_warning = Some(format!(
                                    "SMTP accepted the message, but saving a Sent copy to {mailbox:?} failed"
                                ));
                            }
                        }
                    }
                    if copy_to.is_none() && !receipt.server_files_sent_copy && targets.is_empty() {
                        sent_copy_warning = Some("message submitted, but no Sent mailbox was advertised".to_owned());
                    }
                    WorkerEventKind::MessageSent {
                        smtp_status: receipt.smtp_status,
                        sent_mailbox,
                        sent_copy_saved,
                        sent_copy_warning,
                        sent_copy_disabled: copy_to.as_ref().is_some_and(Vec::is_empty),
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

/// Choose the newest prefetch window, but after a small body burst give an
/// awaiting unread-count update priority. Returns None only for an empty queue.
fn pop_background_action(pending: &mut VecDeque<Action>, bodies_since_status: usize) -> Option<Action> {
    // Interactive reads have priority even over unread-count polling; only
    // one in-flight IMAP operation may precede the newest selected message.
    if let Some(index) = pending.iter().rposition(|action| matches!(&action.kind,
        ActionKind::FetchBody { .. })) {
        let newest = pending.remove(index);
        pending.retain(|action| !matches!(&action.kind, ActionKind::FetchBody { .. }));
        return newest;
    }
    let status_index = (bodies_since_status >= 4).then(|| pending.iter().position(|action| {
        matches!(&action.kind, ActionKind::MailboxStatus(_))
    })).flatten();
    if let Some(index) = status_index {
        return pending.remove(index);
    }
    let prefetch_index = pending.iter().rposition(|action| {
        matches!(&action.kind, ActionKind::PrefetchBodies { .. })
    });
    if let Some(index) = prefetch_index {
        let newest = pending.remove(index);
        pending.retain(|action| !matches!(&action.kind, ActionKind::PrefetchBodies { .. }));
        newest
    } else {
        pending.pop_front()
    }
}

/// Unread-count checks and low-priority preview preloads share one background
/// session. Neither can block the foreground SELECT/FETCH path. When the cursor
/// moves again, discard pending batches and stop the old batch *between* bodies.
fn run_status_worker(
    account: usize,
    mut backend: Box<dyn MailBackend>,
    receiver: Receiver<Action>,
    runtime_tx: SyncSender<RuntimeEvent>,
) {
    let mut connected = false;
    let mut pending = VecDeque::<Action>::new();
    // A neighboring window often overlaps the one currently downloading.
    // Suppress duplicate PEEK fetches briefly without keeping a permanent
    // shadow cache (the UI may evict large previews from its bounded cache).
    let mut recent = HashMap::<(String, MessageId), Instant>::new();
    // Never let an aggressive, multi-second prefetch run starve unread counts.
    let mut bodies_since_status = 0usize;
    loop {
        if pending.is_empty() {
            match receiver.recv() {
                Ok(action) => pending.push_back(action),
                Err(_) => break,
            }
        }
        while let Ok(action) = receiver.try_recv() {
            pending.push_back(action);
        }
        if pending.iter().any(|action| matches!(&action.kind, ActionKind::Shutdown)) {
            break;
        }
        let Some(action) = pop_background_action(&mut pending, bodies_since_status) else { continue; };
        let queue_wait = action.queued_at.elapsed();
        let started = Instant::now();
        match action.kind {
            ActionKind::FetchBody { mailbox, id } => {
                // This connection can read a selected message while the
                // foreground connection is executing a slow header FETCH.
                let result = (|| {
                    if !connected {
                        backend.connect()?;
                        connected = true;
                    }
                    backend.fetch_body(&mailbox, &id)
                })();
                if result.is_err() { connected = false; }
                crate::perf::record("worker_priority_fetch_body", Some(queue_wait), started.elapsed(), result.is_ok());
                let kind = match result {
                    Ok(body) => {
                        recent.insert((mailbox, id), Instant::now());
                        WorkerEventKind::BodyLoaded(body)
                    }
                    Err(error) => WorkerEventKind::BodyLoadFailed { error: format!("{error:#}") },
                };
                if runtime_tx.send(RuntimeEvent::Worker(WorkerEvent {
                    account, request_id: action.id, kind,
                })).is_err() { break; }
            }
            ActionKind::NewMailPreview { mailbox, count } => {
                let result = (|| {
                    if !connected {
                        backend.connect()?;
                        connected = true;
                    }
                    Ok::<_, anyhow::Error>(new_mail_preview(&mut *backend, &mailbox, count))
                })();
                let kind = result.unwrap_or(WorkerEventKind::NewMailPreview {
                    count, sender: String::new(), subject: String::new(), excerpt: String::new(),
                });
                if runtime_tx.send(RuntimeEvent::Worker(WorkerEvent {
                    account, request_id: action.id, kind,
                })).is_err() { break; }
            }
            ActionKind::MailboxStatus(name) => {
                bodies_since_status = 0;
                let result = (|| {
                    if !connected {
                        backend.connect()?;
                        connected = true;
                    }
                    backend.mailbox_status(&name)
                })();
                if result.is_err() { connected = false; }
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
            ActionKind::PrefetchBodies { mailbox, ids } => {
                if recent.len() > 256 {
                    recent.retain(|_, at| at.elapsed() < Duration::from_secs(4));
                }
                if !connected {
                    let result = backend.connect();
                    crate::perf::record("prefetch_connect", Some(queue_wait), started.elapsed(), result.is_ok());
                    if result.is_err() { continue; } // Optional; never interrupt the UI.
                    connected = true;
                }
                let mut remaining = ids.into_iter();
                while let Some(id) = remaining.next() {
                    let key = (mailbox.clone(), id.clone());
                    if recent.get(&key).is_some_and(|at| at.elapsed() < Duration::from_secs(4)) {
                        continue;
                    }
                    // An IMAP body fetch uses PEEK and must not mark mail seen.
                    let fetched_at = Instant::now();
                    let result = backend.fetch_body(&mailbox, &id);
                    bodies_since_status = bodies_since_status.saturating_add(1);
                    crate::perf::record("prefetch_body", None, fetched_at.elapsed(), result.is_ok());
                    if let Ok(body) = result {
                        recent.insert(key, Instant::now());
                        if runtime_tx.send(RuntimeEvent::Worker(WorkerEvent {
                            account,
                            request_id: action.id,
                            kind: WorkerEventKind::BodyPrefetched { mailbox: mailbox.clone(), body },
                        })).is_err() { return; }
                    }
                    // The user may have reversed direction while this download
                    // was in flight. Obey the latest window before fetching more.
                    while let Ok(new_action) = receiver.try_recv() {
                        pending.push_back(new_action);
                    }
                    if pending.iter().any(|next| matches!(&next.kind,
                        ActionKind::Shutdown | ActionKind::PrefetchBodies { .. })) {
                        break; // Superseded window or shutdown: abandon the rest.
                    }
                    let interactive_waiting = pending.iter().any(|next| matches!(&next.kind,
                        ActionKind::FetchBody { .. }));
                    let status_waiting = bodies_since_status >= 4 && pending.iter().any(|next| matches!(&next.kind,
                        ActionKind::MailboxStatus(_)));
                    if interactive_waiting || status_waiting {
                        // Yield to an explicit reader fetch first, or to STATUS
                        // after a short burst. Resume only if no newer window
                        // supersedes us before the next work item.
                        let rest = remaining.collect::<Vec<_>>();
                        if !rest.is_empty() {
                            pending.push_back(Action::new(ActionKind::PrefetchBodies {
                                mailbox: mailbox.clone(), ids: rest,
                            }));
                        }
                        break;
                    }
                }
            }
            ActionKind::Shutdown => break,
            _ => {} // The background worker never performs foreground mutations.
        }
    }
}

/// Best effort: a failed/empty body still yields the sender and subject.
/// fetch_body uses IMAP BODY.PEEK and must never change the Seen flag.
fn new_mail_preview(backend: &mut dyn MailBackend, mailbox: &str, count: usize) -> WorkerEventKind {
    let mut sender = String::new();
    let mut subject = String::new();
    let mut excerpt = String::new();
    if let Ok(page) = backend.open_mailbox(mailbox, 0, 1) {
        if let Some(message) = page.messages.first() {
            sender = if message.from_name.is_empty() { message.from_address.clone() }
                else { message.from_name.clone() };
            subject = message.subject.clone();
            if let Ok(body) = backend.fetch_body(mailbox, &message.id) {
                excerpt = body.display_text(false).chars().take(400).collect();
            }
        }
    }
    WorkerEventKind::NewMailPreview { count, sender, subject, excerpt }
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
    use super::{compact_navigation_queue, pop_background_action, write_export, Action, ActionKind, WorkerHandle, RuntimeEvent, WorkerEventKind};
    use crate::backend::MailBackend;
    use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage};
    use std::collections::VecDeque;
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::time::Duration;

    fn open(name: &str) -> Action {
        Action::new(ActionKind::OpenMailbox { name: name.into(), offset: 0, limit: 50, hint: None })
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

    #[test]
    fn background_scheduler_yields_to_status_after_four_bodies() {
        let mut pending = VecDeque::from([
            Action::new(ActionKind::MailboxStatus("INBOX".into())),
            Action::new(ActionKind::PrefetchBodies { mailbox: "A".into(), ids: vec![] }),
            Action::new(ActionKind::PrefetchBodies { mailbox: "B".into(), ids: vec![] }),
        ]);
        let next = pop_background_action(&mut pending, 0).unwrap();
        assert!(matches!(next.kind, ActionKind::PrefetchBodies { mailbox, .. } if mailbox == "B"));
        assert_eq!(pending.len(), 1); // Older preview window discarded.
        assert!(matches!(&pending[0].kind, ActionKind::MailboxStatus(_)));

        pending.push_back(Action::new(ActionKind::PrefetchBodies {
            mailbox: "C".into(), ids: vec![],
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
            Action::new(ActionKind::PrefetchBodies { mailbox: "INBOX".into(), ids: vec![] }),
            body("INBOX"),
            body("INBOX"),
        ]);
        let expected = pending.back().unwrap().id;
        let next = pop_background_action(&mut pending, 4).unwrap();
        assert_eq!(next.id, expected);
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|a| !matches!(&a.kind, ActionKind::FetchBody { .. })));
        assert!(matches!(pop_background_action(&mut pending, 4).unwrap().kind, ActionKind::MailboxStatus(_)));
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
            if let Some(started) = &self.status_started { let _ = started.send(()); }
            if let Some(release) = &self.status_release { let _ = release.recv_timeout(Duration::from_secs(2)); }
            Ok(MessagePage { messages: vec![], offset, next_offset: offset, has_more: false })
        }
        fn fetch_body(&mut self, _mailbox: &str, id: &MessageId) -> anyhow::Result<MessageBody> {
            if let Some(started) = &self.status_started { let _ = started.send(()); }
            if let Some(release) = &self.status_release { let _ = release.recv_timeout(Duration::from_secs(2)); }
            Ok(MessageBody {
                id: id.clone(), plain_text: Some("prefetched".into()), plain_fallback: None,
                html_source: None, html_text: None, html_lines: None, html_renderer: None,
                html_render_width: 0, attachment_count: 0, attachment_names: Vec::new(),
            })
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
        let requested = worker.post(ActionKind::OpenMailbox { name: "All Mail".into(), offset: 0, limit: 50, hint: None }).unwrap();
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
    fn interactive_body_can_finish_while_foreground_headers_are_blocked() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let foreground = Box::new(TestBackend {
            status_started: Some(started_tx), status_release: Some(release_rx),
        });
        let background = Box::new(TestBackend { status_started: None, status_release: None });
        let (events_tx, events_rx) = mpsc::sync_channel(8);
        let mut worker = WorkerHandle::spawn(0, foreground, Some(background), None, events_tx);
        worker.post(ActionKind::OpenMailbox {
            name: "INBOX".into(), offset: 0, limit: 100, hint: None,
        }).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).expect("header fetch should be in flight");
        let id = MessageId("imap-uid:9:123".into());
        let requested = worker.post(ActionKind::FetchBody { mailbox: "INBOX".into(), id: id.clone() }).unwrap();
        let RuntimeEvent::Worker(event) = events_rx.recv_timeout(Duration::from_millis(500))
            .expect("selected body must not wait for foreground headers") else {
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
        let foreground = Box::new(TestBackend { status_started: None, status_release: None });
        let background = Box::new(TestBackend { status_started: None, status_release: None });
        let mut worker = WorkerHandle::spawn(0, foreground, Some(background), None, tx);
        let id = MessageId("imap-uid:9:123".into());
        let requested = worker.post(ActionKind::PrefetchBodies {
            mailbox: "INBOX".into(), ids: vec![id.clone()],
        }).unwrap();
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
    fn slow_background_prefetch_never_blocks_interactive_open() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let foreground = Box::new(TestBackend { status_started: None, status_release: None });
        let background = Box::new(TestBackend { status_started: Some(started_tx), status_release: Some(release_rx) });
        let (tx, rx) = mpsc::sync_channel(16);
        let mut worker = WorkerHandle::spawn(0, foreground, Some(background), None, tx);
        worker.post(ActionKind::PrefetchBodies {
            mailbox: "INBOX".into(), ids: vec![MessageId("imap-uid:1:1".into())],
        }).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).expect("prefetch should start");
        let requested = worker.post(ActionKind::OpenMailbox {
            name: "INBOX".into(), offset: 0, limit: 10, hint: None,
        }).unwrap();
        let RuntimeEvent::Worker(event) = rx.recv_timeout(Duration::from_millis(500))
            .expect("interactive mailbox open must not await prefetch") else {
            panic!("expected foreground event");
        };
        assert_eq!(event.request_id, requested);
        assert!(matches!(event.kind, WorkerEventKind::MailboxOpened { .. }));
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
