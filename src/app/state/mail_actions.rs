//! Mail actions behavior.

use super::*;

impl AccountState {
    /// Apply one Seen state transition exactly once, including the cached
    /// mailbox unread count. Used by both optimistic UI and worker ACKs:
    /// applying the same flag a second time must not double-adjust counts.
    pub(super) fn apply_seen_flag(&mut self, id: &MessageId, seen: bool) {
        let mut unread_delta = 0isize;
        if let Some(message) = self.messages.iter_mut().find(|message| &message.id == id) {
            let was_unread = message.unread();
            message.flags.set(MessageFlags::SEEN, seen);
            unread_delta = match (was_unread, message.unread()) {
                (true, false) => -1,
                (false, true) => 1,
                _ => 0,
            };
        }
        if unread_delta != 0
            && let Some(current) = self.current_mailbox.as_deref()
            && let Some(mailbox) = self
                .mailboxes
                .iter_mut()
                .find(|mailbox| mailbox.name == current && mailbox.counts_known)
        {
            mailbox.unread = mailbox.unread.saturating_add_signed(unread_delta);
        }
    }
}

impl AccountState {
    /// Commit before changing any visible state. If the journal cannot be
    /// written, the UI action must fail without removing cached mail.
    fn enqueue_mutation(&mut self, entry: cache::DeferredMutation) -> std::io::Result<()> {
        if !entry.has_valid_identity()
            || self.deferred_mutations.len() >= cache::MAX_DEFERRED_MUTATIONS
        {
            return Err(std::io::Error::other(
                "invalid mailbox generation or offline queue full",
            ));
        }
        if self.deferred_mutations.iter().any(|existing| {
            existing.mailbox == entry.mailbox
                && existing.ids.iter().any(|id| entry.ids.contains(id))
        }) {
            return Err(std::io::Error::other(
                "a mailbox change is already queued for this message",
            ));
        }
        let mut next = self.deferred_mutations.clone();
        next.push(entry);
        let writer = self.cache_writer.as_ref().ok_or_else(|| {
            std::io::Error::other("offline mutation queue is unavailable for this account")
        })?;
        writer.save_mutations(&next)?;
        self.deferred_mutations = next;
        Ok(())
    }

    fn save_deferred_mutations(&self) -> std::io::Result<()> {
        self.cache_writer
            .as_ref()
            .ok_or_else(|| std::io::Error::other("offline journal is unavailable"))?
            .save_mutations(&self.deferred_mutations)
    }

    /// Hide locally queued moves/deletes on both cached restoration and a
    /// later server-page refresh, until we receive a definite IMAP ACK.
    pub(super) fn overlay_deferred_mutations(&mut self) {
        let Some(mailbox) = self.current_mailbox.as_deref() else {
            return;
        };
        let Some(validity) = self.uid_validities.get(mailbox).copied() else {
            return;
        };
        let hidden = self
            .deferred_mutations
            .iter()
            .filter(|entry| entry.mailbox == mailbox && entry.validity == validity)
            .filter(|entry| {
                !matches!(&entry.kind, cache::DeferredMutationKind::Unarchive { .. })
                    || !(mailbox.to_ascii_lowercase().starts_with("[gmail]/")
                        || mailbox.to_ascii_lowercase().starts_with("[googlemail]/"))
            })
            .flat_map(|entry| entry.ids.iter())
            .collect::<HashSet<_>>();
        if hidden.is_empty() {
            return;
        }
        self.messages
            .retain(|message| !hidden.contains(&message.id));
        for id in hidden {
            self.bodies.remove(id);
            self.pending_bodies.remove(id);
            self.pending_seen.remove(id);
        }
        self.message_index = self
            .message_index
            .min(self.messages.len().saturating_sub(1));
        self.keep_message_cursor_visible();
    }

