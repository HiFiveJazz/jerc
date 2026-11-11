//! Small, private, per-account persistent mailbox index.
//!
//! The UI reads this once before connecting. Writes are serialized on a
//! dedicated thread so a slow disk cannot stall keyboard input or painting.
//! This is a display cache, never an authority for IMAP UID mutations.
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
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
/// Bounded pending flag changes, scoped to the server's mailbox generation.
pub(crate) const MAX_DEFERRED_SEEN: usize = 512;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    let mut selected = HashMap::with_capacity(MAX_BODIES_PER_FOLDER.min(bodies.len()));
    let limit = messages.len().min(MAX_MESSAGES_PER_FOLDER);
    let mut positions = (0..limit).collect::<Vec<_>>();
    positions.sort_by_key(|index| index.abs_diff(selected_index));
    for index in positions {
        let message = &messages[index];
        if selected.len() >= MAX_BODIES_PER_FOLDER {
            break;
        }
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
            selected.insert(message.id.clone(), body.clone());
        }
    }
    selected
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
        let changed =
            self.mailboxes.is_some() || !self.folders.is_empty() || self.deferred_seen.is_some();
        if let Some(mailboxes) = self.mailboxes.take() {
            apply(snapshot, Change::Mailboxes(mailboxes));
        }
        if let Some(entries) = self.deferred_seen.take() {
            apply(snapshot, Change::DeferredSeen(entries));
        }
        for (name, folder) in self.folders.drain() {
            match folder {
                Some(folder) => apply(snapshot, Change::Folder(name, folder)),
                None => apply(snapshot, Change::RemoveFolder(name)),
            }
        }
        changed
    }
}

pub(crate) struct Writer {
    wake: Option<SyncSender<()>>,
    pending: Arc<Mutex<PendingChanges>>,
    join: Option<JoinHandle<()>>,
    body_store: BodyStore,
}

impl Writer {
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
    let identity = crate::backend::redact_source(&account.source);
    let hash = stable_key(&format!("{}\0{identity}", account.name));
    let path = dir.join(format!("{hash:016x}.json"));
    let body_store = BodyStore {
        dir: dir.join(format!("{hash:016x}.bodies")),
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
    let snapshot = loaded.unwrap_or_default();
    let (wake, rx) = mpsc::sync_channel::<()>(1);
    let pending = Arc::new(Mutex::new(PendingChanges::default()));
    let thread_pending = Arc::clone(&pending);
    // The original snapshot belongs to the UI. Reopen the small index on the
    // background writer instead of cloning all cached rows on startup.
    let writer_body_store = body_store.clone();
    let join = thread::Builder::new()
        .name("mail-cache-writer".into())
        .spawn(move || {
            let mut stored = read_snapshot(&path).unwrap_or_default();
            let mut body_writes = 0;
            while rx.recv().is_ok() {
                // Coalesce status replies and quick reader changes into a
                // single disk write, without ever queueing duplicate bodies.
                while rx.recv_timeout(Duration::from_millis(160)).is_ok() {}
                let (index_changed, bodies) = if let Ok(mut changes) = thread_pending.lock() {
                    let changed = changes.drain_into(&mut stored);
                    (changed, changes.drain_bodies())
                } else {
                    (false, Vec::new())
                };
                if index_changed {
                    let _ = write_snapshot(&path, &stored);
                }
                for (mailbox, validity, body) in bodies {
                    if writer_body_store.write(&mailbox, validity, &body).is_ok() {
                        body_writes += 1;
                    }
                }
                if body_writes >= PRUNE_AFTER_WRITES {
                    let _ = writer_body_store.prune();
                    body_writes = 0;
                }
            }
            // Final flush includes updates sent while another write was active.
            let (index_changed, bodies) = if let Ok(mut changes) = thread_pending.lock() {
                let changed = changes.drain_into(&mut stored);
                (changed, changes.drain_bodies())
            } else {
                (false, Vec::new())
            };
            if index_changed {
                let _ = write_snapshot(&path, &stored);
            }
            let mut final_writes = 0;
            for (mailbox, validity, body) in bodies {
                if writer_body_store.write(&mailbox, validity, &body).is_ok() {
                    final_writes += 1;
                }
            }
            if body_writes + final_writes > 0 {
                let _ = writer_body_store.prune();
            }
        });
    match join {
        Ok(join) => (
            snapshot,
            Some(Writer {
                wake: Some(wake),
                pending,
                join: Some(join),
                body_store,
            }),
        ),
        Err(_) => (snapshot, None),
    }
}

fn apply(snapshot: &mut Snapshot, change: Change) {
    match change {
        Change::Mailboxes(mailboxes) => snapshot.mailboxes = mailboxes,
        Change::Folder(name, mut folder) => {
            // Never cache UID-based IMAP data without a server UIDVALIDITY.
            if folder.uid_validity.is_none() {
                return;
            }
            if folder.messages.len() > MAX_MESSAGES_PER_FOLDER {
                folder.messages.truncate(MAX_MESSAGES_PER_FOLDER);
                folder.next_page_offset = folder.messages.len();
                folder.has_more = true;
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
            if snapshot.folders.len() < MAX_FOLDERS || snapshot.folders.contains_key(&name) {
                snapshot.folders.insert(name, folder);
            }
        }
        Change::RemoveFolder(name) => {
            snapshot.folders.remove(&name);
        }
        Change::Body(..) => {} // Persisted separately; never grows startup JSON.
        Change::DeferredSeen(entries) => {
            snapshot.deferred_seen = entries
                .into_iter()
                .filter(DeferredSeen::has_valid_identity)
                .take(MAX_DEFERRED_SEEN)
                .collect();
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

    fn write(&self, mailbox: &str, validity: u32, body: &MessageBody) -> std::io::Result<()> {
        if !body.id.0.starts_with(&format!("imap-uid:{validity}:")) {
            return Err(std::io::Error::other("body UIDVALIDITY mismatch"));
        }
        fs::create_dir_all(&self.dir)?;
        secure_private_dir(&self.dir)?;
        let path = self.body_path(mailbox, validity, &body.id);
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        serde_json::to_writer(
            &mut tmp,
            &BodyFileRef {
                version: VERSION,
                mailbox,
                validity,
                body,
            },
        )
        .map_err(std::io::Error::other)?;
        tmp.flush()?;
        if tmp.as_file().metadata()?.len() > MAX_BODY_FILE_BYTES {
            return Err(std::io::Error::other("body exceeds offline cache limit"));
        }
        tmp.persist(path).map_err(|error| error.error)?;
        Ok(())
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
    .map(String::len)
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

fn write_snapshot(path: &PathBuf, snapshot: &Snapshot) -> std::io::Result<()> {
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
    serde_json::to_writer(
        &mut tmp,
        &CacheFileRef {
            version: VERSION,
            snapshot,
        },
    )
    .map_err(std::io::Error::other)?;
    tmp.flush()?;
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
            id: MessageId(format!("imap-uid:{uid_validity}:{uid}")),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            from_name: "Sender".into(),
            from_address: "sender@example.test".into(),
            to: "recipient@example.test".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: format!("Subject {uid}"),
            timestamp: i64::from(uid),
            date_label: "Today".into(),
            size: 25,
            flags: crate::model::MessageFlags::empty(),
            has_attachment_hint: false,
            attachment_names_hint: vec![],
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
        message.html_lines = Some(vec![crate::model::HtmlLine::default()]);
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
