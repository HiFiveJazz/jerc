//! Independently written mailbox shards. The small manifest is the commit
//! point for metadata, folder revisions and deferred Seen preferences.
use super::*;

const SHARDED_VERSION: u8 = 2;
const INDEX_VERSION: u8 = 3;
// Bound startup file opens and reclaim superseded inline body data.
pub(super) const MAX_PAGE_REVISIONS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Shard {
    file: String,
    bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pages: Vec<PageShard>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PageShard {
    file: String,
    bytes: u64,
}

impl Shard {
    fn total_bytes(&self) -> u64 {
        self.pages
            .iter()
            .fold(self.bytes, |sum, page| sum.saturating_add(page.bytes))
    }

    fn files(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.file.as_str()).chain(self.pages.iter().map(|page| page.file.as_str()))
    }
}

// A page is an immutable suffix, not an in-place append. The manifest
// publishes the ordered chain and its deferred Seen entries atomically.
#[derive(Deserialize)]
struct PageFile {
    name: String,
    start: usize,
    messages: Vec<MessageSummary>,
    bodies: HashMap<MessageId, Option<MessageBody>>,
    next_page_offset: usize,
    has_more: bool,
    identity: CacheIdentity,
    uid_next: Option<u32>,
}

#[derive(Serialize)]
struct PageRef<'a> {
    name: &'a str,
    start: usize,
    messages: &'a [MessageSummary],
    bodies: HashMap<&'a MessageId, Option<&'a MessageBody>>,
    next_page_offset: usize,
    has_more: bool,
    identity: &'a CacheIdentity,
    uid_next: Option<u32>,
}

struct PendingAppend {
    start: usize,
    body_changes: HashSet<MessageId>,
}

#[derive(Deserialize)]
struct DiskIndex {
    version: u8,
    #[serde(default)]
    snapshot: Option<Snapshot>, // version one
    #[serde(default)]
    mailboxes: Vec<Mailbox>,
    #[serde(default)]
    deferred_seen: Vec<DeferredSeen>,
    #[serde(default)]
    folders: HashMap<String, Shard>,
}

#[derive(Serialize)]
struct ManifestRef<'a> {
    version: u8,
    mailboxes: &'a [Mailbox],
    deferred_seen: &'a [DeferredSeen],
    folders: &'a HashMap<String, Shard>,
}

#[derive(Deserialize)]
struct FolderFile {
    name: String,
    folder: CachedMailbox,
}

// Navigation is deliberately excluded from mailbox payloads.
#[derive(Serialize)]
struct FolderRef<'a> {
    name: &'a str,
    folder: FolderDataRef<'a>,
}

#[derive(Serialize)]
struct FolderDataRef<'a> {
    messages: &'a [MessageSummary],
    thread_messages: &'a [MessageSummary],
    bodies: &'a HashMap<MessageId, MessageBody>,
    next_page_offset: usize,
    has_more: bool,
    #[serde(rename = "uid_validity")]
    identity: &'a Option<CacheIdentity>,
    uid_next: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct NavigationPosition {
    pub(super) identity: CacheIdentity,
    pub(super) index: usize,
    pub(super) offset: usize,
}

#[derive(Serialize, Deserialize)]
struct NavigationFile {
    version: u8,
    folders: HashMap<String, NavigationPosition>,
}

pub(super) fn navigation_path(path: &Path) -> PathBuf {
    path.with_extension("navigation.json")
}

pub(super) fn shard_dir(path: &Path) -> PathBuf {
    path.with_extension("folders")
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("index.lock")
}

// Explicitly unlock even if a subprocess inherited the file descriptor
// between fork and exec. Closing our handle alone may leave the lock held.
struct IndexWriteLock(File);

impl Drop for IndexWriteLock {
    fn drop(&mut self) {
        // An interrupted unlock must not leave an inherited descriptor holding
        // the transaction lock after this guard is dropped.
        while self
            .0
            .unlock()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::Interrupted)
        {}
    }
}

// The lock spans only local index transactions. It prevents a second writer
// (including an account reload) from deleting a revision still being published
// by the first. Persistent contention leaves the checkpoint dirty for retry.
fn writer_lock(path: &Path) -> std::io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no cache parent directory"))?;
    fs::create_dir_all(parent)?;
    secure_private_dir(parent)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(lock_path(path))?;
    lock.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => std::io::ErrorKind::WouldBlock.into(),
        fs::TryLockError::Error(error) => error,
    })?;
    Ok(lock)
}