    /// The IMAP backend SELECTs the named source and checks every embedded
    /// UIDVALIDITY *again*, immediately before executing each mutation. This
    /// permits replay for unopened mailboxes without trusting a cached UID.
    /// Persist NeedsReview before posting: a crash after SMTP-like ambiguous
    /// network success must not cause an unsafe automatic duplicate move.
    pub(super) fn pump_deferred_mutations(&mut self) {
        if self.capabilities.is_none() {
            return;
        }
        if !self.inflight_deferred_mutations.is_empty() {
            return;
        }
        let ready = self
            .deferred_mutations
            .iter()
            .filter(|entry| entry.state == cache::DeferredMutationState::Queued)
            .take(1)
            .cloned()
            .collect::<Vec<_>>();
        for entry in ready {
            if !entry.has_valid_identity() {
                continue;
            }
            if let Some(record) = self
                .deferred_mutations
                .iter_mut()
                .find(|record| record.mailbox == entry.mailbox && record.ids == entry.ids)
            {
                record.state = cache::DeferredMutationState::NeedsReview;
            } else {
                continue;
            }
            if self.save_deferred_mutations().is_err() {
                if let Some(record) = self
                    .deferred_mutations
                    .iter_mut()
                    .find(|record| record.mailbox == entry.mailbox && record.ids == entry.ids)
                {
                    record.state = cache::DeferredMutationState::Queued;
                }
                break;
            }
            let action = match &entry.kind {
                cache::DeferredMutationKind::Delete => ActionKind::DeleteMany {
                    mailbox: entry.mailbox.clone(),
                    ids: entry.ids.clone(),
                },
                cache::DeferredMutationKind::Move { destination } => ActionKind::MoveMany {
                    source: entry.mailbox.clone(),
                    destination: destination.clone(),
                    ids: entry.ids.clone(),
                },
                cache::DeferredMutationKind::Archive { destination } => ActionKind::ArchiveMany {
                    source: entry.mailbox.clone(),
                    destination: destination.clone(),
                    ids: entry.ids.clone(),
                },
                cache::DeferredMutationKind::Unarchive { destination } => {
                    ActionKind::UnarchiveMany {
                        source: entry.mailbox.clone(),
                        destination: destination.clone(),
                        ids: entry.ids.clone(),
                    }
                }
            };
            match self.worker.post(action) {
                Ok(request_id) => {
                    self.inflight_deferred_mutations.insert(request_id, entry);
                    self.pending_mutation_labels
                        .insert(request_id, "offline mailbox action synchronized");
                }
                Err(_) => {
                    if let Some(record) = self
                        .deferred_mutations
                        .iter_mut()
                        .find(|record| record.mailbox == entry.mailbox && record.ids == entry.ids)
                    {
                        record.state = cache::DeferredMutationState::Queued;
                    }
                    // Not submitted to a worker, so definitely safe to retry.
                    let _ = self.save_deferred_mutations();
                    break;
                }
            }
        }
    }

    pub(super) fn finish_deferred_mutation(&mut self, request_id: u64, succeeded: bool) -> bool {
        let Some(entry) = self.inflight_deferred_mutations.remove(&request_id) else {
            return false;
        };
        if succeeded {
            self.deferred_mutations
                .retain(|record| !(record.mailbox == entry.mailbox && record.ids == entry.ids));
            let destination = match &entry.kind {
                cache::DeferredMutationKind::Delete => None,
                cache::DeferredMutationKind::Move { destination }
                | cache::DeferredMutationKind::Archive { destination }
                | cache::DeferredMutationKind::Unarchive { destination } => {
                    Some(destination.as_str())
                }
            };
            if let Some(destination) = destination {
                self.invalidate_remote_folder(destination);
            }
        }
        // On failure NeedsReview stays on disk. Failure may be a timeout
        // after a successful COPY, and automatic retries could duplicate mail.
        let _ = self.save_deferred_mutations();
        true
    }
}

impl App {
    pub(super) fn toggle_seen(&mut self) {
        let Some(account) = self.account_mut() else {
            return;
        };
        let Some((id, seen)) = account.selected_message().map(|message| {
            (
                message.id.clone(),
                !message.flags.contains(MessageFlags::SEEN),
            )
        }) else {
            return;
        };
        let _ = account.request_seen(id, seen);
    }

    /// Space toggles the current message, then advances one visible row
    /// (without wrapping past the last message). In visual mode it toggles
    /// the whole range as a set and exits visual mode without moving.
    pub(super) fn toggle_message_mark(&mut self) {
        let visual_range = self.visual_range();
        let Some(account) = self.account() else {
            return;
        };
        let ids = if let Some((first, last)) = visual_range {
            account.visible_message_indices()[first..=last]
                .iter()
                .map(|&index| account.messages[index].id.clone())
                .collect::<Vec<_>>()
        } else {
            account
                .selected_message()
                .map(|m| vec![m.id.clone()])
                .unwrap_or_default()
        };
        if ids.is_empty() {
            return;
        }
        toggle_message_marks(&mut self.message_marked, &ids);
        self.visual_anchor = None;
        if visual_range.is_none() {
            // Reuse normal navigation for filtered order, viewport scroll,
            // preview updates, and pagination/prefetch near the list end.
            self.move_selection(1);
        }
    }

