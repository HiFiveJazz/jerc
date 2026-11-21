//! Reconnect lifecycle regression tests. All requests are correlated by ID;
//! tests deliberately deliver stale, failed and out-of-order worker events.

use super::account_sync::StatusRequest;
use super::*;
use crate::model::{Capabilities, Mailbox, MailboxRole, MessageFlags, MessageId, MessagePage};
use crate::worker::{RuntimeEvent, WorkerEvent, WorkerEventKind, WorkerHandle};
use std::sync::mpsc::{Receiver, sync_channel};

fn fixture() -> (tempfile::TempDir, App, Receiver<RuntimeEvent>) {
    let root = tempfile::tempdir().unwrap();
    for name in ["cur", "new", "tmp"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    let (tx, rx) = sync_channel(128);
    let worker = WorkerHandle::spawn(
        0,
        Box::new(crate::backend::MaildirBackend::new(root.path().into())),
        None,
        None,
        tx,
    );
    let config = Config::single_maildir("Local".into(), root.path().into());
    (root, App::new(&config, vec![worker]), rx)
}

fn event(id: u64, kind: WorkerEventKind) -> WorkerEvent {
    WorkerEvent {
        account: 0,
        request_id: id,
        kind,
    }
}

fn error(id: u64, message: &str) -> WorkerEvent {
    event(id, WorkerEventKind::Error(message.into()))
}

fn inbox() -> Mailbox {
    Mailbox {
        name: "INBOX".into(),
        role: MailboxRole::Inbox,
        total: 1,
        unread: 1,
        counts_known: true,
    }
}

fn online(app: &mut App) {
    let account = &mut app.accounts[0];
    account.capabilities = Some(Capabilities::MAILDIR);
    account.mailboxes = vec![inbox()];
    account.current_mailbox = Some("INBOX".into());
    account
        .cache_identities
        .insert("INBOX".into(), CacheIdentity::Imap(81));
    account.unverified_mailboxes.remove("INBOX");
}

fn cached_message(app: &mut App) -> MessageId {
    let raw = b"From: friend@example.org\r\nSubject: Safe offline message\r\n\r\nhi";
    let id = MessageId("imap-uid:81:9".into());
    app.accounts[0].messages.push(
        crate::mail::parse_summary(id.clone(), raw, raw.len() as u64, MessageFlags::empty())
            .unwrap(),
    );
    id
}

fn waiting_for_list(app: &mut App, id: u64) {
    online(app);
    app.accounts[0].mailbox_list_request_id = Some(id);
    app.accounts[0].mailboxes_loading = true;
}

fn waiting_for_page(app: &mut App, id: u64) {
    online(app);
    let account = &mut app.accounts[0];
    account.page_request_id = Some(id);
    account.page_inflight = Some(("INBOX".into(), 0));
    account.messages_loading = true;
}

fn page() -> MessagePage {
    MessagePage {
        messages: vec![],
        offset: 0,
        next_offset: 0,
        has_more: false,
    }
}

#[test]
fn imap_discovery_preserves_cached_counts_including_zero_and_new_folders() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 500);
    let mut archive = Mailbox::new("Archive", MailboxRole::Archive);
    archive.total = 400;
    archive.unread = 25;
    archive.counts_known = true;
    let mut sent = Mailbox::new("Sent", MailboxRole::Sent);
    sent.counts_known = true;
    app.accounts[0]
        .mailboxes
        .extend([archive, sent, Mailbox::new("Removed", MailboxRole::Other)]);
    app.handle_worker_event(event(
        500,
        WorkerEventKind::Mailboxes(vec![
            Mailbox::new("Archive", MailboxRole::Archive),
            Mailbox::new("INBOX", MailboxRole::Inbox),
            Mailbox::new("Sent", MailboxRole::Sent),
            Mailbox::new("New", MailboxRole::Other),
        ]),
    ));
    let folders = &app.accounts[0].mailboxes;
    assert_eq!(folders.len(), 4);
    assert_eq!(folders[0].name, "Archive");
    assert_eq!((folders[0].total, folders[0].unread), (400, 25));
    assert!(folders[0].counts_known);
    assert_eq!(folders[1], inbox());
    assert!(folders[2].counts_known);
    assert_eq!((folders[2].total, folders[2].unread), (0, 0));
    assert!(!folders[3].counts_known);
    assert!(!folders.iter().any(|folder| folder.name == "Removed"));
    app.accounts[0].mark_disconnected();
    assert_eq!(app.accounts[0].mailboxes[0].unread, 25);
    assert!(app.accounts[0].mailboxes[0].counts_known);
}

#[test]
fn authoritative_discovery_replaces_cached_counts_even_with_zero() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 501);
    app.accounts[0].queue_count_reconciliation("INBOX");
    let mut server = inbox();
    server.total = 0;
    server.unread = 0;
    app.handle_worker_event(event(501, WorkerEventKind::Mailboxes(vec![server.clone()])));
    assert_eq!(app.accounts[0].mailboxes, vec![server]);
    // No extra STATUS is needed for an already authoritative JMAP-style LIST.
    let account = &mut app.accounts[0];
    account.messages_loading = false;
    account.page_inflight = None;
    account.pending_bodies.clear();
    App::pump_mailbox_status(account);
    assert!(!account.status_requests.busy());
}

#[test]
fn cached_status_reconciliation_updates_counts_without_new_mail_alert() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.new_mail_notifications_enabled = true;
    app.new_mail_bell_enabled = true;
    app.accounts[0].status_requests.start(
        502,
        StatusRequest {
            name: "INBOX".into(),
            reason: StatusReason::Reconcile,
        },
    );
    app.handle_worker_event(event(
        502,
        WorkerEventKind::MailboxStatus {
            name: "INBOX".into(),
            total: 30,
            unread: 12,
        },
    ));
    let folder = &app.accounts[0].mailboxes[0];
    assert!(folder.counts_known);
    assert_eq!((folder.total, folder.unread), (30, 12));
    assert_eq!(app.new_mail_since_launch, 0);
    assert!(!app.new_mail_bell_pending);
    assert!(app.new_mail_notices.is_empty());
}

#[test]
fn failed_cached_status_leaves_last_known_counts_visible() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].status_requests.start(
        503,
        StatusRequest {
            name: "INBOX".into(),
            reason: StatusReason::Reconcile,
        },
    );
    app.handle_worker_event(error(503, "background mailbox count: timeout"));
    assert_eq!(app.accounts[0].mailboxes, vec![inbox()]);
    assert!(!app.accounts[0].status_requests.busy());
}

#[test]
fn list_transport_failure_always_clears_footer_spinner() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 10);
    assert!(app.syncing_with_server());
    app.handle_worker_event(error(10, "broken pipe"));
    assert!(!app.syncing_with_server());
    assert!(app.accounts[0].capabilities.is_none());
    assert!(app.accounts[0].mailbox_list_request_id.is_none());
}

#[test]
fn list_permission_error_finishes_spinner_but_keeps_connection() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 11);
    app.handle_worker_event(error(11, "permission denied"));
    assert!(!app.syncing_with_server());
    assert!(app.accounts[0].capabilities.is_some());
}

#[test]
fn unrelated_failure_cannot_cancel_matching_list_request() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 12);
    app.handle_worker_event(error(999, "message not found"));
    assert_eq!(app.accounts[0].mailbox_list_request_id, Some(12));
    assert!(app.syncing_with_server());
}

#[test]
fn stale_list_failure_cannot_clear_newer_list_request() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 22);
    app.handle_worker_event(error(21, "permission denied"));
    assert!(app.accounts[0].mailboxes_loading);
    assert_eq!(app.accounts[0].mailbox_list_request_id, Some(22));
}

#[test]
fn stale_list_result_does_not_replace_folder_names() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 23);
    app.handle_worker_event(event(22, WorkerEventKind::Mailboxes(vec![])));
    assert_eq!(app.accounts[0].mailboxes.len(), 1);
    assert_eq!(app.accounts[0].mailboxes[0].name, "INBOX");
    assert!(app.accounts[0].mailboxes_loading);
}

