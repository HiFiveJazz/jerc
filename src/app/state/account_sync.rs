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

/// Counts used for notifications must come from live server observations,
/// not cached/optimistic UI totals. Keep only one Inbox baseline per account.
#[derive(Debug, Default)]
pub(super) struct InboxNotifications {
    baseline: Option<InboxObservation>,
    checkpoint: Option<crate::backend::InboxCheckpoint>,
    pending: VecDeque<(u64, i64)>,
    previewed: VecDeque<MessageId>,
}

#[derive(Debug)]
struct InboxObservation {
    name: String,
    total: usize,
    at: i64,
    request_id: u64,
}

impl InboxNotifications {
    /// Returns the count-only fallback and live observation timestamps.
    /// The first live observation and reconciliation never announce arrivals.
    pub(super) fn observe(
        &mut self,
        name: &str,
        total: usize,
        from_poll: bool,
        at: i64,
        request_id: u64,
    ) -> Option<(usize, i64, i64)> {
        if self
            .baseline
            .as_ref()
            .is_some_and(|old| request_id <= old.request_id)
        {
            return None;
        }
        let old = self.baseline.replace(InboxObservation {
            name: name.to_owned(),
            total,
            at,
            request_id,
        })?;
        let count = inbox_new_mail_count(from_poll, old.name == name, old.total, total);
        (count > 0).then_some((count, old.at, at))
    }

    pub(super) fn checkpoint_for(&self, name: &str) -> Option<crate::backend::InboxCheckpoint> {
        self.baseline.as_ref().filter(|old| old.name == name)?;
        self.checkpoint.clone()
    }

    pub(super) fn needs_baseline(&self, name: &str) -> bool {
        self.baseline.as_ref().is_none_or(|old| old.name != name)
    }

    pub(super) fn observe_check(
        &mut self,
        name: &str,
        total: usize,
        from_poll: bool,
        at: i64,
        request_id: u64,
        check: Option<crate::backend::InboxCheck>,
    ) -> Option<(usize, Option<MessageId>, i64)> {
        if self
            .baseline
            .as_ref()
            .is_some_and(|old| request_id <= old.request_id)
        {
            return None;
        }
        let had_checkpoint = self.checkpoint_for(name).is_some();
        let legacy = self.observe(name, total, from_poll, at, request_id);
        let Some(check) = check else {
            self.checkpoint = None;
            return if had_checkpoint {
                None
            } else {
                legacy.map(|(count, _, through)| (count, None, through))
            };
        };
        // If identity support is unavailable, generic count alerts remain
        // possible only between two count-only observations. Never infer
        // a subject from a count. Switching generations silently rebaselines.
        let has_checkpoint = check.checkpoint.is_some();
        self.checkpoint = check.checkpoint;
        if from_poll && had_checkpoint && has_checkpoint && check.count > 0 {
            return Some((check.count, check.candidate, at));
        }
        if !had_checkpoint && !has_checkpoint {
            return legacy.map(|(count, _, through)| (count, None, through));
        }
        None
    }

    pub(super) fn clear_pending(&mut self) {
        self.pending.clear();
    }

    pub(super) fn reset(&mut self) {
        self.baseline = None;
        self.checkpoint = None;
        self.clear_pending();
        self.previewed.clear();
    }

    fn start_preview(&mut self, request_id: u64, at: i64) {
        self.pending.push_back((request_id, at));
        // A stalled worker must not accumulate unbounded preview requests.
        while self.pending.len() > 16 {
            self.pending.pop_front();
        }
    }

    pub(super) fn finish_preview(&mut self, request_id: u64, now: i64) -> bool {
        let Some(index) = self.pending.iter().position(|(id, _)| *id == request_id) else {
            return false;
        };
        let (_, at) = self.pending.remove(index).expect("located pending preview");
        // Discard responses that were delayed behind other work or a clock jump.
        (0..=60).contains(&now.saturating_sub(at))
    }

    pub(super) fn first_preview(&mut self, id: MessageId) -> bool {
        if self.previewed.contains(&id) {
            return false;
        }
        self.previewed.push_back(id);
        while self.previewed.len() > 32 {
            self.previewed.pop_front();
        }
        true
    }
}

impl AccountState {
    /// Only a verified arrival may cause a MIME fetch. A generic count
    /// notice is sufficient when the backend cannot establish an identity.
    pub(super) fn queue_new_mail_preview(
        &mut self,
        mailbox: String,
        count: usize,
        candidate: Option<MessageId>,
        at: i64,
    ) -> Option<NewMailNotice> {
        if let Some(id) = candidate
            && let Ok(request_id) =
                self.worker
                    .post(ActionKind::NewMailPreview { mailbox, count, id })
        {
            self.inbox_notifications.start_preview(request_id, at);
            return None;
        }
        Some(NewMailNotice {
            account: self.name.clone(),
            count,
            sender: String::new(),
            subject: String::new(),
            excerpt: String::new(),
        })
    }

