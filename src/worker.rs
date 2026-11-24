use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::backend::{ErrorKind, MailBackend, MailboxSyncHint};
use crate::cache::RawStore;
use crate::mail_image::MailImageEvent;
use crate::model::{
    CacheIdentity, Capabilities, Mailbox, MessageBody, MessageId, MessagePage, MessageSummary,
    OutgoingAttachment, OutgoingMessage,
};
use crate::outgoing::MailSender;

mod background;
mod foreground;
mod handle;
mod helpers;
mod idle;
// Keep test access to the same internal functions without reintroducing
// production imports or moving tests into the execution path.
#[cfg(test)]
use self::foreground::{compact_navigation_queue, pop_background_action};
#[cfg(test)]
use self::helpers::{download_attachment_job, fetch_attachments, write_export_stream};
#[cfg(test)]
use self::idle::{run_idle_worker, run_selected_idle_worker};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub enum ActionKind {
    Connect,
    ListMailboxes,
    MailboxStatus(String),
    InboxStatus {
        name: String,
        previous: Option<crate::backend::InboxCheckpoint>,
    },
    /// Wake token for the independent priority queue; never sent to IMAP.
    WakePriority,
    /// Wake from a dedicated IDLE session; the UI owns STATUS reconciliation.
    WatchMailboxChanged(String),
    WatchAccountChanged,
    /// Best-effort preview of an explicitly identified Inbox arrival.
    NewMailPreview {
        mailbox: String,
        count: usize,
        id: MessageId,
    },
    OpenMailbox {
        name: String,
        offset: usize,
        limit: usize,
        hint: Option<MailboxSyncHint>,
    },
    QueryMailbox {
        name: String,
        view: crate::backend::MailboxView,
        cached_ids: Vec<MessageId>,
        offset: usize,
        limit: usize,
    },
    /// Independent bounded conversation lookup; never supersedes mailbox pages.
    DiscoverThread {
        name: String,
        view: crate::backend::MailboxView,
        cached_ids: Vec<MessageId>,
        limit: usize,
    },
    FetchBody {
        mailbox: String,
        id: MessageId,
        html_width: Option<usize>,
    },
    /// Read-only, opportunistic preview fetches on the background session.
    PrefetchBodies {
        mailbox: String,
        ids: Vec<MessageId>,
        /// Warm only the first fetched body, not every speculative neighbor.
        html_width: Option<usize>,
    },
    /// Low-priority read-only header page for another folder. Cache is a
    /// display hint only; the backend must validate UIDVALIDITY.
    WarmMailbox {
        name: String,
        limit: usize,
        hint: Option<MailboxSyncHint>,
    },
    FetchRawForCompose {
        mailbox: String,
        id: MessageId,
        cache: Option<(PathBuf, CacheIdentity)>,
    },
    /// Priority read + MIME preparation on the existing background IMAP
    /// worker; a foreground fallback is available if that queue fills.
    PrepareCompose {
        mailbox: String,
        id: MessageId,
        summary: MessageSummary,
        intent: crate::app::ComposeIntent,
        own_addresses: Vec<String>,
        cache: Option<(PathBuf, CacheIdentity)>,
        /// Never perform network I/O for unverified cached UIDs.
        cache_only: bool,
    },
    FetchAttachments {
        mailbox: String,
        id: MessageId,
        /// Present only for generation-scoped IMAP UIDs; never trust a
        /// recycled UID or write a cache entry without UIDVALIDITY.
        cache: Option<(PathBuf, CacheIdentity)>,
    },
    RemoteAttachment {
        mailbox: String,
        job: RemoteAttachmentJob,
    },
    ExportRaw {
        mailbox: String,
        id: MessageId,
        path: PathBuf,
    },
    SetSeen {
        mailbox: String,
        id: MessageId,
        seen: bool,
    },
    Delete {
        mailbox: String,
        id: MessageId,
    },
    DeleteMany {
        mailbox: String,
        ids: Vec<MessageId>,
    },
    Move {
        source: String,
        destination: String,
        id: MessageId,
    },
    MoveMany {
        source: String,
        destination: String,
        ids: Vec<MessageId>,
    },
    ArchiveMany {
        source: String,
        destination: String,
        ids: Vec<MessageId>,
    },
    UnarchiveMany {
        source: String,
        destination: String,
        ids: Vec<MessageId>,
    },
    Send {
        message: OutgoingMessage,
        sent_mailbox: Option<String>,
        /// None = legacy auto; Some(empty) = disable; Some(names) = APPEND to
        /// every explicit folder, even when the SMTP provider files Sent.
        copy_to: Option<Vec<String>>,
    },
    SaveDraft {
        message: OutgoingMessage,
        mailbox: String,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum RemoteAttachmentJob {
    Preview {
        index: usize,
        file: crate::backend::RemoteAttachment,
    },
    Open(crate::backend::RemoteAttachment),
    Save {
        directory: PathBuf,
        files: Vec<(usize, crate::backend::RemoteAttachment)>,
    },
}

#[derive(Debug, Clone)]
pub enum RemoteAttachmentResult {
    Preview {
        index: usize,
        file: OutgoingAttachment,
    },
    Open(Arc<tempfile::TempPath>),
    Saved {
        count: usize,
        directory: PathBuf,
        error: Option<String>,
    },
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

/// Compact, cloneable diagnostic transported to the UI with its category intact.
#[derive(Debug, Clone)]
pub struct WorkerError {
    pub kind: ErrorKind,
    message: String,
    pub background_status: bool,
}

impl WorkerError {
    fn from_error(error: anyhow::Error) -> Self {
        Self {
            kind: crate::backend::classify(&error),
            message: format!("{error:#}"),
            background_status: false,
        }
    }

    fn background_status(error: anyhow::Error) -> Self {
        let mut result = Self::from_error(error);
        result.message = format!("background mailbox count: {}", result.message);
        result.background_status = true;
        result
    }
}
impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl From<String> for WorkerError {
    fn from(message: String) -> Self {
        Self {
            kind: crate::backend::legacy_kind(&message),
            background_status: message.starts_with("background mailbox count:"),
            message,
        }
    }
}
impl From<&str> for WorkerError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

#[derive(Debug, Clone)]
pub enum WorkerEventKind {
    Connected {
        capabilities: Capabilities,
    },
    Mailboxes(Vec<Mailbox>),
    MailboxStatus {
        name: String,
        total: usize,
        unread: usize,
        inbox: Option<crate::backend::InboxCheck>,
    },
    MailboxChanged {
        name: String,
    },
    AccountChanged,
    NewMailPreview {
        count: usize,
        id: Option<MessageId>,
        sender: String,
        subject: String,
        excerpt: String,
    },
    MailboxOpened {
        name: String,
        page: MessagePage,
        identity: Option<CacheIdentity>,
        uid_next: Option<u32>,
        elapsed_micros: u128,
    },
    MailboxQueried {
        name: String,
        page: MessagePage,
        identity: Option<CacheIdentity>,
        memberships: std::collections::HashMap<MessageId, String>,
        elapsed_micros: u128,
    },
    ThreadDiscovered {
        name: String,
        page: MessagePage,
        identity: Option<CacheIdentity>,
        memberships: HashMap<MessageId, String>,
    },
    ThreadDiscoveryFailed,
    MailboxOpenFailed {
        name: String,
        error: WorkerError,
    },
    MailboxWarmed {
        name: String,
        page: MessagePage,
        identity: Option<CacheIdentity>,
        uid_next: Option<u32>,
    },
    MailboxWarmFailed {
        name: String,
    },
    BodyLoaded(MessageBody),
    BodyPrefetched {
        mailbox: String,
        body: MessageBody,
    },
    BodyLoadFailed {
        error: WorkerError,
    },
    DraftPrepared {
        id: MessageId,
        // Parsing large draft MIME belongs on a mail worker, not the UI loop.
        result: Result<Box<crate::app::Composer>, String>,
    },
    ComposePrepared {
        id: MessageId,
        // Most worker events contain small headers or status updates. Keeping
        // the large, rarely sent composer inline enlarges *every* worker and
        // runtime event slot, including two preallocated event queues.
        result: Result<Box<crate::app::Composer>, String>,
    },
    AttachmentsLoaded {
        mailbox: String,
        id: MessageId,
        attachments: Vec<OutgoingAttachment>,
    },
    RemoteAttachmentsListed {
        mailbox: String,
        id: MessageId,
        attachments: Vec<crate::backend::RemoteAttachment>,
    },
    RemoteAttachmentCompleted(Result<RemoteAttachmentResult, String>),
    MessageExported {
        path: PathBuf,
    },
    SeenChanged {
        id: MessageId,
        seen: bool,
    },
    MutationComplete,
    DraftSaved {
        mailbox: String,
    },
    DraftSaveFailed {
        mailbox: String,
        error: WorkerError,
    },
    MessageSent {
        smtp_status: String,
        sent_mailbox: Option<String>,
        affected_mailboxes: Vec<String>,
        sent_copy_saved: bool,
        sent_copy_warning: Option<String>,
        sent_copy_disabled: bool,
    },
    Error(WorkerError),
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
    AccountCleanupFailed {
        name: String,
        error: String,
    },
    Worker(WorkerEvent),
    /// Account reloads reuse numeric account indices. Worker results therefore
    /// travel through a per-reload relay and keep their *account epoch*, so an
    /// event queued before a reload can never update the new account at that index.
    ScopedWorker {
        epoch: u64,
        event: Box<RuntimeEvent>,
    },
    /// Foreground events carry the worker generation so results from a retired
    /// blocked IMAP session can never update a recovered account.
    ForegroundWorker {
        generation: u64,
        event: WorkerEvent,
    },
    MailImage(MailImageEvent),
    SetupDiscovered {
        email: String,
        result: Result<crate::discovery::MailConfiguration, String>,
    },
    SetupLoginFinished {
        request_id: u64,
        result: Result<Option<String>, String>,
    },
    SetupTestFinished {
        request_id: u64,
        report: crate::setup::probe::ProbeReport,
    },
    InputStopped,
}

/// One optional extra IMAP IDLE connection watches the mailbox currently
/// displayed by the active account. The Inbox retains its own watcher.
/// Changing folders cancels the previous watch without blocking the UI.
struct SelectedIdleTarget {
    state: Mutex<SelectedIdleState>,
    changed: Condvar,
}

struct SelectedIdleState {
    mailbox: Option<String>,
    revision: u64,
    cancel: Arc<AtomicBool>,
    stopped: bool,
}

impl SelectedIdleTarget {
    fn new() -> Self {
        Self {
            state: Mutex::new(SelectedIdleState {
                mailbox: None,
                revision: 0,
                cancel: Arc::new(AtomicBool::new(false)),
                stopped: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn set_mailbox(&self, name: Option<&str>) {
        let mut state = self.state.lock().expect("selected IDLE lock");
        if state.stopped || state.mailbox.as_deref() == name {
            return;
        }
        state.cancel.store(true, Ordering::Release);
        state.mailbox = name.map(str::to_owned);
        state.revision = state.revision.wrapping_add(1);
        state.cancel = Arc::new(AtomicBool::new(false));
        self.changed.notify_all();
    }

    fn stop(&self) {
        let mut state = self.state.lock().expect("selected IDLE lock");
        state.stopped = true;
        state.cancel.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    fn wait_for_mailbox(&self) -> Option<(String, u64, Arc<AtomicBool>)> {
        self.wait_for_mailbox_timeout(Duration::from_secs(30))
    }

    fn wait_for_mailbox_timeout(
        &self,
        timeout: Duration,
    ) -> Option<(String, u64, Arc<AtomicBool>)> {
        let state = self.state.lock().expect("selected IDLE lock");
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                !state.stopped && state.mailbox.is_none()
            })
            .expect("selected IDLE lock");
        if state.mailbox.is_none() {
            state.stopped = true;
        }
        if state.stopped {
            return None;
        }
        Some((
            state.mailbox.clone()?,
            state.revision,
            Arc::clone(&state.cancel),
        ))
    }

    /// After a successful watch that cannot use push, wait for the mailbox
    /// to change. After a transport error, retry after bounded backoff or
    /// immediately on selection change. No busy polling on unsupported IDLE.
    fn wait_for_change(&self, revision: u64, retry: Option<Duration>) {
        let state = self.state.lock().expect("selected IDLE lock");
        let unchanged =
            |state: &mut SelectedIdleState| !state.stopped && state.revision == revision;
        if let Some(delay) = retry {
            drop(
                self.changed
                    .wait_timeout_while(state, delay, unchanged)
                    .expect("selected IDLE lock"),
            );
        } else {
            drop(
                self.changed
                    .wait_while(state, unchanged)
                    .expect("selected IDLE lock"),
            );
        }
    }
}

pub struct WorkerHandle {
    sender: SyncSender<Action>,
    // STATUS runs on a separate IMAP session. A stalled unread-count lookup
    // must never get ahead of an interactive SELECT or FETCH.
    status_sender: Option<SyncSender<Action>>,
    // Explicit reads must not fail just because speculative prefetch/STATUS
    // filled the background queue. Both channels feed the SAME IMAP session.
    priority_sender: Option<SyncSender<Action>>,
    join: Option<JoinHandle<()>>,
    status_join: Option<JoinHandle<()>>,
    idle_stop: Arc<AtomicBool>,
    idle_join: Option<JoinHandle<()>>,
    selected_idle: Option<Arc<SelectedIdleTarget>>,
    selected_idle_join: Option<JoinHandle<Box<dyn MailBackend>>>,
    selected_idle_backend: Option<Box<dyn MailBackend>>,
    selected_idle_account: usize,
    foreground_generation: Arc<AtomicU64>,
    unacknowledged_writes: Mutex<HashSet<u64>>,
    recovery: Option<ForegroundRecovery>,
    retired_foregrounds: Vec<JoinHandle<()>>,
    last_foreground_recovery: Option<Instant>,
}

struct ForegroundRecovery {
    account: usize,
    config: crate::config::AccountConfig,
    events: SyncSender<RuntimeEvent>,
}

/// Never retire a worker that may have submitted an IMAP/SMTP write. A
/// completion can be lost, but it must not become a duplicated operation.
fn is_foreground_write(kind: &ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::SetSeen { .. }
            | ActionKind::Delete { .. }
            | ActionKind::DeleteMany { .. }
            | ActionKind::Move { .. }
            | ActionKind::MoveMany { .. }
            | ActionKind::ArchiveMany { .. }
            | ActionKind::UnarchiveMany { .. }
            | ActionKind::Send { .. }
            | ActionKind::SaveDraft { .. }
            | ActionKind::ExportRaw { .. }
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod foreground_recovery_tests;

#[cfg(test)]
mod foreground_ack_safety_tests;
