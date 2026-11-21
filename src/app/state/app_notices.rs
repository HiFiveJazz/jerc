//! Warnings, notices and status-bar expiration
use super::*;

impl App {
    /// Build notices from durable operation state without modifying the journals.
    /// An online delete temporarily enters Queued and then NeedsReview while
    /// its IMAP ACK is outstanding; neither state should flash a warning.
    pub(super) fn warning_entries(&self) -> Vec<WarningNotice> {
        let mut notices = self
            .startup_warnings
            .iter()
            .map(|message| WarningNotice {
                identities: vec![format!("configuration:{message}")],
                message: message.clone(),
            })
            .collect::<Vec<_>>();

        for account in &self.accounts {
            if account.offline_incident_active {
                notices.push(WarningNotice {
                    identities: vec![format!("connection:{}:{}", account.name, account.offline_incident_number)],
                    message: format!("{}: connection unavailable; cached mail remains readable. Jerc will retry automatically.", account.name),
                });
            }
            let mut queued_keys = Vec::new();
            let mut queued_deletes = 0usize;
            let mut queued_changes = 0usize;
            let mut sending_now = 0usize;
            let mut changing_now = 0usize;
            let in_flight_mutations = account
                .inflight_deferred_mutations
                .values()
                .map(mutation_notice_key)
                .collect::<HashSet<_>>();
            let in_flight_sends = account
                .inflight_outbox
                .values()
                .map(String::as_str)
                .collect::<HashSet<_>>();
            for entry in &account.deferred_mutations {
                let key = mutation_notice_key(entry);
                let was_offline = account.offline_notice_mutations.contains(&key);
                if !(was_offline || account.capabilities.is_none()) {
                    continue;
                }
                let in_flight = in_flight_mutations.contains(&key);
                if entry.state == cache::DeferredMutationState::Queued || in_flight {
                    queued_keys.push(format!("queue-mutation:{}:{key}", account.name));
                    if in_flight {
                        changing_now += entry.ids.len();
                    } else {
                        match &entry.kind {
                            cache::DeferredMutationKind::Delete => {
                                queued_deletes += entry.ids.len()
                            }
                            _ => queued_changes += entry.ids.len(),
                        }
                    }
                }
            }
            let mut queued_sends = 0usize;
            for entry in &account.outbox {
                let was_offline = account.offline_notice_sends.contains(&entry.id);
                if !(was_offline || account.capabilities.is_none()) {
                    continue;
                }
                let in_flight = in_flight_sends.contains(entry.id.as_str());
                if entry.state == crate::outgoing::outbox::SendState::Queued || in_flight {
                    queued_keys.push(format!("queue-send:{}:{}", account.name, entry.id));
                    if in_flight {
                        sending_now += 1;
                    } else {
                        queued_sends += 1;
                    }
                }
            }
            if !queued_keys.is_empty() {
                queued_keys.sort();
                // Each action keeps its identity until its ACK, including
                // while in flight. Aggregate size shrinking is not a new alert.
                let mut parts = Vec::new();
                let when = if account.capabilities.is_none() {
                    "when the connection resumes"
                } else {
                    "for synchronization"
                };
                if queued_sends > 0 {
                    parts.push(format!(
                        "{} queued for sending {when}",
                        notice_count(queued_sends, "email")
                    ));
                }
                if queued_deletes > 0 {
                    parts.push(format!(
                        "{} queued for deletion {when}",
                        notice_count(queued_deletes, "email")
                    ));
                }
                if queued_changes > 0 {
                    parts.push(format!(
                        "{} queued {when}",
                        notice_count(queued_changes, "mailbox change")
                    ));
                }
                if sending_now > 0 {
                    parts.push(format!(
                        "{} submitted from the offline queue; awaiting server confirmation",
                        notice_count(sending_now, "email")
                    ));
                }
                if changing_now > 0 {
                    parts.push(format!(
                        "{} submitted from the offline queue; awaiting server confirmation",
                        notice_count(changing_now, "mailbox change")
                    ));
                }
                notices.push(WarningNotice {
                    identities: queued_keys,
                    message: format!("{}: {}.", account.name, parts.join("; ")),
                });
            }

            let review_mutations = account
                .deferred_mutations
                .iter()
                .filter(|entry| {
                    entry.state == cache::DeferredMutationState::NeedsReview
                        && !in_flight_mutations.contains(&mutation_notice_key(entry))
                })
                .collect::<Vec<_>>();
            if !review_mutations.is_empty() {
                let mut keys = review_mutations
                    .iter()
                    .map(|entry| mutation_notice_key(entry))
                    .collect::<Vec<_>>();
                keys.sort();
                let count = review_mutations
                    .iter()
                    .map(|entry| entry.ids.len())
                    .sum::<usize>();
                let verb = if count == 1 { "requires" } else { "require" };
                notices.push(WarningNotice {
                    identities: keys.into_iter().map(|key| format!("review-mutation:{}:{key}", account.name)).collect(),
                    message: format!("{}: {} {verb} manual review; the IMAP result is uncertain, so Jerc will NOT retry automatically.", account.name, notice_count(count, "mailbox change")),
                });
            }
            let review_sends = account
                .outbox
                .iter()
                .filter(|entry| {
                    entry.state == crate::outgoing::outbox::SendState::NeedsReview
                        && !in_flight_sends.contains(entry.id.as_str())
                })
                .collect::<Vec<_>>();
            if !review_sends.is_empty() {
                let mut ids = review_sends
                    .iter()
                    .map(|entry| entry.id.as_str())
                    .collect::<Vec<_>>();
                ids.sort();
                let verb = if review_sends.len() == 1 {
                    "requires"
                } else {
                    "require"
                };
                notices.push(WarningNotice {
                    identities: ids.into_iter().map(|id| format!("review-outbox:{}:{id}", account.name)).collect(),
                    message: format!("{}: {} {verb} manual review. Check Sent before retrying: The server may have accepted the email. Jerc will NOT resend automatically.", account.name, notice_count(review_sends.len(), "Outbox email")),
                });
            }
            if account
                .cache_writer
                .as_ref()
                .is_some_and(|writer| !writer.mutations_available())
            {
                notices.push(WarningNotice {
                    identities: vec![format!("mutation-storage:{}", account.name)],
                    message: format!(
                        "{}: offline mutation journal is unreadable; it has not been overwritten",
                        account.name
                    ),
                });
            }
            if account
                .outbox_store
                .as_ref()
                .is_some_and(|store| !store.is_readable())
            {
                notices.push(WarningNotice {
                    identities: vec![format!("outbox-storage:{}", account.name)],
                    message: format!(
                        "{}: offline Outbox is unreadable; it has not been overwritten",
                        account.name
                    ),
                });
            }
        }
        notices
    }

