//! Per-account connection, mailbox selection and preview/prefetch behavior.
use super::*;

impl AccountState {
    /// Only a single foreground reconnect may be queued at a time. The
    /// request ID keeps unrelated worker failures from cancelling it.
    pub(super) fn request_connect(&mut self) -> bool {
        if self.connecting {
            return false;
        }
        let Ok(id) = self.worker.post(ActionKind::Connect) else {
            return false;
        };
        // A manual refresh can supersede an older mailbox LIST. Its eventual
        // response must not block discovery on the new connection.
        self.abandon_mailbox_list();
        self.inbox_notifications.reset();
        self.connecting = true;
        self.connect_request_id = Some(id);
        self.sync_progress_at = Some(Instant::now());
        true
    }

    /// Start mailbox discovery only if the worker accepted the action.
    /// Unlike an untracked boolean this also handles LIST failures and stale
    /// completions from a previous connection generation.
    pub(super) fn request_mailbox_list(&mut self) -> bool {
        if self.mailboxes_loading {
            return false;
        }
        let Ok(id) = self.worker.post(ActionKind::ListMailboxes) else {
            return false;
        };
        self.mailbox_list_request_id = Some(id);
        self.mailboxes_loading = true;
        self.sync_progress_at = Some(Instant::now());
        true
    }

    pub(super) fn abandon_mailbox_list(&mut self) {
        if let Some(id) = self.mailbox_list_request_id.take() {
            self.abandoned_list_requests.push_back(id);
            while self.abandoned_list_requests.len() > 64 {
                self.abandoned_list_requests.pop_front();
            }
        }
        self.mailboxes_loading = false;
        self.push_list_inflight = false;
    }

    pub(super) fn is_abandoned_list_request(&mut self, id: u64) -> bool {
        if let Some(index) = self
            .abandoned_list_requests
            .iter()
            .position(|old| *old == id)
        {
            self.abandoned_list_requests.remove(index);
            return true;
        }
        false
    }

    /// The foreground transport is no longer trustworthy. Read-only request
    /// IDs are abandoned (late responses must not overwrite cached mail), but
    /// durable Outbox/mutation journals and their in-flight IDs are retained:
    /// an ambiguous SMTP/IMAP write must never be silently retried.
    pub(super) fn mark_disconnected(&mut self) {
        if !self.offline_incident_active {
            self.offline_incident_number = self.offline_incident_number.wrapping_add(1);
            self.offline_incident_active = true;
        }
        // Preserve the offline identity of any work that was already queued
        // when a live connection failed; keep its notice through replay.
        for entry in &self.deferred_mutations {
            if entry.state == cache::DeferredMutationState::Queued {
                self.offline_notice_mutations
                    .insert(mutation_notice_key(entry));
            }
        }
        for entry in &self.outbox {
            if entry.state == crate::outgoing::outbox::SendState::Queued {
                self.offline_notice_sends.insert(entry.id.clone());
            }
        }
        self.capabilities = None;
        self.inline_thread_requests.clear();
        if self.native_view.is_some() {
            self.restore_thread_fallback();
        }
        self.sync_progress_at = None;
        self.abandon_mailbox_list();
        self.pending_account_refresh = false;
        self.page_request_id = None;
        self.page_inflight = None;
        self.messages_loading = false;
        self.body_request_id = None;
        self.pending_bodies.clear();
        self.status_requests.reset_connection();
        self.inbox_notifications.reset();
        self.unverified_mailboxes
            .extend(self.cache_identities.keys().cloned());
    }

