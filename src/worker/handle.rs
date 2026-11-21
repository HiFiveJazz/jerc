//! Worker lifecycle, bounded queues and foreground recovery.
use super::background::run_status_worker;
use super::foreground::run_worker;
use super::idle::{run_idle_worker, run_selected_idle_worker};
use super::*;

impl WorkerHandle {
    pub fn spawn(
        account: usize,
        backend: Box<dyn MailBackend>,
        status_backend: Option<Box<dyn MailBackend>>,
        outgoing: Option<Box<dyn MailSender>>,
        runtime_tx: SyncSender<RuntimeEvent>,
    ) -> Self {
        Self::spawn_with_idle(account, backend, status_backend, None, outgoing, runtime_tx)
    }

    /// The IDLE watcher is optional and completely separate from the two
    /// existing workers. Existing callers/tests use `spawn` unchanged.
    pub fn spawn_with_idle(
        account: usize,
        backend: Box<dyn MailBackend>,
        status_backend: Option<Box<dyn MailBackend>>,
        idle_backend: Option<Box<dyn MailBackend>>,
        outgoing: Option<Box<dyn MailSender>>,
        runtime_tx: SyncSender<RuntimeEvent>,
    ) -> Self {
        Self::spawn_with_idle_and_selected(
            account,
            backend,
            status_backend,
            idle_backend,
            None,
            outgoing,
            runtime_tx,
        )
    }

    /// The additional watcher is created only for IMAP accounts. It stays
    /// dormant unless the active account displays a non-Inbox mailbox.
    pub fn spawn_with_idle_and_selected(
        account: usize,
        mut backend: Box<dyn MailBackend>,
        status_backend: Option<Box<dyn MailBackend>>,
        idle_backend: Option<Box<dyn MailBackend>>,
        selected_idle_backend: Option<Box<dyn MailBackend>>,
        mut outgoing: Option<Box<dyn MailSender>>,
        runtime_tx: SyncSender<RuntimeEvent>,
    ) -> Self {
        let (sender, receiver) = sync_channel::<Action>(64);
        let foreground_generation = Arc::new(AtomicU64::new(0));
        let unacknowledged_writes = Mutex::new(HashSet::new());
        let status_events = runtime_tx.clone();
        let active = Arc::clone(&foreground_generation);
        let join = thread::Builder::new()
            .name(format!("mail-worker-{account}"))
            .spawn(move || {
                run_worker(
                    account,
                    0,
                    active,
                    &mut *backend,
                    &mut outgoing,
                    receiver,
                    runtime_tx,
                );
            })
            .expect("failed to spawn mail worker");
        let (status_sender, priority_sender, status_join) =
            if let Some(status_backend) = status_backend {
                let (tx, rx) = sync_channel::<Action>(16);
                let (priority_tx, priority_rx) = sync_channel::<Action>(8);
                let join = thread::Builder::new()
                    .name(format!("mail-status-{account}"))
                    .spawn(move || {
                        run_status_worker(account, status_backend, rx, priority_rx, status_events)
                    })
                    .expect("failed to spawn mailbox status worker");
                (Some(tx), Some(priority_tx), Some(join))
            } else {
                (None, None, None)
            };
        let idle_stop = Arc::new(AtomicBool::new(false));
        let idle_join = idle_backend.map(|backend| {
            let stop = Arc::clone(&idle_stop);
            let wake = status_sender.clone().unwrap_or_else(|| sender.clone());
            thread::Builder::new()
                .name(format!("mail-idle-{account}"))
                .spawn(move || run_idle_worker(backend, stop, wake))
                .expect("failed to spawn IMAP IDLE worker")
        });
        let (selected_idle, selected_idle_join) = if let Some(backend) = selected_idle_backend {
            let target = Arc::new(SelectedIdleTarget::new());
            let watch_target = Arc::clone(&target);
            let wake = status_sender.clone().unwrap_or_else(|| sender.clone());
            let join = thread::Builder::new()
                .name(format!("mail-idle-selected-{account}"))
                .spawn(move || run_selected_idle_worker(backend, watch_target, wake))
                .expect("failed to spawn selected mailbox IDLE worker");
            (Some(target), Some(join))
        } else {
            (None, None)
        };
        Self {
            sender,
            status_sender,
            priority_sender,
            join: Some(join),
            status_join,
            idle_stop,
            idle_join,
            selected_idle,
            selected_idle_join,
            foreground_generation,
            unacknowledged_writes,
            recovery: None,
            retired_foregrounds: Vec::new(),
            last_foreground_recovery: None,
        }
    }

