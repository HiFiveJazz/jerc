//! Dedicated IMAP IDLE event watchers; no mailbox mutations here.
use super::*;

pub(super) fn run_idle_worker(
    mut backend: Box<dyn MailBackend>,
    stop: Arc<AtomicBool>,
    wake: SyncSender<Action>,
) {
    let mut backoff_secs: u64 = 2;
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let result = (|| -> anyhow::Result<()> {
            backend.connect()?;
            if backend.watch_is_account_wide() {
                return backend.watch_mailbox("", &stop, &mut || {
                    // Account-wide push must not silently drop the trailing
                    // event of a burst due to the IMAP leading-edge throttle.
                    if !stop.load(Ordering::Acquire) {
                        let _ = wake.try_send(Action::new(ActionKind::WatchAccountChanged));
                    }
                });
            }
            let inbox = backend
                .list_mailboxes()?
                .into_iter()
                .find(|mailbox| mailbox.role == crate::model::MailboxRole::Inbox);
            let Some(inbox) = inbox else { return Ok(()) };
            let mut last_wake = Instant::now() - Duration::from_secs(2);
            backend.watch_mailbox(&inbox.name, &stop, &mut || {
                // A UID+FLAGS event burst only needs one STATUS reconciliation.
                if last_wake.elapsed() >= Duration::from_secs(1) {
                    let _ = wake.try_send(Action::new(ActionKind::WatchMailboxChanged(
                        inbox.name.clone(),
                    )));
                    last_wake = Instant::now();
                }
            })
        })();
        crate::perf::record("imap_idle_session", None, started.elapsed(), result.is_ok());
        if result.is_ok() || stop.load(Ordering::Relaxed) {
            break; // No IDLE capability / huge Inbox: retain existing polling.
        }
        for _ in 0..backoff_secs {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

/// A second, dormant-until-selected watcher: never SELECTs the Inbox,
/// never holds the foreground or background IMAP session, and exits promptly
/// on folder switches even when the previous watcher is inside IDLE.
pub(super) fn run_selected_idle_worker(
    mut backend: Box<dyn MailBackend>,
    target: Arc<SelectedIdleTarget>,
    wake: SyncSender<Action>,
) {
    let mut backoff_secs: u64 = 2;
    while let Some((mailbox, revision, cancel)) = target.wait_for_mailbox() {
        if cancel.load(Ordering::Acquire) {
            backoff_secs = 2;
            continue;
        }
        let started = Instant::now();
        let result = (|| -> anyhow::Result<()> {
            backend.connect()?;
            if cancel.load(Ordering::Acquire) {
                return Ok(());
            }
            let mut last_wake = Instant::now() - Duration::from_secs(2);
            backend.watch_mailbox(&mailbox, &cancel, &mut || {
                if !cancel.load(Ordering::Acquire) && last_wake.elapsed() >= Duration::from_secs(1)
                {
                    let _ = wake.try_send(Action::new(ActionKind::WatchMailboxChanged(
                        mailbox.clone(),
                    )));
                    last_wake = Instant::now();
                }
            })
        })();
        crate::perf::record(
            "imap_idle_selected_session",
            None,
            started.elapsed(),
            result.is_ok(),
        );
        if cancel.load(Ordering::Acquire) {
            backoff_secs = 2;
            continue;
        }
        if result.is_ok() {
            // Unsupported IDLE or too-large mailbox: existing STATUS polling
            // continues; wait for selection change rather than reconnecting.
            backoff_secs = 2;
            target.wait_for_change(revision, None);
        } else {
            target.wait_for_change(revision, Some(Duration::from_secs(backoff_secs)));
            backoff_secs = (backoff_secs * 2).min(60);
        }
    }
}
