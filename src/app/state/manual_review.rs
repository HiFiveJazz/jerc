//! Explicit resolution of uncertain durable operations. Never retries an IMAP
//! mutation or SMTP send; only the selected, verified record is removed.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReviewOperation {
    Mutation(cache::DeferredMutation),
    Outbox(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewItem {
    pub(crate) account: usize,
    pub(crate) operation: ReviewOperation,
}

impl App {
    fn review_items(&self) -> Vec<ReviewItem> {
        let mut items = Vec::new();
        for (account_index, account) in self.accounts.iter().enumerate() {
            // Do not allow manual deletion of any record while an unacknowledged
            // foreground write might still be completing for this account.
            if account.worker.has_unacknowledged_writes() {
                continue;
            }
            for entry in &account.deferred_mutations {
                if entry.state == cache::DeferredMutationState::NeedsReview
                    && !account
                        .inflight_deferred_mutations
                        .values()
                        .any(|inflight| mutation_notice_key(inflight) == mutation_notice_key(entry))
                {
                    items.push(ReviewItem {
                        account: account_index,
                        operation: ReviewOperation::Mutation(entry.clone()),
                    });
                }
            }
            for entry in &account.outbox {
                if entry.state == crate::outgoing::outbox::SendState::NeedsReview
                    && !account.inflight_outbox.values().any(|id| id == &entry.id)
                {
                    items.push(ReviewItem {
                        account: account_index,
                        operation: ReviewOperation::Outbox(entry.id.clone()),
                    });
                }
            }
        }
        items
    }

    pub fn manual_review_count(&self) -> usize {
        self.review_items().len()
    }

    pub fn manual_review_confirmation_open(&self) -> bool {
        self.manual_review_pending.is_some()
    }

    pub fn manual_review_details(&self) -> String {
        let items = self.review_items();
        let Some(item) = items.get(
            self.manual_review_selected
                .min(items.len().saturating_sub(1)),
        ) else {
            return "No unresolved operations remain.".to_owned();
        };
        self.review_item_details(item)
    }

    pub(super) fn review_item_details(&self, item: &ReviewItem) -> String {
        let account = &self.accounts[item.account];
        let heading = format!("{}  |  Manual review", account.name);
        match &item.operation {
            ReviewOperation::Mutation(entry) => {
                let action = match &entry.kind {
                    cache::DeferredMutationKind::Delete => "Delete".to_owned(),
                    cache::DeferredMutationKind::Move { destination } => {
                        format!("Move to {destination}")
                    }
                    cache::DeferredMutationKind::Archive { destination } => {
                        format!("Archive to {destination}")
                    }
                    cache::DeferredMutationKind::Unarchive { destination } => {
                        format!("Unarchive to {destination}")
                    }
                };
                let ids = entry
                    .ids
                    .iter()
                    .take(12)
                    .map(|id| id.0.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let extra = if entry.ids.len() > 12 {
                    format!(" (+{} more)", entry.ids.len() - 12)
                } else {
                    String::new()
                };
                format!(
                    "{heading}\n\nOperation: {action}\nSource: {}\nUIDVALIDITY: {}\nMessage IDs: {ids}{extra}\n\nResult uncertain. Check the server; Jerc will not retry automatically.",
                    entry.mailbox, entry.validity
                )
            }
            ReviewOperation::Outbox(id) => {
                let Some(entry) = account.outbox.iter().find(|entry| &entry.id == id) else {
                    return "Outbox entry changed; reopen the review screen.".to_owned();
                };
                format!(
                    "{heading}\n\nOutgoing email (SMTP result uncertain)\nTo: {}\nSubject: {}\nOutbox ID: {}\n\nSMTP result uncertain. Check Sent and recipients; Jerc will not resend automatically.",
                    entry.message.to, entry.message.subject, entry.id
                )
            }
        }
    }

    pub(super) fn handle_manual_review_key(&mut self, key: KeyEvent) {
        if self.manual_review_pending.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') if key.modifiers.is_empty() => {
                    if let Some(item) = self.manual_review_pending.take() {
                        self.resolve_review_item(item);
                    }
                }
                KeyCode::Esc | KeyCode::Char('n' | 'N') => self.manual_review_pending = None,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.manual_review_visible = false,
            KeyCode::Char('j') | KeyCode::Down => {
                let count = self.manual_review_count();
                self.manual_review_selected =
                    (self.manual_review_selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.manual_review_selected = self.manual_review_selected.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('r') if key.modifiers.is_empty() => {
                self.manual_review_pending = self
                    .review_items()
                    .get(self.manual_review_selected)
                    .cloned();
            }
            _ => {}
        }
    }

    pub(super) fn resolve_review_item(&mut self, item: ReviewItem) -> bool {
        // Recheck on confirmation: a late worker ACK must never turn a
        // confirmation into deletion of another entry at the same index.
        if !self.review_items().contains(&item) {
            self.status = StatusLine::temporary_error(
                "review item changed; nothing removed",
                Duration::from_secs(6),
            );
            return false;
        }
        let notice_identity = match &item.operation {
            ReviewOperation::Mutation(entry) => key_digest(&format!(
                "review-mutation:{}:{}",
                self.accounts[item.account].name,
                mutation_notice_key(entry)
            )),
            ReviewOperation::Outbox(id) => key_digest(&format!(
                "review-outbox:{}:{id}",
                self.accounts[item.account].name
            )),
        };
        let account = &mut self.accounts[item.account];
        let result = match item.operation {
            ReviewOperation::Mutation(entry) => {
                let next = account
                    .deferred_mutations
                    .iter()
                    .filter(|record| *record != &entry)
                    .cloned()
                    .collect::<Vec<_>>();
                let result = account
                    .cache_writer
                    .as_ref()
                    .ok_or_else(|| std::io::Error::other("mutation journal unavailable"))
                    .and_then(|store| store.save_mutations(&next));
                if result.is_ok() {
                    account.deferred_mutations = next;
                    account
                        .offline_notice_mutations
                        .remove(&mutation_notice_key(&entry));
                    account.invalidate_remote_folder(&entry.mailbox);
                    account.unverified_mailboxes.insert(entry.mailbox.clone());
                    if let cache::DeferredMutationKind::Move { destination }
                    | cache::DeferredMutationKind::Archive { destination }
                    | cache::DeferredMutationKind::Unarchive { destination } = &entry.kind
                    {
                        account.invalidate_remote_folder(destination);
                    }
                    if account.capabilities.is_some()
                        && account.current_mailbox.as_deref() == Some(entry.mailbox.as_str())
                    {
                        account.restore_message_index = Some(account.message_index);
                        let _ = account.request_mailbox_page(entry.mailbox.clone(), 0);
                    }
                }
                result.map_err(|error| error.to_string())
            }
            ReviewOperation::Outbox(id) => {
                let next = account
                    .outbox
                    .iter()
                    .filter(|record| record.id != id)
                    .cloned()
                    .collect::<Vec<_>>();
                let result = account
                    .outbox_store
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Outbox journal unavailable"))
                    .and_then(|store| store.save(&next));
                if result.is_ok() {
                    account.outbox = next;
                    account.offline_notice_sends.remove(&id);
                }
                result.map_err(|error| error.to_string())
            }
        };
        match result {
            Ok(()) => {
                // A future user action against the same UID must be capable
                // of producing a new notice, even after this one is resolved.
                self.acknowledged_warning_keys.remove(&notice_identity);
                self.notice_first_seen.remove(&notice_identity);
                if let Some(store) = &self.notice_ack_store
                    && let Err(error) =
                        store.save_state(&self.acknowledged_warning_keys, &self.notice_first_seen)
                {
                    self.status = StatusLine::temporary_error(
                        format!("resolved journal, but could not save notice state: {error}"),
                        Duration::from_secs(8),
                    );
                    return true;
                }
                self.manual_review_selected = self
                    .manual_review_selected
                    .min(self.manual_review_count().saturating_sub(1));
                self.status = StatusLine::temporary(
                    "review item resolved; no operation was retried",
                    Duration::from_secs(6),
                );
                true
            }
            Err(error) => {
                self.status = StatusLine::temporary_error(
                    format!("could not save resolution: {error}"),
                    Duration::from_secs(8),
                );
                false
            }
        }
    }
}
