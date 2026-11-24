//! Small, private, per-account persistent mailbox index.
//!
//! The UI reads this once before connecting. Writes are serialized on a
//! dedicated thread so a slow disk cannot stall keyboard input or painting.
//! This is a display cache, never an authority for IMAP UID mutations.
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::AccountConfig;
use crate::model::{CacheIdentity, Mailbox, MessageBody, MessageId, MessageSummary};

mod body_store;
mod index_store;
mod raw_store;
mod writer;
use self::index_store::{IndexCheckpoint, NavigationPosition};
#[cfg(test)]
use self::writer::apply;
use self::writer::{apply_tracked, run_cache_writer};
#[cfg(test)]
use self::writer::{flush_cache_changes, wait_for_cache_batch};

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

pub(crate) fn write_json_buffered<W: Write, T: Serialize>(
    sink: W,
    value: &T,
) -> std::io::Result<()> {
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread_messages: Vec<MessageSummary>,
    pub bodies: HashMap<MessageId, MessageBody>,
    #[serde(default)]
    pub message_index: usize,
    #[serde(default)]
    pub message_view_offset: usize,
    pub next_page_offset: usize,
    pub has_more: bool,
    /// Server-scoped identity; old numeric UIDVALIDITY snapshots still load.
    #[serde(default, rename = "uid_validity", alias = "identity")]
    pub identity: Option<CacheIdentity>,
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

// Serialize the live snapshot by reference: cloning a potentially large mailbox
// cache on every background write would add unnecessary allocations and latency.
#[cfg(test)]
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
            thread_messages: self.thread_messages.clone(),
            bodies: persistable_bodies_near_compact(
                &self.messages,
                &self.bodies,
                self.message_index,
            ),
            message_index: self.message_index,
            message_view_offset: self.message_view_offset,
            next_page_offset: self.next_page_offset,
            has_more: self.has_more,
            identity: self.identity.clone(),
            uid_next: self.uid_next,
        }
    }
}

pub(crate) enum Change {
    Mailboxes(Vec<Mailbox>),
    Folder(String, CachedMailbox),
    RemoveFolder(String),
    Navigation(String, CacheIdentity, usize, usize),
    Body(String, CacheIdentity, MessageBody),
    DeferredSeen(Vec<DeferredSeen>),
}