    /// Return to the canonical cache when a temporary server view cannot
    /// continue. The local projection never becomes a cache snapshot itself.
    pub(super) fn restore_thread_fallback(&mut self) {
        let mut fallback = self.local_thread_view.clone();
        let native_anchor = match &fallback {
            Some(crate::threading::LocalThreadView::Conversation(id)) => self
                .messages
                .iter()
                .find(|m| &m.id == id)
                .and_then(|m| m.thread_id.clone()),
            _ => None,
        };
        if let Some(name) = self.current_mailbox.clone()
            && !self.restore_cached_mailbox(&name)
        {
            self.messages.clear();
            self.bodies.clear();
            self.has_more = false;
            self.next_page_offset = 0;
            self.message_index = 0;
            self.message_view_offset = 0;
        }
        if let Some(crate::threading::LocalThreadView::Conversation(seed)) = &fallback
            && !self.messages.iter().any(|m| &m.id == seed)
            && let Some(native) = native_anchor
            && let Some(message) = self
                .messages
                .iter()
                .find(|m| m.thread_id.as_deref() == Some(native.as_str()))
        {
            fallback = Some(crate::threading::LocalThreadView::Conversation(
                message.id.clone(),
            ));
        }
        self.native_view = None;
        self.local_thread_view = fallback;
        self.search_query.clear();
        self.search_editing = false;
        self.native_search_editing = false;
        self.normalize_search_selection();
    }

    #[must_use]
    pub fn selected_mailbox(&self) -> Option<&Mailbox> {
        self.mailboxes.get(self.mailbox_index)
    }

    #[must_use]
    pub fn cached_mailbox_unverified(&self) -> bool {
        self.current_mailbox
            .as_ref()
            .is_some_and(|name| self.unverified_mailboxes.contains(name))
    }

    /// Only foreground mailbox synchronization is shown as a progress spinner.
    /// Background STATUS polling and IDLE do not keep the spinner running.
    #[must_use]
    pub fn syncing_with_server(&self) -> bool {
        self.connecting
            || self.mailboxes_loading
            || self.messages_loading
            || self.page_inflight.is_some()
    }

    #[must_use]
    pub fn selected_message(&self) -> Option<&MessageSummary> {
        // Unverified cached rows are safe to READ. Only UID-bearing server
        // requests must wait for SELECT to verify UIDVALIDITY.
        if matches!(
            self.effective_local_thread_view(),
            Some(crate::threading::LocalThreadView::Conversation(_))
        ) && !self.visible_message_indices().contains(&self.message_index)
        {
            return None;
        }
        if matches!(
            self.effective_local_thread_view(),
            Some(crate::threading::LocalThreadView::Collapsed)
        ) {
            let mut index = self.local_thread_index.borrow_mut();
            let representative = index.representative(&self.messages, self.message_index)?;
            if self
                .inline_thread_extra
                .contains(&self.messages[representative].id)
                || (representative != self.message_index
                    && !index
                        .group_members(&self.messages, self.message_index)
                        .iter()
                        .any(|&i| self.expanded_threads.contains(&self.messages[i].id)))
            {
                return None;
            }
        }
        self.messages.get(self.message_index)
    }

    #[must_use]
    pub fn selected_body(&self) -> Option<&MessageBody> {
        self.selected_message()
            .and_then(|msg| self.bodies.get(&msg.id))
    }

    #[must_use]
    pub fn is_loading_body(&self) -> bool {
        self.selected_message()
            .is_some_and(|message| self.pending_bodies.contains(&message.id))
    }

    pub(super) fn clamp_selection(&mut self) {
        self.mailbox_index = self
            .mailbox_index
            .min(self.mailboxes.len().saturating_sub(1));
        self.keep_mailbox_cursor_visible();
        self.message_index = self
            .message_index
            .min(self.messages.len().saturating_sub(1));
    }

    pub(super) fn keep_mailbox_cursor_visible(&mut self) {
        self.mailbox_view_offset = message_view_offset(
            self.mailbox_view_offset,
            self.mailbox_index,
            self.mailboxes.len(),
            self.mailbox_view_height,
        );
    }

    /// Search sees the canonical rows, including replies hidden by grouping.
    /// A temporary server view or an active filter takes precedence.
    pub(super) fn effective_local_thread_view(&self) -> Option<crate::threading::LocalThreadView> {
        if self.native_view.is_some() || self.search_editing || !self.search_query.trim().is_empty()
        {
            return None;
        }
        self.local_thread_view.clone().or_else(|| {
            self.threading_enabled
                .then_some(crate::threading::LocalThreadView::Collapsed)
        })
    }

    pub(crate) fn collapsed_threads(&self) -> bool {
        matches!(
            &self.native_view,
            Some(crate::backend::MailboxView::Threads)
        ) || matches!(
            self.effective_local_thread_view(),
            Some(crate::threading::LocalThreadView::Collapsed)
        )
    }