#[test]
fn matching_empty_list_result_ends_sync() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 24);
    app.handle_worker_event(event(24, WorkerEventKind::Mailboxes(vec![])));
    assert!(!app.syncing_with_server());
    assert!(app.accounts[0].mailbox_list_request_id.is_none());
    assert!(app.accounts[0].mailboxes.is_empty());
}

#[test]
fn stale_connected_result_does_not_override_pending_connect() {
    let (_root, mut app, _rx) = fixture();
    app.accounts[0].connecting = true;
    app.accounts[0].connect_request_id = Some(30);
    app.handle_worker_event(event(
        29,
        WorkerEventKind::Connected {
            capabilities: Capabilities::MAILDIR,
        },
    ));
    assert!(app.accounts[0].connecting);
    assert!(app.accounts[0].capabilities.is_none());
    assert!(app.accounts[0].mailbox_list_request_id.is_none());
}

#[test]
fn matching_connect_result_schedules_correlated_list() {
    let (_root, mut app, _rx) = fixture();
    app.accounts[0].connecting = true;
    app.accounts[0].connect_request_id = Some(31);
    app.handle_worker_event(event(
        31,
        WorkerEventKind::Connected {
            capabilities: Capabilities::MAILDIR,
        },
    ));
    assert!(!app.accounts[0].connecting);
    assert!(app.accounts[0].capabilities.is_some());
    assert!(app.accounts[0].mailboxes_loading);
    assert!(app.accounts[0].mailbox_list_request_id.is_some());
}

#[test]
fn manually_reconnecting_supersedes_old_list_request() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 32);
    assert!(app.accounts[0].request_connect());
    let connect_id = app.accounts[0].connect_request_id.unwrap();
    assert!(!app.accounts[0].mailboxes_loading);
    assert!(app.accounts[0].mailbox_list_request_id.is_none());
    app.handle_worker_event(event(32, WorkerEventKind::Mailboxes(vec![])));
    assert_eq!(app.accounts[0].mailboxes.len(), 1);
    assert_eq!(app.accounts[0].mailboxes[0].name, "INBOX");
    assert_eq!(app.accounts[0].connect_request_id, Some(connect_id));
}

#[test]
fn transport_error_drops_stale_list_and_page_together() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 40);
    cached_message(&mut app);
    app.accounts[0].page_inflight = Some(("INBOX".into(), 0));
    app.accounts[0].page_request_id = Some(41);
    app.accounts[0].messages_loading = true;
    app.handle_worker_event(error(40, "connection reset by peer"));
    assert!(!app.syncing_with_server());
    assert_eq!(app.accounts[0].messages.len(), 1);
    assert!(app.accounts[0].unverified_mailboxes.contains("INBOX"));
}

#[test]
fn generic_transport_error_retires_orphaned_page_request() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 42);
    app.handle_worker_event(error(500, "network is unreachable"));
    assert!(app.accounts[0].page_request_id.is_none());
    assert!(app.accounts[0].page_inflight.is_none());
    assert!(!app.accounts[0].messages_loading);
    assert!(!app.syncing_with_server());
}

#[test]
fn unrelated_application_error_does_not_cancel_page_request() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 43);
    app.handle_worker_event(error(501, "message rejected by policy"));
    assert_eq!(app.accounts[0].page_request_id, Some(43));
    assert!(app.syncing_with_server());
}

#[test]
fn old_page_result_after_disconnect_cannot_overwrite_cache() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 44);
    cached_message(&mut app);
    app.handle_worker_event(error(502, "connection closed"));
    app.handle_worker_event(event(
        44,
        WorkerEventKind::MailboxOpened {
            name: "INBOX".into(),
            page: page(),
            identity: Some(CacheIdentity::Imap(81)),
            uid_next: None,
            elapsed_micros: 1,
        },
    ));
    assert_eq!(app.accounts[0].messages.len(), 1);
    assert!(!app.syncing_with_server());
}

#[test]
fn matching_page_success_finishes_spinner() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 45);
    app.handle_worker_event(event(
        45,
        WorkerEventKind::MailboxOpened {
            name: "INBOX".into(),
            page: page(),
            identity: Some(CacheIdentity::Imap(81)),
            uid_next: None,
            elapsed_micros: 5,
        },
    ));
    assert!(!app.syncing_with_server());
    assert!(app.accounts[0].page_inflight.is_none());
}

#[test]
fn stale_page_success_keeps_new_page_inflight() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 46);
    app.handle_worker_event(event(
        45,
        WorkerEventKind::MailboxOpened {
            name: "INBOX".into(),
            page: page(),
            identity: Some(CacheIdentity::Imap(81)),
            uid_next: None,
            elapsed_micros: 5,
        },
    ));
    assert_eq!(app.accounts[0].page_request_id, Some(46));
    assert!(app.syncing_with_server());
}

#[test]
fn matching_page_failure_finishes_spinner_without_erasing_cache() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 47);
    cached_message(&mut app);
    app.handle_worker_event(event(
        47,
        WorkerEventKind::MailboxOpenFailed {
            name: "INBOX".into(),
            error: "permission denied".into(),
        },
    ));
    assert!(!app.syncing_with_server());
    assert_eq!(app.accounts[0].messages.len(), 1);
    assert!(app.accounts[0].capabilities.is_some());
}

#[test]
fn stale_page_failure_cannot_clear_newer_page() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 48);
    app.handle_worker_event(event(
        47,
        WorkerEventKind::MailboxOpenFailed {
            name: "INBOX".into(),
            error: "broken pipe".into(),
        },
    ));
    assert_eq!(app.accounts[0].page_request_id, Some(48));
    assert!(app.accounts[0].capabilities.is_some());
    assert!(app.syncing_with_server());
}

#[test]
fn page_transport_failure_clears_spinner_and_marks_cache_unverified() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 49);
    cached_message(&mut app);
    app.handle_worker_event(event(
        49,
        WorkerEventKind::MailboxOpenFailed {
            name: "INBOX".into(),
            error: "broken pipe".into(),
        },
    ));
    assert!(!app.syncing_with_server());
    assert!(app.accounts[0].capabilities.is_none());
    assert!(app.accounts[0].unverified_mailboxes.contains("INBOX"));
    assert_eq!(app.accounts[0].messages.len(), 1);
}

#[test]
fn priority_body_transport_failure_does_not_kill_foreground_page() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 50);
    app.accounts[0].body_request_id = Some(51);
    app.accounts[0]
        .pending_bodies
        .insert(MessageId("imap-uid:81:9".into()));
    app.handle_worker_event(event(
        51,
        WorkerEventKind::BodyLoadFailed {
            error: "connection reset by peer".into(),
        },
    ));
    assert!(app.accounts[0].capabilities.is_some());
    assert_eq!(app.accounts[0].page_request_id, Some(50));
    assert!(app.accounts[0].pending_bodies.is_empty());
}

#[test]
fn stale_body_failure_cannot_erase_newer_body_request() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 52);
    app.accounts[0].body_request_id = Some(53);
    let id = MessageId("imap-uid:81:9".into());
    app.accounts[0].pending_bodies.insert(id.clone());
    app.handle_worker_event(event(
        52,
        WorkerEventKind::BodyLoadFailed {
            error: "broken pipe".into(),
        },
    ));
    assert_eq!(app.accounts[0].body_request_id, Some(53));
    assert!(app.accounts[0].pending_bodies.contains(&id));
}

#[test]
fn background_status_transport_error_preserves_foreground_page() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 54);
    app.accounts[0].status_requests.start(
        55,
        StatusRequest {
            name: "INBOX".into(),
            reason: StatusReason::Refresh,
        },
    );
    app.handle_worker_event(error(55, "background mailbox count: broken pipe"));
    assert_eq!(app.accounts[0].page_request_id, Some(54));
    assert!(app.accounts[0].capabilities.is_some());
    assert!(app.syncing_with_server());
    assert!(!app.accounts[0].status_requests.busy());
}