    /// A discovery response owns names, roles and ordering. IMAP LIST has no
    /// counts; keep the last known values while scheduling STATUS separately.
    /// Protocols supplying authoritative counts (e.g. JMAP) need no count fanout.
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
            let kind =
                if account.mailboxes.iter().any(|mailbox| {
                    mailbox.name == request.name && mailbox.role == MailboxRole::Inbox
                }) {
                    ActionKind::InboxStatus {
                        name: request.name.clone(),
                        previous: account.inbox_notifications.checkpoint_for(&request.name),
                    }
                } else {
                    ActionKind::MailboxStatus(request.name.clone())
                };
            match account.worker.post(kind) {
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
    fn notification_baseline_is_live_and_ignores_stale_observations() {
        let mut notices = InboxNotifications::default();
        assert_eq!(notices.observe("INBOX", 500, true, 100, 1), None);
        assert_eq!(
            notices.observe("INBOX", 502, true, 110, 2),
            Some((2, 100, 110))
        );
        assert_eq!(notices.observe("INBOX", 400, true, 115, 1), None);
        assert_eq!(notices.observe("INBOX", 502, true, 120, 3), None);
        assert_eq!(notices.observe("INBOX", 510, false, 130, 4), None);
        assert_eq!(
            notices.observe("INBOX", 511, true, 140, 5),
            Some((1, 130, 140))
        );
        assert_eq!(notices.observe("Other Inbox", 900, true, 150, 6), None);
        notices.reset();
        assert_eq!(notices.observe("Other Inbox", 950, true, 160, 7), None);
        assert_eq!(
            notices.observe("Other Inbox", 951, true, 170, 8),
            Some((1, 160, 170))
        );
    }

    #[test]
    fn identity_observation_detects_constant_count_and_rejects_stale_generation() {
        use crate::backend::{InboxCheck, InboxCheckpoint};
        let mut notices = InboxNotifications::default();
        let check = |next, count| InboxCheck {
            checkpoint: Some(InboxCheckpoint::Imap { validity: 1, next }),
            count,
            candidate: (count > 0).then(|| MessageId("imap-uid:1:10".into())),
        };
        assert!(
            notices
                .observe_check("INBOX", 10, true, 100, 1, Some(check(10, 0)))
                .is_none()
        );
        assert_eq!(
            notices.observe_check("INBOX", 10, true, 101, 2, Some(check(11, 1))),
            Some((1, Some(MessageId("imap-uid:1:10".into())), 101))
        );
        assert!(
            notices
                .observe_check("INBOX", 11, true, 102, 1, Some(check(12, 1)))
                .is_none()
        );
        assert!(
            notices
                .observe_check("INBOX", 11, true, 103, 3, Some(check(11, 0)))
                .is_none()
        );
        notices.reset();
        assert!(
            notices
                .observe_check("INBOX", 12, true, 104, 4, Some(check(12, 1)))
                .is_none()
        );
        assert!(
            notices
                .observe_check("INBOX", 100, true, 105, 5, Some(InboxCheck::default()))
                .is_none()
        );
        assert_eq!(
            notices.observe_check("INBOX", 101, true, 106, 6, Some(InboxCheck::default())),
            Some((1, None, 106))
        );
    }

    #[test]
    fn notification_previews_are_single_use_bounded_and_expire() {
        let mut notices = InboxNotifications::default();
        notices.start_preview(1, 100);
        notices.start_preview(2, 101);
        assert!(!notices.finish_preview(99, 102));
        assert!(notices.finish_preview(2, 102));
        assert!(!notices.finish_preview(2, 102));
        assert!(!notices.finish_preview(1, 161));
        notices.start_preview(3, 200);
        assert!(!notices.finish_preview(3, 199));
        notices.start_preview(4, 210);
        notices.clear_pending();
        assert!(!notices.finish_preview(4, 211));
        notices.start_preview(5, 220);
        notices.reset();
        assert!(!notices.finish_preview(5, 221));
        for id in 6..50 {
            notices.start_preview(id, 230);
        }
        assert_eq!(notices.pending.len(), 16);
        assert!(!notices.finish_preview(6, 231));
        assert!(notices.finish_preview(49, 231));
    }

    #[test]
    fn notification_preview_identity_is_not_reused() {
        let mut notices = InboxNotifications::default();
        let id = MessageId("imap-uid:123:9".into());
        assert!(notices.first_preview(id.clone()));
        assert!(!notices.first_preview(id));
        assert!(notices.first_preview(MessageId("imap-uid:456:9".into())));
        for i in 10..100 {
            assert!(notices.first_preview(MessageId(format!("imap-uid:123:{i}"))));
        }
        assert_eq!(notices.previewed.len(), 32);
    }

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
