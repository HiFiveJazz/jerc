//! Small, private, per-account persistent mailbox index.
//!
//! The UI reads this once before connecting. Writes are serialized on a
//! dedicated thread so a slow disk cannot stall keyboard input or painting.
//! This is a display cache, never an authority for IMAP UID mutations.
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::AccountConfig;
use crate::model::{Mailbox, MessageBody, MessageId, MessageSummary};

const VERSION: u8 = 1;
const MAX_CACHE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FOLDERS: usize = 256;
pub(crate) const MAX_MESSAGES_PER_FOLDER: usize = 400;
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_BODIES_PER_FOLDER: usize = 12;
// Keep the startup index small. Additional bodies live in a separate, lazily
// accessed, per-account disk store. No database dependency or eager scan.
const MAX_BODY_FILE_BYTES: u64 = 512 * 1024;
const MAX_BODY_STORE_BYTES: u64 = 64 * 1024 * 1024;
const PRUNE_AFTER_WRITES: usize = 8;
// Display-cache changes can wait briefly; unlike outgoing mail and offline
// mutations, this index is not a crash-durable journal. Coalesce bursts but
// bound the delay under continuous background sync activity.
const CACHE_WRITE_QUIET: Duration = Duration::from_millis(500);
const CACHE_WRITE_MAX_DELAY: Duration = Duration::from_secs(2);
/// Bounded pending flag changes, scoped to the server's mailbox generation.
pub(crate) const MAX_DEFERRED_SEEN: usize = 512;
/// Queue is bounded independently of mailbox index and MIME storage.
pub(crate) const MAX_DEFERRED_MUTATIONS: usize = 256;
const MAX_MUTATION_IDS: usize = 512;
const MAX_MUTATION_JOURNAL_BYTES: u64 = 256 * 1024;

// serde_json emits many tiny Write calls. Without buffering, writing a large
// mailbox index can generate hundreds of thousands of write(2) syscalls.
// Keep the existing streaming format and atomic temp-file replacement, but
// coalesce those fragments before they reach the filesystem.
const CACHE_JSON_BUFFER_BYTES: usize = 64 * 1024;

fn write_json_buffered<W: Write, T: Serialize>(sink: W, value: &T) -> std::io::Result<()> {
    let mut writer = BufWriter::with_capacity(CACHE_JSON_BUFFER_BYTES, sink);
    serde_json::to_writer(&mut writer, value).map_err(std::io::Error::other)?;
    writer.flush()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DeferredMutationKind {
    Delete,
    Move { destination: String },
    Archive { destination: String },
    Unarchive { destination: String },
}

/// Once submitted to IMAP, a non-idempotent COPY/MOVE may have partially
/// succeeded even if the worker reported an error. Never blindly retry it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DeferredMutationState {
    #[default]
    Queued,
    NeedsReview,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeferredMutation {
    pub mailbox: String,
    pub validity: u32,
    pub ids: Vec<MessageId>,
    pub kind: DeferredMutationKind,
    #[serde(default)]
    pub state: DeferredMutationState,
}

impl DeferredMutation {
    pub(crate) fn has_valid_identity(&self) -> bool {
        !self.mailbox.is_empty()
            && self.mailbox.len() <= 512
            && !self.ids.is_empty()
            && self.ids.len() <= MAX_MUTATION_IDS
            && self.ids.iter().all(|id| {
                id.0.len() <= 128 && id.0.starts_with(&format!("imap-uid:{}:", self.validity))
            })
            && match &self.kind {
                DeferredMutationKind::Delete => true,
                DeferredMutationKind::Move { destination }
                | DeferredMutationKind::Archive { destination }
                | DeferredMutationKind::Unarchive { destination } => {
                    !destination.is_empty()
                        && destination.len() <= 512
                        && destination != &self.mailbox
                }
            }
    }
}

/// Separate, synchronously committed WAL for destructive operations. The
/// coalescing mailbox cache writer must not race with this journal, and an
/// abrupt process exit must not lose an acknowledged user action.
#[derive(Debug, Clone)]
pub(crate) struct MutationStore {
    path: PathBuf,
    readable: bool,
}

#[derive(Serialize, Deserialize)]
struct MutationFile {
    version: u8,
    entries: Vec<DeferredMutation>,
}

// Serialize the existing slice directly; avoid cloning the operation journal.
#[derive(Serialize)]
struct MutationFileRef<'a> {
    version: u8,
    entries: &'a [DeferredMutation],
}