    pub(super) fn toggle_visual(&mut self) {
        if self.focus == Focus::Preview && self.message_open {
            if self.reader_line_count == 0 {
                return;
            }
            if self.reader_visual_anchor.is_some() {
                self.reader_visual_anchor = None;
                self.reader_visual_linewise = false;
            } else {
                self.reader_visual_linewise = false;
                self.reader_visual_anchor = Some(ReaderPosition {
                    line: self.reader_cursor,
                    column: self.reader_column,
                });
            }
            self.reader_pending_inner_object = false;
            return;
        }

        if self.message_open
            || self
                .account()
                .is_none_or(|account| account.messages.is_empty())
        {
            return;
        }
        self.visual_anchor = if self.visual_anchor.is_some() {
            None
        } else {
            self.account().map(|account| account.message_index)
        };
    }

    pub(super) fn toggle_reader_linewise_visual(&mut self) {
        if self.focus != Focus::Preview || !self.message_open || self.reader_line_count == 0 {
            return;
        }

        if self.reader_visual_anchor.is_some() && self.reader_visual_linewise {
            self.reader_visual_anchor = None;
            self.reader_visual_linewise = false;
        } else {
            let anchor_line = self
                .reader_visual_anchor
                .map_or(self.reader_cursor, |anchor| anchor.line);
            self.reader_visual_anchor = Some(ReaderPosition {
                line: anchor_line,
                column: 0,
            });
            self.reader_visual_linewise = true;
        }
        self.reader_pending_inner_object = false;
    }

    #[must_use]
    pub fn reader_visual_range(&self) -> Option<ReaderSelection> {
        let anchor = self.reader_visual_anchor?;
        if self.reader_visual_linewise {
            let (start_line, end_line) = if anchor.line <= self.reader_cursor {
                (anchor.line, self.reader_cursor)
            } else {
                (self.reader_cursor, anchor.line)
            };
            return Some(ReaderSelection {
                start: ReaderPosition {
                    line: start_line,
                    column: 0,
                },
                end: ReaderPosition {
                    line: end_line,
                    column: usize::MAX,
                },
            });
        }
        let cursor = ReaderPosition {
            line: self.reader_cursor,
            column: self.reader_column,
        };
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        Some(ReaderSelection { start, end })
    }

    #[must_use]
    pub fn visual_range(&self) -> Option<(usize, usize)> {
        let anchor_index = self.visual_anchor?;
        let account = self.account()?;
        let visible = account.visible_message_indices();
        let anchor = visible.iter().position(|index| *index == anchor_index)?;
        let current = visible
            .iter()
            .position(|index| *index == account.message_index)?;
        Some(if anchor <= current {
            (anchor, current)
        } else {
            (current, anchor)
        })
    }

    #[must_use]
    pub fn message_is_visual_selected(&self, index: usize) -> bool {
        let Some((start, end)) = self.visual_range() else {
            return false;
        };
        let Some(account) = self.account() else {
            return false;
        };
        account
            .visible_message_indices()
            .iter()
            .position(|candidate| *candidate == index)
            .is_some_and(|position| (start..=end).contains(&position))
    }

    pub(super) fn selected_message_ids(&self) -> Vec<MessageId> {
        let Some(account) = self.account() else {
            return Vec::new();
        };
        let visible_indices = account.visible_message_indices();
        let visible = visible_indices
            .iter()
            .map(|&index| account.messages[index].id.clone())
            .collect::<Vec<_>>();
        let all = account
            .messages
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>();
        let selected = visible_indices
            .iter()
            .position(|&index| index == account.message_index)
            .unwrap_or(0);
        let anchor = self.visual_anchor.and_then(|index| {
            visible_indices
                .iter()
                .position(|&candidate| candidate == index)
        });
        crate::app::vim::selected(&visible, &all, selected, anchor, &self.message_marked)
    }

