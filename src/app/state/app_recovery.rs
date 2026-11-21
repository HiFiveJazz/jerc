//! Connection stall recovery, resume and reconnect logic
use super::*;

impl App {
    /// Called on a fixed timer independent of Inbox polling preferences.
    /// Network work stays in mail workers; the TUI never blocks on DNS/TLS.
    pub fn recover_stalled_foreground_accounts(&mut self) {
        let pending_send = self.pending_send;
        let pending_compose = self.pending_compose.as_ref().map(|pending| pending.account);
        let pending_draft = self.pending_draft.as_ref().map(|pending| pending.account);
        for (index, account) in self.accounts.iter_mut().enumerate() {
            let elapsed = account.sync_progress_at.map(|started| started.elapsed());
            let recovery_timeout = if account.connecting || account.mailboxes_loading {
                FOREGROUND_RECOVERY_TIMEOUT
            } else {
                FOREGROUND_PAGE_RECOVERY_TIMEOUT
            };
            if account.syncing_with_server() && elapsed.is_some_and(|age| age >= recovery_timeout) {
                // Only retire a read-only foreground session. Pending writes,
                // SMTP or draft actions could already have reached a server;
                // abandoning their result could cause a duplicate send/move.
                let safe_to_rotate = pending_send.is_none_or(|(owner, _)| owner != index)
                    && pending_compose != Some(index)
                    && pending_draft != Some(index)
                    && account.pending_mutation_labels.is_empty()
                    && account.inflight_deferred_mutations.is_empty()
                    && account.inflight_outbox.is_empty()
                    && account.pending_seen.is_empty()
                    && account.inflight_deferred_seen.is_empty();
                if safe_to_rotate {
                    match account.worker.recover_stalled_foreground() {
                        Ok(true) => {
                            account.foreground_generation = account.worker.foreground_generation();
                            account.connecting = false;
                            account.connect_request_id = None;
                            account.mark_disconnected();
                            // New worker has an independent socket. It can
                            // reconnect immediately even if the old LIST is
                            // still waiting for its OS network timeout.
                            let _ = account.request_connect();
                            self.status = StatusLine::temporary(
                                format!("{}: replacing stalled IMAP connection", account.name),
                                Duration::from_secs(4),
                            );
                            continue;
                        }
                        Ok(false) => {} // backoff or retired-worker cap
                        Err(error) => {
                            self.status = StatusLine::temporary_error(
                                format!(
                                    "{}: could not restart IMAP worker: {error:#}",
                                    account.name
                                ),
                                Duration::from_secs(6),
                            );
                        }
                    }
                }
            }
        }
    }

    /// System resume: proactively replace sockets for read-only sessions. A
    /// possibly submitted write is NEVER detached, retried, or discarded.
    /// Its journal remains NeedsReview until a definitive server ACK or an
    /// explicit user resolution.
    pub fn handle_system_resume(&mut self) {
        let pending_send = self.pending_send;
        let pending_compose = self.pending_compose.as_ref().map(|pending| pending.account);
        let pending_draft = self.pending_draft.as_ref().map(|pending| pending.account);
        let mut reconnected = 0usize;
        for (index, account) in self.accounts.iter_mut().enumerate() {
            let write_pending = pending_send.is_some_and(|(owner, _)| owner == index)
                || pending_compose == Some(index)
                || pending_draft == Some(index)
                || !account.pending_mutation_labels.is_empty()
                || !account.inflight_deferred_mutations.is_empty()
                || !account.inflight_outbox.is_empty()
                || !account.pending_seen.is_empty()
                || !account.inflight_deferred_seen.is_empty()
                || account.worker.has_unacknowledged_writes();
            if write_pending {
                // Keep waiting for the actual worker reply. On failure the
                // normal worker error handler preserves uncertain journals.
                continue;
            }
            let already_offline = account.offline_incident_active;
            match account.worker.recover_stalled_foreground() {
                Ok(true) => {
                    account.foreground_generation = account.worker.foreground_generation();
                    account.connecting = false;
                    account.connect_request_id = None;
                    account.mark_disconnected();
                    // A routine resume is not itself evidence of an outage.
                    // A real failed reconnect will create its own notice.
                    if !already_offline {
                        account.offline_incident_active = false;
                    }
                    if account.request_connect() {
                        reconnected += 1;
                    }
                }
                Ok(false) => {
                    // A worker may be under recovery backoff, or the account
                    // might use Maildir (no network socket to replace).
                    if account.capabilities.is_none() && !account.connecting {
                        let _ = account.request_connect();
                    }
                }
                Err(error) => {
                    self.status = StatusLine::temporary_error(
                        format!("{}: resume recovery unavailable: {error:#}", account.name),
                        Duration::from_secs(6),
                    );
                }
            }
        }
        if reconnected > 0 {
            self.status = StatusLine::temporary(
                format!("resumed from sleep; reconnecting {reconnected} mail account(s)"),
                Duration::from_secs(5),
            );
        }
    }

    /// Auto-reconnect uses its original 10-second cadence. Stall detection
    /// runs on a separate, faster timer to avoid increasing retry traffic.
    pub fn reconnect_offline_accounts(&mut self) {
        for account in &mut self.accounts {
            if account.syncing_with_server()
                && account
                    .sync_progress_at
                    .is_some_and(|since| since.elapsed() >= FOREGROUND_SYNC_TIMEOUT)
            {
                account.connecting = false;
                account.connect_request_id = None;
                account.mark_disconnected();
                self.status = StatusLine::temporary_error(
                    format!(
                        "{}: sync timed out; cached mail remains available",
                        account.name
                    ),
                    Duration::from_secs(8),
                );
                continue;
            }
            if account.capabilities.is_none() {
                account.request_connect();
            }
        }
    }
}
