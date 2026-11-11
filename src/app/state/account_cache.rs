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
        cached.bodies =
            cache::persistable_bodies_near(&cached.messages, &cached.bodies, cached.message_index);
        self.mailbox_cache.insert(name.clone(), cached);
        self.warm_folder_order.retain(|entry| entry != &name);
        self.warm_folder_order.push_back(name);
        while self.warm_folder_order.len() > MAX_WARM_MAILBOXES {
            if let Some(old) = self.warm_folder_order.pop_front() {
                self.mailbox_cache.remove(&old);
            }
        }
    }

    /// One bounded representation for both in-session and on-disk snapshots.
    /// Keeping these paths identical avoids restoring different cursors or
    /// bodies depending on whether Jerc was restarted between folder visits.
    pub(super) fn current_cache_snapshot(&self) -> Option<CachedMailbox> {
        let name = self.current_mailbox.as_deref()?;
        let max_messages = cache::MAX_MESSAGES_PER_FOLDER;
        let truncated = self.messages.len() > max_messages;
        let retained = self.messages.len().min(max_messages);
        Some(CachedMailbox {
            messages: self.messages.iter().take(max_messages).cloned().collect(),
            bodies: cache::persistable_bodies_near(
                &self.messages,
                &self.bodies,
                self.message_index,
            ),
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
        if let Some(snapshot) = self.current_cache_snapshot() {
            writer.send(cache::Change::Folder(name.to_owned(), snapshot));
        }
    }

    /// Invalidate the full derived folder snapshot (in-memory, LRU, disk).
    /// Call this after a mutation that changes a folder whose new UIDs are
    /// unknown. It deliberately does not clear the active mailbox: callers
    /// may still have a valid, optimistically updated source view.
    pub(super) fn forget_cached_folder(&mut self, name: &str) {
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
