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
        let mut known: HashSet<_> = cached.messages.iter().map(|m| m.id.clone()).collect();
        cached
            .thread_messages
            .retain(|m| known.insert(m.id.clone()));
        cached.thread_messages.truncate(limit);
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
        if self.native_view.is_some() {
            return None;
        }
        let name = self.current_mailbox.as_deref()?;
        let max_messages = cache::MAX_MESSAGES_PER_FOLDER;
        let messages = self.canonical_messages();
        let truncated = messages.len() > max_messages;
        let retained = messages.len().min(max_messages);
        let selected = self.cache_selected_index(messages);
        Some(CachedMailbox {
            thread_messages: self
                .messages
                .iter()
                .skip(messages.len())
                .take(max_messages)
                .cloned()
                .collect(),
            messages: messages.iter().take(max_messages).cloned().collect(),
            bodies: if compact {
                cache::persistable_bodies_near_compact(messages, &self.bodies, selected)
            } else {
                cache::persistable_bodies_near(messages, &self.bodies, selected)
            },
            message_index: selected.min(retained.saturating_sub(1)),
            message_view_offset: self.message_view_offset,
            next_page_offset: if truncated {
                max_messages
            } else {
                self.next_page_offset
            },
            has_more: truncated || self.has_more,
            identity: self.cache_identities.get(name).cloned(),
            uid_next: self.uid_nexts.get(name).copied(),
        })
    }

    fn cache_selected_index(&self, messages: &[MessageSummary]) -> usize {
        if self.message_index >= messages.len() {
            self.local_thread_index
                .borrow_mut()
                .representative(&self.messages, self.message_index)
                .unwrap_or(0)
        } else {
            self.message_index
        }
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
            self.cache_identities
                .get(mailbox)
                .map(CacheIdentity::generation),
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
        // Only persist server-verified identities. Cached snapshots are display hints,
        // never an authority for subsequent IMAP mutations.
        if self.unverified_mailboxes.contains(name) || !self.cache_identities.contains_key(name) {
            return;
        }
        if let Some(snapshot) = self.persistent_cache_snapshot() {
            writer.send(cache::Change::Folder(name.to_owned(), snapshot));
        }
    }

    /// Persist only the small navigation state at deliberate boundaries.
    pub(super) fn persist_current_navigation(&self) {
        let (Some(name), Some(writer)) = (self.current_mailbox.as_deref(), &self.cache_writer)
        else {
            return;
        };
        // Navigation is a display hint, so an offline cached generation
        // is sufficient; it never authorizes a remote UID operation.
        if self.native_view.is_some() {
            return;
        }
        let Some(identity) = self.cache_identities.get(name) else {
            return;
        };
        let messages = self.canonical_messages();
        let selected = self.cache_selected_index(messages).min(
            messages
                .len()
                .min(cache::MAX_MESSAGES_PER_FOLDER)
                .saturating_sub(1),
        );
        writer.send(cache::Change::Navigation(
            name.to_owned(),
            identity.generation(),
            selected,
            self.message_view_offset,
        ));
    }

    /// Give cold common destinations and changed cached folders a bounded
    /// background header warm-up. This is deliberately not a thread per folder.
    pub(super) fn plan_mailbox_warming(&mut self) {
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
            // A valid warm page should not be downloaded again on every LIST.
            // Cold speculative work is restricted to commonly used folders.
            if self.mailbox_cache.contains_key(&folder.name)
                || self
                    .mailbox_warm_request
                    .as_ref()
                    .is_some_and(|(_, name)| name == &folder.name)
                || self.mailbox_warm_queue.contains(&folder.name)
                || !matches!(
                    folder.role,
                    MailboxRole::Drafts
                        | MailboxRole::Sent
                        | MailboxRole::Trash
                        | MailboxRole::Inbox
                )
            {
                continue;
            }
            self.mailbox_warm_queue.push_back(folder.name.clone());
        }
    }

    /// One page at a time, only on the independent read-only session. The
    /// queue survives a busy worker and is retried after other UI events.
    pub(super) fn pump_mailbox_warming(&mut self) {
        if self.capabilities.is_none()
            || self.connecting
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
                cached.identity.clone().map(|identity| MailboxSyncHint {
                    identity,
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
        identity: Option<CacheIdentity>,
        uid_next: Option<u32>,
    ) {
        let previous = self.mailbox_cache.remove(&name);
        let same_generation = identity.as_ref().is_some_and(|identity| {
            previous
                .as_ref()
                .and_then(|cached| cached.identity.as_ref())
                .is_some_and(|old| old.same_generation(identity))
        });
        let supplemental_compatible = previous
            .as_ref()
            .and_then(|cached| cached.identity.as_ref())
            .is_some_and(|old| match (old, identity.as_ref()) {
                (CacheIdentity::Imap(a), Some(CacheIdentity::Imap(b))) => a == b,
                (
                    CacheIdentity::Jmap {
                        email_state: Some(a),
                        ..
                    },
                    Some(CacheIdentity::Jmap {
                        email_state: Some(b),
                        ..
                    }),
                ) => a == b,
                _ => false,
            });
        let thread_messages = if same_generation && supplemental_compatible && page.has_more {
            previous
                .as_ref()
                .map(|cached| {
                    cached
                        .thread_messages
                        .iter()
                        .filter(|m| !page.messages.iter().any(|row| row.id == m.id))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let live = page
            .messages
            .iter()
            .map(|message| &message.id)
            .collect::<HashSet<_>>();
        let (bodies, message_index, message_view_offset) = match previous {
            Some(cached) if same_generation => (
                cached
                    .bodies
                    .into_iter()
                    .filter(|(id, _)| live.contains(id))
                    .collect(),
                cached.message_index,
                cached.message_view_offset,
            ),
            _ => (HashMap::new(), 0, 0),
        };
        let cached = CachedMailbox {
            thread_messages,
            messages: page.messages,
            bodies,
            message_index,
            message_view_offset,
            next_page_offset: page.next_offset,
            has_more: page.has_more,
            identity: identity.clone(),
            uid_next,
        };
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::Folder(
                name.clone(),
                cached.for_persistence(),
            ));
        }
        self.remember_mailbox(name.clone(), cached);
        if let Some(identity) = identity {
            self.cache_identities.insert(name.clone(), identity);
        } else {
            self.cache_identities.remove(&name);
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
        if self
            .mailbox_warm_request
            .as_ref()
            .is_some_and(|(_, pending)| pending == name)
        {
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
        if self
            .mailbox_warm_request
            .as_ref()
            .is_some_and(|(_, pending)| pending == name)
        {
            self.mailbox_warm_request = None;
        }
        self.mailbox_warm_queue.retain(|queued| queued != name);
        self.mailbox_cache.remove(name);
        self.warm_folder_order.retain(|entry| entry != name);
        self.remove_persistent_folder(name);
    }

    /// Invalidate a remote-changed folder's headers and schedule a new STATUS.
    /// Keep the last known count visible until the server supplies its replacement.
    pub(super) fn invalidate_remote_folder(&mut self, name: &str) {
        self.forget_cached_folder(name);
        self.queue_count_reconciliation(name);
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
        if let Some(identity) = cached.identity {
            self.cache_identities.insert(name.to_owned(), identity);
        } else {
            self.cache_identities.remove(name);
        }
        self.messages = cached.messages;
        let mut ids: HashSet<_> = self.messages.iter().map(|m| m.id.clone()).collect();
        let extra: Vec<_> = cached
            .thread_messages
            .into_iter()
            .filter(|m| ids.insert(m.id.clone()))
            .take(cache::MAX_MESSAGES_PER_FOLDER)
            .collect();
        let extra_ids = extra.iter().map(|m| m.id.clone()).collect();
        self.messages.extend(extra);
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
        self.native_search_editing = false;
        self.native_view = None;
        self.local_thread_view = None;
        self.reset_inline_threads();
        self.inline_thread_extra = extra_ids;
        *self.local_thread_index.get_mut() = Default::default();
        self.clamp_selection();
        self.overlay_deferred_seen();
        self.overlay_deferred_mutations();
        self.normalize_search_selection();
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