    /// Snapshot visible names for confirmation. The preview must match the
    /// identities that will be sent to the worker even if the list changes.
    pub(super) fn message_review_items(&self, ids: &[MessageId]) -> Vec<String> {
        let Some(account) = self.account() else {
            return Vec::new();
        };
        // Build one lookup instead of scanning a large mailbox for every mark.
        let by_id: HashMap<&MessageId, &MessageSummary> = account
            .messages
            .iter()
            .map(|message| (&message.id, message))
            .collect();
        ids.iter()
            .map(|id| {
                by_id.get(id).map_or_else(
                    || "(message no longer loaded)".to_owned(),
                    |message| {
                        let subject = if message.subject.trim().is_empty() {
                            "(no subject)"
                        } else {
                            message.subject.as_str()
                        };
                        format!("{subject}  ·  {}", message.from_address)
                    },
                )
            })
            .collect()
    }

    pub(super) fn delete_selected(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let items = self.message_review_items(&ids);
        let Some(account) = self.account() else {
            return;
        };
        let Some(source) = account.current_mailbox.clone() else {
            return;
        };
        let current_role = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.name == source)
            .map(|mailbox| mailbox.role.clone());
        let count = ids.len();

        if current_role == Some(MailboxRole::Trash) {
            self.confirmation = Some(MutationConfirmation {
                prompt: format!(
                    "Permanently delete {count} message{}? This cannot be undone.",
                    if count == 1 { "" } else { "s" }
                ),
                items,
                review: ReviewScroll::default(),
                source,
                ids,
                kind: MutationKind::Delete,
            });
            return;
        }