impl MutationStore {
    fn read(&self) -> std::io::Result<Vec<DeferredMutation>> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_MUTATION_JOURNAL_BYTES {
            return Err(std::io::Error::other(
                "invalid offline operation journal file",
            ));
        }
        let file = File::open(&self.path)?;
        if file.metadata()?.len() > MAX_MUTATION_JOURNAL_BYTES {
            return Err(std::io::Error::other(
                "offline operation journal exceeds size limit",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_MUTATION_JOURNAL_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MUTATION_JOURNAL_BYTES {
            return Err(std::io::Error::other(
                "offline operation journal exceeds size limit",
            ));
        }
        let journal: MutationFile =
            serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        if journal.version != VERSION
            || journal.entries.len() > MAX_DEFERRED_MUTATIONS
            || !journal
                .entries
                .iter()
                .all(DeferredMutation::has_valid_identity)
        {
            return Err(std::io::Error::other("invalid offline operation journal"));
        }
        Ok(journal.entries)
    }

    pub(crate) fn save(&self, entries: &[DeferredMutation]) -> std::io::Result<()> {
        if !self.readable {
            return Err(std::io::Error::other(
                "offline operation journal needs manual recovery (not overwritten)",
            ));
        }
        if entries.len() > MAX_DEFERRED_MUTATIONS
            || !entries.iter().all(DeferredMutation::has_valid_identity)
        {
            return Err(std::io::Error::other(
                "invalid offline operation identity or queue full",
            ));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("no journal directory"))?;
        fs::create_dir_all(parent)?;
        secure_private_dir(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        write_json_buffered(
            &mut tmp,
            &MutationFileRef {
                version: VERSION,
                entries,
            },
        )?;
        if tmp.as_file().metadata()?.len() > MAX_MUTATION_JOURNAL_BYTES {
            return Err(std::io::Error::other(
                "offline operation journal exceeds size limit",
            ));
        }
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path).map_err(|error| error.error)?;
        #[cfg(unix)]
        {
            // Some filesystems cannot fsync directories; once rename succeeds
            // do not falsely report a failed operation (which the UI might retry).
            let _ = File::open(parent).and_then(|dir| dir.sync_all());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeferredSeen {
    pub mailbox: String,
    pub validity: u32,
    pub id: MessageId,
    pub seen: bool,
}

impl DeferredSeen {
    pub(crate) fn has_valid_identity(&self) -> bool {
        !self.mailbox.is_empty()
            && self.mailbox.len() <= 512
            && self.id.0.len() <= 128
            && self
                .id
                .0
                .starts_with(&format!("imap-uid:{}:", self.validity))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CachedMailbox {
    pub messages: Vec<MessageSummary>,
    pub bodies: HashMap<MessageId, MessageBody>,
    pub message_index: usize,
    pub message_view_offset: usize,
    pub next_page_offset: usize,
    pub has_more: bool,
    /// RFC 3501 UIDVALIDITY is required for safely reusing cached IMAP UIDs.
    pub uid_validity: Option<u32>,
    /// UIDNEXT from SELECT. Optional for caches written by older releases.
    #[serde(default)]
    pub uid_next: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    #[serde(default)]
    pub mailboxes: Vec<Mailbox>,
    #[serde(default)]
    pub folders: HashMap<String, CachedMailbox>,
    /// Pending idempotent read/unread changes (not offline deletes or moves).
    #[serde(default)]
    pub deferred_seen: Vec<DeferredSeen>,
    /// Lives in an independent crash-durable file, not the display snapshot.
    #[serde(skip)]
    pub deferred_mutations: Vec<DeferredMutation>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    version: u8,
    snapshot: Snapshot,
}

// Serialize the live snapshot by reference: cloning a potentially large mailbox
// cache on every background write would add unnecessary allocations and latency.
#[derive(Serialize)]
struct CacheFileRef<'a> {
    version: u8,
    snapshot: &'a Snapshot,
}

/// Copy only the bodies the on-disk cache can actually keep. The active
/// reader may hold up to 80 bodies (including large rich HTML layouts), but
/// disk persistence only retains the first 12 eligible bodies among the first
/// 400 message headers. Avoid cloning the full working set on every update.
#[cfg(test)]
pub(crate) fn persistable_bodies(
    messages: &[MessageSummary],
    bodies: &HashMap<MessageId, MessageBody>,
) -> HashMap<MessageId, MessageBody> {
    persistable_bodies_near(messages, bodies, 0)
}

/// Cache the currently selected message before the surrounding previews.
/// After restarting, re-opening that message should not wait for Gmail.
pub(crate) fn persistable_bodies_near(
    messages: &[MessageSummary],
    bodies: &HashMap<MessageId, MessageBody>,
    selected_index: usize,
) -> HashMap<MessageId, MessageBody> {
    persistable_bodies_near_impl(messages, bodies, selected_index, false)
}

/// Writer snapshots keep the original text/HTML, but never copy the derived
/// rich terminal layout. The live in-session cache keeps that layout so folder
/// switches and the reader stay fast.
pub(crate) fn persistable_bodies_near_compact(
    messages: &[MessageSummary],
    bodies: &HashMap<MessageId, MessageBody>,
    selected_index: usize,
) -> HashMap<MessageId, MessageBody> {
    persistable_bodies_near_impl(messages, bodies, selected_index, true)
}

/// Select the same bounded nearest-to-cursor body set for both the in-memory
/// LRU and the writer, but let the former retain ownership of rich layouts.
fn eligible_body_ids_near(
    messages: &[MessageSummary],
    bodies: &HashMap<MessageId, MessageBody>,
    selected_index: usize,
) -> Vec<MessageId> {
    let mut selected = Vec::with_capacity(MAX_BODIES_PER_FOLDER.min(bodies.len()));
    let limit = messages.len().min(MAX_MESSAGES_PER_FOLDER);
    let mut positions = (0..limit).collect::<Vec<_>>();
    positions.sort_by_key(|index| index.abs_diff(selected_index));
    for index in positions {
        if selected.len() >= MAX_BODIES_PER_FOLDER {
            break;
        }
        let message = &messages[index];
        let Some(body) = bodies.get(&message.id) else {
            continue;
        };
        let bytes = body
            .plain_text
            .as_deref()
            .unwrap_or_default()
            .len()
            .saturating_add(body.html_source.as_deref().unwrap_or_default().len())
            .saturating_add(body.html_text.as_deref().unwrap_or_default().len());
        if bytes <= MAX_BODY_BYTES {
            selected.push(message.id.clone());
        }
    }
    selected
}

/// Prune a warmed in-session cache without deep-cloning rich HTML layout data.
/// Cached bodies remain fully rendered and ready for immediate folder switches.
pub(crate) fn retain_bodies_near(
    messages: &[MessageSummary],
    bodies: &mut HashMap<MessageId, MessageBody>,
    selected_index: usize,
) {
    let keep = eligible_body_ids_near(messages, bodies, selected_index)
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    bodies.retain(|id, _| keep.contains(id));
}

fn persistable_bodies_near_impl(
    messages: &[MessageSummary],
    bodies: &HashMap<MessageId, MessageBody>,
    selected_index: usize,
    compact: bool,
) -> HashMap<MessageId, MessageBody> {
    let ids = eligible_body_ids_near(messages, bodies, selected_index);
    let mut selected = HashMap::with_capacity(ids.len());
    for id in ids {
        if let Some(body) = bodies.get(&id) {
            let copy = if compact {
                compact_body(body)
            } else {
                Some(body.clone())
            };
            if let Some(copy) = copy {
                selected.insert(id, copy);
            }
        }
    }
    selected
}

impl CachedMailbox {
    /// Produce a minimal copy for the asynchronous disk writer without copying
    /// expensive rich HTML spans from the already-warmed in-memory folder.
    pub(crate) fn for_persistence(&self) -> Self {
        Self {
            messages: self.messages.clone(),
            bodies: persistable_bodies_near_compact(
                &self.messages,
                &self.bodies,
                self.message_index,
            ),
            message_index: self.message_index,
            message_view_offset: self.message_view_offset,
            next_page_offset: self.next_page_offset,
            has_more: self.has_more,
            uid_validity: self.uid_validity,
            uid_next: self.uid_next,
        }
    }
}

pub(crate) enum Change {
    Mailboxes(Vec<Mailbox>),
    Folder(String, CachedMailbox),
    RemoveFolder(String),
    Body(String, u32, MessageBody),
    DeferredSeen(Vec<DeferredSeen>),
}

/// A single latest change per folder. The wake channel only holds a token;
/// arbitrary numbers of reader selections cannot queue expensive snapshots.
#[derive(Default)]
struct PendingChanges {
    mailboxes: Option<Vec<Mailbox>>,
    folders: HashMap<String, Option<CachedMailbox>>,
    bodies: HashMap<(String, MessageId), (u32, MessageBody)>,
    deferred_seen: Option<Vec<DeferredSeen>>,
}

impl PendingChanges {
    fn insert(&mut self, change: Change) {
        match change {
            Change::Mailboxes(mailboxes) => self.mailboxes = Some(mailboxes),
            Change::DeferredSeen(entries) => self.deferred_seen = Some(entries),
            Change::Folder(name, folder) => {
                if self.folders.len() < MAX_FOLDERS || self.folders.contains_key(&name) {
                    self.folders.insert(name, Some(folder));
                }
            }
            Change::Body(mailbox, validity, body) => {
                let key = (mailbox, body.id.clone());
                if self.bodies.len() < 64 || self.bodies.contains_key(&key) {
                    self.bodies.insert(key, (validity, body));
                }
            }
            Change::RemoveFolder(name) => {
                if self.folders.len() == MAX_FOLDERS
                    && !self.folders.contains_key(&name)
                    && let Some(evict) = self.folders.keys().next().cloned()
                {
                    self.folders.remove(&evict);
                }
                self.folders.insert(name, None);
            }
        }
    }

    fn drain_bodies(&mut self) -> Vec<(String, u32, MessageBody)> {
        std::mem::take(&mut self.bodies)
            .into_iter()
            .map(|((mailbox, _), (validity, body))| (mailbox, validity, body))
            .collect()
    }

    fn drain_into(&mut self, snapshot: &mut Snapshot) -> bool {
        let mut changed = false;
        if let Some(mailboxes) = self.mailboxes.take() {
            changed |= apply(snapshot, Change::Mailboxes(mailboxes));
        }
        if let Some(entries) = self.deferred_seen.take() {
            changed |= apply(snapshot, Change::DeferredSeen(entries));
        }
        // Do not short-circuit: every pending change must still be applied.
        for (name, folder) in self.folders.drain() {
            changed |= match folder {
                Some(folder) => apply(snapshot, Change::Folder(name, folder)),
                None => apply(snapshot, Change::RemoveFolder(name)),
            };
        }
        changed
    }
}

pub(crate) struct Writer {
    wake: Option<SyncSender<()>>,
    pending: Arc<Mutex<PendingChanges>>,
    join: Option<JoinHandle<()>>,
    body_store: BodyStore,
    mutation_store: MutationStore,
}

impl Writer {
    #[cfg(test)]
    pub(crate) fn for_test_mutations(path: PathBuf) -> Self {
        Self {
            wake: None,
            pending: Arc::new(Mutex::new(PendingChanges::default())),
            join: None,
            body_store: BodyStore {
                dir: path.with_extension("bodies"),
            },
            mutation_store: MutationStore {
                path,
                readable: true,
            },
        }
    }

    pub(crate) fn save_mutations(&self, entries: &[DeferredMutation]) -> std::io::Result<()> {
        self.mutation_store.save(entries)
    }

    pub(crate) fn mutations_available(&self) -> bool {
        self.mutation_store.readable
    }

    pub(crate) fn body_store(&self) -> BodyStore {
        self.body_store.clone()
    }

    pub(crate) fn raw_store(&self) -> RawStore {
        RawStore {
            dir: self.body_store.dir.with_extension("raw"),
        }
    }

    pub(crate) fn send(&self, change: Change) {
        let Some(wake) = &self.wake else { return };
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(change);
            // Capacity one, coalesced by folder: never block terminal input.
            match wake.try_send(()) {
                Ok(()) | Err(TrySendError::Full(())) => {}
                Err(TrySendError::Disconnected(())) => {}
            }
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.wake.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Only IMAP data is persisted. Maildir remains authoritative on disk already.
/// The key uses the account's name and non-secret IMAP identity, not tokens or
/// credential commands. Files contain message metadata, so permissions matter.
pub(crate) fn open(account: &AccountConfig) -> (Snapshot, Option<Writer>) {
    if std::env::var_os("JERC_DISABLE_CACHE").is_some()
        || !crate::backend::ImapSource::supports(&account.source)
    {
        return (Snapshot::default(), None);
    }
    let Some(dir) = dirs::cache_dir().map(|path| path.join("jerc").join("mail-index")) else {
        return (Snapshot::default(), None);
    };
    let stem = account_cache_stem(account);
    let path = dir.join(format!("{stem}.json"));
    let mut mutation_store = MutationStore {
        path: dir.join(format!("{stem}.mutations.json")),
        readable: true,
    };
    let mutations = match mutation_store.read() {
        Ok(entries) => entries,
        Err(_) => {
            // Refuse to overwrite an unreadable journal. The user must
            // inspect/recover it, rather than silently losing operations.
            mutation_store.readable = false;
            Vec::new()
        }
    };
    let body_store = BodyStore {
        dir: dir.join(format!("{stem}.bodies")),
    };
    let cache_start = Instant::now();
    let loaded = read_snapshot(&path);
    // A successful disk read with no usable folders is not a warm mailbox
    // cache. The log contains timing only, never account or message details.
    crate::perf::record(
        "cache_read",
        None,
        cache_start.elapsed(),
        loaded
            .as_ref()
            .is_some_and(|snapshot| !snapshot.folders.is_empty()),
    );
    let mut snapshot = loaded.unwrap_or_default();
    // Clone the startup index once using shared message headers and body text.
    // The background writer needs independent folder maps, but not another
    // expensive JSON parse or another copy of every RFC 5322 header string.
    let writer_snapshot = snapshot.clone();
    snapshot.deferred_mutations = mutations;
    let (wake, rx) = mpsc::sync_channel::<()>(1);
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    let thread_pending = Arc::clone(&pending);
    // The UI and writer own separate mutable indexes but share immutable
    // message metadata until a header is actually changed.
    let writer_body_store = body_store.clone();
    let join = thread::Builder::new()
        .name("mail-cache-writer".into())
        .spawn(move || {
            run_cache_writer(rx, path, thread_pending, writer_body_store, writer_snapshot);
        });
    match join {
        Ok(join) => (
            snapshot,
            Some(Writer {
                wake: Some(wake),
                pending,
                join: Some(join),
                body_store,
                mutation_store,
            }),
        ),
        Err(_) => (snapshot, None),
    }
}

// Keep the real writer loop in one place so regression tests can exercise the
// same batching, final flush, and shutdown behavior as production.
fn run_cache_writer(
    rx: Receiver<()>,
    path: PathBuf,
    pending: Arc<Mutex<PendingChanges>>,
    body_store: BodyStore,
    mut stored: Snapshot,
) {
    let mut index_dirty = false;
    let mut body_writes = 0;
    while rx.recv().is_ok() {
        // Repeated status updates are often identical. Batch real
        // changes without starving disk persistence on busy servers.
        wait_for_cache_batch(&rx, CACHE_WRITE_QUIET, CACHE_WRITE_MAX_DELAY);
        body_writes +=
            flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut index_dirty);
        if body_writes >= PRUNE_AFTER_WRITES {
            let _ = body_store.prune();
            body_writes = 0;
        }
    }
    // The sender disconnects on shutdown. Persist any pending updates
    // before joining the thread, even if the quiet timer did not fire.
    body_writes += flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut index_dirty);
    if body_writes > 0 {
        let _ = body_store.prune();
    }
}

// The event channel carries only wake tokens, not snapshots. Waiting for a
// quiet period collapses rapid navigation and STATUS updates; the deadline
// prevents endless traffic from postponing persistence indefinitely.
fn wait_for_cache_batch(rx: &Receiver<()>, quiet: Duration, max_delay: Duration) {
    let start = Instant::now();
    loop {
        let remaining = max_delay.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(quiet.min(remaining)) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

// Keep the index dirty if a write fails. An unchanged follow-up update or a
// clean shutdown must still have a chance to retry the failed checkpoint.
fn flush_cache_changes(
    path: &Path,
    stored: &mut Snapshot,
    pending: &Arc<Mutex<PendingChanges>>,
    body_store: &BodyStore,
    index_dirty: &mut bool,
) -> usize {
    let (changed, bodies) = if let Ok(mut changes) = pending.lock() {
        let changed = changes.drain_into(stored);
        (changed, changes.drain_bodies())
    } else {
        (false, Vec::new())
    };
    *index_dirty |= changed;
    if *index_dirty && write_snapshot(path, stored).is_ok() {
        *index_dirty = false;
    }
    let mut writes = 0;
    for (mailbox, validity, body) in bodies {
        if matches!(body_store.write(&mailbox, validity, &body), Ok(true)) {
            writes += 1;
        }
    }
    writes
}

/// Return true only when the on-disk representation would actually change.
/// Flags remain mutable, while shared immutable headers compare by value.
fn apply(snapshot: &mut Snapshot, change: Change) -> bool {
    match change {
        Change::Mailboxes(mailboxes) => {
            if snapshot.mailboxes == mailboxes {
                return false;
            }
            snapshot.mailboxes = mailboxes;
            true
        }
        Change::Folder(name, mut folder) => {
            // Never cache UID-based IMAP data without a server UIDVALIDITY.
            if folder.uid_validity.is_none() {
                return false;
            }
            if folder.messages.len() > MAX_MESSAGES_PER_FOLDER {
                folder.messages.truncate(MAX_MESSAGES_PER_FOLDER);
                folder.next_page_offset = folder.messages.len();
                folder.has_more = true;
            }
            for body in folder.bodies.values_mut() {
                body.html_lines = None;
                body.html_renderer = None;
                body.html_render_width = 0;
            }
            folder.bodies.retain(|id, body| {
                folder.messages.iter().any(|message| &message.id == id)
                    && body.plain_text.as_deref().unwrap_or_default().len()
                        + body.html_source.as_deref().unwrap_or_default().len()
                        + body.html_text.as_deref().unwrap_or_default().len()
                        <= MAX_BODY_BYTES
            });
            if folder.bodies.len() > MAX_BODIES_PER_FOLDER {
                let keep = folder
                    .messages
                    .iter()
                    .filter_map(|message| {
                        folder
                            .bodies
                            .contains_key(&message.id)
                            .then_some(message.id.clone())
                    })
                    .take(MAX_BODIES_PER_FOLDER)
                    .collect::<Vec<_>>();
                folder.bodies.retain(|id, _| keep.contains(id));
            }
            if snapshot.folders.len() >= MAX_FOLDERS && !snapshot.folders.contains_key(&name) {
                return false;
            }
            if snapshot
                .folders
                .get(&name)
                .is_some_and(|old| old == &folder)
            {
                return false;
            }
            snapshot.folders.insert(name, folder);
            true
        }
        Change::RemoveFolder(name) => snapshot.folders.remove(&name).is_some(),
        Change::Body(..) => false, // Persisted separately; never grows startup JSON.
        Change::DeferredSeen(entries) => {
            let entries: Vec<_> = entries
                .into_iter()
                .filter(DeferredSeen::has_valid_identity)
                .take(MAX_DEFERRED_SEEN)
                .collect();
            if snapshot.deferred_seen == entries {
                return false;
            }
            snapshot.deferred_seen = entries;
            true
        }
    }
}

/// Lazy, bounded body store. Only the selected message is read at the UI
/// boundary; the writer persists fetched bodies off the terminal thread.
/// Folder+UIDVALIDITY+UID are part of the file identity, so a UID reset
/// cannot resurrect the content of a different email.
#[derive(Clone)]
pub(crate) struct BodyStore {
    dir: PathBuf,
}

#[derive(Deserialize)]
struct BodyFile {
    version: u8,
    mailbox: String,
    validity: u32,
    body: MessageBody,
}

#[derive(Serialize)]
struct BodyFileRef<'a> {
    version: u8,
    mailbox: &'a str,
    validity: u32,
    body: &'a MessageBody,
}

impl BodyStore {
    fn body_path(&self, mailbox: &str, validity: u32, id: &MessageId) -> PathBuf {
        use ring::digest::{SHA256, digest};
        let key = format!("{mailbox}\0{validity}\0{}", id.0);
        let hash = digest(&SHA256, key.as_bytes());
        let hex = hash.as_ref()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        self.dir.join(format!("{hex}.json"))
    }

    /// Cheap existence probe for prefetch filtering. The reader validates the
    /// complete record on actual use, falling back to IMAP if it is corrupt.
    pub(crate) fn has_candidate(&self, mailbox: &str, validity: u32, id: &MessageId) -> bool {
        if !id.0.starts_with(&format!("imap-uid:{validity}:")) {
            return false;
        }
        fs::symlink_metadata(self.body_path(mailbox, validity, id)).is_ok_and(|metadata| {
            metadata.file_type().is_file() && metadata.len() <= MAX_BODY_FILE_BYTES
        })
    }

    pub(crate) fn read(&self, mailbox: &str, validity: u32, id: &MessageId) -> Option<MessageBody> {
        if !id.0.starts_with(&format!("imap-uid:{validity}:")) {
            return None;
        }
        let path = self.body_path(mailbox, validity, id);
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_BODY_FILE_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        File::open(path)
            .ok()?
            .take(MAX_BODY_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_BODY_FILE_BYTES {
            return None;
        }
        let record: BodyFile = serde_json::from_slice(&bytes).ok()?;
        (record.version == VERSION
            && record.mailbox == mailbox
            && record.validity == validity
            && record.body.id == *id)
            .then_some(record.body)
    }

    fn write(&self, mailbox: &str, validity: u32, body: &MessageBody) -> std::io::Result<bool> {
        if !body.id.0.starts_with(&format!("imap-uid:{validity}:")) {
            return Err(std::io::Error::other("body UIDVALIDITY mismatch"));
        }
        let path = self.body_path(mailbox, validity, &body.id);
        // An immutable UID-scoped MIME body is often scheduled for persistence
        // more than once. Read/compare the existing entry before creating a
        // temporary file; enriched metadata will still trigger replacement.
        if self.read(mailbox, validity, &body.id).as_ref() == Some(body) {
            return Ok(false);
        }
        fs::create_dir_all(&self.dir)?;
        secure_private_dir(&self.dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        write_json_buffered(
            &mut tmp,
            &BodyFileRef {
                version: VERSION,
                mailbox,
                validity,
                body,
            },
        )?;
        if tmp.as_file().metadata()?.len() > MAX_BODY_FILE_BYTES {
            return Err(std::io::Error::other("body exceeds offline cache limit"));
        }
        tmp.persist(path).map_err(|error| error.error)?;
        Ok(true)
    }

    fn prune(&self) -> std::io::Result<()> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("json")) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_file() {
                continue;
            }
            total = total.saturating_add(metadata.len());
            files.push((
                path,
                metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                metadata.len(),
            ));
        }
        if total <= MAX_BODY_STORE_BYTES {
            return Ok(());
        }
        files.sort_by_key(|(_, modified, _)| *modified);
        for (path, _, size) in files {
            if total <= MAX_BODY_STORE_BYTES {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
        Ok(())
    }
}

/// A small, private raw-MIME cache for previously opened attachments.
/// Only UIDVALIDITY-scoped IMAP messages are eligible. The reader never scans
/// this directory at startup; access and pruning happen on the mail worker.
#[derive(Debug, Clone)]
pub(crate) struct RawStore {
    pub(crate) dir: PathBuf,
}

const MAX_RAW_CACHE_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RAW_CACHE_BYTES: u64 = 128 * 1024 * 1024;

impl RawStore {
    fn raw_path(&self, mailbox: &str, validity: u32, id: &MessageId) -> PathBuf {
        use ring::digest::{SHA256, digest};
        let key = format!("{mailbox}\0{validity}\0{}", id.0);
        let hash = digest(&SHA256, key.as_bytes());
        let hex = hash.as_ref()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        self.dir.join(format!("{hex}.eml"))
    }

    fn valid_id(validity: u32, id: &MessageId) -> bool {
        id.0.starts_with(&format!("imap-uid:{validity}:"))
    }

    /// Disk reads happen on a worker, never in the terminal render loop.
    pub(crate) fn read(&self, mailbox: &str, validity: u32, id: &MessageId) -> Option<Vec<u8>> {
        if !Self::valid_id(validity, id) {
            return None;
        }
        let path = self.raw_path(mailbox, validity, id);
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_RAW_CACHE_FILE_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        File::open(path)
            .ok()?
            .take(MAX_RAW_CACHE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        (!bytes.is_empty() && bytes.len() as u64 <= MAX_RAW_CACHE_FILE_BYTES).then_some(bytes)
    }

    /// Large messages remain available through IMAP but aren't stored twice.
    /// Atomic replacement prevents partial/corrupt cache entries on crashes.
    pub(crate) fn write(
        &self,
        mailbox: &str,
        validity: u32,
        id: &MessageId,
        raw: &[u8],
    ) -> std::io::Result<()> {
        if !Self::valid_id(validity, id) {
            return Err(std::io::Error::other("raw MIME UIDVALIDITY mismatch"));
        }
        if raw.is_empty() || raw.len() as u64 > MAX_RAW_CACHE_FILE_BYTES {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?;
        secure_private_dir(&self.dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        tmp.write_all(raw)?;
        tmp.flush()?;
        tmp.persist(self.raw_path(mailbox, validity, id))
            .map_err(|error| error.error)?;
        self.prune()
    }

    fn prune(&self) -> std::io::Result<()> {
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(&self.dir)? {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("eml")) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_file() {
                continue;
            }
            total = total.saturating_add(metadata.len());
            files.push((
                path,
                metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                metadata.len(),
            ));
        }
        if total > MAX_RAW_CACHE_BYTES {
            files.sort_by_key(|(_, modified, _)| *modified);
            for (path, _, size) in files {
                if total <= MAX_RAW_CACHE_BYTES {
                    break;
                }
                if fs::remove_file(path).is_ok() {
                    total = total.saturating_sub(size);
                }
            }
        }
        Ok(())
    }
}

fn secure_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Strip derived, potentially large layout rows before copying a message to
/// the async writer. HTML is reflowed at the current width when reopened.
pub(crate) fn compact_body(body: &MessageBody) -> Option<MessageBody> {
    let size = [
        body.plain_text.as_ref(),
        body.plain_fallback.as_ref(),
        body.html_source.as_ref(),
        body.html_text.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|text| text.len())
    .sum::<usize>();
    if size > MAX_BODY_FILE_BYTES as usize / 2 {
        return None;
    }
    Some(MessageBody {
        id: body.id.clone(),
        plain_text: body.plain_text.clone(),
        plain_fallback: body.plain_fallback.clone(),
        html_source: body.html_source.clone(),
        html_text: body.html_text.clone(),
        html_lines: None,
        html_renderer: None,
        html_render_width: 0,
        attachment_count: body.attachment_count,
        attachment_names: body.attachment_names.clone(),
    })
}

/// Stable, per-account name for all IMAP cache artifacts.
/// Keep this shared with account removal so purge cannot guess a filename.
pub(crate) fn account_cache_stem(account: &AccountConfig) -> String {
    let identity = crate::backend::redact_source(&account.source);
    let hash = stable_key(&format!("{}\0{identity}", account.name));
    format!("{hash:016x}")
}

fn stable_key(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn read_snapshot(path: &PathBuf) -> Option<Snapshot> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_CACHE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return None;
    }
    let file: CacheFile = serde_json::from_slice(&bytes).ok()?;
    if file.version != VERSION || file.snapshot.folders.len() > MAX_FOLDERS {
        return None;
    }
    // Discard any legacy or invalid records lacking UIDVALIDITY.
    let mut snapshot = file.snapshot;
    snapshot
        .deferred_seen
        .retain(DeferredSeen::has_valid_identity);
    snapshot.deferred_seen.truncate(MAX_DEFERRED_SEEN);
    snapshot.folders.retain(|_, folder| {
        folder.uid_validity.is_some_and(|validity| {
            let prefix = format!("imap-uid:{validity}:");
            folder.messages.len() <= MAX_MESSAGES_PER_FOLDER
                && folder
                    .messages
                    .iter()
                    .all(|message| message.id.0.starts_with(&prefix))
                && folder.bodies.keys().all(|id| id.0.starts_with(&prefix))
        })
    });
    Some(snapshot)
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::other("no cache parent directory"));
    };
    fs::create_dir_all(parent)?;
    secure_private_dir(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    write_json_buffered(
        &mut tmp,
        &CacheFileRef {
            version: VERSION,
            snapshot,
        },
    )?;
    if tmp.as_file().metadata()?.len() > MAX_CACHE_BYTES {
        return Err(std::io::Error::other("mail cache size limit exceeded"));
    }
    tmp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_json_coalesces_small_serde_writes_without_changing_bytes() {
        #[derive(Default)]
        struct CountingSink {
            bytes: Vec<u8>,
            writes: usize,
        }

        impl Write for CountingSink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        // A realistic JSON collection with many tiny values; previously each
        // string/colon/comma could cause its own write(2) syscall.
        let headers = vec!["subject-and-sender"; 1_000];
        let expected = serde_json::to_vec(&headers).unwrap();
        assert!(expected.len() < CACHE_JSON_BUFFER_BYTES);
        let mut sink = CountingSink::default();
        write_json_buffered(&mut sink, &headers).unwrap();
        assert_eq!(sink.bytes, expected);
        assert_eq!(sink.writes, 1);
    }

    #[test]
    fn raw_mime_store_is_private_uid_scoped_and_persistent() {
        let dir = tempfile::tempdir().unwrap();
        let store = RawStore {
            dir: dir.path().join("raw"),
        };
        let id = MessageId("imap-uid:42:7".into());
        let raw = b"From: sender@example.org\r\nSubject: Patch\r\n\r\nHello";
        store.write("INBOX", 42, &id, raw).unwrap();
        assert_eq!(store.read("INBOX", 42, &id), Some(raw.to_vec()));
        assert_eq!(store.read("INBOX", 43, &id), None);
        assert_eq!(store.read("Sent", 42, &id), None);
        assert_eq!(
            store.read("INBOX", 42, &MessageId("imap-uid:42:8".into())),
            None
        );
        assert_eq!(
            store.read("INBOX", 42, &MessageId("imap-uid:7".into())),
            None
        );
        assert!(store.write("INBOX", 43, &id, raw).is_err());
        // A later instance must find the same bytes without an IMAP session.
        let reopened = RawStore {
            dir: store.dir.clone(),
        };
        assert_eq!(reopened.read("INBOX", 42, &id), Some(raw.to_vec()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&store.dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(store.raw_path("INBOX", 42, &id))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn raw_mime_store_does_not_follow_cache_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let store = RawStore {
            dir: dir.path().join("raw"),
        };
        std::fs::create_dir_all(&store.dir).unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let id = MessageId("imap-uid:12:9".into());
        std::os::unix::fs::symlink(&victim, store.raw_path("INBOX", 12, &id)).unwrap();
        assert!(store.read("INBOX", 12, &id).is_none());
        store.write("INBOX", 12, &id, b"safe").unwrap();
        assert_eq!(std::fs::read(victim).unwrap(), b"untouched");
        assert_eq!(store.read("INBOX", 12, &id), Some(b"safe".to_vec()));
    }

    #[test]
    fn identical_index_updates_do_not_rewrite_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let body_store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        let mut snapshot = Snapshot::default();
        let mut dirty = false;

        let inbox = folder(41, 3);
        {
            let mut changes = pending.lock().unwrap();
            changes.insert(Change::Mailboxes(vec![Mailbox::new(
                "INBOX",
                crate::model::MailboxRole::Inbox,
            )]));
            changes.insert(Change::Folder("INBOX".into(), inbox.clone()));
        }
        assert_eq!(
            flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
            0
        );
        assert!(path.exists());
        assert!(!dirty);
        #[cfg(unix)]
        let original_inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&path).unwrap().ino()
        };

        // A fresh IMAP snapshot with equal values (not equal Arc identities)
        // must not replace the existing inode, even after another checkpoint.
        {
            let mut changes = pending.lock().unwrap();
            changes.insert(Change::Mailboxes(vec![Mailbox::new(
                "INBOX",
                crate::model::MailboxRole::Inbox,
            )]));
            changes.insert(Change::Folder("INBOX".into(), folder(41, 3)));
            changes.insert(Change::RemoveFolder("Unknown".into()));
            changes.insert(Change::DeferredSeen(vec![]));
        }
        assert_eq!(
            flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
            0
        );
        assert!(!dirty);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().ino(), original_inode);
        }

        // Flags really are mutable, and must cause a new checkpoint without
        // copying the shared immutable RFC 5322 header allocation.
        let original_headers = Arc::clone(&snapshot.folders["INBOX"].messages[0].headers);
        let mut changed = folder(41, 3);
        changed.messages[0]
            .flags
            .set(crate::model::MessageFlags::SEEN, true);
        pending
            .lock()
            .unwrap()
            .insert(Change::Folder("INBOX".into(), changed));
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
        assert!(!dirty);
        assert!(!snapshot.folders["INBOX"].messages[0].unread());
        assert_eq!(
            read_snapshot(&path).unwrap().folders["INBOX"].messages[0].flags,
            snapshot.folders["INBOX"].messages[0].flags
        );
        // Original in-memory snapshots remain unaffected by COW/flag updates.
        assert_eq!(original_headers.subject, "Subject 1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(fs::metadata(&path).unwrap().ino(), original_inode);
        }
    }

    #[test]
    fn uidvalidity_and_changed_headers_trigger_checkpoints() {
        let mut snapshot = Snapshot::default();
        assert!(apply(
            &mut snapshot,
            Change::Folder("INBOX".into(), folder(1, 2))
        ));
        assert!(!apply(
            &mut snapshot,
            Change::Folder("INBOX".into(), folder(1, 2))
        ));
        let mut edited = folder(1, 2);
        edited.messages[0].subject = "Updated subject".into();
        assert!(apply(&mut snapshot, Change::Folder("INBOX".into(), edited)));
        assert!(!apply(
            &mut snapshot,
            Change::RemoveFolder("Unknown".into())
        ));
        assert!(apply(
            &mut snapshot,
            Change::Folder("INBOX".into(), folder(2, 2))
        ));
        assert_eq!(snapshot.folders["INBOX"].uid_validity, Some(2));
        assert!(apply(&mut snapshot, Change::RemoveFolder("INBOX".into())));
        assert!(!apply(&mut snapshot, Change::RemoveFolder("INBOX".into())));
    }

    #[test]
    fn identical_body_is_not_rewritten_but_enriched_body_is() {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let id = MessageId("imap-uid:17:42".into());
        let original = body(id.clone(), "Hello");
        assert!(store.write("INBOX", 17, &original).unwrap());
        let path = store.body_path("INBOX", 17, &id);
        #[cfg(unix)]
        let original_inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&path).unwrap().ino()
        };
        assert!(!store.write("INBOX", 17, &original).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().ino(), original_inode);
        }
        let mut enriched = original.clone();
        enriched.attachment_count = 1;
        assert!(store.write("INBOX", 17, &enriched).unwrap());
        assert_eq!(store.read("INBOX", 17, &id).unwrap().attachment_count, 1);
        // Corrupted persisted entries are repaired rather than treated as
        // equivalent based on their filename or UID alone.
        fs::write(&path, b"not json").unwrap();
        assert!(store.write("INBOX", 17, &enriched).unwrap());
        assert_eq!(store.read("INBOX", 17, &id).unwrap(), enriched);
    }

    #[test]
    fn failed_index_checkpoint_is_retried_on_shutdown_without_new_changes() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("not-a-dir");
        fs::write(&parent, b"blocking file").unwrap();
        let path = parent.join("index.json");
        let body_store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        pending
            .lock()
            .unwrap()
            .insert(Change::Mailboxes(vec![Mailbox::new(
                "INBOX",
                crate::model::MailboxRole::Inbox,
            )]));
        let mut stored = Snapshot::default();
        let mut dirty = false;
        flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
        assert!(dirty);
        fs::remove_file(&parent).unwrap();
        // No new event needed: final flush retries the still-dirty snapshot.
        flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
        assert!(!dirty);
        assert_eq!(read_snapshot(&path).unwrap().mailboxes[0].name, "INBOX");
    }

    #[test]
    fn continuous_cache_events_have_a_maximum_batch_delay() {
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(()).unwrap();
        // A run of updates every ~2 ms must not postpone the checkpoint until
        // the producer stops. Use a generous CI time bound, not a precise timer.
        let sender = thread::spawn(move || {
            for _ in 0..100 {
                let _ = tx.try_send(());
                thread::sleep(Duration::from_millis(2));
            }
        });
        rx.recv().unwrap();
        let started = Instant::now();
        wait_for_cache_batch(&rx, Duration::from_millis(50), Duration::from_millis(60));
        assert!(started.elapsed() < Duration::from_millis(190));
        sender.join().unwrap();
    }

    #[test]
    fn buffered_json_stays_bounded_across_multiple_64k_blocks() {
        #[derive(Default)]
        struct CountingSink {
            bytes: Vec<u8>,
            write_calls: usize,
        }

        impl Write for CountingSink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.write_calls += 1;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        // Several hundred KiB of many small JSON fields used to issue tens
        // of thousands of write(2) calls before we buffered serialization.
        let values = (0..20_000)
            .map(|index| format!("Subject {index:05}: sender@example.test"))
            .collect::<Vec<_>>();
        let expected = serde_json::to_vec(&values).unwrap();
        assert!(expected.len() > CACHE_JSON_BUFFER_BYTES * 4);
        let mut sink = CountingSink::default();
        write_json_buffered(&mut sink, &values).unwrap();
        assert_eq!(sink.bytes, expected);
        assert!(sink.write_calls > 1);
        assert!(
            sink.write_calls <= expected.len() / CACHE_JSON_BUFFER_BYTES + 2,
            "buffered writer made {} calls for {} bytes",
            sink.write_calls,
            expected.len()
        );
    }

    #[test]
    fn unchanged_multi_folder_refreshes_never_replace_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let body_store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        let mut snapshot = Snapshot::default();
        let mut dirty = false;
        let mailboxes = vec![
            Mailbox::new("INBOX", crate::model::MailboxRole::Inbox),
            Mailbox::new("Sent", crate::model::MailboxRole::Sent),
        ];

        {
            let mut changes = pending.lock().unwrap();
            changes.insert(Change::Mailboxes(mailboxes.clone()));
            changes.insert(Change::Folder("INBOX".into(), folder(81, 120)));
            changes.insert(Change::Folder("Sent".into(), folder(82, 40)));
        }
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
        assert!(!dirty);
        let original_bytes = fs::read(&path).unwrap();
        #[cfg(unix)]
        let original_inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&path).unwrap().ino()
        };

        // Repeated independent IMAP STATUS results arrive in different orders
        // and reconstruct equal headers with fresh allocations.
        for _ in 0..5 {
            let mut changes = pending.lock().unwrap();
            changes.insert(Change::Folder("Sent".into(), folder(82, 40)));
            changes.insert(Change::Mailboxes(mailboxes.clone()));
            changes.insert(Change::Folder("INBOX".into(), folder(81, 120)));
            drop(changes);
            assert_eq!(
                flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
                0
            );
            assert!(!dirty);
            assert_eq!(fs::read(&path).unwrap(), original_bytes);
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(fs::metadata(&path).unwrap().ino(), original_inode);
            }
        }

        // A genuine flag change must still persist, without losing the other
        // previously warmed folder.
        let mut sent = folder(82, 40);
        sent.messages[0]
            .flags
            .set(crate::model::MessageFlags::SEEN, true);
        pending
            .lock()
            .unwrap()
            .insert(Change::Folder("Sent".into(), sent));
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
        let reloaded = read_snapshot(&path).unwrap();
        assert!(!reloaded.folders["Sent"].messages[0].unread());
        assert_eq!(reloaded.folders["INBOX"].messages.len(), 120);
        assert!(!dirty);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(fs::metadata(&path).unwrap().ino(), original_inode);
        }
    }

    #[test]
    fn changing_only_terminal_html_layout_does_not_rewrite_disk_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let body_store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        let mut stored = Snapshot::default();
        let mut dirty = false;
        let mut initial = folder(41, 1);
        let id = initial.messages[0].id.clone();
        let mut rendered = body(id.clone(), "Visible text");
        rendered.html_source = Some("<p>Visible text</p>".into());
        rendered.html_text = Some("Visible text".into());
        rendered.html_lines = Some(Arc::new(vec![crate::model::HtmlLine::default()]));
        rendered.html_render_width = 80;
        initial.bodies.insert(id.clone(), rendered.clone());
        pending
            .lock()
            .unwrap()
            .insert(Change::Folder("INBOX".into(), initial));
        flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
        assert!(!dirty);
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&path).unwrap().ino()
        };
        let old_bytes = fs::read(&path).unwrap();

        let mut reflowed = folder(41, 1);
        rendered.html_lines = Some(Arc::new(vec![
            crate::model::HtmlLine::default(),
            crate::model::HtmlLine::default(),
        ]));
        rendered.html_render_width = 120;
        reflowed.bodies.insert(id, rendered);
        pending
            .lock()
            .unwrap()
            .insert(Change::Folder("INBOX".into(), reflowed));
        flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
        assert!(!dirty);
        assert_eq!(fs::read(&path).unwrap(), old_bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        }
    }

    #[test]
    fn mailbox_burst_keeps_only_the_final_folder_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let body_store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        let mut snapshot = Snapshot::default();
        let mut dirty = false;
        {
            let mut changes = pending.lock().unwrap();
            changes.insert(Change::Folder("INBOX".into(), folder(14, 1)));
            changes.insert(Change::Folder("Sent".into(), folder(15, 2)));
        }
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
        {
            let mut changes = pending.lock().unwrap();
            changes.insert(Change::Folder("INBOX".into(), folder(14, 2)));
            changes.insert(Change::RemoveFolder("INBOX".into()));
            changes.insert(Change::Folder("INBOX".into(), folder(14, 5)));
            changes.insert(Change::Folder("Sent".into(), folder(15, 4)));
            changes.insert(Change::RemoveFolder("Sent".into()));
            changes.insert(Change::RemoveFolder("NeverLoaded".into()));
            assert_eq!(changes.folders.len(), 3);
        }
        flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty);
        assert!(!dirty);
        let persisted = read_snapshot(&path).unwrap();
        assert_eq!(persisted.folders.len(), 1);
        assert_eq!(persisted.folders["INBOX"].messages.len(), 5);
        assert_eq!(persisted.folders["INBOX"].uid_validity, Some(14));
        assert!(pending.lock().unwrap().folders.is_empty());
    }

    #[test]
    fn queued_body_updates_write_only_the_latest_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let body_store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        let mut snapshot = Snapshot::default();
        let mut dirty = false;
        let id = MessageId("imap-uid:19:4".into());
        let latest = body(id.clone(), "Latest fetched text");
        {
            let mut changes = pending.lock().unwrap();
            changes.insert(Change::Body("INBOX".into(), 19, body(id.clone(), "Old")));
            changes.insert(Change::Body("INBOX".into(), 19, latest.clone()));
        }
        assert_eq!(
            flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
            1
        );
        assert!(
            !path.exists(),
            "body persistence must not rewrite the mailbox index"
        );
        assert_eq!(body_store.read("INBOX", 19, &id), Some(latest.clone()));
        #[cfg(unix)]
        let original_inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(body_store.body_path("INBOX", 19, &id))
                .unwrap()
                .ino()
        };
        pending
            .lock()
            .unwrap()
            .insert(Change::Body("INBOX".into(), 19, latest.clone()));
        assert_eq!(
            flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
            0
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(body_store.body_path("INBOX", 19, &id))
                    .unwrap()
                    .ino(),
                original_inode
            );
        }
        let mut enriched = latest;
        enriched.attachment_count = 2;
        pending
            .lock()
            .unwrap()
            .insert(Change::Body("INBOX".into(), 19, enriched.clone()));
        assert_eq!(
            flush_cache_changes(&path, &mut snapshot, &pending, &body_store, &mut dirty),
            1
        );
        assert_eq!(body_store.read("INBOX", 19, &id), Some(enriched));
    }

    #[test]
    fn failed_checkpoint_retries_the_latest_coalesced_data() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocked-parent");
        fs::write(&blocker, b"not a directory").unwrap();
        let path = blocker.join("index.json");
        let body_store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        let mut stored = Snapshot::default();
        let mut dirty = false;
        pending
            .lock()
            .unwrap()
            .insert(Change::Folder("INBOX".into(), folder(31, 1)));
        flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
        assert!(dirty);

        // More updates can arrive while the filesystem is unavailable; when
        // persistence recovers it must save the newest state, not the first.
        pending
            .lock()
            .unwrap()
            .insert(Change::Folder("INBOX".into(), folder(31, 3)));
        fs::remove_file(&blocker).unwrap();
        flush_cache_changes(&path, &mut stored, &pending, &body_store, &mut dirty);
        assert!(!dirty);
        assert_eq!(
            read_snapshot(&path).unwrap().folders["INBOX"]
                .messages
                .len(),
            3
        );
    }

    fn test_cache_writer(path: PathBuf) -> Writer {
        let (wake, rx) = mpsc::sync_channel::<()>(1);
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        let body_store = BodyStore {
            dir: path.with_extension("bodies"),
        };
        let worker_pending = Arc::clone(&pending);
        let worker_store = body_store.clone();
        let worker_path = path.clone();
        let join = thread::spawn(move || {
            run_cache_writer(
                rx,
                worker_path,
                worker_pending,
                worker_store,
                Snapshot::default(),
            );
        });
        Writer {
            wake: Some(wake),
            pending,
            join: Some(join),
            body_store,
            mutation_store: MutationStore {
                path: path.with_extension("mutations.json"),
                readable: true,
            },
        }
    }

    #[test]
    fn dropping_writer_flushes_batched_index_and_body_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let writer = test_cache_writer(path.clone());
        let id = MessageId("imap-uid:31:1".into());
        writer.send(Change::Folder("INBOX".into(), folder(31, 1)));
        writer.send(Change::Body(
            "INBOX".into(),
            31,
            body(id.clone(), "Saved body"),
        ));
        writer.send(Change::Mailboxes(vec![Mailbox::new(
            "INBOX",
            crate::model::MailboxRole::Inbox,
        )]));
        // No sleeps: an immediate quit must flush every pending cache change.
        drop(writer);
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.mailboxes[0].name, "INBOX");
        assert_eq!(loaded.folders["INBOX"].messages.len(), 1);
        assert_eq!(
            BodyStore {
                dir: path.with_extension("bodies"),
            }
            .read("INBOX", 31, &id),
            Some(body(id, "Saved body"))
        );
    }

    #[test]
    fn destructive_mutation_journal_is_saved_before_cache_batch_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let writer = test_cache_writer(path.clone());
        writer.send(Change::Folder("INBOX".into(), folder(52, 1)));
        let operation = DeferredMutation {
            mailbox: "INBOX".into(),
            validity: 52,
            ids: vec![MessageId("imap-uid:52:1".into())],
            kind: DeferredMutationKind::Delete,
            state: DeferredMutationState::Queued,
        };
        writer
            .save_mutations(std::slice::from_ref(&operation))
            .unwrap();
        // Crash-durable operations do not wait for the 500 ms display-cache
        // debounce or for Writer::drop to join the background worker.
        let journal = MutationStore {
            path: path.with_extension("mutations.json"),
            readable: true,
        };
        assert_eq!(journal.read().unwrap(), vec![operation.clone()]);
        drop(writer);
        assert_eq!(
            read_snapshot(&path).unwrap().folders["INBOX"]
                .messages
                .len(),
            1
        );
        assert_eq!(journal.read().unwrap(), vec![operation]);
    }

    #[test]
    fn pending_writer_coalesces_latest_folder_and_removal() {
        let mut pending = super::PendingChanges::default();
        pending.insert(super::Change::Mailboxes(Vec::new()));
        pending.insert(super::Change::RemoveFolder("INBOX".into()));
        pending.insert(super::Change::RemoveFolder("INBOX".into()));
        assert_eq!(pending.folders.len(), 1);
        let mut stored = super::Snapshot::default();
        pending.drain_into(&mut stored);
        assert!(pending.folders.is_empty());
    }

    #[test]
    fn deferred_seen_journal_roundtrip_and_coalescing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let id = MessageId("imap-uid:17:4".into());
        let entry = DeferredSeen {
            mailbox: "INBOX".into(),
            validity: 17,
            id: id.clone(),
            seen: true,
        };
        let mut pending = PendingChanges::default();
        pending.insert(Change::DeferredSeen(vec![entry.clone()]));
        pending.insert(Change::DeferredSeen(vec![DeferredSeen {
            seen: false,
            ..entry
        }]));
        let mut snapshot = Snapshot::default();
        assert!(pending.drain_into(&mut snapshot));
        assert_eq!(snapshot.deferred_seen.len(), 1);
        assert!(!snapshot.deferred_seen[0].seen);
        write_snapshot(&path, &snapshot).unwrap();
        assert_eq!(
            read_snapshot(&path).unwrap().deferred_seen,
            snapshot.deferred_seen
        );
        pending.insert(Change::DeferredSeen(vec![]));
        assert!(pending.drain_into(&mut snapshot));
        assert!(snapshot.deferred_seen.is_empty());
    }

    #[test]
    fn invalid_deferred_uid_is_rejected_from_the_index() {
        let mut snapshot = Snapshot::default();
        snapshot.deferred_seen.push(DeferredSeen {
            mailbox: "INBOX".into(),
            validity: 10,
            id: MessageId("imap-uid:11:3".into()),
            seen: true,
        });
        let invalid = snapshot.deferred_seen.clone();
        apply(&mut snapshot, Change::DeferredSeen(invalid));
        assert!(snapshot.deferred_seen.is_empty());
    }

    #[test]
    fn body_only_changes_do_not_rewrite_mailbox_index() {
        let mut pending = PendingChanges::default();
        let id = MessageId("imap-uid:10:4".into());
        pending.insert(Change::Body("INBOX".into(), 10, body(id.clone(), "hello")));
        let mut snapshot = Snapshot::default();
        assert!(!pending.drain_into(&mut snapshot));
        assert!(snapshot.folders.is_empty());
        let bodies = pending.drain_bodies();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0].2.id, id);
    }

    #[test]
    fn coalesced_writer_queue_never_exceeds_folder_limit() {
        let mut pending = PendingChanges::default();
        for index in 0..1_000 {
            pending.insert(Change::RemoveFolder(format!("F{index}")));
        }
        assert!(pending.folders.len() <= MAX_FOLDERS);
        pending.insert(Change::RemoveFolder("latest".into()));
        assert!(pending.folders["latest"].is_none());
    }

    #[test]
    fn snapshot_roundtrip_and_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let data = Snapshot {
            mailboxes: vec![Mailbox::new("INBOX", crate::model::MailboxRole::Inbox)],
            folders: HashMap::new(),
            deferred_seen: Vec::new(),
            deferred_mutations: Vec::new(),
        };
        write_snapshot(&path, &data).unwrap();
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.mailboxes[0].name, "INBOX");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn cloned_cache_indexes_share_headers_but_not_flags() {
        let mut original = Snapshot::default();
        original.folders.insert("INBOX".into(), folder(23, 2));
        let mut writer_index = original.clone();
        let original_row = &original.folders["INBOX"].messages[0];
        let writer_row = &writer_index.folders["INBOX"].messages[0];
        assert!(std::sync::Arc::ptr_eq(
            &original_row.headers,
            &writer_row.headers
        ));

        writer_index.folders.get_mut("INBOX").unwrap().messages[0]
            .flags
            .set(crate::model::MessageFlags::SEEN, true);
        assert!(std::sync::Arc::ptr_eq(
            &original.folders["INBOX"].messages[0].headers,
            &writer_index.folders["INBOX"].messages[0].headers,
        ));
        assert!(original.folders["INBOX"].messages[0].unread());
        assert!(!writer_index.folders["INBOX"].messages[0].unread());
    }

    #[test]
    fn older_cache_without_uidnext_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let mut snapshot = Snapshot::default();
        snapshot.folders.insert("INBOX".into(), folder(55, 2));
        write_snapshot(&path, &snapshot).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut value = value;
        value["snapshot"]["folders"]["INBOX"]
            .as_object_mut()
            .unwrap()
            .remove("uid_next");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.folders["INBOX"].uid_validity, Some(55));
        assert_eq!(loaded.folders["INBOX"].uid_next, None);
    }

    #[test]
    fn corrupted_or_old_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(read_snapshot(&path).is_none());
        std::fs::write(&path, br#"{"version":0,"snapshot":{}}"#).unwrap();
        assert!(read_snapshot(&path).is_none());
    }

    #[test]
    fn cache_requires_uidvalidity() {
        let mut snapshot = Snapshot::default();
        let folder = CachedMailbox {
            messages: vec![],
            bodies: HashMap::new(),
            message_index: 0,
            message_view_offset: 0,
            next_page_offset: 0,
            has_more: false,
            uid_validity: None,
            uid_next: None,
        };
        apply(
            &mut snapshot,
            Change::Folder("INBOX".into(), folder.clone()),
        );
        assert!(!snapshot.folders.contains_key("INBOX"));
        apply(
            &mut snapshot,
            Change::Folder(
                "INBOX".into(),
                CachedMailbox {
                    uid_validity: Some(42),
                    ..folder
                },
            ),
        );
        assert!(snapshot.folders.contains_key("INBOX"));
    }

    fn summary(uid_validity: u32, uid: u32) -> MessageSummary {
        MessageSummary {
            headers: std::sync::Arc::new(crate::model::MessageHeaders {
                id: MessageId(format!("imap-uid:{uid_validity}:{uid}")),
                message_id: None,
                in_reply_to: None,
                references: Vec::new().into(),
                from_name: "Sender".into(),
                from_address: "sender@example.test".into(),
                to: "recipient@example.test".into(),
                cc: String::new(),
                bcc: String::new(),
                subject: format!("Subject {uid}"),
                timestamp: i64::from(uid),
                date_label: "Today".into(),
                size: 25,
                has_attachment_hint: false,
                attachment_names_hint: vec![].into(),
            }),
            flags: crate::model::MessageFlags::empty(),
        }
    }

    fn body(id: MessageId, text: &str) -> MessageBody {
        MessageBody {
            id,
            plain_text: Some(text.into()),
            plain_fallback: None,
            html_source: None,
            html_text: None,
            html_lines: None,
            html_renderer: None,
            html_render_width: 0,
            attachment_count: 0,
            attachment_names: vec![],
        }
    }

    fn folder(validity: u32, count: u32) -> CachedMailbox {
        CachedMailbox {
            messages: (1..=count).map(|uid| summary(validity, uid)).collect(),
            bodies: HashMap::new(),
            message_index: 0,
            message_view_offset: 0,
            next_page_offset: count as usize,
            has_more: false,
            uid_validity: Some(validity),
            uid_next: None,
        }
    }

    #[test]
    fn cached_body_clones_share_large_text_and_rendered_html_allocations() {
        use std::sync::Arc;

        let mut folder = folder(42, 1);
        let id = folder.messages[0].id.clone();
        let mut rich = body(id.clone(), &"plain".repeat(4096));
        rich.plain_fallback = Some("fallback".repeat(4096).into());
        rich.html_source = Some(format!("<p>{}</p>", "markup".repeat(4096)).into());
        rich.html_text = Some("rendered".repeat(4096).into());
        rich.html_lines = Some(Arc::new(vec![crate::model::HtmlLine {
            spans: vec![crate::model::HtmlSpan {
                text: "styled span".repeat(4096),
                ..Default::default()
            }],
        }]));
        folder.bodies.insert(id.clone(), rich);

        let live = folder.clone();
        let source = &folder.bodies[&id];
        let copy = &live.bodies[&id];
        for (old, new) in [
            (&source.plain_text, &copy.plain_text),
            (&source.plain_fallback, &copy.plain_fallback),
            (&source.html_source, &copy.html_source),
            (&source.html_text, &copy.html_text),
        ] {
            assert!(Arc::ptr_eq(old.as_ref().unwrap(), new.as_ref().unwrap()));
        }
        assert!(Arc::ptr_eq(
            source.html_lines.as_ref().unwrap(),
            copy.html_lines.as_ref().unwrap()
        ));

        // Writer snapshots omit derived rows while borrowing the same source
        // allocations. Cloning for persistence must not consume the UI cache.
        let disk = folder.for_persistence();
        let saved = &disk.bodies[&id];
        assert!(saved.html_lines.is_none());
        assert!(Arc::ptr_eq(
            source.html_source.as_ref().unwrap(),
            saved.html_source.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            source.plain_text.as_ref().unwrap(),
            saved.plain_text.as_ref().unwrap()
        ));
        assert!(source.html_lines.is_some());

        // Shared allocations serialize as ordinary JSON strings, compatible
        // with existing on-disk cache files and older jerc versions.
        let bytes = serde_json::to_vec(&disk).unwrap();
        let restored: CachedMailbox = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.bodies[&id].html_source, source.html_source);
        assert_eq!(restored.bodies[&id].plain_text, source.plain_text);
        assert!(restored.bodies[&id].html_lines.is_none());
    }

    #[test]
    fn html_reflow_replaces_only_the_current_body_layout() {
        use std::sync::Arc;

        let id = MessageId("imap-uid:42:1".into());
        let mut before = body(id, "base text");
        before.html_source = Some("<p>source</p>".into());
        before.html_text = Some("first render".into());
        before.html_lines = Some(Arc::new(vec![crate::model::HtmlLine::default()]));

        let mut after = before.clone();
        after.html_text = Some("reflowed".into());
        after.html_lines = Some(Arc::new(vec![
            crate::model::HtmlLine::default(),
            crate::model::HtmlLine::default(),
        ]));

        assert_eq!(before.html_text.as_deref(), Some("first render"));
        assert_eq!(after.html_text.as_deref(), Some("reflowed"));
        assert_eq!(before.html_lines.as_ref().unwrap().len(), 1);
        assert_eq!(after.html_lines.as_ref().unwrap().len(), 2);
        assert!(Arc::ptr_eq(
            before.html_source.as_ref().unwrap(),
            after.html_source.as_ref().unwrap()
        ));
    }

    #[test]
    fn persistent_folder_snapshot_drops_derived_html_without_touching_live_body() {
        let mut folder = folder(42, 1);
        folder.message_index = 0;
        folder.message_view_offset = 3;
        folder.uid_next = Some(17);
        let id = folder.messages[0].id.clone();
        let mut rich = body(id.clone(), "Cached text");
        rich.html_source = Some("<p>Cached text</p>".into());
        rich.html_text = Some("Cached text".into());
        rich.html_lines = Some(std::sync::Arc::new(vec![crate::model::HtmlLine {
            spans: vec![crate::model::HtmlSpan {
                text: "Rendered layout".repeat(4096),
                ..Default::default()
            }],
        }]));
        rich.html_renderer = Some("rich".into());
        rich.html_render_width = 72;
        folder.bodies.insert(id.clone(), rich);

        let persistent = folder.for_persistence();
        assert_eq!(persistent.messages[0].id, id);
        assert_eq!(persistent.message_view_offset, 3);
        assert_eq!(persistent.uid_validity, Some(42));
        assert_eq!(persistent.uid_next, Some(17));
        let body = &persistent.bodies[&id];
        assert_eq!(body.html_source.as_deref(), Some("<p>Cached text</p>"));
        assert_eq!(body.html_text.as_deref(), Some("Cached text"));
        assert!(body.html_lines.is_none());
        assert!(body.html_renderer.is_none());
        assert_eq!(body.html_render_width, 0);
        // Live folder switching keeps the expensive layout, by design.
        assert!(folder.bodies[&id].html_lines.is_some());
        assert_eq!(folder.bodies[&id].html_render_width, 72);

        // Even a direct writer change from an older caller must not retain
        // derived rich spans inside the persistent snapshot.
        let mut snapshot = Snapshot::default();
        apply(&mut snapshot, Change::Folder("INBOX".into(), folder));
        assert!(snapshot.folders["INBOX"].bodies[&id].html_lines.is_none());
    }

    #[test]
    fn cached_flags_subject_and_body_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let mut snapshot = Snapshot::default();
        let mut inbox = folder(12, 2);
        inbox.messages[0].subject = "Unicode: café ☃".into();
        inbox.messages[0]
            .flags
            .set(crate::model::MessageFlags::SEEN, true);
        let first_id = inbox.messages[0].id.clone();
        inbox
            .bodies
            .insert(first_id.clone(), body(first_id, "cached offline text"));
        apply(&mut snapshot, Change::Folder("INBOX".into(), inbox));
        write_snapshot(&path, &snapshot).unwrap();

        let loaded = read_snapshot(&path).unwrap();
        let inbox = loaded.folders.get("INBOX").unwrap();
        assert_eq!(inbox.messages.len(), 2);
        assert_eq!(inbox.messages[0].subject, "Unicode: café ☃");
        assert!(!inbox.messages[0].unread());
        let text = inbox.bodies.get(&inbox.messages[0].id).unwrap();
        assert_eq!(text.plain_text.as_deref(), Some("cached offline text"));
    }

    #[test]
    fn mismatched_uidvalidity_is_rejected_on_disk_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let mut snapshot = Snapshot::default();
        let mut inbox = folder(100, 1);
        inbox.messages[0].id = MessageId("imap-uid:99:1".into());
        snapshot.folders.insert("INBOX".into(), inbox);
        snapshot.folders.insert("Archive".into(), folder(100, 1));
        write_snapshot(&path, &snapshot).unwrap();

        let loaded = read_snapshot(&path).unwrap();
        assert!(!loaded.folders.contains_key("INBOX"));
        assert!(loaded.folders.contains_key("Archive"));
    }

    #[test]
    fn mismatched_uidvalidity_in_cached_body_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let mut snapshot = Snapshot::default();
        let mut inbox = folder(100, 1);
        let bad_id = MessageId("imap-uid:99:1".into());
        inbox.bodies.insert(bad_id.clone(), body(bad_id, "stale"));
        snapshot.folders.insert("INBOX".into(), inbox);
        write_snapshot(&path, &snapshot).unwrap();

        assert!(!read_snapshot(&path).unwrap().folders.contains_key("INBOX"));
    }

    #[test]
    fn folder_updates_limit_messages_bodies_and_size() {
        let mut snapshot = Snapshot::default();
        let mut inbox = folder(7, MAX_MESSAGES_PER_FOLDER as u32 + 2);
        let first_id = inbox.messages[0].id.clone();
        let removed_id = inbox.messages[MAX_MESSAGES_PER_FOLDER].id.clone();
        inbox
            .bodies
            .insert(first_id.clone(), body(first_id.clone(), "keep"));
        inbox
            .bodies
            .insert(removed_id.clone(), body(removed_id, "drop"));
        let too_large_id = inbox.messages[1].id.clone();
        inbox.bodies.insert(
            too_large_id.clone(),
            body(too_large_id, &"x".repeat(MAX_BODY_BYTES + 1)),
        );
        apply(&mut snapshot, Change::Folder("INBOX".into(), inbox));

        let cached = snapshot.folders.get("INBOX").unwrap();
        assert_eq!(cached.messages.len(), MAX_MESSAGES_PER_FOLDER);
        assert_eq!(cached.next_page_offset, MAX_MESSAGES_PER_FOLDER);
        assert!(cached.has_more);
        assert_eq!(cached.bodies.len(), 1);
        assert_eq!(cached.bodies[&first_id].plain_text.as_deref(), Some("keep"));
    }

    #[test]
    fn persistence_clones_only_bodies_that_survive_disk_limits() {
        let messages = (1..=MAX_MESSAGES_PER_FOLDER as u32 + 2)
            .map(|uid| summary(7, uid))
            .collect::<Vec<_>>();
        let mut live_bodies = HashMap::new();
        for message in &messages {
            let id = message.id.clone();
            live_bodies.insert(id.clone(), body(id, "cached"));
        }
        // This oversized body is skipped, allowing the next eligible body in.
        let large_id = messages[0].id.clone();
        live_bodies.insert(
            large_id.clone(),
            body(large_id.clone(), &"x".repeat(MAX_BODY_BYTES + 1)),
        );
        let disk_bodies = persistable_bodies(&messages, &live_bodies);
        assert_eq!(disk_bodies.len(), MAX_BODIES_PER_FOLDER);
        assert!(!disk_bodies.contains_key(&large_id));
        assert!(disk_bodies.contains_key(&messages[1].id));
        assert!(disk_bodies.contains_key(&messages[MAX_BODIES_PER_FOLDER].id));
        assert!(!disk_bodies.contains_key(&messages[MAX_MESSAGES_PER_FOLDER].id));
        // Producing the snapshot must never consume or trim the warm reader cache.
        assert_eq!(live_bodies.len(), messages.len());

        // The existing writer's validation should produce exactly the same
        // persisted set as cloning the entire working set before filtering.
        let mut expected = Snapshot::default();
        apply(
            &mut expected,
            Change::Folder(
                "INBOX".into(),
                CachedMailbox {
                    messages: messages.clone(),
                    bodies: live_bodies,
                    message_index: 0,
                    message_view_offset: 0,
                    next_page_offset: messages.len(),
                    has_more: false,
                    uid_validity: Some(7),
                    uid_next: None,
                },
            ),
        );
        let mut reduced = Snapshot::default();
        apply(
            &mut reduced,
            Change::Folder(
                "INBOX".into(),
                CachedMailbox {
                    messages: messages
                        .iter()
                        .take(MAX_MESSAGES_PER_FOLDER)
                        .cloned()
                        .collect(),
                    bodies: disk_bodies,
                    message_index: 0,
                    message_view_offset: 0,
                    next_page_offset: MAX_MESSAGES_PER_FOLDER,
                    has_more: true,
                    uid_validity: Some(7),
                    uid_next: None,
                },
            ),
        );
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::to_value(&reduced).unwrap()
        );
    }

    #[test]
    fn folder_removal_survives_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let mut snapshot = Snapshot::default();
        apply(&mut snapshot, Change::Folder("INBOX".into(), folder(8, 1)));
        apply(
            &mut snapshot,
            Change::Folder("Archive".into(), folder(8, 1)),
        );
        apply(&mut snapshot, Change::RemoveFolder("Archive".into()));
        write_snapshot(&path, &snapshot).unwrap();

        let loaded = read_snapshot(&path).unwrap();
        assert!(loaded.folders.contains_key("INBOX"));
        assert!(!loaded.folders.contains_key("Archive"));
    }

    #[test]
    fn cache_over_size_limit_is_ignored_without_loading_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.json");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_CACHE_BYTES + 1).unwrap();
        assert!(read_snapshot(&path).is_none());
    }

    #[test]
    fn body_store_roundtrip_is_lazy_and_scoped_to_uid_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let id = MessageId("imap-uid:17:42".into());
        let mut message = body(id.clone(), "Offline message");
        message.html_lines = Some(std::sync::Arc::new(vec![crate::model::HtmlLine::default()]));
        let compact = compact_body(&message).unwrap();
        assert!(compact.html_lines.is_none());
        store.write("INBOX", 17, &compact).unwrap();
        assert!(store.has_candidate("INBOX", 17, &id));
        assert!(!store.has_candidate("INBOX", 18, &id));
        assert_eq!(
            store.read("INBOX", 17, &id).unwrap().plain_text.as_deref(),
            Some("Offline message")
        );
        assert!(store.read("INBOX", 18, &id).is_none());
        assert!(store.read("Archive", 17, &id).is_none());
        assert!(
            store
                .read("INBOX", 17, &MessageId("imap-uid:17:43".into()))
                .is_none()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = store.body_path("INBOX", 17, &id);
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn oversized_body_and_corrupt_file_are_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        let id = MessageId("imap-uid:4:9".into());
        assert!(
            compact_body(&body(id.clone(), &"a".repeat(MAX_BODY_FILE_BYTES as usize))).is_none()
        );
        store.write("INBOX", 4, &body(id.clone(), "okay")).unwrap();
        std::fs::write(store.body_path("INBOX", 4, &id), b"not json").unwrap();
        assert!(store.read("INBOX", 4, &id).is_none());
    }

    #[test]
    fn store_prunes_to_disk_budget_without_reading_body_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore {
            dir: dir.path().join("bodies"),
        };
        std::fs::create_dir_all(&store.dir).unwrap();
        for index in 0..2 {
            let file = File::create(store.dir.join(format!("{index}.json"))).unwrap();
            file.set_len(MAX_BODY_STORE_BYTES / 2 + 1).unwrap();
        }
        store.prune().unwrap();
        let remaining = std::fs::read_dir(&store.dir).unwrap().count();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn cache_identity_keys_are_stable_and_distinct() {
        assert_eq!(stable_key("account\0imap-a"), stable_key("account\0imap-a"));
        assert_ne!(stable_key("account\0imap-a"), stable_key("account\0imap-b"));
        assert_ne!(stable_key("account\0imap-a"), stable_key("other\0imap-a"));
    }
}