#[test]
fn unrelated_status_error_cannot_cancel_outstanding_list() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 56);
    app.accounts[0].status_requests.start(
        57,
        StatusRequest {
            name: "INBOX".into(),
            reason: StatusReason::Initial,
        },
    );
    app.handle_worker_event(error(57, "background mailbox count: timeout"));
    assert_eq!(app.accounts[0].mailbox_list_request_id, Some(56));
    assert!(app.accounts[0].mailboxes_loading);
    assert!(app.accounts[0].capabilities.is_some());
}

#[test]
fn attachment_transport_failure_preserves_foreground_sync() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 58);
    let id = MessageId("imap-uid:81:9".into());
    app.attachment_pending_request_id = Some(59);
    app.attachment_pending = Some(id);
    app.handle_worker_event(error(59, "connection reset by peer"));
    assert_eq!(app.accounts[0].page_request_id, Some(58));
    assert!(app.accounts[0].capabilities.is_some());
    assert!(app.attachment_pending.is_none());
}

#[test]
fn disconnect_does_not_cancel_inflight_connect_from_other_worker_error() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 60);
    app.accounts[0].connecting = true;
    app.accounts[0].connect_request_id = Some(61);
    app.handle_worker_event(error(62, "network unreachable"));
    assert_eq!(app.accounts[0].connect_request_id, Some(61));
    assert!(app.accounts[0].connecting);
    assert!(app.accounts[0].page_request_id.is_none());
}

#[test]
fn correlated_connect_failure_stops_spinner_and_allows_retry() {
    let (_root, mut app, _rx) = fixture();
    app.accounts[0].connecting = true;
    app.accounts[0].connect_request_id = Some(63);
    app.handle_worker_event(error(63, "connecting to IMAP server: network unreachable"));
    assert!(!app.syncing_with_server());
    assert!(app.accounts[0].connect_request_id.is_none());
    app.reconnect_offline_accounts();
    assert!(app.accounts[0].connecting);
}

#[test]
fn worker_stopped_resets_all_read_only_progress_flags() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 64);
    app.accounts[0].connecting = true;
    app.accounts[0].connect_request_id = Some(65);
    app.accounts[0].page_request_id = Some(66);
    app.accounts[0].page_inflight = Some(("INBOX".into(), 0));
    app.accounts[0].messages_loading = true;
    app.handle_worker_event(event(0, WorkerEventKind::Stopped));
    assert!(!app.syncing_with_server());
    assert!(app.accounts[0].capabilities.is_none());
    assert!(app.status.text.contains("worker stopped"));
}

#[test]
fn repeated_disconnect_does_not_erase_cached_messages() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let id = cached_message(&mut app);
    app.accounts[0].mark_disconnected();
    app.accounts[0].mark_disconnected();
    assert_eq!(app.accounts[0].messages[0].id, id);
    assert!(app.accounts[0].unverified_mailboxes.contains("INBOX"));
    assert!(!app.syncing_with_server());
}

#[test]
fn offline_account_never_schedules_unverified_page_fetch() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].mark_disconnected();
    assert!(!app.accounts[0].request_mailbox_page("INBOX".into(), 0));
    assert!(app.accounts[0].page_request_id.is_none());
    assert!(!app.syncing_with_server());
}

#[test]
fn reconnecting_account_never_schedules_page_before_list() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].connecting = true;
    assert!(!app.accounts[0].request_mailbox_page("INBOX".into(), 0));
    assert!(app.accounts[0].page_inflight.is_none());
}

#[test]
fn status_inflight_is_requeued_on_disconnect_and_late_result_ignored() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 67);
    app.accounts[0].status_requests.start(
        68,
        StatusRequest {
            name: "INBOX".into(),
            reason: StatusReason::Initial,
        },
    );
    app.accounts[0].mark_disconnected();
    assert!(!app.accounts[0].status_requests.busy());
    // An old STATUS completion cannot be treated as a fresh count.
    assert!(app.accounts[0].status_requests.finish(68).is_none());
    app.handle_worker_event(event(
        68,
        WorkerEventKind::MailboxStatus {
            name: "INBOX".into(),
            total: 900,
            unread: 899,
        },
    ));
    assert_eq!(app.accounts[0].mailboxes[0].total, 1);
}

#[test]
fn old_status_reply_does_not_finish_new_status() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].status_requests.start(
        70,
        StatusRequest {
            name: "INBOX".into(),
            reason: StatusReason::Refresh,
        },
    );
    app.handle_worker_event(event(
        69,
        WorkerEventKind::MailboxStatus {
            name: "INBOX".into(),
            total: 20,
            unread: 18,
        },
    ));
    assert!(app.accounts[0].status_requests.busy());
    assert_eq!(app.accounts[0].mailboxes[0].total, 1);
}

#[test]
fn disconnected_account_retains_deferred_read_flags() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0]
        .deferred_seen
        .push(crate::cache::DeferredSeen {
            mailbox: "INBOX".into(),
            validity: 81,
            id: MessageId("imap-uid:81:9".into()),
            seen: true,
        });
    app.accounts[0].mark_disconnected();
    assert_eq!(app.accounts[0].deferred_seen.len(), 1);
}

#[test]
fn disconnect_does_not_requeue_uncertain_mailbox_move() {
    use crate::cache::{DeferredMutation, DeferredMutationKind, DeferredMutationState};
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].deferred_mutations.push(DeferredMutation {
        mailbox: "INBOX".into(),
        validity: 81,
        ids: vec![MessageId("imap-uid:81:9".into())],
        kind: DeferredMutationKind::Move {
            destination: "Archive".into(),
        },
        state: DeferredMutationState::NeedsReview,
    });
    app.accounts[0].mark_disconnected();
    assert_eq!(
        app.accounts[0].deferred_mutations[0].state,
        DeferredMutationState::NeedsReview
    );
}

#[test]
fn transport_failure_on_other_account_does_not_cancel_current_spinner() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 72);
    app.handle_worker_event(WorkerEvent {
        account: 99,
        request_id: 73,
        kind: WorkerEventKind::Error("connection reset".into()),
    });
    assert!(app.syncing_with_server());
    assert_eq!(app.accounts[0].page_request_id, Some(72));
}

#[test]
fn footer_stops_spinning_after_failed_mailbox_list() {
    use ratatui::{Terminal, backend::TestBackend};
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 74);
    cached_message(&mut app);
    app.sidebar_visible = false;
    app.handle_worker_event(error(74, "broken pipe"));
    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
    terminal
        .draw(|frame| crate::ui::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let footer = (0..100u16)
        .map(|x| buffer[(x, 11)].symbol())
        .collect::<Vec<_>>()
        .concat();
    assert!(
        !footer.contains("Syncing with server"),
        "stale footer: {footer}"
    );
    let row = (0..100u16)
        .map(|x| buffer[(x, 1)].symbol())
        .collect::<Vec<_>>()
        .concat();
    assert!(
        row.contains("Safe offline message"),
        "cached message lost: {row}"
    );
}

#[test]
fn late_error_from_abandoned_list_cannot_disconnect_new_session() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 80);
    assert!(app.accounts[0].request_connect());
    let active = app.accounts[0].connect_request_id.unwrap();
    app.handle_worker_event(event(
        active,
        WorkerEventKind::Connected {
            capabilities: Capabilities::MAILDIR,
        },
    ));
    let current_list = app.accounts[0].mailbox_list_request_id.unwrap();
    app.handle_worker_event(error(80, "broken pipe"));
    assert_eq!(app.accounts[0].mailbox_list_request_id, Some(current_list));
    assert!(app.accounts[0].capabilities.is_some());
    assert!(app.syncing_with_server());
}

#[test]
fn late_background_status_error_does_not_disconnect_recovered_session() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 81);
    app.handle_worker_event(error(8000, "background mailbox count: connection reset"));
    assert_eq!(app.accounts[0].page_request_id, Some(81));
    assert!(app.accounts[0].capabilities.is_some());
}