    /// Counts refer to known/cached members, not a promise of completeness.
    #[cfg(test)]
    pub(crate) fn inline_thread_rows(&self) -> Vec<(usize, crate::threading::ThreadRow)> {
        self.inline_thread_rows_shared().as_ref().clone()
    }

    pub(crate) fn inline_thread_rows_shared(
        &self,
    ) -> Arc<Vec<(usize, crate::threading::ThreadRow)>> {
        if !matches!(
            self.effective_local_thread_view(),
            Some(crate::threading::LocalThreadView::Collapsed)
        ) {
            return Arc::default();
        }
        self.local_thread_index.borrow_mut().inline_rows(
            &self.messages,
            &self.expanded_threads,
            &self.inline_thread_extra,
        )
    }

    pub(super) fn inline_group_ids(&self) -> HashSet<MessageId> {
        self.local_thread_index
            .borrow_mut()
            .group_members(&self.messages, self.message_index)
            .iter()
            .map(|&i| self.messages[i].id.clone())
            .collect()
    }

    pub(crate) fn selected_inline_expanded(&self) -> bool {
        self.native_view.is_none()
            && self.collapsed_threads()
            && !self.expanded_threads.is_empty()
            && self
                .inline_group_ids()
                .iter()
                .any(|id| self.expanded_threads.contains(id))
    }

    pub(super) fn collapse_inline_thread(&mut self) -> bool {
        if !self.selected_inline_expanded() {
            return false;
        }
        let ids = self.inline_group_ids();
        self.expanded_threads.retain(|id| !ids.contains(id));
        self.inline_thread_requests
            .retain(|_, seed| !ids.contains(seed));
        self.last_prefetch_anchor = None;
        self.smooth_message_target = None;
        self.normalize_search_selection();
        true
    }

    pub(super) fn reset_inline_threads(&mut self) {
        self.expanded_threads.clear();
        self.inline_thread_extra.clear();
        self.inline_thread_requests.clear();
    }

    pub(super) fn remaining_for_pagination(&self, visible_remaining: usize) -> usize {
        if self.native_view.is_none() && self.collapsed_threads() {
            let representative = self
                .local_thread_index
                .borrow_mut()
                .representative(&self.messages, self.message_index)
                .unwrap_or(self.message_index);
            self.canonical_messages()
                .len()
                .saturating_sub(representative.saturating_add(1))
        } else {
            visible_remaining
        }
    }

    pub(super) fn canonical_messages(&self) -> &[MessageSummary] {
        // The regular page window always precedes supplemental thread rows.
        if self.inline_thread_extra.is_empty() {
            return &self.messages;
        }
        let end = self
            .messages
            .iter()
            .position(|m| self.inline_thread_extra.contains(&m.id))
            .unwrap_or(self.messages.len());
        &self.messages[..end]
    }

    #[must_use]
    pub fn visible_message_indices(&self) -> Vec<usize> {
        Arc::try_unwrap(self.visible_message_indices_shared())
            .unwrap_or_else(|rows| rows.as_ref().clone())
    }

