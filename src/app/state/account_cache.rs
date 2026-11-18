//! In-session and durable mailbox display caches. Never authoritative for UID mutations.
use super::*;

impl AccountState {
    /// Keep only a few inactive mailboxes warm. The active mailbox has its
    /// own independently bounded body cache, and disk holds the rest.
    pub(super) fn remember_mailbox(&mut self, name: String, mut cached: CachedMailbox) {
        let limit = cache::MAX_MESSAGES_PER_FOLDER;
        if cached.messages.len() > limit {
            cached.messages.truncate(limit);
            cached.next_page_offset = limit;
            cached.has_more = true;
        }
        cached.message_index = cached
            .message_index
            .min(cached.messages.len().saturating_sub(1));
        cache::retain_bodies_near(&cached.messages, &mut cached.bodies, cached.message_index);
        self.mailbox_cache.insert(name.clone(), cached);
        self.warm_folder_order.retain(|entry| entry != &name);
        self.warm_folder_order.push_back(name);
        while self.warm_folder_order.len() > MAX_WARM_MAILBOXES {
            if let Some(old) = self.warm_folder_order.pop_front() {
                self.mailbox_cache.remove(&old);
            }
        }
    }

    /// In-memory and persistent snapshots share cursor and size bounds.
    /// Only the in-memory copy keeps derived rich HTML layout data, so folder
    /// switches stay fast without making the writer retain redundant spans.
    pub(super) fn current_cache_snapshot(&self) -> Option<CachedMailbox> {
        self.cache_snapshot(false)
    }

    /// The disk writer must never retain fully rendered HTML spans alongside
    /// the UI's own rich previews. Only source representations are persisted.
    pub(super) fn persistent_cache_snapshot(&self) -> Option<CachedMailbox> {
        self.cache_snapshot(true)
    }

    fn cache_snapshot(&self, compact: bool) -> Option<CachedMailbox> {
        let name = self.current_mailbox.as_deref()?;
        let max_messages = cache::MAX_MESSAGES_PER_FOLDER;
        let truncated = self.messages.len() > max_messages;
        let retained = self.messages.len().min(max_messages);
        Some(CachedMailbox {
            messages: self.messages.iter().take(max_messages).cloned().collect(),
            bodies: if compact {
                cache::persistable_bodies_near_compact(
                    &self.messages,
                    &self.bodies,
                    self.message_index,
                )
            } else {
                cache::persistable_bodies_near(
                    &self.messages,
                    &self.bodies,
                    self.message_index,
                )
            },
            message_index: self.message_index.min(retained.saturating_sub(1)),
            message_view_offset: self.message_view_offset,
            next_page_offset: if truncated {
                max_messages
            } else {
                self.next_page_offset
            },
            has_more: truncated || self.has_more,
            uid_validity: self.uid_validities.get(name).copied(),
            uid_next: self.uid_nexts.get(name).copied(),
        })
    }

    pub(super) fn save_current_cache(&mut self) {
        if let (Some(name), Some(snapshot)) =
            (self.current_mailbox.clone(), self.current_cache_snapshot())
        {
            self.remember_mailbox(name, snapshot);
        }
    }

    /// Store a compact readable body without rewriting the startup index.
    /// The asynchronous writer owns all filesystem work.
    pub(super) fn persist_body(&self, body: &MessageBody) {
        let Some(mailbox) = self.current_mailbox.as_deref() else {
            return;
        };
        if self.unverified_mailboxes.contains(mailbox)
            || !self.messages.iter().any(|message| message.id == body.id)
        {
            return;
        }
        let (Some(writer), Some(validity)) = (
            self.cache_writer.as_ref(),
            self.uid_validities.get(mailbox).copied(),
        ) else {
            return;
        };
        if let Some(compact) = cache::compact_body(body) {
            writer.send(cache::Change::Body(mailbox.to_owned(), validity, compact));
        }
    }