        if let Some(trash) = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == MailboxRole::Trash)
            .map(|mailbox| mailbox.name.clone())
        {
            self.confirmation = Some(MutationConfirmation {
                prompt: format!(
                    "Move {count} message{} to Trash?",
                    if count == 1 { "" } else { "s" }
                ),
                items,
                review: ReviewScroll::default(),
                source,
                ids,
                kind: MutationKind::Move { destination: trash },
            });
        } else {
            self.confirmation = Some(MutationConfirmation {
                prompt: format!(
                    "Delete {count} message{}? This server has no Trash mailbox.",
                    if count == 1 { "" } else { "s" }
                ),
                items,
                review: ReviewScroll::default(),
                source,
                ids,
                kind: MutationKind::Delete,
            });
        }
    }

    pub(super) fn delete_selected_immediate(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        let Some(source) = account.current_mailbox.clone() else {
            return;
        };
        let current_role = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.name == source)
            .map(|mailbox| mailbox.role.clone());
        let kind = if current_role == Some(MailboxRole::Trash) {
            MutationKind::Delete
        } else if let Some(trash) = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == MailboxRole::Trash)
            .map(|mailbox| mailbox.name.clone())
        {
            MutationKind::Move { destination: trash }
        } else {
            MutationKind::Delete
        };
        self.start_mutation(source, ids, kind);
    }

    /// Toggle spam by moving the selected message(s) to Junk, or back to Inbox.
    /// Use discovered mailbox roles rather than guessing Gmail or Outlook paths.
    pub(super) fn toggle_spam_selected(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        let Some(source) = account.current_mailbox.clone() else {
            return;
        };
        let in_junk = account
            .mailboxes
            .iter()
            .any(|mailbox| mailbox.name == source && mailbox.role == MailboxRole::Junk);
        let target_role = if in_junk {
            MailboxRole::Inbox
        } else {
            MailboxRole::Junk
        };
        let destination = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == target_role && mailbox.name != source)
            .map(|mailbox| mailbox.name.clone());
        let Some(destination) = destination else {
            self.status = StatusLine::error(if in_junk {
                "cannot mark as not spam: no Inbox mailbox found"
            } else {
                "cannot mark as spam: no Junk/Spam mailbox found"
            });
            return;
        };
        // Move is the existing IMAP worker operation, which already updates
        // the source list and invalidates the destination cache.
        self.start_mutation(source, ids, MutationKind::Move { destination });
        if !self.status.is_error {
            self.status = StatusLine::temporary(
                if in_junk {
                    "moved out of Spam to Inbox"
                } else {
                    "marked as Spam"
                },
                Duration::from_secs(2),
            );
        }
    }

    pub(super) fn unarchive_selected(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        let Some(source) = account.current_mailbox.clone() else {
            return;
        };
        if !account
            .mailboxes
            .iter()
            .any(|m| m.name == source && m.role == MailboxRole::Archive)
        {
            self.status = StatusLine::temporary(
                "open Archive / All Mail, then press U to restore to INBOX",
                Duration::from_secs(3),
            );
            return;
        }
        let Some(inbox) = account
            .mailboxes
            .iter()
            .find(|m| m.role == MailboxRole::Inbox && m.name != source)
            .map(|m| m.name.clone())
        else {
            self.status = StatusLine::error("no INBOX available to restore archived mail");
            return;
        };
        self.start_mutation(source, ids, MutationKind::Unarchive { destination: inbox });
    }

    pub(super) fn archive_selected(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        let Some(source) = account.current_mailbox.clone() else {
            return;
        };
        if account
            .mailboxes
            .iter()
            .any(|mailbox| mailbox.name == source && mailbox.role == MailboxRole::Archive)
        {
            self.status = StatusLine::temporary(
                "message is already in Archive / All Mail",
                Duration::from_secs(2),
            );
            return;
        }
        let destination = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == MailboxRole::Archive && mailbox.name != source)
            .map(|mailbox| mailbox.name.clone());
        let Some(destination) = destination else {
            self.status = StatusLine::temporary(
                "this account has no advertised Archive/All Mail mailbox",
                Duration::from_secs(3),
            );
            return;
        };
        // Gmail All Mail is virtual. The IMAP UID delete/expunge used to
        // remove INBOX membership depends on the account's deletion settings,
        // so require explicit confirmation instead of hiding the risk.
        if source.eq_ignore_ascii_case("INBOX")
            && (destination.to_ascii_lowercase().starts_with("[gmail]/")
                || destination
                    .to_ascii_lowercase()
                    .starts_with("[googlemail]/"))
        {
            let items = self.message_review_items(&ids);
            self.confirmation = Some(MutationConfirmation {
                items,
                review: ReviewScroll::default(),
                prompt: format!(
                    "Archive {} Gmail message(s)? This uses IMAP expunge; check Gmail deletion settings first. [y/N]",
                    ids.len()
                ),
                source,
                ids,
                kind: MutationKind::Archive { destination },
            });
        } else {
            self.start_mutation(source, ids, MutationKind::Archive { destination });
        }
    }

    pub(super) fn handle_remove_account_confirmation_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let Some(name) = self.remove_account_confirmation.take() else {
                    return;
                };
                match crate::setup::remove_account(&self.setup_config_path, &name) {
                    Ok(()) => {
                        self.status = StatusLine::info(format!(
                            "Removed {name:?} from local config; server mail untouched"
                        ));
                        self.setup_reload_requested = true;
                    }
                    Err(error) => {
                        self.status =
                            StatusLine::error(format!("Cannot remove account: {error:#}"));
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.remove_account_confirmation = None;
                self.status =
                    StatusLine::temporary("account removal cancelled", Duration::from_secs(2));
            }
            _ => {}
        }
    }

    pub(super) fn handle_detach_confirmation_key(&mut self, key: KeyEvent) {
        let Some(review) = self.detach_confirmation.as_mut() else {
            return;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('j') => review.review.queue(review.paths.len(), SMOOTH_SCROLL_ROWS),
                KeyCode::Char('k') => review.review.queue(review.paths.len(), -SMOOTH_SCROLL_ROWS),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => review.review.queue(review.paths.len(), 1),
            KeyCode::Char('k') | KeyCode::Up => review.review.queue(review.paths.len(), -1),
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                self.detach_confirmation = None;
            }
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                let Some(confirmation) = self.detach_confirmation.take() else {
                    return;
                };
                let paths: HashSet<PathBuf> = confirmation.paths.into_iter().collect();
                if let Some(composer) = self.composer.as_mut() {
                    // Use the existing detach operation so its undo stack
                    // continues to work for multi-selection as well.
                    let names = composer
                        .attachment_paths
                        .iter()
                        .cloned()
                        .chain(
                            composer
                                .attachments
                                .split(';')
                                .map(str::trim)
                                .filter(|part| !part.is_empty())
                                .map(PathBuf::from),
                        )
                        .collect::<Vec<_>>();
                    let original_selected = composer.attachment_manager_selected;
                    for index in (0..names.len()).rev() {
                        if paths.contains(&names[index]) {
                            composer.attachment_manager_selected = index;
                            let _ = composer.remove_managed_attachment();
                        }
                    }
                    composer.attachment_manager_visual_anchor = None;
                    composer.attachment_manager_marked.clear();
                    composer.attachment_manager_selected =
                        original_selected.min(composer.attachment_count().saturating_sub(1));
                    if let Some(picker) = composer.file_picker.as_mut() {
                        picker.visual_anchor = None;
                    }
                }
                self.status = StatusLine::temporary(
                    format!("detached {} file(s)", paths.len()),
                    Duration::from_secs(2),
                );
            }
            _ => {}
        }
    }

    pub(super) fn handle_confirmation_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(confirmation) = self.confirmation.as_mut() {
                match key.code {
                    KeyCode::Char('j') => confirmation
                        .review
                        .queue(confirmation.items.len(), SMOOTH_SCROLL_ROWS),
                    KeyCode::Char('k') => confirmation
                        .review
                        .queue(confirmation.items.len(), -SMOOTH_SCROLL_ROWS),
                    _ => {}
                }
            }
            return;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(confirmation) = self.confirmation.as_mut() {
                    confirmation.review.queue(confirmation.items.len(), 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(confirmation) = self.confirmation.as_mut() {
                    confirmation.review.queue(confirmation.items.len(), -1);
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let Some(confirmation) = self.confirmation.take() else {
                    return;
                };
                self.start_mutation(confirmation.source, confirmation.ids, confirmation.kind);
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('d') => {
                self.confirmation = None;
                self.status = StatusLine::temporary("delete cancelled", Duration::from_secs(2));
            }
            _ => {}
        }
    }

    pub(super) fn start_mutation(
        &mut self,
        source: String,
        ids: Vec<MessageId>,
        kind: MutationKind,
    ) {
        if ids.is_empty() {
            return;
        }
        // Store an IMAP mutation in a private, crash-durable journal BEFORE
        // optimistic removal. Replay validates the UID generation on SELECT.
        // Maildir and uncached accounts retain their direct worker path.
        let is_unverified = self
            .account()
            .is_some_and(|account| account.unverified_mailboxes.contains(&source));
        let validity = self.account().and_then(|account| {
            account.cache_writer.as_ref()?;
            account.uid_validities.get(&source).copied()
        });
        if let Some(validity) = validity {
            let journal_kind = match &kind {
                MutationKind::Delete => cache::DeferredMutationKind::Delete,
                MutationKind::Move { destination } => cache::DeferredMutationKind::Move {
                    destination: destination.clone(),
                },
                MutationKind::Archive { destination } => cache::DeferredMutationKind::Archive {
                    destination: destination.clone(),
                },
                MutationKind::Unarchive { destination } => cache::DeferredMutationKind::Unarchive {
                    destination: destination.clone(),
                },
            };
            let entry = cache::DeferredMutation {
                mailbox: source.clone(),
                validity,
                ids: ids.clone(),
                kind: journal_kind,
                state: cache::DeferredMutationState::Queued,
            };
            let result = self
                .account_mut()
                .expect("account exists")
                .enqueue_mutation(entry);
            if let Err(error) = result {
                self.status = StatusLine::error(format!("mailbox change not queued: {error}"));
                return;
            }
            self.remove_messages_locally(&source, &ids, &kind);
            self.visual_anchor = None;
            self.message_marked.clear();
            if self.message_open {
                self.close_message();
            }
            if let Some(account) = self.account_mut() {
                account.pump_deferred_mutations();
            }
            self.status = if is_unverified {
                StatusLine::info("mailbox action saved offline; will sync on reconnect")
            } else {
                StatusLine::info("mailbox action saved; syncing with server")
            };
            return;
        }
        if is_unverified {
            self.status = StatusLine::error("offline action requires a cached IMAP UIDVALIDITY");
            return;
        }
        let action = match &kind {
            MutationKind::Delete => ActionKind::DeleteMany {
                mailbox: source.clone(),
                ids: ids.clone(),
            },
            MutationKind::Move { destination } => ActionKind::MoveMany {
                source: source.clone(),
                destination: destination.clone(),
                ids: ids.clone(),
            },
            MutationKind::Archive { destination } => ActionKind::ArchiveMany {
                source: source.clone(),
                destination: destination.clone(),
                ids: ids.clone(),
            },
            MutationKind::Unarchive { destination } => ActionKind::UnarchiveMany {
                source: source.clone(),
                destination: destination.clone(),
                ids: ids.clone(),
            },
        };

        let moving_to_trash = matches!(&kind, MutationKind::Move { destination }
            if self.account().is_some_and(|account| account.mailboxes.iter()
                .any(|mailbox| mailbox.name == *destination && mailbox.role == MailboxRole::Trash)));
        let (pending_label, complete_label) = match &kind {
            MutationKind::Delete => (
                "permanently deleting message(s)…",
                "message(s) permanently deleted",
            ),
            MutationKind::Move { .. } if moving_to_trash => (
                "deleting message(s) to Trash…",
                "message(s) deleted to Trash",
            ),
            MutationKind::Move { .. } => ("moving message(s)…", "message(s) moved"),
            MutationKind::Archive { .. } => ("archiving message(s)…", "message(s) archived"),
            MutationKind::Unarchive { .. } => (
                "restoring message(s) to INBOX…",
                "message(s) restored to INBOX",
            ),
        };
        let started = self
            .account_mut()
            .is_some_and(|account| match account.worker.post(action) {
                Ok(request) => {
                    account
                        .pending_mutation_labels
                        .insert(request, complete_label);
                    true
                }
                Err(_) => false,
            });
        if !started {
            self.status = StatusLine::error("mail worker queue is busy; action not started");
            return;
        }

        self.remove_messages_locally(&source, &ids, &kind);
        self.visual_anchor = None;
        self.message_marked.clear();
        if self.message_open {
            self.close_message();
        }
        self.status = StatusLine::info(pending_label);
    }

    pub(super) fn remove_messages_locally(
        &mut self,
        source: &str,
        ids: &[MessageId],
        kind: &MutationKind,
    ) {
        let Some(account) = self.account_mut() else {
            return;
        };
        let selected_ids: HashSet<&MessageId> = ids.iter().collect();
        let removed: Vec<MessageSummary> = account
            .messages
            .iter()
            .filter(|message| selected_ids.contains(&message.id))
            .cloned()
            .collect();
        if removed.is_empty() {
            return;
        }
        // Gmail All Mail is a view containing both archived and INBOX messages.
        // Unarchiving adds the INBOX label without removing that All Mail row.
        if let MutationKind::Unarchive { destination } = kind
            && (source.to_ascii_lowercase().starts_with("[gmail]/")
                || source.to_ascii_lowercase().starts_with("[googlemail]/"))
        {
            if !account.unverified_mailboxes.contains(source) {
                account.forget_cached_folder(destination);
            }
            if let Some(mailbox) = account
                .mailboxes
                .iter_mut()
                .find(|mailbox| mailbox.name == *destination && mailbox.counts_known)
            {
                mailbox.total = mailbox.total.saturating_add(removed.len());
                mailbox.unread = mailbox
                    .unread
                    .saturating_add(removed.iter().filter(|m| m.unread()).count());
            }
            return;
        }
        account
            .messages
            .retain(|message| !selected_ids.contains(&message.id));
        for id in ids {
            account.bodies.remove(id);
            account.pending_bodies.remove(id);
            account.pending_seen.remove(id);
        }
        account.message_index = account
            .message_index
            .min(account.messages.len().saturating_sub(1));
        account.keep_message_cursor_visible();

        let removed_unread = removed.iter().filter(|message| message.unread()).count();
        if let Some(mailbox) = account
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == source && mailbox.counts_known)
        {
            mailbox.total = mailbox.total.saturating_sub(removed.len());
            mailbox.unread = mailbox.unread.saturating_sub(removed_unread);
        }
        if let MutationKind::Move { destination }
        | MutationKind::Archive { destination }
        | MutationKind::Unarchive { destination } = kind
        {
            // A destination cache cannot know about the moved messages without
            // a server response containing their destination UIDs. Drop only
            // that cache; the active source list stays updated locally.
            if !account.unverified_mailboxes.contains(source) {
                account.forget_cached_folder(destination);
            }
            if let Some(mailbox) = account
                .mailboxes
                .iter_mut()
                .find(|mailbox| mailbox.name == *destination && mailbox.counts_known)
            {
                mailbox.total = mailbox.total.saturating_add(removed.len());
                mailbox.unread = mailbox.unread.saturating_add(removed_unread);
            }
        }
        account.normalize_search_selection();
        account.persist_current_cache();
        account.persist_mailboxes();
    }
}