#[test]
fn jmap_refresh_preserves_inflight_discovery_and_page_correlation() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 700);
    app.accounts[0].jmap = true;
    app.refresh();
    assert_eq!(app.accounts[0].mailbox_list_request_id, Some(700));
    assert!(!app.accounts[0].connecting);
    app.accounts[0].mailboxes_loading = false;
    app.accounts[0].mailbox_list_request_id = None;
    waiting_for_page(&mut app, 701);
    app.refresh();
    assert_eq!(app.accounts[0].page_request_id, Some(701));
    assert!(!app.accounts[0].connecting);
}

#[test]
fn account_push_bursts_queue_one_followup_without_status_fanout() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].jmap = true;
    app.handle_worker_event(event(710, WorkerEventKind::AccountChanged));
    let request = app.accounts[0].mailbox_list_request_id.unwrap();
    assert!(app.accounts[0].push_list_inflight);
    assert!(!app.accounts[0].pending_account_refresh);
    for id in 711..720 {
        app.handle_worker_event(event(id, WorkerEventKind::AccountChanged));
    }
    assert_eq!(app.accounts[0].mailbox_list_request_id, Some(request));
    assert!(app.accounts[0].pending_account_refresh);
    assert!(!app.accounts[0].status_requests.busy());
    app.handle_worker_event(event(request, WorkerEventKind::Mailboxes(vec![inbox()])));
    assert!(!app.accounts[0].push_list_inflight);
    assert!(app.accounts[0].pending_account_refresh);
    let page_request = app.accounts[0].page_request_id.unwrap();
    app.handle_worker_event(event(
        page_request,
        WorkerEventKind::MailboxOpened {
            name: "INBOX".into(),
            page: page(),
            identity: Some(CacheIdentity::Imap(81)),
            uid_next: None,
            elapsed_micros: 0,
        },
    ));
    assert!(app.accounts[0].push_list_inflight);
    assert!(!app.accounts[0].pending_account_refresh);
    assert_ne!(app.accounts[0].mailbox_list_request_id, Some(request));
}

#[test]
fn refresh_does_not_queue_multiple_connects_while_one_is_pending() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 82);
    cached_message(&mut app);
    app.refresh();
    let id = app.accounts[0].connect_request_id;
    for _ in 0..25 {
        app.refresh();
        app.reconnect_offline_accounts();
    }
    assert!(id.is_some());
    assert_eq!(app.accounts[0].connect_request_id, id);
    assert_eq!(app.accounts[0].messages.len(), 1);
}

#[test]
fn background_status_and_priority_body_errors_never_cancel_active_connect() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].connecting = true;
    app.accounts[0].connect_request_id = Some(84);
    app.accounts[0].status_requests.start(
        85,
        StatusRequest {
            name: "INBOX".into(),
            reason: StatusReason::Refresh,
        },
    );
    app.accounts[0].body_request_id = Some(86);
    app.handle_worker_event(error(85, "background mailbox count: timeout"));
    app.handle_worker_event(event(
        86,
        WorkerEventKind::BodyLoadFailed {
            error: "unexpected EOF".into(),
        },
    ));
    assert_eq!(app.accounts[0].connect_request_id, Some(84));
    assert!(app.accounts[0].connecting);
}

#[test]
fn disconnected_account_stays_idle_even_with_pending_background_refresh() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].status_requests.start(
        87,
        StatusRequest {
            name: "INBOX".into(),
            reason: StatusReason::Refresh,
        },
    );
    app.accounts[0].mark_disconnected();
    assert!(!app.syncing_with_server());
    assert!(!app.accounts[0].status_requests.busy());
}

#[test]
fn reconnect_cycle_can_repeat_without_stranding_progress_indicators() {
    let (_root, mut app, _rx) = fixture();
    for turn in 0..12 {
        assert!(app.accounts[0].request_connect(), "connect {turn}");
        let connect_id = app.accounts[0].connect_request_id.unwrap();
        app.handle_worker_event(event(
            connect_id,
            WorkerEventKind::Connected {
                capabilities: Capabilities::MAILDIR,
            },
        ));
        let list_id = app.accounts[0].mailbox_list_request_id.unwrap();
        app.handle_worker_event(event(list_id, WorkerEventKind::Mailboxes(vec![])));
        assert!(!app.syncing_with_server(), "connected sync #{turn}");
        app.accounts[0].mark_disconnected();
        assert!(!app.syncing_with_server(), "disconnected sync #{turn}");
    }
}

#[test]
fn abandoned_list_tombstones_remain_memory_bounded() {
    let (_root, mut app, _rx) = fixture();
    for id in 1..=500 {
        app.accounts[0].mailbox_list_request_id = Some(id);
        app.accounts[0].mailboxes_loading = true;
        app.accounts[0].abandon_mailbox_list();
    }
    assert_eq!(app.accounts[0].abandoned_list_requests.len(), 64);
    assert!(app.accounts[0].is_abandoned_list_request(500));
    assert!(!app.accounts[0].is_abandoned_list_request(1));
}

#[test]
fn sync_spinner_is_not_activated_by_status_only_or_pending_review() {
    use crate::cache::{DeferredMutation, DeferredMutationKind, DeferredMutationState};
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].status_requests.start(
        90,
        StatusRequest {
            name: "INBOX".into(),
            reason: StatusReason::Initial,
        },
    );
    app.accounts[0].deferred_mutations.push(DeferredMutation {
        mailbox: "INBOX".into(),
        validity: 81,
        ids: vec![MessageId("imap-uid:81:9".into())],
        kind: DeferredMutationKind::Delete,
        state: DeferredMutationState::NeedsReview,
    });
    assert!(!app.syncing_with_server());
    assert!(
        app.visible_warnings()
            .iter()
            .any(|warning| warning.contains("review"))
    );
}

#[test]
fn stalled_list_request_times_out_without_stranding_spinner() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 100);
    cached_message(&mut app);
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(121));
    app.reconnect_offline_accounts();
    assert!(!app.syncing_with_server());
    assert!(app.accounts[0].capabilities.is_none());
    assert_eq!(app.accounts[0].messages.len(), 1);
    assert!(app.status.text.contains("sync timed out"));
}

#[test]
fn recent_sync_request_cannot_be_timed_out_by_reconnect_timer() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 101);
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(15));
    app.reconnect_offline_accounts();
    assert!(app.syncing_with_server());
    assert_eq!(app.accounts[0].mailbox_list_request_id, Some(101));
}

#[test]
fn stalled_page_request_times_out_but_preserves_cached_rows() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 102);
    let id = cached_message(&mut app);
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(121));
    app.reconnect_offline_accounts();
    assert!(!app.syncing_with_server());
    assert_eq!(app.accounts[0].messages[0].id, id);
    assert!(app.accounts[0].unverified_mailboxes.contains("INBOX"));
}

#[test]
fn stalled_connect_request_releases_lock_and_can_retry() {
    let (_root, mut app, _rx) = fixture();
    app.accounts[0].connecting = true;
    app.accounts[0].connect_request_id = Some(103);
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(121));
    app.reconnect_offline_accounts();
    assert!(!app.accounts[0].connecting);
    assert!(!app.syncing_with_server());
    app.reconnect_offline_accounts();
    assert!(app.accounts[0].connecting);
    assert_ne!(app.accounts[0].connect_request_id, Some(103));
}

#[test]
fn late_list_response_after_timeout_cannot_resurrect_old_session() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 104);
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(121));
    app.reconnect_offline_accounts();
    app.handle_worker_event(event(104, WorkerEventKind::Mailboxes(vec![])));
    assert!(app.accounts[0].capabilities.is_none());
    assert_eq!(app.accounts[0].mailboxes.len(), 1);
    assert!(!app.syncing_with_server());
}