/// A single latest change per folder. The wake channel only holds a token;
/// arbitrary numbers of reader selections cannot queue expensive snapshots.
#[derive(Default)]
struct PendingChanges {
    mailboxes: Option<Vec<Mailbox>>,
    folders: HashMap<String, Option<CachedMailbox>>,
    bodies: HashMap<(String, MessageId), (CacheIdentity, MessageBody)>,
    deferred_seen: Option<Vec<DeferredSeen>>,
    navigation: HashMap<String, NavigationPosition>,
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
            Change::Navigation(name, identity, index, offset) => {
                if self.navigation.len() < MAX_FOLDERS || self.navigation.contains_key(&name) {
                    self.navigation.insert(
                        name,
                        NavigationPosition {
                            identity,
                            index,
                            offset,
                        },
                    );
                }
            }
            Change::RemoveFolder(name) => {
                self.navigation.remove(&name);
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

    fn drain_bodies(&mut self) -> Vec<(String, CacheIdentity, MessageBody)> {
        std::mem::take(&mut self.bodies)
            .into_iter()
            .map(|((mailbox, _), (validity, body))| (mailbox, validity, body))
            .collect()
    }

    #[cfg(test)]
    fn drain_into(&mut self, snapshot: &mut Snapshot) -> bool {
        self.drain_tracked(snapshot, &mut IndexCheckpoint::default())
    }

    fn drain_tracked(&mut self, snapshot: &mut Snapshot, dirty: &mut IndexCheckpoint) -> bool {
        let mut changed = false;
        if let Some(mailboxes) = self.mailboxes.take() {
            changed |= apply_tracked(snapshot, Change::Mailboxes(mailboxes), dirty);
        }
        if let Some(entries) = self.deferred_seen.take() {
            changed |= apply_tracked(snapshot, Change::DeferredSeen(entries), dirty);
        }
        for (name, folder) in self.folders.drain() {
            let change = match folder {
                Some(folder) => Change::Folder(name, folder),
                None => Change::RemoveFolder(name),
            };
            changed |= apply_tracked(snapshot, change, dirty);
        }
        for (name, position) in self.navigation.drain() {
            changed |= dirty.apply_navigation(snapshot, &name, &position);
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

/// Remote backends with stable message identities share the persistent cache.
/// Maildir remains authoritative on disk already.
pub(crate) fn supports_mailbox_cache(source: &str) -> bool {
    crate::backend::BackendKind::from_source(source)
        .is_some_and(crate::backend::BackendKind::persistent_remote_cache)
}

/// The key uses the account's name and non-secret identity, not tokens or
/// credential commands. Files contain message metadata, so permissions matter.
pub(crate) fn open(account: &AccountConfig) -> (Snapshot, Option<Writer>) {
    if std::env::var_os("JERC_DISABLE_CACHE").is_some() || !supports_mailbox_cache(&account.source)
    {
        return (Snapshot::default(), None);
    }
    let Some(dir) = dirs::cache_dir().map(|path| path.join("jerc").join("mail-index")) else {
        return (Snapshot::default(), None);
    };
    open_in(account, &dir)
}

fn open_in(account: &AccountConfig, dir: &Path) -> (Snapshot, Option<Writer>) {
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
    let loaded = index_store::load(&path);
    // A successful disk read with no usable folders is not a warm mailbox
    // cache. The log contains timing only, never account or message details.
    crate::perf::record(
        "cache_read",
        None,
        cache_start.elapsed(),
        loaded
            .as_ref()
            .is_some_and(|(snapshot, _)| !snapshot.folders.is_empty()),
    );
    let (mut snapshot, checkpoint) = loaded.unwrap_or_default();
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
            run_cache_writer(
                rx,
                path,
                thread_pending,
                writer_body_store,
                writer_snapshot,
                checkpoint,
            );
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
    validity: CacheIdentity,
    body: MessageBody,
}

#[derive(Serialize)]
struct BodyFileRef<'a> {
    version: u8,
    mailbox: &'a str,
    validity: &'a CacheIdentity,
    body: &'a MessageBody,
}

// JMAP Email IDs are account-scoped, not mailbox-scoped. Body and raw-MIME
// blobs therefore have one key per (account, Email ID), regardless of labels.
// IMAP keeps the original folder+UIDVALIDITY+UID isolation unchanged.
fn cache_message_key(mailbox: &str, identity: &CacheIdentity, id: &MessageId) -> String {
    use ring::digest::{SHA256, digest};
    let key = match identity {
        CacheIdentity::Imap(value) => format!("{mailbox}\0{value}\0{}", id.0),
        CacheIdentity::Jmap { account_id, .. } => format!("jmap\0{account_id}\0{}", id.0),
    };
    let hash = digest(&SHA256, key.as_bytes());
    hash.as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// Prior JMAP builds included the mailbox in the cache key. Keep read-only
// compatibility so opening a message after an upgrade needs no redownload.
fn legacy_jmap_message_key(
    mailbox: &str,
    identity: &CacheIdentity,
    id: &MessageId,
) -> Option<String> {
    use ring::digest::{SHA256, digest};
    if !matches!(identity, CacheIdentity::Jmap { .. }) {
        return None;
    }
    let key = serde_json::to_string(&(mailbox, identity.generation(), id)).ok()?;
    let hash = digest(&SHA256, key.as_bytes());
    Some(
        hash.as_ref()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

fn account_body_identity(identity: &CacheIdentity) -> CacheIdentity {
    match identity {
        CacheIdentity::Jmap { account_id, .. } => CacheIdentity::Jmap {
            account_id: account_id.clone(),
            mailbox_id: "@account".to_owned(),
            email_state: None,
            query_state: None,
        },
        _ => identity.generation(),
    }
}

fn account_body_mailbox<'a>(mailbox: &'a str, identity: &CacheIdentity) -> &'a str {
    if matches!(identity, CacheIdentity::Jmap { .. }) {
        "@jmap-account"
    } else {
        mailbox
    }
}

/// A small, private raw-MIME cache for previously opened attachments.
/// Only server-scoped message identities are eligible. The reader never scans
/// this directory at startup; access and pruning happen on the mail worker.
#[derive(Debug, Clone)]
pub(crate) struct RawStore {
    pub(crate) dir: PathBuf,
}

const MAX_RAW_CACHE_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RAW_CACHE_BYTES: u64 = 128 * 1024 * 1024;

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

/// Stable, per-account name for all remote mailbox cache artifacts.
/// Keep this shared with account removal so purge cannot guess a filename.
pub(crate) fn account_cache_stem(account: &AccountConfig) -> String {
    let mut identity = crate::backend::redact_source(&account.source);
    // JMAP discovery may use a custom session endpoint on the same host.
    // Keep distinct endpoints isolated without hashing any credentials.
    if crate::backend::BackendKind::from_source(&account.source)
        == Some(crate::backend::BackendKind::Jmap)
        && let Ok(url) = url::Url::parse(&account.source)
    {
        identity.push_str(match url.path() {
            "" | "/" => "/.well-known/jmap",
            path => path,
        });
    }
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

#[cfg(test)]
pub(crate) fn read_snapshot(path: &Path) -> Option<Snapshot> {
    index_store::load(path).map(|(snapshot, _)| snapshot)
}

// Version-one fixtures exercise migration and backward compatibility.
#[cfg(test)]
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
mod tests;
#[cfg(test)]
pub(crate) use tests::test_cache_writer;

#[cfg(test)]
mod offline_mutation_journal_tests;
