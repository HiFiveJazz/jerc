//! Coalesced asynchronous cache writes and atomic index persistence.
use super::*;

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

pub(super) fn run_cache_writer(
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
pub(super) fn wait_for_cache_batch(rx: &Receiver<()>, quiet: Duration, max_delay: Duration) {
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
pub(super) fn flush_cache_changes(
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
        if matches!(body_store.write(&mailbox, &validity, &body), Ok(true)) {
            writes += 1;
        }
    }
    writes
}

/// Return true only when the on-disk representation would actually change.
/// Flags remain mutable, while shared immutable headers compare by value.
pub(super) fn apply(snapshot: &mut Snapshot, change: Change) -> bool {
    match change {
        Change::Mailboxes(mailboxes) => {
            if snapshot.mailboxes == mailboxes {
                return false;
            }
            snapshot.mailboxes = mailboxes;
            true
        }
        Change::Folder(name, mut folder) => {
            // Never persist messages without a matching server identity.
            if !folder.identity.as_ref().is_some_and(|identity| {
                identity.is_valid()
                    && folder
                        .messages
                        .iter()
                        .chain(&folder.thread_messages)
                        .all(|message| identity.accepts(&message.id))
                    && folder.bodies.keys().all(|id| identity.accepts(id))
            }) {
                return false;
            }
            if folder.messages.len() > MAX_MESSAGES_PER_FOLDER {
                folder.messages.truncate(MAX_MESSAGES_PER_FOLDER);
                folder.next_page_offset = folder.messages.len();
                folder.has_more = true;
            }
            let mut ids: HashSet<_> = folder.messages.iter().map(|m| m.id.clone()).collect();
            folder.thread_messages.retain(|m| ids.insert(m.id.clone()));
            folder.thread_messages.truncate(MAX_MESSAGES_PER_FOLDER);
            for body in folder.bodies.values_mut() {
                body.html_lines = None;
                body.html_renderer = None;
                body.html_render_width = 0;
            }
            folder.bodies.retain(|id, body| {
                folder
                    .messages
                    .iter()
                    .chain(&folder.thread_messages)
                    .any(|message| &message.id == id)
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