    pub(super) fn persist_mailboxes(&self) {
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::Mailboxes(self.mailboxes.clone()));
        }
    }

    pub(super) fn persist_current_cache(&mut self) {
        let (Some(name), Some(writer)) = (self.current_mailbox.as_deref(), &self.cache_writer)
        else {
            return;
        };
        // Only persist server-verified UIDs. Cached snapshots are display hints,
        // never an authority for subsequent IMAP mutations.
        if self.unverified_mailboxes.contains(name) || !self.uid_validities.contains_key(name) {
            return;
        }
        if let Some(snapshot) = self.persistent_cache_snapshot() {
            writer.send(cache::Change::Folder(name.to_owned(), snapshot));
        }
    }

    /// Give Drafts/Sent/Trash and then the remaining folders a bounded
    /// background header warm-up. This is deliberately not a thread per folder.
    pub(super) fn plan_mailbox_warming(&mut self) {
        self.mailbox_warm_queue.clear();
        self.mailbox_warm_request = None;
        let selected = self.current_mailbox.as_deref();
        // Sort common destinations first, preserving server LIST order within
        // a role. Do not prefetch enormous All Mail/Archive on startup.
        let mut candidates = self.mailboxes.iter().collect::<Vec<_>>();
        candidates.sort_by_key(|folder| match folder.role {
            MailboxRole::Drafts => 0,
            MailboxRole::Sent => 1,
            MailboxRole::Trash => 2,
            MailboxRole::Inbox => 3,
            _ => 4,
        });
        for folder in candidates {
            if selected == Some(folder.name.as_str()) {
                continue;
            }
            // Even a cached folder benefits from an eventual delta sync.
            // Its existing rows remain available immediately meanwhile.
            self.mailbox_warm_queue.push_back(folder.name.clone());
        }
    }

    /// One page at a time, only on the independent read-only session. The
    /// queue survives a busy worker and is retried after other UI events.
    pub(super) fn pump_mailbox_warming(&mut self) {
        if self.capabilities.is_none() || self.connecting
            || self.mailbox_warm_request.is_some()
            || !self.pending_mutation_labels.is_empty()
        {
            return;
        }
        while let Some(name) = self.mailbox_warm_queue.pop_front() {
            if self.current_mailbox.as_deref() == Some(name.as_str()) {
                continue;
            }
            let hint = self.mailbox_cache.get(&name).and_then(|cached| {
                cached.uid_validity.map(|uid_validity| MailboxSyncHint {
                    uid_validity,
                    uid_next: cached.uid_next,
                    messages: cached.messages.clone(),
                })
            });
            match self.worker.post(ActionKind::WarmMailbox {
                name: name.clone(),
                limit: self.page_size.min(40),
                hint,
            }) {
                Ok(id) => self.mailbox_warm_request = Some((id, name)),
                Err(_) => self.mailbox_warm_queue.push_front(name),
            }
            break;
        }
    }

    /// Replace a warmed folder using ownership rather than cloning every rich
    /// preview from the old cache. This matters when background sync refreshes
    /// several folders that already have rendered HTML in memory.
    pub(super) fn accept_warmed_mailbox(
        &mut self,
        name: String,
        page: crate::model::MessagePage,
        uid_validity: Option<u32>,
        uid_next: Option<u32>,
    ) {
        let previous = self.mailbox_cache.remove(&name);
        let same_generation = uid_validity.is_some()
            && previous.as_ref().is_some_and(|cached| cached.uid_validity == uid_validity);
        let live = page.messages.iter().map(|message| &message.id).collect::<HashSet<_>>();
        let (bodies, message_index, message_view_offset) = match previous {
            Some(cached) if same_generation => (
                cached.bodies.into_iter()
                    .filter(|(id, _)| live.contains(id))
                    .collect(),
                cached.message_index,
                cached.message_view_offset,
            ),
            _ => (HashMap::new(), 0, 0),
        };
        let cached = CachedMailbox {
            messages: page.messages,
            bodies,
            message_index,
            message_view_offset,
            next_page_offset: page.next_offset,
            has_more: page.has_more,
            uid_validity,
            uid_next,
        };
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::Folder(name.clone(), cached.for_persistence()));
        }
        self.remember_mailbox(name.clone(), cached);
        if let Some(validity) = uid_validity {
            self.uid_validities.insert(name.clone(), validity);
        }
        if let Some(next) = uid_next {
            self.uid_nexts.insert(name.clone(), next);
        }
        self.unverified_mailboxes.remove(&name);
    }

    /// A successful folder mutation makes any prior in-flight preload stale.
    pub(super) fn rewarm_mailbox(&mut self, name: &str) {
        if self.current_mailbox.as_deref() == Some(name) {
            return;
        }
        if self.mailbox_warm_request.as_ref().is_some_and(|(_, pending)| pending == name) {
            self.mailbox_warm_request = None;
        }
        self.mailbox_warm_queue.retain(|pending| pending != name);
        if self.mailboxes.iter().any(|folder| folder.name == name) {
            self.mailbox_warm_queue.push_front(name.to_owned());
        }
        self.pump_mailbox_warming();
    }

    /// Invalidate the full derived folder snapshot (in-memory, LRU, disk).
    /// Call this after a mutation that changes a folder whose new UIDs are
    /// unknown. It deliberately does not clear the active mailbox: callers
    /// may still have a valid, optimistically updated source view.
    pub(super) fn forget_cached_folder(&mut self, name: &str) {
        if self.mailbox_warm_request.as_ref().is_some_and(|(_, pending)| pending == name) {
            self.mailbox_warm_request = None;
        }
        self.mailbox_warm_queue.retain(|queued| queued != name);
        self.mailbox_cache.remove(name);
        self.warm_folder_order.retain(|entry| entry != name);
        self.remove_persistent_folder(name);
    }

    /// Invalidate both a remote-changed folder's cache and its STATUS counts.
    /// Do not call this for optimistically updated mutations: those know the
    /// count delta but cannot infer the destination UIDs.
    pub(super) fn invalidate_remote_folder(&mut self, name: &str) {
        self.forget_cached_folder(name);
        if let Some(mailbox) = self
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == name)
        {
            mailbox.counts_known = false;
        }
    }

    pub(super) fn remove_persistent_folder(&self, name: &str) {
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::RemoveFolder(name.to_owned()));
        }
    }

    pub(super) fn restore_cached_mailbox(&mut self, name: &str) -> bool {
        // Once selected, the live vectors own the folder. Moving them avoids
        // duplicating potentially megabytes of HTML previews on every switch.
        let Some(cached) = self.mailbox_cache.remove(name) else {
            return false;
        };
        self.warm_folder_order.retain(|entry| entry != name);
        self.current_mailbox = Some(name.to_owned());
        self.messages = cached.messages;
        self.bodies = cached.bodies;
        self.message_index = cached
            .message_index
            .min(self.messages.len().saturating_sub(1));
        self.message_view_offset = cached.message_view_offset;
        self.next_page_offset = cached.next_page_offset;
        self.has_more = cached.has_more;
        self.page_inflight = None;
        self.page_request_id = None;
        self.body_request_id = None;
        self.last_prefetch_window = None;
        self.messages_loading = false;
        self.preview_scroll = 0;
        self.preview_max_scroll = 0;
        self.pending_bodies.clear();
        self.pending_seen.clear();
        self.search_query.clear();
        self.search_editing = false;
        self.clamp_selection();
        self.overlay_deferred_seen();
        self.overlay_deferred_mutations();
        // This precedes the network refresh. It identifies the moment cached
        // rows become available to the UI, not the terminal's paint latency.
        crate::perf::record("ui_cache_restore", None, Duration::ZERO, true);
        true
    }

    pub(super) fn invalidate_current_cache(&mut self) {
        if let Some(name) = self.current_mailbox.clone() {
            self.forget_cached_folder(&name);
        }
    }
}
