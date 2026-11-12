//! Worker event dispatch. Each event has one owner: mailbox sync, MIME,
//! outgoing mutations, or errors. The UI thread remains the sole owner of
//! application state; these handlers never mutate it from a worker thread.

use super::*;

mod content;
mod errors;
mod mailbox;
mod outgoing;

/// This exact error is emitted only when OAuth token acquisition fails.
/// Since the token request precedes SMTP connection/MAIL/RCPT/DATA, it is
/// safe to queue or requeue the unsent message without risking duplication.
pub(super) fn is_pre_smtp_oauth_network_failure(error: &str) -> bool {
    error.trim() == "OAuth token request failed (network/TLS error)"
}

/// Limit reconnect transitions to recognizable transport failures, not IMAP
/// permissions, SMTP rejected recipients, or other application errors.
pub(super) fn is_connection_failure(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "oauth token request failed (network/tls error)",
        "not connected",
        "connection reset",
        "connection refused",
        "connection closed",
        "connection aborted",
        "connection lost",
        "broken pipe",
        "timed out",
        "timeout",
        "network is unreachable",
        "network unreachable",
        "no route to host",
        "failed to lookup address",
        "failed to resolve",
        "name resolution",
        "dns error",
        "failed to connect",
        "connecting to imap server",
        "unexpected eof",
    ]
    .iter()
    .any(|&phrase| error.contains(phrase))
}

#[derive(Debug, Clone, Copy)]
enum EventGroup {
    Mailbox,
    Content,
    Outgoing,
    Errors,
}

impl App {
    pub fn handle_foreground_worker_event(&mut self, generation: u64, event: WorkerEvent) {
        if self
            .accounts
            .get(event.account)
            .is_none_or(|account| account.foreground_generation != generation)
        {
            return;
        }
        if let Some(account) = self.accounts.get(event.account) {
            account
                .worker
                .acknowledge_foreground_event(event.request_id);
        }
        self.handle_worker_event(event);
    }

    pub fn handle_worker_event(&mut self, event: WorkerEvent) {
        let group = match &event.kind {
            WorkerEventKind::Connected { .. }
            | WorkerEventKind::Mailboxes(..)
            | WorkerEventKind::MailboxChanged { .. }
            | WorkerEventKind::MailboxStatus { .. }
            | WorkerEventKind::NewMailPreview { .. }
            | WorkerEventKind::MailboxOpened { .. }
            | WorkerEventKind::MailboxOpenFailed { .. } => EventGroup::Mailbox,
            WorkerEventKind::BodyLoadFailed { .. }
            | WorkerEventKind::BodyLoaded(..)
            | WorkerEventKind::BodyPrefetched { .. }
            | WorkerEventKind::AttachmentsLoaded { .. }
            | WorkerEventKind::RawLoaded { .. }
            | WorkerEventKind::MessageExported { .. } => EventGroup::Content,
            WorkerEventKind::SeenChanged { .. }
            | WorkerEventKind::MutationComplete
            | WorkerEventKind::DraftSaveFailed { .. }
            | WorkerEventKind::DraftSaved { .. }
            | WorkerEventKind::MessageSent { .. } => EventGroup::Outgoing,
            WorkerEventKind::Error(..) | WorkerEventKind::Stopped => EventGroup::Errors,
        };
        match group {
            EventGroup::Mailbox => self.handle_mailbox_event(event),
            EventGroup::Content => self.handle_content_event(event),
            EventGroup::Outgoing => self.handle_outgoing_event(event),
            EventGroup::Errors => self.handle_errors_event(event),
        }
    }
}
