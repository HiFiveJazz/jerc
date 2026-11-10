//! Worker event dispatch. Each event has one owner: mailbox sync, MIME,
//! outgoing mutations, or errors. The UI thread remains the sole owner of
//! application state; these handlers never mutate it from a worker thread.

use super::*;

mod content;
mod errors;
mod mailbox;
mod outgoing;

#[derive(Debug, Clone, Copy)]
enum EventGroup {
    Mailbox,
    Content,
    Outgoing,
    Errors,
}

impl App {
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