#[test]
fn timeout_preserves_uncertain_write_state_for_manual_review() {
    use crate::cache::{DeferredMutation, DeferredMutationKind, DeferredMutationState};
    let (_root, mut app, _rx) = fixture();
    waiting_for_list(&mut app, 105);
    app.accounts[0].deferred_mutations.push(DeferredMutation {
        mailbox: "INBOX".into(),
        validity: 81,
        ids: vec![MessageId("imap-uid:81:9".into())],
        kind: DeferredMutationKind::Delete,
        state: DeferredMutationState::NeedsReview,
    });
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(121));
    app.reconnect_offline_accounts();
    assert_eq!(
        app.accounts[0].deferred_mutations[0].state,
        DeferredMutationState::NeedsReview
    );
    assert!(
        app.visible_warnings()
            .iter()
            .any(|warning| warning.contains("manual review"))
    );
}

#[test]
fn unrelated_worker_error_must_not_cancel_pending_reply_fetch() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let id = cached_message(&mut app);
    app.pending_compose = Some(PendingCompose {
        account: 0,
        request_id: 110,
        message_id: id,
        intent: ComposeIntent::Reply {
            quote: true,
            audience: ReplyAudience::Sender,
        },
    });
    app.handle_worker_event(error(111, "unrelated mailbox permission denied"));
    assert!(app.pending_compose.is_some());
    assert_eq!(app.pending_compose.as_ref().unwrap().request_id, 110);
}

#[test]
fn matching_worker_error_may_cancel_pending_reply_fetch() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let id = cached_message(&mut app);
    app.pending_compose = Some(PendingCompose {
        account: 0,
        request_id: 112,
        message_id: id,
        intent: ComposeIntent::Forward,
    });
    app.handle_worker_event(error(112, "could not download original MIME"));
    assert!(app.pending_compose.is_none());
}

#[test]
fn old_raw_reply_completion_cannot_replace_newer_compose_request() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let id = cached_message(&mut app);
    app.pending_compose = Some(PendingCompose {
        account: 0,
        request_id: 114,
        message_id: id.clone(),
        intent: ComposeIntent::Forward,
    });
    app.handle_worker_event(event(
        113,
        WorkerEventKind::RawLoaded {
            id,
            raw: b"From: friend@example.org\r\n\r\nold reply".to_vec(),
        },
    ));
    assert!(app.pending_compose.is_some());
    assert!(app.composer.is_none());
}

#[test]
fn retired_foreground_events_cannot_disconnect_a_recovered_account() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let account = &mut app.accounts[0];
    account.foreground_generation = 1;
    account.connecting = true;
    account.connect_request_id = Some(77);
    account.mailboxes_loading = false;
    let old_folders = account.mailboxes.clone();
    app.handle_foreground_worker_event(0, error(10, "connection reset by peer"));
    app.handle_foreground_worker_event(0, event(11, WorkerEventKind::Mailboxes(vec![])));
    app.handle_foreground_worker_event(
        0,
        event(
            12,
            WorkerEventKind::Connected {
                capabilities: Capabilities::MAILDIR,
            },
        ),
    );
    assert_eq!(app.accounts[0].mailboxes.len(), old_folders.len());
    assert!(app.accounts[0].capabilities.is_some());
    assert!(app.accounts[0].connecting);
    assert_eq!(app.accounts[0].connect_request_id, Some(77));
    app.handle_foreground_worker_event(
        1,
        event(
            77,
            WorkerEventKind::Connected {
                capabilities: Capabilities::MAILDIR,
            },
        ),
    );
    assert!(!app.accounts[0].connecting);
    assert!(app.accounts[0].mailboxes_loading);
}

#[test]
fn stale_refresh_recovers_without_waiting_for_old_mailbox_list() {
    let (root, mut app, rx) = fixture();
    let (tx, rx2) = std::sync::mpsc::sync_channel(32);
    let config = Config::single_maildir("Local".into(), root.path().into());
    // The original foreground worker may be blocked in a transport read.
    // Recovery uses a new independently connected worker.
    app.accounts[0]
        .worker
        .enable_foreground_recovery(0, config.accounts[0].clone(), tx);
    online(&mut app);
    let cached = cached_message(&mut app);
    let account = &mut app.accounts[0];
    account.mailboxes_loading = true;
    account.mailbox_list_request_id = Some(99);
    account.sync_progress_at = Some(Instant::now() - Duration::from_secs(16));
    assert!(app.syncing_with_server());
    app.recover_stalled_foreground_accounts();
    assert_eq!(app.accounts[0].foreground_generation, 1);
    assert!(app.accounts[0].connecting);
    assert_eq!(app.accounts[0].messages[0].id, cached);
    let RuntimeEvent::ForegroundWorker { generation, event } = rx2
        .recv_timeout(Duration::from_secs(1))
        .expect("new worker should connect")
    else {
        panic!("expected new foreground connect event")
    };
    assert_eq!(generation, 1);
    assert!(matches!(event.kind, WorkerEventKind::Connected { .. }));
    app.handle_foreground_worker_event(generation, event);
    assert!(app.accounts[0].mailboxes_loading);
    // The original account's worker is still alive; its events are from the
    // old generation and cannot overwrite the new session.
    app.handle_foreground_worker_event(0, error(99, "broken pipe"));
    assert!(app.accounts[0].mailboxes_loading);
    drop(rx);
}

#[test]
fn healthy_short_refresh_does_not_rotate_workers() {
    let (root, mut app, _rx) = fixture();
    let (tx, _events) = std::sync::mpsc::sync_channel(32);
    let config = Config::single_maildir("Local".into(), root.path().into());
    app.accounts[0]
        .worker
        .enable_foreground_recovery(0, config.accounts[0].clone(), tx);
    waiting_for_list(&mut app, 100);
    app.accounts[0].sync_progress_at = Some(Instant::now());
    app.recover_stalled_foreground_accounts();
    assert_eq!(app.accounts[0].foreground_generation, 0);
    assert_eq!(app.accounts[0].mailbox_list_request_id, Some(100));
    assert!(app.syncing_with_server());
}

#[test]
fn unsafe_pending_send_never_retires_a_foreground_worker() {
    let (root, mut app, _rx) = fixture();
    let (tx, _events) = std::sync::mpsc::sync_channel(32);
    let config = Config::single_maildir("Local".into(), root.path().into());
    app.accounts[0]
        .worker
        .enable_foreground_recovery(0, config.accounts[0].clone(), tx);
    waiting_for_list(&mut app, 101);
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(16));
    app.pending_send = Some((0, 102));
    app.recover_stalled_foreground_accounts();
    assert_eq!(app.accounts[0].foreground_generation, 0);
    assert!(app.accounts[0].mailboxes_loading);
    assert_eq!(app.pending_send, Some((0, 102)));
}

#[test]
fn queued_mutation_prevents_abandoning_a_possible_server_write() {
    let (root, mut app, _rx) = fixture();
    let (tx, _events) = std::sync::mpsc::sync_channel(32);
    let config = Config::single_maildir("Local".into(), root.path().into());
    app.accounts[0]
        .worker
        .enable_foreground_recovery(0, config.accounts[0].clone(), tx);
    waiting_for_list(&mut app, 103);
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(16));
    app.accounts[0]
        .pending_mutation_labels
        .insert(104, "pending move");
    app.recover_stalled_foreground_accounts();
    assert_eq!(app.accounts[0].foreground_generation, 0);
    assert!(app.accounts[0].mailboxes_loading);
    assert!(app.accounts[0].pending_mutation_labels.contains_key(&104));
}

#[test]
fn stalled_account_recovery_does_not_change_another_healthy_account() {
    let root = tempfile::tempdir().unwrap();
    for name in ["cur", "new", "tmp"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    let mut config = Config::single_maildir("University".into(), root.path().into());
    let mut gmail = config.accounts[0].clone();
    gmail.name = "Gmail".into();
    config.accounts.push(gmail);
    let (tx, _rx) = std::sync::mpsc::sync_channel(32);
    let mut workers = Vec::new();
    for (index, account) in config.accounts.iter().enumerate() {
        let mut worker = WorkerHandle::spawn(
            index,
            Box::new(crate::backend::MaildirBackend::new(root.path().into())),
            None,
            None,
            tx.clone(),
        );
        worker.enable_foreground_recovery(index, account.clone(), tx.clone());
        workers.push(worker);
    }
    let mut app = App::new(&config, workers);
    app.accounts[0].capabilities = Some(Capabilities::MAILDIR);
    app.accounts[0].mailboxes_loading = true;
    app.accounts[0].mailbox_list_request_id = Some(105);
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(16));
    app.accounts[1].capabilities = Some(Capabilities::MAILDIR);
    app.recover_stalled_foreground_accounts();
    assert_eq!(app.accounts[0].foreground_generation, 1);
    assert_eq!(app.accounts[1].foreground_generation, 0);
    assert!(app.accounts[1].capabilities.is_some());
    assert!(!app.accounts[1].syncing_with_server());
}

