//! Worker error and shutdown handling.

use super::*;

impl App {
    pub(super) fn handle_errors_event(&mut self, event: WorkerEvent) {
        let Some(account) = self.accounts.get_mut(event.account) else {
            return;
        };
        match event.kind {
            WorkerEventKind::Error(error) => {
                if self.attachment_pending_request_id == Some(event.request_id) {
                    self.attachment_pending_request_id = None;
                    self.attachment_pending = None;
                    self.attachment_source = None;
                    self.status = StatusLine::error(format!("could not load attachments: {error}"));
                    return;
                }
                if let Some(request) = account.status_requests.finish(event.request_id) {
                    // STATUS is best-effort. A failure must not tear down an
                    // unrelated page load, draft, SMTP send, or current reader.
                    if request.reason == StatusReason::Refresh {
                        self.status = StatusLine::temporary(
                            format!("{}: Inbox check failed ({error})", account.name),
                            Duration::from_secs(4),
                        );
                    }
                    Self::pump_mailbox_status(account);
                    return;
                }
                // Correlate errors by request ID: a failed background action
                // must not silently unlock every other pending mail mutation.
                let mutation_failed = account.pending_mutation_labels
                    .remove(&event.request_id)
                    .is_some();
                let failed_seen = account.pending_seen.iter()
                    .find_map(|(id, request_id)| (*request_id == event.request_id).then(|| id.clone()));
                if let Some(id) = &failed_seen {
                    account.pending_seen.remove(id);
                }
                account.connecting = false;
                account.mailboxes_loading = false;
                // A failure in an unrelated mutation/compose request must not
                // cancel a currently loading mailbox or email. Navigation has
                // dedicated error variants above and is correlated by ID.
                if account.page_request_id.is_none() {
                    account.messages_loading = false;
                    account.page_inflight = None;
                }
                if account.body_request_id.is_none() {
                    account.pending_bodies.clear();
                }
                if self
                    .pending_compose
                    .as_ref()
                    .is_some_and(|pending| pending.account == event.account)
                {
                    self.pending_compose = None;
                }
                let send_failed = self.pending_send == Some((event.account, event.request_id));
                if send_failed {
                    self.pending_send = None;
                    if let Some(composer) = self.composer.as_mut() {
                        composer.sending = false;
                        composer.confirm_send = false;
                    }
                }
                if (mutation_failed || failed_seen.is_some())
                    && let Some(name) = account.current_mailbox.clone()
                {
                    account.invalidate_current_cache();
                    let _ = account.request_mailbox_page(name, 0);
                }
                // STATUS failures are mailbox-local. Do not leave every other
                // folder stuck on an unknown count just because one provider
                // rejects STATUS for a special mailbox.
                Self::pump_mailbox_status(account);
                self.status = if send_failed {
                    // A transport error could happen after the SMTP server accepted
                    // DATA. Never retry automatically: the user must check Sent.
                    StatusLine::error(format!(
                        "send failed or status uncertain: {error}; check Sent before retrying (draft retained)"
                    ))
                } else {
                    StatusLine::error(format!("{}: {error}", account.name))
                };
            }
            WorkerEventKind::Stopped => {}
            _ => unreachable!("worker event routed to wrong handler"),
        }
    }
}