#[cfg(test)]
mod offline_mutation_journal_tests {
    use super::*;

    fn queued() -> DeferredMutation {
        DeferredMutation {
            mailbox: "INBOX".into(),
            validity: 81,
            ids: vec![MessageId("imap-uid:81:77".into())],
            kind: DeferredMutationKind::Archive {
                destination: "Archive".into(),
            },
            state: DeferredMutationState::Queued,
        }
    }

    #[test]
    fn offline_mutations_are_crash_durable_uid_scoped_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = MutationStore {
            path: dir.path().join("index.mutations.json"),
            readable: true,
        };
        let pending = queued();
        assert!(pending.has_valid_identity());
        store.save(std::slice::from_ref(&pending)).unwrap();
        assert_eq!(store.read().unwrap(), vec![pending.clone()]);
        let mut uncertain = pending;
        uncertain.state = DeferredMutationState::NeedsReview;
        store.save(std::slice::from_ref(&uncertain)).unwrap();
        let restarted = MutationStore {
            path: store.path.clone(),
            readable: true,
        };
        assert_eq!(restarted.read().unwrap(), vec![uncertain]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn malformed_uid_or_corrupt_journal_is_rejected_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = MutationStore {
            path: dir.path().join("mutations.json"),
            readable: true,
        };
        let mut record = queued();
        record.validity = 82;
        assert!(!record.has_valid_identity());
        assert!(store.save(&[record]).is_err());
        fs::write(&store.path, b"{invalid").unwrap();
        assert!(store.read().is_err());
        let locked = MutationStore {
            path: store.path.clone(),
            readable: false,
        };
        assert!(locked.save(&[queued()]).is_err());
        assert_eq!(fs::read(store.path).unwrap(), b"{invalid");
    }
}