    /// The displayed non-Inbox folder gets at most one extra IDLE watch.
    /// The method is nonblocking and idempotent on every UI loop iteration.
    pub fn set_selected_idle_mailbox(&self, mailbox: Option<&str>) {
        if let Some(target) = &self.selected_idle {
            target.set_mailbox(mailbox);
        }
    }

    /// Opt in only for configured IMAP accounts. Maildir and custom test
    /// backends keep their original lifetime unless configured explicitly.
    pub fn enable_foreground_recovery(
        &mut self,
        account: usize,
        config: crate::config::AccountConfig,
        events: SyncSender<RuntimeEvent>,
    ) {
        self.recovery = Some(ForegroundRecovery {
            account,
            config,
            events,
        });
    }

    pub fn foreground_generation(&self) -> u64 {
        self.foreground_generation.load(Ordering::Acquire)
    }

    /// A submitted write remains unsafe to abandon even after its worker
    /// finishes: the ACK may still be sitting in the UI event channel.
    /// Release this guard only when the matching event reaches the UI.
    pub fn acknowledge_foreground_event(&self, request_id: u64) {
        self.unacknowledged_writes
            .lock()
            .expect("write journal lock")
            .remove(&request_id);
    }

    /// Replace ONLY the stuck foreground session. Other sessions and the
    /// locally durable Outbox/mutation journals are left alone. Limit the
    /// detached old sessions and enforce backoff during prolonged outages.
    pub fn recover_stalled_foreground(&mut self) -> anyhow::Result<bool> {
        let Some(recovery) = &self.recovery else {
            return Ok(false);
        };
        if !self
            .unacknowledged_writes
            .lock()
            .expect("write journal lock")
            .is_empty()
            || self
                .last_foreground_recovery
                .is_some_and(|at| at.elapsed() < Duration::from_secs(20))
        {
            return Ok(false);
        }
        // Completed retired workers can be reaped without blocking the UI.
        self.retired_foregrounds.retain(|join| !join.is_finished());
        if self.retired_foregrounds.len() >= 2 {
            return Ok(false);
        }
        // Constructing transports is local; actual network activity starts in
        // the new worker. If setup fails, leave the existing worker untouched.
        let mut backend = crate::backend::from_account(&recovery.config)?;
        let mut outgoing = crate::outgoing::from_account(&recovery.config)?;
        let (sender, receiver) = sync_channel::<Action>(64);
        let account = recovery.account;
        let events = recovery.events.clone();
        let generation = self.foreground_generation.load(Ordering::Acquire) + 1;
        let active = Arc::clone(&self.foreground_generation);
        let join = thread::Builder::new()
            .name(format!("mail-worker-{account}-gen-{generation}"))
            .spawn(move || {
                run_worker(
                    account,
                    generation,
                    active,
                    &mut *backend,
                    &mut outgoing,
                    receiver,
                    events,
                );
            })?;
        self.foreground_generation
            .store(generation, Ordering::Release);
        let old_sender = std::mem::replace(&mut self.sender, sender);
        drop(old_sender);
        if let Some(old) = self.join.replace(join) {
            self.retired_foregrounds.push(old);
        }
        self.last_foreground_recovery = Some(Instant::now());
        crate::perf::record("worker_foreground_replaced", None, Duration::ZERO, true);
        Ok(true)
    }

