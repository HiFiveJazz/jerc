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
        cached.message_index = cached.message_index.min(cached.messages.len().saturating_sub(1));
        cached.bodies = cache::persistable_bodies_near(
            &cached.messages,
            &cached.bodies,
            cached.message_index,
        );
        self.mailbox_cache.insert(name.clone(), cached);
        self.warm_folder_order.retain(|entry| entry != &name);
        self.warm_folder_order.push_back(name);
        while self.warm_folder_order.len() > MAX_WARM_MAILBOXES {
            if let Some(old) = self.warm_folder_order.pop_front() {
                self.mailbox_cache.remove(&old);
            }
        }
    }

    pub(super) fn save_current_cache(&mut self) {
        let Some(name) = self.current_mailbox.clone() else {
            return;
        };
        let uid_validity = self.uid_validities.get(&name).copied();
        let uid_next = self.uid_nexts.get(&name).copied();
        let truncated = self.messages.len() > cache::MAX_MESSAGES_PER_FOLDER;
        self.remember_mailbox(
            name,
            CachedMailbox {
                messages: self.messages.iter().take(cache::MAX_MESSAGES_PER_FOLDER).cloned().collect(),
                bodies: cache::persistable_bodies_near(&self.messages, &self.bodies, self.message_index),
                message_index: self.message_index.min(cache::MAX_MESSAGES_PER_FOLDER.saturating_sub(1)),
                message_view_offset: self.message_view_offset,
                next_page_offset: if truncated { cache::MAX_MESSAGES_PER_FOLDER } else { self.next_page_offset },
                has_more: truncated || self.has_more,
                uid_validity,
                uid_next,
            },
        );
    }

    pub(super) fn persist_mailboxes(&self) {
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::Mailboxes(self.mailboxes.clone()));
        }
    }

    pub(super) fn persist_current_cache(&mut self) {
        let Some(name) = self.current_mailbox.as_ref() else {
            return;
        };
        let Some(writer) = &self.cache_writer else {
            return;
        };
        // Don't clone 80 warm preview bodies into the in-session cache and
        // then clone them *again* for a disk writer that can only keep 12.
        // In-session snapshots are captured separately when changing folders.
        let Some(uid_validity) = self.uid_validities.get(name).copied() else {
            return;
        };
        if self.unverified_mailboxes.contains(name) {
            return;
        }
        let truncated = self.messages.len() > cache::MAX_MESSAGES_PER_FOLDER;
        writer.send(cache::Change::Folder(
            name.clone(),
            CachedMailbox {
                messages: self
                    .messages
                    .iter()
                    .take(cache::MAX_MESSAGES_PER_FOLDER)
                    .cloned()
                    .collect(),
                bodies: cache::persistable_bodies_near(&self.messages, &self.bodies, self.message_index),
                message_index: self.message_index,
                message_view_offset: self.message_view_offset,
                next_page_offset: if truncated {
                    cache::MAX_MESSAGES_PER_FOLDER
                } else {
                    self.next_page_offset
                },
                has_more: truncated || self.has_more,
                uid_validity: Some(uid_validity),
                uid_next: self.uid_nexts.get(name).copied(),
            },
        ));
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
        // This precedes the network refresh. It identifies the moment cached
        // rows become available to the UI, not the terminal's paint latency.
        crate::perf::record("ui_cache_restore", None, Duration::ZERO, true);
        true
    }

    pub(super) fn invalidate_current_cache(&mut self) {
        if let Some(name) = self.current_mailbox.as_ref() {
            self.mailbox_cache.remove(name);
            self.warm_folder_order.retain(|entry| entry != name);
            self.remove_persistent_folder(name);
        }
    }

}
