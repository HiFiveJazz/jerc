//! Worker event dispatch. Each event has one owner: mailbox sync, MIME,
//! outgoing mutations, or errors. The UI thread remains the sole owner of
//! application state; these handlers never mutate it from a worker thread.

use super::*;

mod content;
mod errors;
mod mailbox;
mod outgoing;

#[cfg(test)]
pub(super) fn is_pre_smtp_oauth_network_failure(error: &str) -> bool {
    crate::backend::legacy_kind(error) == crate::backend::ErrorKind::PreSmtpOAuth
}

#[cfg(test)]
pub(super) fn is_connection_failure(error: &str) -> bool {
    crate::backend::legacy_kind(error).is_connection_failure()
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
            | WorkerEventKind::AccountChanged
            | WorkerEventKind::MailboxStatus { .. }
            | WorkerEventKind::NewMailPreview { .. }
            | WorkerEventKind::MailboxOpened { .. }
            | WorkerEventKind::MailboxQueried { .. }
            | WorkerEventKind::ThreadDiscovered { .. }
            | WorkerEventKind::ThreadDiscoveryFailed
            | WorkerEventKind::MailboxOpenFailed { .. }
            | WorkerEventKind::MailboxWarmed { .. }
            | WorkerEventKind::MailboxWarmFailed { .. } => EventGroup::Mailbox,
            WorkerEventKind::BodyLoadFailed { .. }
            | WorkerEventKind::BodyLoaded(..)
            | WorkerEventKind::BodyPrefetched { .. }
            | WorkerEventKind::AttachmentsLoaded { .. }
            | WorkerEventKind::RemoteAttachmentsListed { .. }
            | WorkerEventKind::RemoteAttachmentCompleted(_)
            | WorkerEventKind::DraftPrepared { .. }
            | WorkerEventKind::ComposePrepared { .. }
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
        // Retry an immediately-opened composer after the mailbox's authoritative
        // identity is known, without waiting for another keypress.
        if matches!(group, EventGroup::Mailbox | EventGroup::Content) {
            self.try_prepare_open_composer();
        }
    }
}