#[test]
fn retired_page_result_cannot_replace_cached_mail_or_clear_new_sync() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let cached = cached_message(&mut app);
    let account = &mut app.accounts[0];
    account.foreground_generation = 2;
    account.messages_loading = true;
    account.page_request_id = Some(201);
    account.page_inflight = Some(("INBOX".into(), 0));
    app.handle_foreground_worker_event(
        1,
        event(
            200,
            WorkerEventKind::MailboxOpened {
                name: "INBOX".into(),
                page: page(),
                identity: Some(CacheIdentity::Imap(81)),
                uid_next: Some(100),
                elapsed_micros: 1,
            },
        ),
    );
    assert!(app.accounts[0].messages_loading);
    assert_eq!(app.accounts[0].page_request_id, Some(201));
    assert_eq!(app.accounts[0].messages[0].id, cached);
    app.handle_foreground_worker_event(
        1,
        event(
            200,
            WorkerEventKind::MailboxOpenFailed {
                name: "INBOX".into(),
                error: "connection reset".into(),
            },
        ),
    );
    assert!(app.accounts[0].capabilities.is_some());
    assert!(app.accounts[0].messages_loading);
}

#[test]
fn recovered_maildir_connect_list_and_select_finish_footer_sync() {
    let (root, mut app, _rx) = fixture();
    let (tx, events) = std::sync::mpsc::sync_channel(64);
    let config = Config::single_maildir("Local".into(), root.path().into());
    app.accounts[0]
        .worker
        .enable_foreground_recovery(0, config.accounts[0].clone(), tx);
    waiting_for_list(&mut app, 301);
    app.accounts[0].sync_progress_at = Some(Instant::now() - Duration::from_secs(16));
    app.recover_stalled_foreground_accounts();
    assert!(app.accounts[0].connecting);
    let mut connected = false;
    let mut listed = false;
    let mut opened = false;
    for _ in 0..16 {
        let RuntimeEvent::ForegroundWorker { generation, event } = events
            .recv_timeout(Duration::from_secs(2))
            .expect("recovery must progress")
        else {
            panic!("unexpected non-foreground event")
        };
        assert_eq!(generation, 1);
        match &event.kind {
            WorkerEventKind::Connected { .. } => connected = true,
            WorkerEventKind::Mailboxes(_) => listed = true,
            WorkerEventKind::MailboxOpened { .. } => opened = true,
            _ => {}
        }
        app.handle_foreground_worker_event(generation, event);
        if connected && listed && opened {
            break;
        }
    }
    assert!(
        connected && listed && opened,
        "replacement must progress through LIST/SELECT"
    );
    assert!(!app.accounts[0].syncing_with_server());
}

#[test]
fn connection_notice_clears_on_confirmed_list_not_just_connect() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].mark_disconnected();
    assert!(
        app.all_warnings()
            .iter()
            .any(|w| w.contains("connection unavailable"))
    );
    let incident = app.accounts[0].offline_incident_number;

    // A stale connect response must not clear the notice.
    app.accounts[0].connect_request_id = Some(101);
    app.handle_worker_event(event(
        100,
        WorkerEventKind::Connected {
            capabilities: Capabilities::MAILDIR,
        },
    ));
    assert!(app.accounts[0].offline_incident_active);

    app.handle_worker_event(event(
        101,
        WorkerEventKind::Connected {
            capabilities: Capabilities::MAILDIR,
        },
    ));
    assert!(
        app.accounts[0].offline_incident_active,
        "successful connect is not yet a completed mailbox synchronization"
    );
    let list_id = app.accounts[0].mailbox_list_request_id.unwrap();
    app.handle_worker_event(event(list_id, WorkerEventKind::Mailboxes(vec![inbox()])));
    assert!(!app.accounts[0].offline_incident_active);
    assert!(
        !app.all_warnings()
            .iter()
            .any(|w| w.contains("connection unavailable"))
    );

    // A later outage has a new identity; don't suppress it as previously fixed.
    app.accounts[0].mark_disconnected();
    assert_eq!(app.accounts[0].offline_incident_number, incident + 1);
    assert!(
        app.all_warnings()
            .iter()
            .any(|w| w.contains("connection unavailable"))
    );
}

#[test]
fn successful_correlated_page_clears_stale_connection_notice() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].mark_disconnected();
    assert!(app.accounts[0].offline_incident_active);
    // Simulate a connection restored before a manual page refresh.
    app.accounts[0].capabilities = Some(Capabilities::MAILDIR);
    app.accounts[0].current_mailbox = Some("INBOX".into());
    app.accounts[0].page_request_id = Some(200);
    app.accounts[0].page_inflight = Some(("INBOX".into(), 0));

    // Late superseded pages must not clear an active incident.
    app.handle_worker_event(event(
        199,
        WorkerEventKind::MailboxOpened {
            name: "INBOX".into(),
            page: page(),
            identity: None,
            uid_next: None,
            elapsed_micros: 100,
        },
    ));
    assert!(app.accounts[0].offline_incident_active);

    app.handle_worker_event(event(
        200,
        WorkerEventKind::MailboxOpened {
            name: "INBOX".into(),
            page: page(),
            identity: None,
            uid_next: None,
            elapsed_micros: 100,
        },
    ));
    assert!(!app.accounts[0].offline_incident_active);
    assert!(
        !app.all_warnings()
            .iter()
            .any(|w| w.contains("connection unavailable"))
    );
}

#[test]
fn failed_reconnect_does_not_clear_connection_notice() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].mark_disconnected();
    app.accounts[0].connect_request_id = Some(301);
    app.accounts[0].connecting = true;
    app.handle_worker_event(error(301, "OAuth token request failed (network/TLS error)"));
    assert!(app.accounts[0].offline_incident_active);
    assert!(app.accounts[0].capabilities.is_none());
    assert!(
        app.all_warnings()
            .iter()
            .any(|w| w.contains("connection unavailable"))
    );
}

#[test]
fn native_query_results_do_not_replace_canonical_cache() {
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 800);
    let original = cached_message(&mut app);
    let account = &mut app.accounts[0];
    let identity = CacheIdentity::Jmap {
        account_id: "a".into(),
        mailbox_id: "m".into(),
        email_state: Some("s".into()),
        query_state: None,
    };
    account
        .cache_identities
        .insert("INBOX".into(), identity.clone());
    account.save_current_cache();
    account.native_view = Some(crate::backend::MailboxView::Search("test".into()));
    let result = crate::mail::parse_summary(
        MessageId("result".into()),
        b"Subject: Result\r\n\r\n",
        19,
        MessageFlags::empty(),
    )
    .unwrap();
    app.handle_worker_event(event(
        800,
        WorkerEventKind::MailboxOpened {
            name: "INBOX".into(),
            page: MessagePage {
                messages: vec![result],
                offset: 0,
                next_offset: 1,
                has_more: false,
            },
            identity: Some(identity.clone()),
            uid_next: None,
            elapsed_micros: 5,
        },
    ));
    let account = &app.accounts[0];
    assert_eq!(account.messages[0].id.0, "result");
    assert!(account.current_cache_snapshot().is_none());
    assert_eq!(account.mailbox_cache["INBOX"].messages[0].id, original);
    assert_eq!(account.cache_identities["INBOX"], identity);
}