    /// Full details remain available via ! even after acknowledging the banner.
    #[must_use]
    pub fn all_warnings(&self) -> Vec<String> {
        self.warning_entries()
            .into_iter()
            .map(|notice| notice.message)
            .collect()
    }

    /// Only notices not yet inspected appear in the top banner.
    #[must_use]
    pub fn visible_warnings(&self) -> Vec<String> {
        self.warning_entries()
            .into_iter()
            .filter(|notice| {
                notice
                    .identities
                    .iter()
                    .any(|key| !self.acknowledged_warning_keys.contains(&key_digest(key)))
            })
            .map(|notice| notice.message)
            .collect()
    }

    /// Compact mail-browser indicator for *active* notices. Acknowledgment
    /// only silences unsolicited warnings: it must not hide unresolved work.
    /// The same string is used by the tab renderer and its click hitbox.
    #[must_use]
    pub fn warning_banner_label(&self) -> Option<String> {
        (!self.notice_rows().is_empty()).then(|| format!("  {}", crate::ui::NOTICE_HAZARD_ICON))
    }

    /// Store only digests of the inspected identities, never mailbox names or
    /// message UIDs. Acknowledgment silences the banner, not the durable WAL.
    pub fn inspect_warnings(&mut self) {
        let current = self.notice_identity_digests();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let active = current.iter().cloned().collect::<HashSet<_>>();
        self.notice_first_seen
            .retain(|identity, _| active.contains(identity));
        for (order, identity) in current.iter().enumerate() {
            self.notice_first_seen
                .entry(identity.clone())
                .or_insert(now.saturating_add(order as u64));
        }
        // Keep explicit dismissals of still-active rows separate from the
        // ordinary banner acknowledgment. Both are digests in the same private
        // persisted store, so no new journal or persistence component is needed.
        let active_dismissals = self
            .all_notice_rows()
            .iter()
            .map(|row| notices::dismissed_notice_key(&row.id))
            .collect::<HashSet<_>>();
        self.acknowledged_warning_keys
            .retain(|key| active_dismissals.contains(key));
        self.acknowledged_warning_keys.extend(current);
        if let Some(store) = &self.notice_ack_store
            && let Err(error) =
                store.save_state(&self.acknowledged_warning_keys, &self.notice_first_seen)
        {
            self.status = StatusLine::temporary_error(
                format!("could not persist notice acknowledgment: {error}"),
                Duration::from_secs(6),
            );
        }
        self.show_startup_warnings = true;
    }

    /// The runtime uses this deadline to repaint even without another keypress.
    #[must_use]
    pub fn status_expiration_deadline(&self) -> Option<Instant> {
        self.status.expires_at
    }

    pub fn expire_status(&mut self) -> bool {
        if !self.status.expired() {
            return false;
        }
        self.status = StatusLine::info("ready");
        true
    }
}
