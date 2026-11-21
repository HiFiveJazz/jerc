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
    /// Revalidate a displayed cached count without treating it as a new-mail baseline.
    Reconcile,
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
        // A pending reconciliation must establish a server baseline before
        // an IDLE/poll can infer new mail from the last known count.
        let reason = match (existing.map(|request| request.reason), reason) {
            (Some(StatusReason::Reconcile), _) | (_, StatusReason::Reconcile) => {
                StatusReason::Reconcile
            }
            (Some(StatusReason::Refresh), _) => StatusReason::Refresh,
            (_, reason) => reason,
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
                    && (request.reason != StatusReason::Initial || !mailbox.counts_known)
            })
        });
    }

    fn pop_ready(&mut self, mailboxes: &[Mailbox]) -> Option<StatusRequest> {
        while let Some(request) = self.pending.pop_front() {
            if mailboxes.iter().any(|mailbox| {
                mailbox.name == request.name
                    && (request.reason != StatusReason::Initial || !mailbox.counts_known)
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
            let reason = if in_flight.request.reason == StatusReason::Reconcile {
                StatusReason::Reconcile
            } else {
                StatusReason::Refresh
            };
            self.enqueue(in_flight.request.name, reason);
        }
    }

    pub(super) fn busy(&self) -> bool {
        self.in_flight.is_some()
    }
}

impl AccountState {
    /// A discovery response owns names, roles and ordering. IMAP LIST has no
    /// counts; keep the last known values while scheduling STATUS separately.
    /// Protocols supplying authoritative counts (e.g. JMAP) need no STATUS.
    pub(super) fn reconcile_mailbox_list(&mut self, mut discovered: Vec<Mailbox>) {
        self.status_requests.pending.retain(|request| {
            discovered.iter().any(|mailbox| {
                mailbox.name == request.name
                    && (!mailbox.counts_known || request.reason == StatusReason::Refresh)
            })
        });
        let previous: HashMap<_, _> = self
            .mailboxes
            .iter()
            .filter(|mailbox| mailbox.counts_known)
            .map(|mailbox| (mailbox.name.as_str(), (mailbox.total, mailbox.unread)))
            .collect();
        for mailbox in &mut discovered {
            if mailbox.counts_known {
                continue;
            }
            if let Some(&(total, unread)) = previous.get(mailbox.name.as_str()) {
                mailbox.total = total;
                mailbox.unread = unread;
                mailbox.counts_known = true;
            }
            let reason = if mailbox.counts_known {
                StatusReason::Reconcile
            } else {
                StatusReason::Initial
            };
            self.status_requests.enqueue(mailbox.name.clone(), reason);
        }
        let changed = discovered
            .iter()
            .filter(|mailbox| {
                mailbox.counts_known
                    && previous
                        .get(mailbox.name.as_str())
                        .is_some_and(|counts| *counts != (mailbox.total, mailbox.unread))
                    && self.mailbox_cache.contains_key(&mailbox.name)
            })
            .map(|mailbox| mailbox.name.clone())
            .collect::<Vec<_>>();
        self.mailboxes = discovered;
        self.mailbox_warm_queue
            .retain(|name| self.mailboxes.iter().any(|mailbox| &mailbox.name == name));
        if self
            .mailbox_warm_request
            .as_ref()
            .is_some_and(|(_, name)| !self.mailboxes.iter().any(|mailbox| &mailbox.name == name))
        {
            self.mailbox_warm_request = None;
        }
        for name in changed {
            if !self.mailbox_warm_queue.contains(&name) {
                self.mailbox_warm_queue.push_back(name);
            }
        }
    }

    /// A mutation invalidates freshness, not the last known display value.
    pub(super) fn queue_count_reconciliation(&mut self, name: &str) {
        if let Some(mailbox) = self.mailboxes.iter().find(|mailbox| mailbox.name == name) {
            let reason = if mailbox.counts_known {
                StatusReason::Reconcile
            } else {
                StatusReason::Initial
            };
            self.status_requests.enqueue(name.to_owned(), reason);
        }
    }

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
            || account.mailboxes_loading
            || account.messages_loading
            || !account.pending_bodies.is_empty()
            || !account.pending_mutation_labels.is_empty()
        {
            return;
        }
        // A JMAP push is account-wide: reconcile all mailbox metadata with
        // one list/delta request rather than issuing STATUS for each folder.
        // A dirty bit keeps at most one follow-up if more events arrive.
        if account.pending_account_refresh
            && account.page_inflight.is_none()
            && account.pending_seen.is_empty()
            && account.request_mailbox_list()
        {
            account.pending_account_refresh = false;
            account.push_list_inflight = true;
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
    fn cached_counts_do_not_skip_status_or_trigger_startup_notifications() {
        let mut queue = MailboxStatusQueue::default();
        queue.enqueue("INBOX".into(), StatusReason::Reconcile);
        queue.enqueue("INBOX".into(), StatusReason::Refresh);
        queue.retain_mailboxes(&[known("INBOX", true)]);
        let request = queue.pop_ready(&[known("INBOX", true)]).unwrap();
        assert_eq!(request.reason, StatusReason::Reconcile);
        queue.start(10, request);
        queue.reset_connection();
        assert!(queue.finish(10).is_none());
        let request = queue.pop_ready(&[known("INBOX", true)]).unwrap();
        assert_eq!(request.reason, StatusReason::Reconcile);
        queue.start(11, request);
        // An arrival during reconciliation still merits a later refresh.
        queue.enqueue("INBOX".into(), StatusReason::Refresh);
        assert_eq!(queue.finish(11).unwrap().reason, StatusReason::Reconcile);
        assert_eq!(
            queue.pop_ready(&[known("INBOX", true)]).unwrap().reason,
            StatusReason::Refresh
        );
    }

    #[test]
    fn reconcile_requests_for_removed_folders_are_discarded() {
        let mut queue = MailboxStatusQueue::default();
        queue.enqueue("Deleted".into(), StatusReason::Reconcile);
        queue.retain_mailboxes(&[known("INBOX", true)]);
        assert!(queue.pop_ready(&[known("INBOX", true)]).is_none());
    }

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