#[test]
fn offline_conversation_projection_does_not_modify_canonical_cache() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let account = &mut app.accounts[0];
    for (uid, raw) in [
        (
            10,
            "Subject: Reply\r\nMessage-ID: <b@example.org>\r\nReferences: <a@example.org>\r\n\r\n",
        ),
        (
            9,
            "Subject: Original\r\nMessage-ID: <a@example.org>\r\n\r\n",
        ),
        (
            8,
            "Subject: Unrelated\r\nMessage-ID: <c@example.org>\r\n\r\n",
        ),
    ] {
        let mut message = crate::mail::parse_summary(
            MessageId(format!("imap-uid:81:{uid}")),
            raw.as_bytes(),
            0,
            MessageFlags::empty(),
        )
        .unwrap();
        message.timestamp = uid;
        account.messages.push(message);
    }
    account.next_page_offset = 3;
    account.has_more = true;
    account.mark_disconnected();
    assert!(app.accounts[0].threading_enabled);
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 2]);
    assert!(app.accounts[0].local_thread_view.is_none());
    assert_eq!(
        app.accounts[0].inline_thread_rows(),
        vec![
            (0, crate::threading::ThreadRow::Collapsed(2)),
            (2, crate::threading::ThreadRow::Plain)
        ]
    );
    assert_eq!(
        app.accounts[0]
            .current_cache_snapshot()
            .unwrap()
            .messages
            .len(),
        3
    );
    app.open_native_thread();
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 1, 2]);
    assert!(app.accounts[0].native_view.is_none());
    assert!(!app.accounts[0].request_next_page());
    app.collapse_selected_thread();
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 2]);
    app.begin_search();
    app.accounts[0].search_query = "Original".into();
    app.accounts[0].normalize_search_selection();
    assert_eq!(app.accounts[0].visible_message_indices(), vec![1]);
    app.accounts[0].search_query.clear();
    app.accounts[0].search_editing = false;
    app.accounts[0].normalize_search_selection();
    assert_eq!(app.accounts[0].message_index, 0);
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 2]);
    app.toggle_native_threads();
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 1, 2]);
    assert!(!app.accounts[0].threading_enabled);
    assert_eq!(app.accounts[0].next_page_offset, 3);
    app.toggle_native_threads();
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 2]);
    assert!(app.accounts[0].threading_enabled);
}

#[test]
fn native_memberships_enrich_only_correlated_canonical_rows_and_survive_disconnect() {
    use crate::threading::LocalThreadView;
    let (_root, mut app, _rx) = fixture();
    waiting_for_page(&mut app, 850);
    let original = cached_message(&mut app);
    let account = &mut app.accounts[0];
    account.save_current_cache();
    account.native_view = Some(crate::backend::MailboxView::Threads);
    account.local_thread_view = Some(LocalThreadView::Collapsed);
    let identity = CacheIdentity::Imap(81);
    let result = account.messages[0].clone();
    let queried = |id| {
        event(
            id,
            WorkerEventKind::MailboxQueried {
                name: "INBOX".into(),
                page: MessagePage {
                    messages: vec![result.clone()],
                    offset: 0,
                    next_offset: 1,
                    has_more: false,
                },
                identity: Some(identity.clone()),
                memberships: HashMap::from([
                    (original.clone(), "gmail:123".into()),
                    (MessageId("imap-uid:81:99".into()), "gmail:123".into()),
                ]),
                elapsed_micros: 5,
            },
        )
    };
    app.handle_worker_event(queried(849));
    assert!(
        app.accounts[0].mailbox_cache["INBOX"].messages[0]
            .thread_id
            .is_none()
    );
    app.handle_worker_event(queried(850));
    assert_eq!(app.accounts[0].mailbox_cache["INBOX"].messages.len(), 1);
    assert_eq!(
        app.accounts[0].mailbox_cache["INBOX"].messages[0]
            .thread_id
            .as_deref(),
        Some("gmail:123")
    );
    app.accounts[0].mark_disconnected();
    assert_eq!(
        app.accounts[0].local_thread_view,
        Some(LocalThreadView::Collapsed)
    );
    assert_eq!(app.accounts[0].messages[0].id, original);
    assert_eq!(
        app.accounts[0].messages[0].thread_id.as_deref(),
        Some("gmail:123")
    );
}

#[test]
fn browser_thread_shortcuts_work_with_split_preview_and_reader_bindings_are_preserved() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let id = cached_message(&mut app);
    let account = &mut app.accounts[0];
    account.messages[0].thread_id = Some("same".into());
    let mut reply = account.messages[0].clone();
    reply.id = MessageId("imap-uid:81:10".into());
    account.messages.push(reply);
    app.accounts[0].mark_disconnected();
    app.preview_split = true;
    app.focus = Focus::Messages;
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    app.handle_key(key('t'));
    assert!(!app.accounts[0].threading_enabled);
    app.handle_key(key('t'));
    assert!(app.accounts[0].threading_enabled);
    app.handle_key(key('o'));
    assert!(app.accounts[0].selected_inline_expanded());
    assert_eq!(app.accounts[0].selected_message().unwrap().id, id);
    app.message_open = true;
    app.focus = Focus::Preview;
    app.handle_key(key('t'));
    assert!(app.accounts[0].threading_enabled);
    assert!(app.status.text.contains("rich and plain"));
    app.handle_key(key('o'));
    assert!(app.accounts[0].selected_inline_expanded());
    assert!(app.message_open);
    assert!(app.status.text.contains("not on a link"));
}

fn inline_message(uid: i64, subject: &str, native: Option<&str>) -> MessageSummary {
    let raw = format!("Subject: {subject}\r\n\r\nbody");
    let mut message = crate::mail::parse_summary(
        MessageId(format!("imap-uid:81:{uid}")),
        raw.as_bytes(),
        raw.len() as u64,
        MessageFlags::empty(),
    )
    .unwrap();
    message.timestamp = uid;
    message.thread_id = native.map(str::to_owned);
    message
}

#[test]
fn independent_inline_expansions_and_child_collapse_keep_other_rows() {
    use crate::threading::ThreadRow::*;
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let account = &mut app.accounts[0];
    account.messages = vec![
        inline_message(10, "A reply", Some("a")),
        inline_message(9, "B reply", Some("b")),
        inline_message(8, "A root", Some("a")),
        inline_message(7, "Other", None),
        inline_message(6, "B root", Some("b")),
    ];
    account.mark_disconnected();
    assert_eq!(
        app.accounts[0].inline_thread_rows(),
        vec![(0, Collapsed(2)), (1, Collapsed(2)), (3, Plain)]
    );
    app.open_native_thread();
    app.accounts[0].message_index = 1;
    app.open_native_thread();
    assert_eq!(
        app.accounts[0].inline_thread_rows(),
        vec![
            (0, Expanded(2)),
            (2, Child { last: true }),
            (1, Expanded(2)),
            (4, Child { last: true }),
            (3, Plain)
        ]
    );
    app.accounts[0].message_index = 2;
    app.collapse_selected_thread();
    assert_eq!(app.accounts[0].message_index, 0);
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 1, 4, 3]);
    app.toggle_native_threads();
    assert!(app.accounts[0].inline_thread_rows().is_empty());
    assert_eq!(
        app.accounts[0].visible_message_indices(),
        vec![0, 1, 2, 3, 4]
    );
}