fn transaction_lock(path: &Path) -> std::io::Result<IndexWriteLock> {
    writer_lock(path).map(IndexWriteLock)
}

// All callers of flush run on the cache writer (or in tests). A short,
// bounded wait handles transient contention without blocking terminal input
// or waiting indefinitely for another process. Uncontended saves do not sleep.
fn wait_for_transaction_lock(path: &Path) -> std::io::Result<IndexWriteLock> {
    let start = Instant::now();
    let timeout = Duration::from_millis(250);
    loop {
        match transaction_lock(path) {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let remaining = timeout.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    return Err(error);
                }
                thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            result => return result,
        }
    }
}

#[cfg(all(test, unix))]
#[test]
fn index_transaction_waits_for_transient_contention() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.json");
    let guard = transaction_lock(&path).unwrap();
    let other_path = path.clone();
    let (ready, started) = mpsc::channel();
    let waiter = thread::spawn(move || {
        // Verify the lock is genuinely held before reporting readiness.
        assert!(
            matches!(transaction_lock(&other_path), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        ready.send(()).unwrap();
        wait_for_transaction_lock(&other_path).map(drop)
    });
    started.recv().unwrap();
    thread::sleep(Duration::from_millis(10));
    drop(guard);
    waiter.join().unwrap().unwrap();
}

#[cfg(all(test, unix))]
#[test]
fn index_transaction_times_out_without_bypassing_lock() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.json");
    let guard = transaction_lock(&path).unwrap();
    assert!(
        matches!(wait_for_transaction_lock(&path), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    drop(guard);
    assert!(wait_for_transaction_lock(&path).is_ok());
}

#[cfg(all(test, unix))]
#[test]
fn index_lock_releases_even_with_a_duplicated_descriptor() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.json");
    let guard = transaction_lock(&path).unwrap();
    let duplicate = guard.0.try_clone().unwrap();
    // Real contention remains nonblocking and retains its error kind.
    assert!(
        matches!(transaction_lock(&path), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    drop(guard);
    // The duplicate models a descriptor inherited by a child before exec.
    // It must not prevent the next transaction from taking the lock.
    let next = transaction_lock(&path).unwrap();
    drop(next);
    drop(duplicate);
    assert!(transaction_lock(&path).is_ok());
}

fn valid_shard_name(name: &str) -> bool {
    name.strip_prefix("folder-")
        .or_else(|| name.strip_prefix("page-"))
        .and_then(|name| name.strip_suffix(".json"))
        .is_some_and(|key| {
            !key.is_empty() && key.len() <= 64 && key.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Option<T> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > limit {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > limit {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn usable_folder(folder: &CachedMailbox) -> bool {
    folder.identity.as_ref().is_some_and(|identity| {
        folder.messages.len() <= MAX_MESSAGES_PER_FOLDER
            && folder.thread_messages.len() <= MAX_MESSAGES_PER_FOLDER
            && identity.is_valid()
            && folder
                .messages
                .iter()
                .chain(&folder.thread_messages)
                .all(|message| identity.accepts(&message.id))
            && folder.bodies.keys().all(|id| identity.accepts(id))
    })
}

fn load_folder(dir: &Path, name: &str, shard: &Shard) -> Option<CachedMailbox> {
    if shard.pages.len() > MAX_PAGE_REVISIONS || shard.total_bytes() > MAX_CACHE_BYTES {
        return None;
    }
    if !valid_shard_name(&shard.file) {
        return None;
    }
    let base_path = dir.join(&shard.file);
    if !fs::symlink_metadata(&base_path).is_ok_and(|m| m.is_file() && m.len() == shard.bytes) {
        return None;
    }
    let file = read_json::<FolderFile>(&base_path, shard.bytes)?;
    if file.name != name || !usable_folder(&file.folder) {
        return None;
    }
    let mut folder = file.folder;
    if shard.pages.is_empty() {
        return Some(folder);
    }
    let mut names = HashSet::new();
    if !shard
        .files()
        .all(|file| valid_shard_name(file) && names.insert(file))
    {
        return None;
    }
    let mut ids: HashSet<_> = folder
        .messages
        .iter()
        .chain(&folder.thread_messages)
        .map(|message| message.id.clone())
        .collect();
    for part in &shard.pages {
        let path = dir.join(&part.file);
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == part.bytes) {
            return None;
        }
        let page = read_json::<PageFile>(&path, part.bytes)?;
        if page.name != name
            || page.start != folder.messages.len()
            || page.messages.is_empty()
            || page.messages.len() > MAX_MESSAGES_PER_FOLDER.saturating_sub(page.start)
            || page.next_page_offset < folder.next_page_offset
            || !page.identity.is_valid()
            || !folder
                .identity
                .as_ref()
                .is_some_and(|id| id.same_generation(&page.identity))
            || !page
                .messages
                .iter()
                .all(|message| page.identity.accepts(&message.id) && ids.insert(message.id.clone()))
        {
            return None;
        }
        folder.messages.extend(page.messages);
        for (id, body) in page.bodies {
            if !ids.contains(&id) {
                return None;
            }
            if let Some(body) = body {
                if body.id != id {
                    return None;
                }
                folder.bodies.insert(id, body);
            } else {
                folder.bodies.remove(&id);
            }
        }
        if folder.bodies.len() > MAX_BODIES_PER_FOLDER {
            return None;
        }
        folder.identity = Some(page.identity);
        folder.next_page_offset = page.next_page_offset;
        folder.has_more = page.has_more;
        folder.uid_next = page.uid_next;
    }
    Some(folder)
}

pub(super) fn load(path: &Path) -> Option<(Snapshot, IndexCheckpoint)> {
    // Existing version-two indexes have a lock file. A read lock prevents
    // the manifest's referenced shards being retired during startup loading.
    // Version-one caches do not have a lock and still load normally.
    let _guard = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path(path))
    {
        Ok(lock) => {
            lock.lock_shared().ok()?;
            Some(lock)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    let disk: DiskIndex = read_json(path, MAX_CACHE_BYTES)?;
    let (mut snapshot, mut checkpoint) = match disk.version {
        VERSION => {
            let mut snapshot = disk.snapshot?;
            if snapshot.folders.len() > MAX_FOLDERS {
                return None;
            }
            snapshot.folders.retain(|_, folder| usable_folder(folder));
            let checkpoint = IndexCheckpoint {
                metadata_dirty: true,
                navigation_dirty: true,
                folders_dirty: snapshot.folders.keys().cloned().collect(),
                ..Default::default()
            };
            (snapshot, checkpoint)
        }
        SHARDED_VERSION | INDEX_VERSION => {
            if disk.folders.len() > MAX_FOLDERS {
                return None;
            }
            let nav_path = navigation_path(path);
            let manifest_bytes = fs::metadata(path).ok()?.len();
            let nav_bytes = fs::symlink_metadata(&nav_path)
                .ok()
                .filter(|m| m.file_type().is_file() && m.len() <= MAX_CACHE_BYTES)
                .map_or(0, |m| m.len());
            let index_bytes = disk.folders.values().fold(manifest_bytes, |sum, shard| {
                sum.saturating_add(shard.total_bytes())
            });
            if index_bytes > MAX_CACHE_BYTES {
                return None;
            }
            let mut folders = HashMap::new();
            let mut committed = disk.folders;
            let original_count = committed.len();
            let dir = shard_dir(path);
            committed.retain(|name, shard| {
                if disk.version == SHARDED_VERSION && !shard.pages.is_empty() {
                    return false;
                }
                let Some(folder) = load_folder(&dir, name, shard) else {
                    return false;
                };
                folders.insert(name.clone(), folder);
                true
            });
            // A bad or oversized optional navigation file cannot invalidate
            // the mailbox data. It will be repaired by the next checkpoint.
            let mut navigation_dirty = false;
            if index_bytes.saturating_add(nav_bytes) <= MAX_CACHE_BYTES
                && let Some(nav) =
                    read_json::<NavigationFile>(&nav_path, MAX_CACHE_BYTES - index_bytes)
                && nav.version == SHARDED_VERSION
                && nav.folders.len() <= MAX_FOLDERS
            {
                for (name, position) in nav.folders {
                    if let Some(folder) = folders.get_mut(&name) {
                        restore_position(folder, &position);
                    }
                }
            } else if nav_path.exists() {
                navigation_dirty = true;
            }
            (
                Snapshot {
                    mailboxes: disk.mailboxes,
                    folders,
                    deferred_seen: disk.deferred_seen,
                    deferred_mutations: Vec::new(),
                },
                IndexCheckpoint {
                    metadata_dirty: original_count != committed.len(),
                    navigation_dirty,
                    committed,
                    manifest_bytes,
                    nav_bytes,
                    ..Default::default()
                },
            )
        }
        _ => return None,
    };
    let old_count = snapshot.deferred_seen.len();
    snapshot
        .deferred_seen
        .retain(DeferredSeen::has_valid_identity);
    snapshot.deferred_seen.truncate(MAX_DEFERRED_SEEN);
    checkpoint.metadata_dirty |= old_count != snapshot.deferred_seen.len();
    Some((snapshot, checkpoint))
}

fn restore_position(folder: &mut CachedMailbox, position: &NavigationPosition) -> bool {
    if !folder
        .identity
        .as_ref()
        .is_some_and(|id| id.same_generation(&position.identity))
    {
        return false;
    }
    let max = folder.messages.len().saturating_sub(1);
    let index = position.index.min(max);
    let offset = position.offset.min(index);
    let changed = (folder.message_index, folder.message_view_offset) != (index, offset);
    folder.message_index = index;
    folder.message_view_offset = offset;
    changed
}

#[derive(Default)]
pub(super) struct IndexCheckpoint {
    pub(super) metadata_dirty: bool,
    pub(super) navigation_dirty: bool,
    pub(super) folders_dirty: HashSet<String>,
    committed: HashMap<String, Shard>,
    appends: HashMap<String, PendingAppend>,
    manifest_bytes: u64,
    nav_bytes: u64,
    cleaned_orphans: bool,
}

impl IndexCheckpoint {
    pub(super) fn is_dirty(&self) -> bool {
        self.metadata_dirty || self.navigation_dirty || !self.folders_dirty.is_empty()
    }

    // Only retain a suffix candidate while every uncommitted change is an
    // append. A failed write can be followed by another page without losing
    // the original start offset or any pending warm-body changes.
    pub(super) fn note_folder_change(
        &mut self,
        name: &str,
        old: Option<&CachedMailbox>,
        new: &CachedMailbox,
    ) {
        let append = old.filter(|old| {
            self.committed.contains_key(name)
                && (!self.folders_dirty.contains(name) || self.appends.contains_key(name))
                && old.messages.len() < new.messages.len()
                && new.messages.starts_with(&old.messages)
                && old.thread_messages == new.thread_messages
                && old
                    .identity
                    .as_ref()
                    .zip(new.identity.as_ref())
                    .is_some_and(|(a, b)| a.same_generation(b))
                && new.next_page_offset >= old.next_page_offset
                && new.bodies.len() <= MAX_BODIES_PER_FOLDER
                && new.bodies.iter().all(|(id, body)| id == &body.id)
                && {
                    let mut ids: HashSet<_> = old
                        .messages
                        .iter()
                        .chain(&old.thread_messages)
                        .map(|message| &message.id)
                        .collect();
                    new.messages[old.messages.len()..]
                        .iter()
                        .all(|message| ids.insert(&message.id))
                }
        });
        if let Some(old) = append {
            let pending = self
                .appends
                .entry(name.to_owned())
                .or_insert_with(|| PendingAppend {
                    start: old.messages.len(),
                    body_changes: HashSet::new(),
                });
            pending.body_changes.extend(
                old.bodies
                    .keys()
                    .chain(new.bodies.keys())
                    .filter(|id| old.bodies.get(*id) != new.bodies.get(*id))
                    .cloned(),
            );
        } else {
            self.appends.remove(name);
        }
        self.folders_dirty.insert(name.to_owned());
    }

    pub(super) fn note_folder_removal(&mut self, name: String) {
        self.appends.remove(&name);
        self.folders_dirty.insert(name);
    }

    pub(super) fn apply_navigation(
        &mut self,
        snapshot: &mut Snapshot,
        name: &str,
        position: &NavigationPosition,
    ) -> bool {
        let changed = snapshot
            .folders
            .get_mut(name)
            .is_some_and(|folder| restore_position(folder, position));
        self.navigation_dirty |= changed;
        changed
    }

    pub(super) fn flush(&mut self, path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
        let _guard = wait_for_transaction_lock(path)?;
        // An account reload or second process can have committed after this
        // writer loaded its snapshot. Never reuse references to retired files.
        let current = read_json::<DiskIndex>(path, MAX_CACHE_BYTES)
            .filter(|disk| matches!(disk.version, SHARDED_VERSION | INDEX_VERSION))
            .map(|disk| disk.folders)
            .unwrap_or_default();
        if self.metadata_dirty || !self.folders_dirty.is_empty() {
            if current != self.committed {
                // Preserve the old account-wide last-writer-wins semantics
                // on concurrent metadata updates, but publish valid shards.
                self.folders_dirty.extend(snapshot.folders.keys().cloned());
                self.folders_dirty.extend(
                    current
                        .keys()
                        .filter(|name| !snapshot.folders.contains_key(*name))
                        .cloned(),
                );
                self.committed = current;
                self.appends.clear();
            }
            self.flush_manifest(path, snapshot)?;
        } else {
            // A navigation-only checkpoint must not rewrite mailbox data,
            // even if another writer has committed a newer manifest.
            self.manifest_bytes = fs::metadata(path)?.len();
            let current_bytes = current.values().fold(self.manifest_bytes, |sum, shard| {
                sum.saturating_add(shard.total_bytes())
            });
            // Use the actual current index for budget checks without adopting
            // its revisions into this writer's potentially older snapshot.
            self.flush_navigation(path, snapshot, current_bytes)?;
            return Ok(());
        }
        let index_bytes = self
            .committed
            .values()
            .fold(self.manifest_bytes, |sum, shard| {
                sum.saturating_add(shard.total_bytes())
            });
        self.flush_navigation(path, snapshot, index_bytes)
    }

    fn flush_navigation(
        &mut self,
        path: &Path,
        snapshot: &Snapshot,
        index_bytes: u64,
    ) -> std::io::Result<()> {
        if self.navigation_dirty {
            let positions = snapshot
                .folders
                .iter()
                .filter_map(|(name, folder)| {
                    Some((
                        name.clone(),
                        NavigationPosition {
                            identity: folder.identity.as_ref()?.generation(),
                            index: folder.message_index,
                            offset: folder.message_view_offset,
                        },
                    ))
                })
                .collect();
            let nav_path = navigation_path(path);
            let (tmp, bytes) = stage_json(
                &nav_path,
                &NavigationFile {
                    version: SHARDED_VERSION,
                    folders: positions,
                },
            )?;
            if index_bytes.saturating_add(bytes) > MAX_CACHE_BYTES {
                return Err(std::io::Error::other("mail cache size limit exceeded"));
            }
            tmp.persist(nav_path).map_err(|error| error.error)?;
            self.nav_bytes = bytes;
            self.navigation_dirty = false;
        }
        Ok(())
    }

    fn flush_manifest(&mut self, path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
        let dir = shard_dir(path);
        // Navigation may have been updated by another process since load.
        self.nav_bytes = fs::symlink_metadata(navigation_path(path))
            .ok()
            .filter(|m| m.file_type().is_file() && m.len() <= MAX_CACHE_BYTES)
            .map_or(0, |m| m.len());
        let mut next = self.committed.clone();
        let mut staged = Vec::new();
        let mut appended = false;
        for name in &self.folders_dirty {
            let Some(folder) = snapshot.folders.get(name) else {
                next.remove(name);
                continue;
            };
            if let Some(append) = self.appends.get(name)
                && let Some(shard) = next.get_mut(name)
                && shard.pages.len() < MAX_PAGE_REVISIONS
                && let Some(identity) = folder.identity.as_ref()
                && append.start < folder.messages.len()
            {
                let value = PageRef {
                    name,
                    start: append.start,
                    messages: &folder.messages[append.start..],
                    bodies: append
                        .body_changes
                        .iter()
                        .map(|id| (id, folder.bodies.get(id)))
                        .collect(),
                    next_page_offset: folder.next_page_offset,
                    has_more: folder.has_more,
                    identity,
                    uid_next: folder.uid_next,
                };
                let (tmp, part) = stage_shard(&dir, "page", &value)?;
                staged.push((tmp, dir.join(&part.file)));
                shard.pages.push(part);
                appended = true;
                continue;
            }
            let value = FolderRef {
                name,
                folder: FolderDataRef {
                    messages: &folder.messages,
                    thread_messages: &folder.thread_messages,
                    bodies: &folder.bodies,
                    next_page_offset: folder.next_page_offset,
                    has_more: folder.has_more,
                    identity: &folder.identity,
                    uid_next: folder.uid_next,
                },
            };
            let (tmp, part) = stage_shard(&dir, "folder", &value)?;
            staged.push((tmp, dir.join(&part.file)));
            next.insert(
                name.clone(),
                Shard {
                    file: part.file,
                    bytes: part.bytes,
                    pages: Vec::new(),
                },
            );
        }
        let (manifest, manifest_bytes) = stage_json(
            path,
            &ManifestRef {
                version: INDEX_VERSION,
                mailboxes: &snapshot.mailboxes,
                deferred_seen: &snapshot.deferred_seen,
                folders: &next,
            },
        )?;
        let index_bytes = next.values().fold(manifest_bytes, |sum, shard| {
            sum.saturating_add(shard.total_bytes())
        });
        if index_bytes > MAX_CACHE_BYTES {
            if appended
                || self.committed.iter().any(|(name, shard)| {
                    !shard.pages.is_empty() && !self.folders_dirty.contains(name)
                })
            {
                // Retry with compacted chains, including previously clean
                // folders if retained revisions consume the cache budget.
                // No new revision has been published yet.
                drop(staged);
                drop(manifest);
                self.appends.clear();
                self.folders_dirty.extend(
                    self.committed
                        .iter()
                        .filter(|(_, shard)| !shard.pages.is_empty())
                        .map(|(name, _)| name.clone()),
                );
                return self.flush_manifest(path, snapshot);
            }
            return Err(std::io::Error::other("mail cache size limit exceeded"));
        }
        if index_bytes.saturating_add(self.nav_bytes) > MAX_CACHE_BYTES {
            // Optional navigation must not prevent a valid metadata/Seen
            // checkpoint. Discard it only when it would exceed the budget.
            fs::remove_file(navigation_path(path))?;
            self.nav_bytes = 0;
            self.navigation_dirty = true;
        }
        let mut published = Vec::new();
        let result = (|| -> std::io::Result<()> {
            for (tmp, dest) in staged {
                tmp.persist_noclobber(&dest).map_err(|error| error.error)?;
                published.push(dest);
            }
            manifest.persist(path).map_err(|error| error.error)?;
            Ok(())
        })();
        if let Err(error) = result {
            for file in published {
                let _ = fs::remove_file(file);
            }
            return Err(error);
        }
        // Only files from our previous committed manifest are candidates for
        // immediate deletion. Never scan or touch body/raw/mutation storage.
        let live_files: HashSet<_> = next.values().flat_map(Shard::files).collect();
        for old in self.committed.values().flat_map(Shard::files) {
            if valid_shard_name(old) && !live_files.contains(old) {
                let _ = fs::remove_file(dir.join(old));
            }
        }
        // A process crash between staging and manifest publication can leave
        // an orphan. Sweep once per writer, while holding the transaction lock.
        if !self.cleaned_orphans {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if let Some(name) = name.to_str()
                        && (valid_shard_name(name) || name.starts_with(".tmp"))
                        && !live_files.contains(name)
                    {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
            self.cleaned_orphans = true;
        }
        self.committed = next;
        self.manifest_bytes = manifest_bytes;
        self.metadata_dirty = false;
        self.folders_dirty.clear();
        self.appends.clear();
        Ok(())
    }
}

// Immutable revisions keep the old manifest valid until the new commit.
fn stage_shard<T: Serialize>(
    dir: &Path,
    prefix: &str,
    value: &T,
) -> std::io::Result<(tempfile::NamedTempFile, PageShard)> {
    let (tmp, bytes) = stage_json(&dir.join("stage"), value)?;
    let token = tmp
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(".tmp"))
        .ok_or_else(|| std::io::Error::other("invalid cache temporary name"))?;
    let file = format!("{prefix}-{token}.json");
    if !valid_shard_name(&file) {
        return Err(std::io::Error::other("invalid cache shard name"));
    }
    Ok((tmp, PageShard { file, bytes }))
}

fn stage_json<T: Serialize>(
    path: &Path,
    value: &T,
) -> std::io::Result<(tempfile::NamedTempFile, u64)> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no cache parent directory"))?;
    fs::create_dir_all(parent)?;
    secure_private_dir(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp")
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    write_json_buffered(&mut tmp, value)?;
    let bytes = tmp.as_file().metadata()?.len();
    if bytes > MAX_CACHE_BYTES {
        return Err(std::io::Error::other("mail cache size limit exceeded"));
    }
    Ok((tmp, bytes))
}
