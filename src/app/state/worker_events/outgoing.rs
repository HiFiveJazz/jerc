//! Seen flags, mailbox mutations, drafts and SMTP completions.

use super::*;

impl App {
    pub(super) fn handle_outgoing_event(&mut self, event: WorkerEvent) {
        // Only parse account identities when an event needs contact/recipient matching.
        let own_addresses = if matches!(&event.kind, WorkerEventKind::MessageSent { .. }) {
            self.accounts
                .iter()
                .filter_map(|a| a.from.parse::<lettre::message::Mailbox>().ok())
                .map(|a| a.email.to_string())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let Some(account) = self.accounts.get_mut(event.account) else {
            return;
        };
        match event.kind {
            WorkerEventKind::SeenChanged { id, seen } => {
                if account.pending_seen.get(&id) != Some(&event.request_id) {
                    return;
                }
                account.pending_seen.remove(&id);
                account.apply_seen_flag(&id, seen);
                account.persist_current_cache();
                account.persist_mailboxes();
            }
            WorkerEventKind::MutationComplete => {
                // Only a tracked, completed mutation may resolve the pending
                // state. A background server-draft cleanup isn't a user action.
                if let Some(message) = account.pending_mutation_labels.remove(&event.request_id) {
                    self.status = StatusLine::temporary(message, Duration::from_secs(2));
                }
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::DraftSaveFailed { mailbox, error } => {
                self.pending_draft_replacements.remove(&(event.account, event.request_id));
                self.status = StatusLine::error(format!(
                    "saved locally only: uploading to {mailbox} failed ({error}); Ctrl+n resumes"
                ));
            }
            WorkerEventKind::DraftSaved { mailbox } => {
                let mut cleanup_warning = false;
                if let Some(source) = self.pending_draft_replacements.remove(&(event.account, event.request_id))
                    && source.mailbox == mailbox
                {
                    // A failed DELETE leaves a harmless duplicate draft, never data loss.
                    cleanup_warning = account.worker.post(ActionKind::Delete {
                        mailbox: source.mailbox, id: source.id,
                    }).is_err();
                }
                // Refresh cached Drafts on the next visit, or immediately if
                // the user is already browsing that folder.
                account.invalidate_remote_folder(&mailbox);
                if account.current_mailbox.as_deref() == Some(mailbox.as_str()) {
                    let _ = account.request_mailbox_page(mailbox.clone(), 0);
                }
                self.status = if cleanup_warning {
                    StatusLine::error("draft saved, but old draft cleanup was not queued; check Drafts for duplicates")
                } else {
                    StatusLine::info(format!(
                        "draft saved in {mailbox}; Ctrl+n resumes editing local recovery copy"
                    ))
                };
            }
            WorkerEventKind::MessageSent {
                smtp_status,
                sent_mailbox,
                sent_copy_saved,
                sent_copy_warning,
                sent_copy_disabled,
            } => {
                let completed_current_send = self.pending_send == Some((event.account, event.request_id));
                if completed_current_send {
                    self.pending_send = None;
                }
                if completed_current_send && let Some(composer) = &self.composer {
                    let mut changed = false;
                    for field in [&composer.to, &composer.cc, &composer.bcc] {
                        for (name, address) in recipients(field) {
                            if !own_addresses
                                .iter()
                                .any(|own| own.eq_ignore_ascii_case(&address))
                            {
                                changed |= self
                                    .contacts
                                    .upsert(&name, &address, false)
                                    .unwrap_or(false);
                            }
                        }
                    }
                    if changed {
                        self.contacts_dirty = true;
                        if self.contacts_save_due.is_none() {
                            self.contacts_save_due = Some(Instant::now() + Duration::from_secs(3));
                        }
                    }
                }
                // A successful SMTP submission makes our mailbox snapshots stale even
                // when the provider files Sent itself (Gmail), and especially when the
                // message was addressed back to this account. Force the next visit to
                // INBOX/Sent to come from the server rather than the in-memory cache.
                let mut refresh_current = None;
                let affected = account
                    .mailboxes
                    .iter()
                    .filter(|mailbox| {
                        matches!(mailbox.role, MailboxRole::Inbox | MailboxRole::Sent)
                            || sent_mailbox
                                .as_deref()
                                .is_some_and(|name| name == mailbox.name)
                    })
                    .map(|mailbox| mailbox.name.clone())
                    .collect::<Vec<_>>();
                for name in &affected {
                    account.invalidate_remote_folder(name);
                }
                account.queue_unknown_mailbox_statuses(sent_mailbox.as_deref());
                if account
                    .current_mailbox
                    .as_ref()
                    .is_some_and(|name| affected.iter().any(|affected| affected == name))
                {
                    refresh_current = account.current_mailbox.clone();
                }
                if let Some(name) = refresh_current {
                    let _ = account.request_mailbox_page(name, 0);
                }
                Self::pump_mailbox_status(account);

                if completed_current_send && event.account == self.account_index
                    && let Some(source) = self.composer.as_ref().and_then(|composer| composer.draft_source.clone())
                {
                    // Retire the server-side draft only after SMTP accepted the
                    // outgoing message. A failure leaves the draft untouched.
                    let _ = account.worker.post(ActionKind::Delete {
                        mailbox: source.mailbox,
                        id: source.id,
                    });
                }
                if completed_current_send && self.resumed_draft_account == Some(event.account) {
                    if let Ok(root) = Self::postponed_drafts_root() {
                        let _ = crate::outgoing::drafts::delete_draft(&root, &account.name);
                    }
                    self.resumed_draft_account = None;
                }
                if completed_current_send && event.account == self.account_index {
                    self.composer = None;
                }
                self.status = if let Some(warning) = sent_copy_warning {
                    StatusLine::temporary(
                        format!("SMTP accepted ({smtp_status}); {warning}"),
                        Duration::from_secs(8),
                    )
                } else if sent_copy_disabled {
                    StatusLine::temporary(
                        format!("Message accepted ({smtp_status}); Sent copy disabled"),
                        Duration::from_secs(5),
                    )
                } else if sent_copy_saved {
                    StatusLine::temporary(
                        format!("SMTP accepted ({smtp_status}); saved to Sent"),
                        Duration::from_secs(5),
                    )
                } else {
                    StatusLine::temporary(
                        format!("SMTP accepted ({smtp_status}); provider handles Sent copy"),
                        Duration::from_secs(5),
                    )
                };
            }
            _ => unreachable!("worker event routed to wrong handler"),
        }
    }
}