#[test]
fn native_discovery_preserves_cached_rows_bodies_and_regular_pagination() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let account = &mut app.accounts[0];
    account.messages = vec![
        inline_message(10, "Reply", Some("a")),
        inline_message(9, "Other", None),
        inline_message(8, "Root", Some("a")),
    ];
    let seed = account.messages[0].id.clone();
    let reply = account.messages[2].id.clone();
    account.bodies.insert(
        reply.clone(),
        crate::mail::parse_body(reply.clone(), b"Subject: Root\r\n\r\nbody").unwrap(),
    );
    account.next_page_offset = 3;
    account.has_more = true;
    app.open_native_thread();
    let account = &app.accounts[0];
    let request = *account
        .inline_thread_requests
        .keys()
        .next()
        .expect("thread query in flight");
    assert_eq!(account.visible_message_indices(), vec![0, 2, 1]);
    assert!(account.page_request_id.is_none());
    assert!(account.bodies.contains_key(&reply));
    let older = inline_message(7, "Older reply", Some("a"));
    let older_id = older.id.clone();
    app.accounts[0].message_index = 2;
    app.message_open = true;
    app.handle_worker_event(event(
        request,
        WorkerEventKind::ThreadDiscovered {
            name: "INBOX".into(),
            page: MessagePage {
                messages: vec![older],
                offset: 0,
                next_offset: 1,
                has_more: false,
            },
            identity: Some(CacheIdentity::Imap(81)),
            memberships: HashMap::from([(seed, "a".into())]),
        },
    ));
    let account = &app.accounts[0];
    assert_eq!(account.visible_message_indices(), vec![0, 2, 3, 1]);
    assert_eq!(account.selected_message().unwrap().id, reply);
    assert!(account.selected_body().is_some());
    assert_eq!(account.next_page_offset, 3);
    assert!(account.has_more);
    assert!(account.inline_thread_extra.contains(&older_id));
    assert_eq!(account.current_cache_snapshot().unwrap().messages.len(), 3);
    app.accounts[0].mark_disconnected();
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 2, 3, 1]);
}

#[test]
fn collapsing_pending_conversation_ignores_late_native_result() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let original = cached_message(&mut app);
    app.open_native_thread();
    let request = *app.accounts[0]
        .inline_thread_requests
        .keys()
        .next()
        .unwrap();
    app.open_native_thread();
    assert!(app.accounts[0].inline_thread_requests.is_empty());
    app.handle_worker_event(event(
        request,
        WorkerEventKind::ThreadDiscovered {
            name: "INBOX".into(),
            page: MessagePage {
                messages: vec![inline_message(7, "Older", Some("a"))],
                offset: 0,
                next_offset: 1,
                has_more: false,
            },
            identity: Some(CacheIdentity::Imap(81)),
            memberships: HashMap::from([(original.clone(), "a".into())]),
        },
    ));
    assert!(app.accounts[0].native_view.is_none());
    assert_eq!(app.accounts[0].messages.len(), 1);
    assert_eq!(app.accounts[0].messages[0].id, original);
    assert!(app.accounts[0].messages[0].thread_id.is_none());
}

#[test]
fn expansion_survives_new_representative_and_local_search() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let account = &mut app.accounts[0];
    account.messages = vec![
        inline_message(10, "Reply", Some("a")),
        inline_message(9, "Other", None),
        inline_message(8, "Root", Some("a")),
    ];
    account.mark_disconnected();
    app.open_native_thread();
    app.accounts[0]
        .messages
        .insert(0, inline_message(11, "New reply", Some("a")));
    let account = &mut app.accounts[0];
    account.message_index = 1;
    assert_eq!(account.visible_message_indices(), vec![0, 1, 3, 2]);
    app.begin_search();
    app.accounts[0].search_query = "Root".into();
    assert_eq!(app.accounts[0].visible_message_indices(), vec![3]);
    assert!(app.accounts[0].inline_thread_rows().is_empty());
    app.accounts[0].search_query.clear();
    app.accounts[0].search_editing = false;
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 1, 3, 2]);
}

#[test]
fn supplemental_thread_reply_is_promoted_by_normal_page_without_duplication() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let account = &mut app.accounts[0];
    account.messages = vec![
        inline_message(10, "Reply", Some("gmail:2")),
        inline_message(9, "Other", None),
    ];
    let seed = account.messages[0].id.clone();
    let older = inline_message(8, "Root", Some("gmail:2"));
    let older_id = older.id.clone();
    account.messages.push(older);
    account.inline_thread_extra.insert(older_id.clone());
    account.expanded_threads.insert(seed);
    account.next_page_offset = 2;
    account.has_more = true;
    account.page_request_id = Some(982);
    account.page_inflight = Some(("INBOX".into(), 2));
    app.handle_worker_event(event(
        982,
        WorkerEventKind::MailboxOpened {
            name: "INBOX".into(),
            page: MessagePage {
                messages: vec![inline_message(8, "Root", None)],
                offset: 2,
                next_offset: 3,
                has_more: false,
            },
            identity: Some(CacheIdentity::Imap(81)),
            uid_next: None,
            elapsed_micros: 5,
        },
    ));
    let account = &app.accounts[0];
    assert!(account.inline_thread_extra.is_empty());
    assert_eq!(account.messages.len(), 3);
    assert_eq!(account.messages[2].id, older_id);
    assert_eq!(account.messages[2].thread_id.as_deref(), Some("gmail:2"));
    assert_eq!(account.next_page_offset, 3);
    assert_eq!(account.visible_message_indices(), vec![0, 2, 1]);
    assert_eq!(account.current_cache_snapshot().unwrap().messages.len(), 3);
}

#[test]
fn native_discovery_cannot_establish_a_new_cache_generation() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let seed = cached_message(&mut app);
    app.open_native_thread();
    let request = *app.accounts[0]
        .inline_thread_requests
        .keys()
        .next()
        .unwrap();
    app.handle_worker_event(event(
        request,
        WorkerEventKind::ThreadDiscovered {
            name: "INBOX".into(),
            page: MessagePage {
                messages: vec![inline_message(8, "Root", Some("a"))],
                offset: 0,
                next_offset: 1,
                has_more: false,
            },
            identity: Some(CacheIdentity::Imap(82)),
            memberships: HashMap::from([(seed, "a".into())]),
        },
    ));
    let account = &app.accounts[0];
    assert_eq!(account.messages.len(), 1);
    assert!(account.messages[0].thread_id.is_none());
    assert_eq!(account.cache_identities["INBOX"], CacheIdentity::Imap(81));
    assert!(account.inline_thread_requests.is_empty());
}

#[test]
fn supplemental_thread_rows_restore_without_advancing_canonical_cursor() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let account = &mut app.accounts[0];
    account.messages = vec![
        inline_message(10, "Reply", Some("a")),
        inline_message(9, "Other", None),
    ];
    let reply = inline_message(7, "Older reply", Some("a"));
    let reply_id = reply.id.clone();
    account.inline_thread_extra.insert(reply_id.clone());
    account.messages.push(reply);
    account.next_page_offset = 2;
    account.has_more = true;
    let snapshot = account.current_cache_snapshot().unwrap();
    assert_eq!(snapshot.messages.len(), 2);
    assert_eq!(snapshot.thread_messages.len(), 1);
    account.remember_mailbox("INBOX".into(), snapshot);
    assert!(account.restore_cached_mailbox("INBOX"));
    assert_eq!(account.next_page_offset, 2);
    assert_eq!(account.canonical_messages().len(), 2);
    assert!(account.inline_thread_extra.contains(&reply_id));
    account.mark_disconnected();
    app.accounts[0].message_index = 0;
    app.open_native_thread();
    assert_eq!(app.accounts[0].visible_message_indices(), vec![0, 2, 1]);
}

#[test]
fn search_index_includes_recipients_and_attachment_names_and_invalidates() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    let account = &mut app.accounts[0];
    let mut message = inline_message(10, "Reply", None);
    message.to = "unique-recipient@example.org".into();
    message.attachment_names_hint = vec!["unique-report.pdf".into()].into_boxed_slice();
    account.messages = vec![message];
    account.search_mode = crate::config::MailSearchMode::Strict;
    account.search_query = "unique-recipient".into();
    assert_eq!(account.visible_message_indices(), vec![0]);
    account.search_query = "unique-report.pdf".into();
    assert_eq!(account.visible_message_indices(), vec![0]);
    assert_eq!(account.visible_message_indices(), vec![0]);
    account.messages[0].attachment_names_hint = Box::default();
    assert!(account.visible_message_indices().is_empty());
}

#[test]
fn imap_capability_enables_native_search_editor() {
    let (_root, mut app, _rx) = fixture();
    online(&mut app);
    app.accounts[0].capabilities = Some(Capabilities::IMAP_BASIC);
    app.begin_native_search();
    assert!(app.accounts[0].native_search_editing);
}
