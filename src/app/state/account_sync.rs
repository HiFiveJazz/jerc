//! Owns the account-level STATUS request lifecycle.
//!
//! Initial mailbox counts, periodic Inbox polling, and IMAP IDLE wakeups all
//! share one bounded queue and exactly one correlated in-flight request.  This
//! is deliberately separate from interactive mailbox and MIME-body fetching.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusReason {
    /// Initial counts are not proof that a message arrived during this session.
    Initial,
    /// An IDLE wakeup or a periodic poll may report a new message.
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusRequest {
    pub(super) name: String,
    pub(super) reason: StatusReason,
}

#[derive(Debug)]
struct StatusInFlight {
    request_id: u64,
    request: StatusRequest,
}

/// At most one queued request per mailbox, regardless of the number of IDLE
/// notifications. The worker has its own small bounded channel; this queue
/// must never grow with the number of notifications received.
#[derive(Debug, Default)]
pub(super) struct MailboxStatusQueue {
    pending: VecDeque<StatusRequest>,
    in_flight: Option<StatusInFlight>,
}

impl MailboxStatusQueue {
    fn enqueue(&mut self, name: String, reason: StatusReason) {
        let existing = self
            .pending
            .iter()
            .position(|request| request.name == name)
            .and_then(|index| self.pending.remove(index));
        let reason = if existing
            .as_ref()
            .is_some_and(|request| request.reason == StatusReason::Refresh)
        {
            StatusReason::Refresh
        } else {
            reason
        };
        let request = StatusRequest { name, reason };
        if reason == StatusReason::Refresh {
            self.pending.push_front(request);
        } else {
            self.pending.push_back(request);
        }
    }

    fn reprioritize(&mut self, name: &str) {
        if let Some(index) = self.pending.iter().position(|request| request.name == name)
            && let Some(request) = self.pending.remove(index)
        {
            self.pending.push_front(request);
        }
    }

    fn retain_mailboxes(&mut self, mailboxes: &[Mailbox]) {
        self.pending.retain(|request| {
            mailboxes.iter().any(|mailbox| {
                mailbox.name == request.name
                    && (request.reason == StatusReason::Refresh || !mailbox.counts_known)
            })
        });
    }

    fn pop_ready(&mut self, mailboxes: &[Mailbox]) -> Option<StatusRequest> {
        while let Some(request) = self.pending.pop_front() {
            if mailboxes.iter().any(|mailbox| {
                mailbox.name == request.name
                    && (request.reason == StatusReason::Refresh || !mailbox.counts_known)
            }) {
                return Some(request);
            }
        }
        None
    }

    fn start(&mut self, request_id: u64, request: StatusRequest) {
        debug_assert!(self.in_flight.is_none());
        self.in_flight = Some(StatusInFlight {
            request_id,
            request,
        });
    }

    /// Stale completions from another request must not clear the live request.
    pub(super) fn finish(&mut self, request_id: u64) -> Option<StatusRequest> {
        if self.in_flight.as_ref()?.request_id != request_id {
            return None;
        }
        self.in_flight.take().map(|in_flight| in_flight.request)
    }

    fn busy(&self) -> bool {
        self.in_flight.is_some()
    }
}

impl AccountState {
    pub(super) fn queue_unknown_mailbox_statuses(&mut self, priority: Option<&str>) {
        self.status_requests.retain_mailboxes(&self.mailboxes);
        for mailbox in &self.mailboxes {
            if !mailbox.counts_known {
                self.status_requests
                    .enqueue(mailbox.name.clone(), StatusReason::Initial);
            }
        }
        if let Some(name) = priority {
            self.status_requests.reprioritize(name);
        }
    }

    pub(super) fn queue_status_refresh(&mut self, name: String) {
        // Never accumulate arbitrary names from a stale watcher or an
        // account reload. Pending STATUS work is bounded by folder count.
        if self.mailboxes.iter().any(|mailbox| mailbox.name == name) {
            self.status_requests.enqueue(name, StatusReason::Refresh);
        }
    }
}

impl App {
    /// Poll just the Inbox count. No bodies and no full folder scans.
    pub fn poll_inboxes(&mut self) {
        for account in &mut self.accounts {
            if account.capabilities.is_none()
                || account.mailboxes_loading
                || account.status_requests.busy()
                || !account.pending_mutation_labels.is_empty()
            {
                continue;
            }
            let Some(inbox) = account
                .mailboxes
                .iter()
                .find(|mailbox| mailbox.role == MailboxRole::Inbox)
                .map(|mailbox| mailbox.name.clone())
            else {
                continue;
            };
            account.queue_status_refresh(inbox);
            Self::pump_mailbox_status(account);
        }
    }

    pub(super) fn pump_mailbox_status(account: &mut AccountState) {
        if account.status_requests.busy()
            || account.messages_loading
            || !account.pending_bodies.is_empty()
            || !account.pending_mutation_labels.is_empty()
        {
            return;
        }
        if let Some(request) = account.status_requests.pop_ready(&account.mailboxes) {
            match account
                .worker
                .post(ActionKind::MailboxStatus(request.name.clone()))
            {
                Ok(request_id) => account.status_requests.start(request_id, request),
                Err(_) => account
                    .status_requests
                    .enqueue(request.name, request.reason),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(name: &str, counts_known: bool) -> Mailbox {
        // Use the same mailbox constructor/parser as the rest of the test suite.
        Mailbox {
            name: name.to_owned(),
            role: MailboxRole::Inbox,
            total: 0,
            unread: 0,
            counts_known,
        }
    }

    #[test]
    fn idle_burst_stays_bounded_and_upgrades_initial_status() {
        let mut queue = MailboxStatusQueue::default();
        queue.enqueue("INBOX".into(), StatusReason::Initial);
        for _ in 0..10_000 {
            queue.enqueue("INBOX".into(), StatusReason::Refresh);
        }
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(
            queue.pop_ready(&[known("INBOX", true)]).unwrap().reason,
            StatusReason::Refresh
        );
    }

    #[test]
    fn arrival_during_inflight_status_is_rechecked_and_stale_ids_ignored() {
        let mut queue = MailboxStatusQueue::default();
        queue.enqueue("INBOX".into(), StatusReason::Initial);
        let request = queue.pop_ready(&[known("INBOX", false)]).unwrap();
        queue.start(42, request);
        queue.enqueue("INBOX".into(), StatusReason::Refresh);
        assert!(queue.finish(41).is_none());
        assert!(queue.busy());
        assert_eq!(queue.finish(42).unwrap().reason, StatusReason::Initial);
        assert_eq!(
            queue.pop_ready(&[known("INBOX", true)]).unwrap().reason,
            StatusReason::Refresh
        );
        assert!(!queue.busy());
    }

    #[test]
    fn initial_count_requests_are_discarded_after_counts_become_known() {
        let mut queue = MailboxStatusQueue::default();
        queue.enqueue("INBOX".into(), StatusReason::Initial);
        assert!(queue.pop_ready(&[known("INBOX", true)]).is_none());
        queue.enqueue("removed".into(), StatusReason::Refresh);
        queue.retain_mailboxes(&[known("INBOX", true)]);
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn pending_refresh_keeps_precedence_over_requeued_initial_count() {
        let mut queue = MailboxStatusQueue::default();
        queue.enqueue("INBOX".into(), StatusReason::Refresh);
        queue.enqueue("INBOX".into(), StatusReason::Initial);
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(
            queue.pop_ready(&[known("INBOX", true)]).unwrap().reason,
            StatusReason::Refresh
        );
    }
}