    /// A snapshot of cached search/thread rows, without copying them on redraw.
    pub(crate) fn visible_message_indices_shared(&self) -> Arc<Vec<usize>> {
        if self.search_query.trim().is_empty() {
            let mut cache = self.local_search_index.borrow_mut();
            if !cache.query.is_empty() {
                *cache = LocalSearchIndex::default();
            }
        }
        // Cached rows stay navigable while IMAP reconciliation runs. They are
        // provisional, so all server-facing UID actions check verification.
        if self.native_view.is_some() || self.native_search_editing {
            return Arc::new((0..self.messages.len()).collect());
        }
        if let Some(view) = self.effective_local_thread_view() {
            if matches!(view, crate::threading::LocalThreadView::Collapsed) {
                return self.local_thread_index.borrow_mut().inline_indices(
                    &self.messages,
                    &self.expanded_threads,
                    &self.inline_thread_extra,
                );
            }
            return Arc::new(
                self.local_thread_index
                    .borrow_mut()
                    .visible_indices(&self.messages, &view),
            );
        }
        let query = self.search_query.trim().to_lowercase();
        if query.is_empty() {
            return Arc::new((0..self.canonical_messages().len()).collect());
        }
        // Explicit body search examines already-loaded readable text only;
        // do not fetch bodies merely because a local filter is being typed.
        if let Some(query) = query.strip_prefix("body:").filter(|q| !q.is_empty()) {
            return Arc::new(
                self.messages
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| {
                        let body = self.bodies.get(&m.id)?;
                        [
                            body.plain_text.as_deref(),
                            body.plain_fallback.as_deref(),
                            body.html_text.as_deref(),
                        ]
                        .into_iter()
                        .flatten()
                        .any(|text| text.to_lowercase().contains(query))
                        .then_some(i)
                    })
                    .collect(),
            );
        }
        self.local_search_index
            .borrow_mut()
            .matches(&self.messages, &query, self.search_mode)
    }

    #[must_use]
    pub fn visible_message_position(&self) -> Option<usize> {
        self.visible_message_indices_shared()
            .iter()
            .position(|index| *index == self.message_index)
    }

    pub(super) fn normalize_search_selection(&mut self) {
        let visible = self.visible_message_indices_shared();
        if visible.is_empty() {
            self.message_index = 0;
            self.message_view_offset = 0;
            return;
        }
        if !visible.contains(&self.message_index) {
            // A reply that becomes hidden when grouping resumes should select
            // its own representative, not jump to an unrelated conversation.
            let representative = if self.native_view.is_none() && self.collapsed_threads() {
                self.local_thread_index
                    .borrow_mut()
                    .representative(&self.messages, self.message_index)
            } else {
                None
            };
            self.message_index = representative
                .filter(|index| visible.contains(index))
                .unwrap_or(visible[0]);
        }
        self.keep_message_cursor_visible();
    }

    pub(super) fn keep_message_cursor_visible(&mut self) {
        let visible = self.visible_message_indices_shared();
        let Some(position) = visible
            .iter()
            .position(|index| *index == self.message_index)
        else {
            self.message_view_offset = 0;
            return;
        };
        self.message_view_offset = message_view_offset(
            self.message_view_offset,
            position,
            visible.len(),
            self.message_view_height,
        );
    }

    pub(super) fn imap_uid_validity(&self, name: &str) -> Option<u32> {
        self.cache_identities
            .get(name)
            .and_then(CacheIdentity::uid_validity)
    }

    pub(super) fn raw_cache_key(
        &self,
        mailbox: &str,
        id: &MessageId,
    ) -> Option<(PathBuf, CacheIdentity)> {
        let identity = self.cache_identities.get(mailbox)?;
        if !identity.accepts(id) {
            return None;
        }
        Some((self.raw_store.as_ref()?.dir.clone(), identity.generation()))
    }

    pub(super) fn invalidate_sync_checkpoint(&mut self, name: &str) {
        if let Some(identity) = self.cache_identities.get_mut(name) {
            identity.invalidate_checkpoint();
        }
    }

    pub(super) fn request_mailbox_page(&mut self, name: String, offset: usize) -> bool {
        // No SELECT/UID fetch against a disconnected or reconnecting session.
        // Cached rows remain readable; replay waits for UIDVALIDITY validation.
        if self.capabilities.is_none() || self.connecting {
            return false;
        }
        if self
            .page_inflight
            .as_ref()
            .is_some_and(|(pending_name, pending_offset)| {
                pending_name == &name && *pending_offset == offset
            })
        {
            return false;
        }
        let pending_name = name.clone();
        if let Some(view) = self.native_view.clone()
            && self.current_mailbox.as_deref() == Some(name.as_str())
        {
            if let Ok(request_id) = self.worker.post(ActionKind::QueryMailbox {
                cached_ids: self
                    .mailbox_cache
                    .get(&name)
                    .map(|cached| {
                        cached
                            .messages
                            .iter()
                            .take(cache::MAX_MESSAGES_PER_FOLDER)
                            .map(|m| m.id.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                name,
                view,
                offset,
                limit: self.page_size,
            }) {
                self.page_inflight = Some((pending_name, offset));
                self.page_request_id = Some(request_id);
                self.messages_loading = offset == 0;
                self.sync_progress_at = Some(Instant::now());
                return true;
            }
            return false;
        }
        // Give the worker only cached headers (never MIME bodies). It must
        // validate UIDVALIDITY with the server before reusing even one row.
        let hint = if offset != 0 {
            None
        } else if self.current_mailbox.as_deref() == Some(name.as_str()) {
            // The active folder is no longer duplicated in mailbox_cache.
            // Use its most recent live headers for IMAP incremental sync.
            self.cache_identities
                .get(&name)
                .cloned()
                .map(|identity| MailboxSyncHint {
                    identity,
                    uid_next: self.uid_nexts.get(&name).copied(),
                    messages: self
                        .canonical_messages()
                        .iter()
                        .take(cache::MAX_MESSAGES_PER_FOLDER)
                        .cloned()
                        .collect(),
                })
        } else {
            self.mailbox_cache.get(&name).and_then(|folder| {
                folder.identity.clone().map(|identity| MailboxSyncHint {
                    identity,
                    uid_next: folder.uid_next,
                    messages: folder.messages.clone(),
                })
            })
        };
        if let Ok(request_id) = self.worker.post(ActionKind::OpenMailbox {
            name,
            offset,
            limit: self.page_size,
            hint,
        }) {
            self.page_inflight = Some((pending_name, offset));
            self.page_request_id = Some(request_id);
            self.messages_loading = offset == 0;
            self.sync_progress_at = Some(Instant::now());
            crate::perf::record_request(
                "ui_mailbox_request",
                request_id,
                None,
                Duration::ZERO,
                true,
            );
            true
        } else {
            false
        }
    }

    pub(super) fn request_next_page(&mut self) -> bool {
        if !self.has_more
            || self.page_inflight.is_some()
            || (self.native_view.is_none()
                && self
                    .search_query
                    .trim()
                    .to_ascii_lowercase()
                    .starts_with("body:"))
        {
            return false;
        }
        let Some(name) = self.current_mailbox.clone() else {
            return false;
        };
        self.request_mailbox_page(name, self.next_page_offset)
    }

    pub(super) fn request_selected_body(&mut self) {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return;
        };
        let Some(id) = self.selected_message().map(|m| m.id.clone()) else {
            return;
        };
        if self.bodies.contains_key(&id) {
            crate::perf::record("ui_preview_cache_hit", None, Duration::ZERO, true);
            return;
        }
        // Reading offline is safe before SELECT; only UID mutations need a
        // verified server generation. Do not start a network fetch on a hit.
        if let (Some(store), Some(validity)) = (
            self.body_store.as_ref(),
            self.cache_identities.get(&mailbox),
        ) && let Some(body) = store.read(&mailbox, validity, &id)
        {
            self.bodies.insert(id, body);
            self.trim_preview_bodies();
            crate::perf::record("ui_preview_disk_hit", None, Duration::ZERO, true);
            return;
        }
        if self.pending_bodies.contains(&id) || self.unverified_mailboxes.contains(&mailbox) {
            return;
        }
        if let Ok(request_id) = self.worker.post(ActionKind::FetchBody {
            mailbox,
            id: id.clone(),
            html_width: self.reader_html_width.filter(|_| self.prefer_html),
        }) {
            crate::perf::record("ui_preview_cache_miss", None, Duration::ZERO, true);
            self.body_request_id = Some(request_id);
            // Only the newest requested reader body matters. The worker coalesces
            // queued body fetches to the newest opened selection; mirror that here so
            // discarded requests cannot leave a permanent "loading" marker.
            self.pending_bodies.clear();
            self.pending_bodies.insert(id);
        }
    }

    /// Queue only nearby, missing preview bodies. A superseding window is
    /// coalesced by the background worker; the foreground always wins.
    pub(super) fn prefetch_near_cursor(&mut self, direction: isize) {
        let visible = self.visible_message_indices_shared();
        let Some(position) = visible
            .iter()
            .position(|index| *index == self.message_index)
        else {
            return;
        };
        self.prefetch_around_position(position, direction);
    }

    pub(super) fn prefetch_around_position(&mut self, position: usize, direction: isize) {
        self.prefetch_window(position, direction, None);
    }

    pub(super) fn prefetch_for_scroll(&mut self, position: usize, destination: usize) {
        if position == destination {
            return;
        }
        self.prefetch_window(
            position,
            if destination < position { -1 } else { 1 },
            Some(destination),
        );
    }

    pub(super) fn prefetch_window(
        &mut self,
        position: usize,
        direction: isize,
        destination: Option<usize>,
    ) {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return;
        };
        if self.unverified_mailboxes.contains(&mailbox) {
            return;
        }
        let visible = self.visible_message_indices_shared();
        if position >= visible.len() {
            return;
        }
        let direction = if direction < 0 { -1 } else { 1 };
        let destination = destination.filter(|target| *target < visible.len());
        let anchor = destination.unwrap_or(position);
        // Keep the existing window while it covers the moving cursor; a new
        // window every animation frame would cancel almost every useful fetch.
        if self.last_prefetch_window.is_some()
            && self
                .last_prefetch_anchor
                .as_ref()
                .is_some_and(|(name, last, dir)| {
                    name == &mailbox
                        && *dir == direction
                        && last.abs_diff(anchor) < PREFETCH_WINDOW_REARM_ROWS
                })
        {
            return;
        }
        let positions = if let Some(target) = destination {
            preview_travel_positions(position, target, visible.len())
        } else {
            std::iter::once(position)
                .chain(preview_neighbor_positions(
                    position,
                    visible.len(),
                    direction,
                    false,
                ))
                .collect::<Vec<_>>()
        };
        let mut raw_bytes = 0u64;
        let ids = positions
            .into_iter()
            .filter_map(|position| self.messages.get(visible[position]))
            .filter(|message| message.size > 0 && message.size <= MAX_PREFETCH_MESSAGE_BYTES)
            .filter(|message| {
                !self.bodies.contains_key(&message.id) && !self.pending_bodies.contains(&message.id)
            })
            .filter(|message| {
                // Disk-cached messages are loaded when selected. Preloading
                // them again over IMAP wastes bandwidth and server latency.
                !self.body_store.as_ref().is_some_and(|store| {
                    self.cache_identities.get(&mailbox).is_some_and(|identity| {
                        store.has_candidate(&mailbox, identity, &message.id)
                    })
                })
            })
            .filter(|message| {
                if raw_bytes.saturating_add(message.size) > MAX_PREFETCH_WINDOW_RAW_BYTES {
                    false
                } else {
                    raw_bytes += message.size;
                    true
                }
            })
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            self.last_prefetch_window = None;
            self.last_prefetch_anchor = None;
            return;
        }
        let window = (mailbox.clone(), ids.clone());
        if self.last_prefetch_window.as_ref() == Some(&window) {
            return;
        }
        if self
            .worker
            .post(ActionKind::PrefetchBodies {
                mailbox: mailbox.clone(),
                ids,
                html_width: self.reader_html_width.filter(|_| self.prefer_html),
            })
            .is_ok()
        {
            crate::perf::record("ui_prefetch_window_submitted", None, Duration::ZERO, true);
            self.last_prefetch_window = Some(window);
            self.last_prefetch_anchor = Some((mailbox, anchor, direction));
        }
    }

    /// Bound preloading by both message count and actual decoded-text size.
    /// A large HTML newsletter may have much more resident data than its raw
    /// RFC822 size suggests. Keep the current message even if it exceeds budget.
    pub(super) fn trim_preview_bodies(&mut self) {
        let bytes = self.bodies.values().map(preview_body_bytes).sum::<usize>();
        if self.bodies.len() <= MAX_CACHED_PREVIEW_BODIES && bytes <= MAX_CACHED_PREVIEW_BYTES {
            return;
        }
        let mut ranked = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| self.bodies.contains_key(&message.id))
            .map(|(position, message)| (position.abs_diff(self.message_index), message.id.clone()))
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(distance, _)| *distance);
        let mut used = 0usize;
        let mut keep = HashSet::new();
        for (_, id) in ranked {
            let Some(body) = self.bodies.get(&id) else {
                continue;
            };
            let weight = preview_body_bytes(body);
            if keep.len() >= MAX_CACHED_PREVIEW_BODIES
                || (!keep.is_empty() && used.saturating_add(weight) > MAX_CACHED_PREVIEW_BYTES)
            {
                continue;
            }
            used = used.saturating_add(weight);
            keep.insert(id);
        }
        self.bodies.retain(|id, _| keep.contains(id));
    }

    /// Record a read/unread preference immediately; the server is reconciled
    /// later, but only after SELECT confirms UIDVALIDITY and the UID is still
    /// present. The on-disk journal is coalesced and bounded.
    pub(super) fn request_seen(&mut self, id: MessageId, seen: bool) -> bool {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return false;
        };
        let verified = !self.unverified_mailboxes.contains(&mailbox);
        let validity = self.imap_uid_validity(&mailbox);
        if !self.messages.iter().any(|message| message.id == id) {
            return false;
        }
        // Maildir and IMAP accounts without persistent caching retain the
        // original immediate-worker behavior. Never send an unverified UID.
        if validity.is_none() && verified {
            if self.pending_seen.contains_key(&id) {
                return false;
            }
            if let Ok(request_id) = self.worker.post(ActionKind::SetSeen {
                mailbox,
                id: id.clone(),
                seen,
            }) {
                self.pending_seen.insert(id.clone(), request_id);
                self.apply_seen_flag(&id, seen);
                return true;
            }
            return false;
        }
        let Some(validity) = validity else {
            return false;
        };
        let entry = cache::DeferredSeen {
            mailbox,
            validity,
            id: id.clone(),
            seen,
        };
        if !entry.has_valid_identity() {
            return false;
        }
        if let Some(previous) = self
            .deferred_seen
            .iter_mut()
            .find(|previous| previous.mailbox == entry.mailbox && previous.id == entry.id)
        {
            previous.seen = seen;
        } else {
            if self.deferred_seen.len() >= cache::MAX_DEFERRED_SEEN {
                return false;
            }
            self.deferred_seen.push(entry);
        }
        self.apply_seen_flag(&id, seen);
        self.persist_deferred_seen();
        // Offline flag changes are stored in the journal, but the cached
        // mailbox count stays at its last server-derived value. Persisting an
        // optimistic count would double-apply this change on next startup.
        self.persist_current_cache();
        if verified {
            self.persist_mailboxes();
        }
        self.pump_deferred_seen();
        true
    }

    pub(super) fn persist_deferred_seen(&self) {
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::DeferredSeen(self.deferred_seen.clone()));
        }
    }

    /// Reapply local preferences after a fresh server FLAGS snapshot (or
    /// restoring a cached mailbox). This does not issue network operations.
    pub(super) fn overlay_deferred_seen(&mut self) {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return;
        };
        let Some(validity) = self.imap_uid_validity(&mailbox) else {
            return;
        };
        let updates = self
            .deferred_seen
            .iter()
            .filter(|entry| entry.mailbox == mailbox && entry.validity == validity)
            .map(|entry| (entry.id.clone(), entry.seen))
            .collect::<Vec<_>>();
        for (id, seen) in updates {
            self.apply_seen_flag(&id, seen);
        }
    }

    /// Replay only for a confirmed generation and UIDs present in the loaded
    /// server page. Missing UIDs remain pending for later pages; changed
    /// generations are discarded rather than risking a write to another mail.
    pub(super) fn pump_deferred_seen(&mut self) {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return;
        };
        if self.unverified_mailboxes.contains(&mailbox) {
            return;
        }
        let Some(validity) = self.imap_uid_validity(&mailbox) else {
            return;
        };
        let ready =
            self.deferred_seen
                .iter()
                .filter(|entry| entry.mailbox == mailbox && entry.validity == validity)
                .filter(|entry| self.messages.iter().any(|message| message.id == entry.id))
                .filter(|entry| !self.pending_seen.contains_key(&entry.id))
                .filter(|entry| {
                    !self.inflight_deferred_seen.values().any(|inflight| {
                        inflight.mailbox == entry.mailbox && inflight.id == entry.id
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
        for entry in ready {
            if let Ok(request_id) = self.worker.post(ActionKind::SetSeen {
                mailbox: entry.mailbox.clone(),
                id: entry.id.clone(),
                seen: entry.seen,
            }) {
                self.pending_seen.insert(entry.id.clone(), request_id);
                self.inflight_deferred_seen.insert(request_id, entry);
            } else {
                break;
            }
        }
    }
}
