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

    pub(super) fn start(&mut self, request_id: u64, request: StatusRequest) {
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

    /// A background STATUS request may never finish when its connection dies.
    /// Clear only that request's correlation, and retry it after reconnection.
    /// Late replies must not be applied to the new connection's counts.
    pub(super) fn reset_connection(&mut self) {
        if let Some(in_flight) = self.in_flight.take() {
            self.enqueue(in_flight.request.name, StatusReason::Refresh);
        }
    }

    pub(super) fn busy(&self) -> bool {
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

/// Periodic fallback checks both the Inbox and the displayed non-Inbox
/// mailbox. IDLE may not be advertised, may disconnect, or may be disabled
/// for huge folders. Only metadata-only STATUS requests are issued.
fn periodic_status_targets(mailboxes: &[Mailbox], active: Option<&str>) -> Vec<String> {
    let mut targets = mailboxes
        .iter()
        .find(|mailbox| mailbox.role == MailboxRole::Inbox)
        .map(|mailbox| vec![mailbox.name.clone()])
        .unwrap_or_default();
    if let Some(name) = active
        && !targets.iter().any(|target| target == name)
        && mailboxes.iter().any(|mailbox| mailbox.name == name)
    {
        targets.push(name.to_owned());
    }
    targets
}

impl App {
    /// Poll Inbox counts for all accounts and the active mailbox for the
    /// visible account. No bodies and no full folder scans.
    pub fn poll_inboxes(&mut self) {
        let visible_account = self.account_index;
        for (index, account) in self.accounts.iter_mut().enumerate() {
            if account.capabilities.is_none()
                || account.connecting
                || account.mailboxes_loading
                || account.status_requests.busy()
                || !account.pending_mutation_labels.is_empty()
            {
                continue;
            }
            let active = (index == visible_account)
                .then_some(account.current_mailbox.as_deref())
                .flatten();
            for name in periodic_status_targets(&account.mailboxes, active) {
                account.queue_status_refresh(name);
            }
            Self::pump_mailbox_status(account);
        }
    }

    pub(super) fn pump_mailbox_status(account: &mut AccountState) {
        if account.capabilities.is_none()
            || account.connecting
            || account.status_requests.busy()
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

    #[test]
    fn periodic_status_fallback_includes_only_inbox_and_active_folder() {
        let folders = vec![
            Mailbox::new("INBOX", MailboxRole::Inbox),
            Mailbox::new("Drafts", MailboxRole::Drafts),
            Mailbox::new("Sent", MailboxRole::Sent),
        ];
        assert_eq!(periodic_status_targets(&folders, None), vec!["INBOX"]);
        assert_eq!(
            periodic_status_targets(&folders, Some("INBOX")),
            vec!["INBOX"]
        );
        assert_eq!(
            periodic_status_targets(&folders, Some("Drafts")),
            vec!["INBOX", "Drafts"]
        );
        assert_eq!(
            periodic_status_targets(&folders, Some("Deleted")),
            vec!["INBOX"]
        );
        assert_eq!(
            periodic_status_targets(&folders[..1], Some("Drafts")),
            vec!["INBOX"]
        );
    }

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

#[cfg(test)]
mod reconnect_lifecycle_tests {
    use super::*;

    #[test]
    fn reset_cancels_inflight_and_preserves_refresh_for_later() {
        let mut queue = MailboxStatusQueue::default();
        queue.start(
            100,
            StatusRequest {
                name: "INBOX".into(),
                reason: StatusReason::Initial,
            },
        );
        queue.reset_connection();
        assert!(!queue.busy());
        assert!(queue.finish(100).is_none());
        let recovered = queue
            .pop_ready(&[Mailbox {
                name: "INBOX".into(),
                role: MailboxRole::Inbox,
                total: 1,
                unread: 0,
                counts_known: true,
            }])
            .unwrap();
        assert_eq!(recovered.reason, StatusReason::Refresh);
    }

    #[test]
    fn reset_is_idempotent_and_does_not_duplicate_pending_refresh() {
        let mut queue = MailboxStatusQueue::default();
        queue.enqueue("INBOX".into(), StatusReason::Refresh);
        queue.start(
            101,
            StatusRequest {
                name: "INBOX".into(),
                reason: StatusReason::Initial,
            },
        );
        queue.reset_connection();
        queue.reset_connection();
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(queue.pending[0].reason, StatusReason::Refresh);
    }

    #[test]
    fn late_completion_after_reset_does_not_evict_new_inflight_request() {
        let mut queue = MailboxStatusQueue::default();
        queue.start(
            102,
            StatusRequest {
                name: "INBOX".into(),
                reason: StatusReason::Refresh,
            },
        );
        queue.reset_connection();
        let request = queue
            .pop_ready(&[Mailbox {
                name: "INBOX".into(),
                role: MailboxRole::Inbox,
                total: 1,
                unread: 0,
                counts_known: true,
            }])
            .unwrap();
        queue.start(103, request);
        assert!(queue.finish(102).is_none());
        assert!(queue.busy());
        assert!(queue.finish(103).is_some());
    }
}