    // The caller may retry an unsent action on another worker queue, so preserve
    // ownership of the original action rather than hiding it behind a thin error.
    #[expect(
        clippy::result_large_err,
        reason = "unsent actions must remain recoverable by callers"
    )]
    pub fn post(&self, kind: ActionKind) -> Result<u64, TrySendError<Action>> {
        let action = Action::new(kind);
        let id = action.id;
        if matches!(
            &action.kind,
            ActionKind::FetchBody { .. }
                | ActionKind::FetchAttachments { .. }
                | ActionKind::RemoteAttachment { .. }
                | ActionKind::PrepareCompose { .. }
                | ActionKind::FetchRawForCompose { .. }
        ) {
            // Priority has a separate bounded input queue, but runs on the
            // existing read-only IMAP connection. A flood of speculative
            // prefetches cannot fill this queue. Wake the worker if it is
            // sleeping on its ordinary STATUS queue.
            if let Some(priority) = &self.priority_sender {
                match priority.try_send(action) {
                    Ok(()) => {
                        if let Some(status) = &self.status_sender {
                            let _ = status.try_send(Action::new(ActionKind::WakePriority));
                        }
                    }
                    Err(TrySendError::Full(action) | TrySendError::Disconnected(action)) => {
                        if matches!(
                            &action.kind,
                            ActionKind::FetchBody { .. }
                                | ActionKind::PrepareCompose { .. }
                                | ActionKind::FetchRawForCompose { .. }
                        ) {
                            // Preserve the existing body fallback, but don't
                            // make attachments wait behind a page-long FETCH.
                            self.sender.try_send(action)?;
                        } else {
                            return Err(TrySendError::Full(action));
                        }
                    }
                }
            } else {
                self.sender.try_send(action)?;
            }
        } else if matches!(&action.kind, ActionKind::DiscoverThread { .. }) {
            self.status_sender
                .as_ref()
                .unwrap_or(&self.sender)
                .try_send(action)?;
        } else if matches!(
            &action.kind,
            ActionKind::PrefetchBodies { .. } | ActionKind::WarmMailbox { .. }
        ) {
            // Speculative downloads must not compete with foreground work.
            match &self.status_sender {
                Some(sender) => sender.try_send(action)?,
                None => return Err(TrySendError::Disconnected(action)),
            }
        } else if matches!(
            &action.kind,
            ActionKind::MailboxStatus(_)
                | ActionKind::WatchMailboxChanged(_)
                | ActionKind::WatchAccountChanged
                | ActionKind::NewMailPreview { .. }
        ) {
            self.status_sender
                .as_ref()
                .unwrap_or(&self.sender)
                .try_send(action)?;
        } else {
            let write = is_foreground_write(&action.kind);
            if write {
                self.unacknowledged_writes
                    .lock()
                    .expect("write journal lock")
                    .insert(id);
            }
            if let Err(error) = self.sender.try_send(action) {
                if write {
                    self.unacknowledged_writes
                        .lock()
                        .expect("write journal lock")
                        .remove(&id);
                }
                return Err(error);
            }
        }
        Ok(id)
    }

    /// Do not make process exit wait for a slow IMAP request or a full action
    /// queue. Dropping a JoinHandle detaches the worker; the process will exit
    /// after the UI finishes its local cleanup. Unlike `shutdown`, this is
    /// ONLY for final process exit, never account reload or tests that need to
    /// observe all queued work finishing.
    ///
    /// The UI refuses ordinary quit while a send or tracked mailbox mutation
    /// is outstanding. Read-only fetches and prefetches can be abandoned.
    pub fn detach_for_exit(&mut self) {
        self.idle_stop.store(true, Ordering::Relaxed);
        if let Some(target) = &self.selected_idle {
            target.stop();
        }
        // No blocking send: the action channels may already be full.
        // No join: an IMAP operation may remain stuck until its I/O timeout.
        drop(self.join.take());
        drop(self.status_join.take());
        drop(self.idle_join.take());
        drop(self.selected_idle_join.take());
        self.retired_foregrounds.clear();
        // Disconnect the background queue when possible. The IDLE watcher may
        // still hold a clone until it observes the stop flag.
        drop(self.status_sender.take());
        drop(self.priority_sender.take());
    }

    /// An account configuration change must never wait for a network worker
    /// on the UI thread. The caller MUST first establish that all submitted
    /// SMTP/IMAP writes have been acknowledged by the UI, then invalidate its
    /// event epoch. This retires only the old worker threads; it does not
    /// abandon a possibly committed server mutation.
    pub fn has_unacknowledged_writes(&self) -> bool {
        !self
            .unacknowledged_writes
            .lock()
            .expect("write journal lock")
            .is_empty()
    }

    pub fn retire_for_reload(&mut self) {
        // Callers must defer reload while any write's completion is unknown.
        assert!(
            !self.has_unacknowledged_writes(),
            "account reload attempted with an unacknowledged mail write"
        );
        let Some(join) = self.join.take() else { return };
        let started = Instant::now();
        self.idle_stop.store(true, Ordering::Release);
        if let Some(target) = self.selected_idle.take() {
            target.stop();
        }
        let selected_idle_join = self.selected_idle_join.take();
        let status_sender = self.status_sender.take();
        let priority_sender = self.priority_sender.take();
        let sender = self.sender.clone();
        let status_join = self.status_join.take();
        let idle_join = self.idle_join.take();
        let retired = std::mem::take(&mut self.retired_foregrounds);
        // The reaper alone can wait for an in-flight connection or SELECT.
        // Shutdown signalling itself never blocks even when queues are full;
        // dropping the last sender also makes receivers exit after queued work.
        let spawn = thread::Builder::new()
            .name("mail-reload-reaper".to_owned())
            .spawn(move || {
                let shutdown_started = Instant::now();
                if let Some(status) = status_sender {
                    let _ = status.try_send(Action::new(ActionKind::Shutdown));
                }
                drop(priority_sender);
                let _ = sender.try_send(Action::new(ActionKind::Shutdown));
                let foreground_ok = join.join().is_ok();
                let status_ok = status_join.is_none_or(|join| join.join().is_ok());
                // IDLE can remain inside TLS connect; do not join a watcher
                // that has not observed its stop flag yet.
                if let Some(join) = idle_join
                    && join.is_finished()
                {
                    let _ = join.join();
                }
                if let Some(join) = selected_idle_join
                    && join.is_finished()
                {
                    let _ = join.join();
                }
                for join in retired {
                    if join.is_finished() {
                        let _ = join.join();
                    }
                }
                crate::perf::record(
                    "worker_reload_reaped",
                    None,
                    shutdown_started.elapsed(),
                    foreground_ok && status_ok,
                );
            });
        crate::perf::record(
            "worker_reload_retire",
            None,
            started.elapsed(),
            spawn.is_ok(),
        );
        // If thread creation fails, captured senders/handles are dropped and
        // network workers detach. The write gate above makes that safe: only
        // read-only work can be abandoned in this exceptional case.
    }

    pub fn shutdown(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        self.idle_stop.store(true, Ordering::Relaxed);
        if let Some(target) = self.selected_idle.take() {
            target.stop();
        }
        // Shutdown is allowed to wait for the bounded queue to drain. Ordinary
        // UI actions use try_send() so backend pressure can never freeze input.
        // The status worker is independent: shut it down as well, but never
        // wait for an idle-count request before signalling the foreground one.
        if let Some(sender) = self.status_sender.take() {
            let _ = sender.send(Action::new(ActionKind::Shutdown));
        }
        drop(self.priority_sender.take());
        let _ = self.sender.send(Action::new(ActionKind::Shutdown));
        let _ = join.join();
        if let Some(status_join) = self.status_join.take() {
            let _ = status_join.join();
        }
        // Connection establishment can outlive the UI. Its send channel is
        // disconnected on shutdown, so an old watcher cannot update a reloaded
        // account. Never block quit on a stuck server handshake.
        self.retired_foregrounds.clear();
        if let Some(idle_join) = self.idle_join.take()
            && idle_join.is_finished()
        {
            let _ = idle_join.join();
        }
        if let Some(selected_join) = self.selected_idle_join.take()
            && selected_join.is_finished()
        {
            let _ = selected_join.join();
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
