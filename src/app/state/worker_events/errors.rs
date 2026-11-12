//! Worker error and shutdown handling.

use super::*;

impl App {
    pub(super) fn handle_errors_event(&mut self, event: WorkerEvent) {
        let Some(account) = self.accounts.get_mut(event.account) else {
            return;
        };
        match event.kind {
            WorkerEventKind::Error(error) => {
                // A superseded LIST may finish *after* a newer connection.
                // Ignore its error rather than disconnecting the new session.
                if account.is_abandoned_list_request(event.request_id) {
                    return;
                }
                // A failed OAuth token HTTP request happens before any SMTP
                // submission. This is the only send failure we can safely
                // queue again without checking Sent first.
                let pre_smtp_oauth_failure = super::is_pre_smtp_oauth_network_failure(&error);
                let failed_connect = account.connect_request_id == Some(event.request_id);
                let failed_list = account.mailbox_list_request_id == Some(event.request_id);
                // STATUS and attachment downloads use independent read-only
                // IMAP sessions. Their failures must not invalidate a healthy
                // foreground SELECT/LIST or leave it stuck behind a retry.
                let failed_status = account.status_requests.finish(event.request_id);
                // The background worker prefixes STATUS errors. A late error
                // from a retired count request is not a foreground failure.
                if error.starts_with("background mailbox count:") && failed_status.is_none() {
                    return;
                }
                let failed_attachment =
                    self.attachment_pending_request_id == Some(event.request_id);
                if failed_connect {
                    account.connect_request_id = None;
                    account.connecting = false;
                }
                if failed_list {
                    account.mailbox_list_request_id = None;
                    account.mailboxes_loading = false;
                }
                if failed_list || failed_connect {
                    account.sync_progress_at = None;
                }
                if failed_connect
                    || (super::is_connection_failure(&error)
                        && failed_status.is_none()
                        && !failed_attachment)
                {
                    account.mark_disconnected();
                }
                if failed_attachment {
                    self.attachment_pending_request_id = None;
                    self.attachment_pending = None;
                    self.attachment_source = None;
                    self.status = StatusLine::error(format!("could not load attachments: {error}"));
                    return;
                }
                if let Some(request) = failed_status {
                    // STATUS is best-effort and owns its own connection.
                    // Requeue the count refresh instead of tearing down an
                    // unrelated foreground page load, draft or SMTP send.
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
                let mutation_failed = account
                    .pending_mutation_labels
                    .remove(&event.request_id)
                    .is_some();
                let uncertain_offline_mutation =
                    account.finish_deferred_mutation(event.request_id, false);
                let failed_seen = account.pending_seen.iter().find_map(|(id, request_id)| {
                    (*request_id == event.request_id).then(|| id.clone())
                });
                if let Some(id) = &failed_seen {
                    account.pending_seen.remove(id);
                }
                if account
                    .inflight_deferred_seen
                    .remove(&event.request_id)
                    .is_some()
                {
                    // SetSeen is idempotent; keep the desired value in the
                    // durable journal for a later verified connection.
                    account.persist_deferred_seen();
                }
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
                if self.pending_compose.as_ref().is_some_and(|pending| {
                    pending.account == event.account && pending.request_id == event.request_id
                }) {
                    self.pending_compose = None;
                }
                let outbox_was_inflight = account.inflight_outbox.contains_key(&event.request_id);
                let outbox_requeued = outbox_was_inflight
                    && pre_smtp_oauth_failure
                    && account.requeue_outbox_before_smtp(event.request_id);
                if !outbox_requeued {
                    account.finish_outbox(event.request_id, false);
                }
                let uncertain_outbox_send = outbox_was_inflight && !outbox_requeued;
                let send_failed = self.pending_send == Some((event.account, event.request_id));
                if send_failed {
                    self.pending_send = None;
                    if let Some(composer) = self.composer.as_mut() {
                        composer.sending = false;
                        composer.confirm_send = false;
                    }
                }
                if mutation_failed && let Some(name) = account.current_mailbox.clone() {
                    account.invalidate_current_cache();
                    let _ = account.request_mailbox_page(name, 0);
                }
                // STATUS failures are mailbox-local. Do not leave every other
                // folder stuck on an unknown count just because one provider
                // rejects STATUS for a special mailbox.
                Self::pump_mailbox_status(account);
                let message = if outbox_requeued {
                    "OAuth unavailable before SMTP; Outbox message remains queued for reconnect"
                        .to_owned()
                } else if uncertain_outbox_send {
                    format!(
                        "offline Outbox send result uncertain ({error}); verify Sent before retrying"
                    )
                } else if uncertain_offline_mutation {
                    format!(
                        "mailbox change result uncertain ({error}); kept for review, not retried automatically"
                    )
                } else if send_failed {
                    // SMTP may have accepted DATA before the transport failed.
                    // Do not auto-retry; keep the composer/draft intact.
                    format!(
                        "send failed or status uncertain: {error}; check Sent before retrying (draft retained)"
                    )
                } else {
                    format!("{}: {error}", account.name)
                };
                self.status = if outbox_requeued {
                    StatusLine::temporary(message, Duration::from_secs(5))
                } else {
                    StatusLine::temporary_error(message, Duration::from_secs(10))
                };
                if send_failed && pre_smtp_oauth_failure {
                    // We now know that SMTP was never contacted. The composer
                    // still contains the complete message and attachments;
                    // send_composer takes the offline Outbox path because the
                    // connection failure cleared account.capabilities above.
                    // If persistence fails, it retains the editable composer.
                    self.send_composer();
                }
            }
            WorkerEventKind::Stopped => {
                account.connecting = false;
                account.connect_request_id = None;
                account.mark_disconnected();
                self.status = StatusLine::temporary_error(
                    format!("{}: mail worker stopped; restart Jerc", account.name),
                    Duration::from_secs(10),
                );
            }
            _ => unreachable!("worker event routed to wrong handler"),
        }
    }
}
