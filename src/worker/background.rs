//! Bounded speculative work and independent mailbox STATUS reconciliation.
use super::foreground::{discover_thread, pop_background_action};
use super::helpers::*;
use super::*;

/// Unread-count checks and low-priority preview preloads share one background
/// session. Neither can block the foreground SELECT/FETCH path. When the cursor
/// moves again, discard pending batches and stop the old batch *between* bodies.
pub(super) fn run_status_worker(
    account: usize,
    mut backend: Box<dyn MailBackend>,
    receiver: Receiver<Action>,
    priority_receiver: Receiver<Action>,
    runtime_tx: SyncSender<RuntimeEvent>,
) {
    let mut connected = false;
    let mut pending = VecDeque::<Action>::new();
    // A neighboring window often overlaps the one currently downloading.
    // Suppress duplicate PEEK fetches briefly without keeping a permanent
    // shadow cache (the UI may evict large previews from its bounded cache).
    let mut recent = HashMap::<(String, MessageId), Instant>::new();
    // Never let an aggressive, multi-second prefetch run starve unread counts.
    let mut bodies_since_status = 0usize;
    loop {
        if pending.is_empty() {
            match receiver.recv() {
                Ok(action) => pending.push_back(action),
                Err(_) => break,
            }
        }
        while let Ok(action) = receiver.try_recv() {
            pending.push_back(action);
        }
        while let Ok(action) = priority_receiver.try_recv() {
            pending.push_front(action);
        }
        if pending
            .iter()
            .any(|action| matches!(&action.kind, ActionKind::Shutdown))
        {
            break;
        }
        let Some(action) = pop_background_action(&mut pending, bodies_since_status) else {
            continue;
        };
        let queue_wait = action.queued_at.elapsed();
        let started = Instant::now();
        match action.kind {
            ActionKind::FetchBody { mailbox, id } => {
                // This connection can read a selected message while the
                // foreground connection is executing a slow header FETCH.
                let result = (|| {
                    if !connected {
                        backend.connect()?;
                        connected = true;
                    }
                    backend.fetch_body(&mailbox, &id)
                })();
                if result.is_err() {
                    connected = false;
                }
                crate::perf::record(
                    "worker_priority_fetch_body",
                    Some(queue_wait),
                    started.elapsed(),
                    result.is_ok(),
                );
                let kind = match result {
                    Ok(body) => {
                        recent.insert((mailbox, id), Instant::now());
                        WorkerEventKind::BodyLoaded(body)
                    }
                    Err(error) => WorkerEventKind::BodyLoadFailed {
                        error: WorkerError::from_error(error),
                    },
                };
                if runtime_tx
                    .send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            ActionKind::PrepareCompose {
                mailbox,
                id,
                summary,
                intent,
                own_addresses,
                cache,
            } => {
                let result = (|| {
                    let raw = fetch_compose_raw(
                        &mut *backend,
                        &mut connected,
                        &mailbox,
                        &id,
                        cache.as_ref(),
                    )?;
                    crate::app::Composer::prepare_original(
                        &summary,
                        id.clone(),
                        raw,
                        intent,
                        &own_addresses,
                    )
                })();
                crate::perf::record(
                    "background_prepare_compose",
                    Some(queue_wait),
                    started.elapsed(),
                    result.is_ok(),
                );
                if runtime_tx
                    .send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind: WorkerEventKind::ComposePrepared {
                            id,
                            result: result.map(Box::new).map_err(|error| format!("{error:#}")),
                        },
                    }))
                    .is_err()
                {
                    break;
                }
            }
            ActionKind::FetchRawForCompose { mailbox, id, cache } => {
                let result =
                    fetch_compose_raw(&mut *backend, &mut connected, &mailbox, &id, cache.as_ref());
                crate::perf::record(
                    "background_fetch_compose_raw",
                    Some(queue_wait),
                    started.elapsed(),
                    result.is_ok(),
                );
                let kind = match result {
                    Ok(raw) => WorkerEventKind::RawLoaded { id, raw },
                    Err(error) => WorkerEventKind::Error(WorkerError::from_error(error)),
                };
                if runtime_tx
                    .send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            ActionKind::RemoteAttachment { mailbox, job } => {
                let result = (|| {
                    if !connected {
                        backend.connect()?;
                        connected = true;
                    }
                    download_attachment_job(&mut *backend, &mailbox, job)
                })();
                let kind = WorkerEventKind::RemoteAttachmentCompleted(
                    result.map_err(|e| format!("{e:#}")),
                );
                if runtime_tx
                    .send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            ActionKind::FetchAttachments { mailbox, id, cache } => {
                // A cache hit works even when IMAP is offline or authenticating.
                // Otherwise connect this independent read-only session and
                // validate UIDVALIDITY during the server fetch.
                let result = (|| {
                    if let Some((dir, validity)) = cache.as_ref()
                        && let Some(raw) =
                            (RawStore { dir: dir.clone() }).read(&mailbox, validity, &id)
                        && let Ok(attachments) = crate::mail::extract_attachments(&raw)
                    {
                        crate::perf::record("attachment_raw_disk_hit", None, Duration::ZERO, true);
                        return Ok(WorkerEventKind::AttachmentsLoaded {
                            mailbox: mailbox.clone(),
                            id: id.clone(),
                            attachments,
                        });
                    }
                    if !connected {
                        backend.connect()?;
                        connected = true;
                    }
                    attachment_event(&mut *backend, &mailbox, &id, cache.as_ref())
                })();
                if result.is_err() {
                    connected = false;
                }
                crate::perf::record(
                    "worker_fetch_attachments",
                    Some(queue_wait),
                    started.elapsed(),
                    result.is_ok(),
                );
                let kind = match result {
                    Ok(event) => event,
                    Err(error) => WorkerEventKind::Error(WorkerError::from_error(error)),
                };
                if runtime_tx
                    .send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            ActionKind::NewMailPreview { mailbox, count } => {
                let result = (|| {
                    if !connected {
                        backend.connect()?;
                        connected = true;
                    }
                    Ok::<_, anyhow::Error>(new_mail_preview(&mut *backend, &mailbox, count))
                })();
                let kind = result.unwrap_or(WorkerEventKind::NewMailPreview {
                    count,
                    sender: String::new(),
                    subject: String::new(),
                    excerpt: String::new(),
                });
                if runtime_tx
                    .send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            ActionKind::WakePriority => {}
            ActionKind::WatchAccountChanged => {
                pending.retain(|action| !matches!(&action.kind, ActionKind::WatchAccountChanged));
                let _ = runtime_tx.send(RuntimeEvent::Worker(WorkerEvent {
                    account,
                    request_id: action.id,
                    kind: WorkerEventKind::AccountChanged,
                }));
            }
            ActionKind::WatchMailboxChanged(name) => {
                if runtime_tx
                    .try_send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind: WorkerEventKind::MailboxChanged { name },
                    }))
                    .is_err()
                {
                    // The periodic STATUS poll will reconcile this missed wake.
                }
            }
            ActionKind::MailboxStatus(name) => {
                bodies_since_status = 0;
                let result = (|| {
                    if !connected {
                        backend.connect()?;
                        connected = true;
                    }
                    backend.mailbox_status(&name)
                })();
                if result.is_err() {
                    connected = false;
                }
                let ok = result.is_ok();
                crate::perf::record("background_status", Some(queue_wait), started.elapsed(), ok);
                let kind = match result {
                    Ok((total, unread)) => WorkerEventKind::MailboxStatus {
                        name,
                        total,
                        unread,
                    },
                    Err(error) => WorkerEventKind::Error(WorkerError::background_status(error)),
                };
                if runtime_tx
                    .send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            ActionKind::PrefetchBodies { mailbox, ids } => {
                if recent.len() > 256 {
                    recent.retain(|_, at| at.elapsed() < Duration::from_secs(4));
                }
                if !connected {
                    let result = backend.connect();
                    crate::perf::record(
                        "prefetch_connect",
                        Some(queue_wait),
                        started.elapsed(),
                        result.is_ok(),
                    );
                    if result.is_err() {
                        continue;
                    } // Optional; never interrupt the UI.
                    connected = true;
                }
                let mut remaining = ids.into_iter();
                while let Some(id) = remaining.next() {
                    let key = (mailbox.clone(), id.clone());
                    if recent
                        .get(&key)
                        .is_some_and(|at| at.elapsed() < Duration::from_secs(4))
                    {
                        continue;
                    }
                    // An IMAP body fetch uses PEEK and must not mark mail seen.
                    let fetched_at = Instant::now();
                    let result = backend.fetch_body(&mailbox, &id);
                    bodies_since_status = bodies_since_status.saturating_add(1);
                    crate::perf::record(
                        "prefetch_body",
                        None,
                        fetched_at.elapsed(),
                        result.is_ok(),
                    );
                    if let Ok(body) = result {
                        recent.insert(key, Instant::now());
                        if runtime_tx
                            .send(RuntimeEvent::Worker(WorkerEvent {
                                account,
                                request_id: action.id,
                                kind: WorkerEventKind::BodyPrefetched {
                                    mailbox: mailbox.clone(),
                                    body,
                                },
                            }))
                            .is_err()
                        {
                            return;
                        }
                    }
                    // The user may have reversed direction while this download
                    // was in flight. Obey the latest window before fetching more.
                    while let Ok(new_action) = receiver.try_recv() {
                        pending.push_back(new_action);
                    }
                    while let Ok(new_action) = priority_receiver.try_recv() {
                        pending.push_front(new_action);
                    }
                    if pending.iter().any(|next| {
                        matches!(
                            &next.kind,
                            ActionKind::Shutdown | ActionKind::PrefetchBodies { .. }
                        )
                    }) {
                        break; // Superseded window or shutdown: abandon the rest.
                    }
                    let interactive_waiting = pending.iter().any(|next| {
                        matches!(
                            &next.kind,
                            ActionKind::FetchBody { .. }
                                | ActionKind::FetchAttachments { .. }
                                | ActionKind::RemoteAttachment { .. }
                        )
                    });
                    let status_waiting = bodies_since_status >= 4
                        && pending
                            .iter()
                            .any(|next| matches!(&next.kind, ActionKind::MailboxStatus(_)));
                    if interactive_waiting || status_waiting {
                        // Yield to an explicit reader fetch first, or to STATUS
                        // after a short burst. Resume only if no newer window
                        // supersedes us before the next work item.
                        let rest = remaining.collect::<Vec<_>>();
                        if !rest.is_empty() {
                            pending.push_back(Action::new(ActionKind::PrefetchBodies {
                                mailbox: mailbox.clone(),
                                ids: rest,
                            }));
                        }
                        break;
                    }
                }
            }
            ActionKind::DiscoverThread {
                name,
                view,
                cached_ids,
                limit,
            } => {
                let kind = if !connected && backend.connect().is_err() {
                    WorkerEventKind::ThreadDiscoveryFailed
                } else {
                    connected = true;
                    discover_thread(&mut *backend, name, &view, &cached_ids, limit)
                };
                if runtime_tx
                    .send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            ActionKind::WarmMailbox { name, limit, hint } => {
                // One bounded page at a time; never retry a broken connection
                // speculatively, and never report routine cache misses in UI.
                let result = (|| {
                    if !connected {
                        backend.connect()?;
                        connected = true;
                    }
                    let page = backend.open_mailbox_with_hint(&name, 0, limit, hint)?;
                    Ok::<_, anyhow::Error>((
                        page,
                        backend.mailbox_cache_identity(&name),
                        backend.mailbox_uid_next(&name),
                    ))
                })();
                if result.is_err() {
                    connected = false;
                }
                crate::perf::record(
                    "background_warm_mailbox",
                    Some(queue_wait),
                    started.elapsed(),
                    result.is_ok(),
                );
                let kind = match result {
                    Ok((page, identity, uid_next)) => WorkerEventKind::MailboxWarmed {
                        name,
                        page,
                        identity,
                        uid_next,
                    },
                    Err(_) => WorkerEventKind::MailboxWarmFailed { name },
                };
                if runtime_tx
                    .send(RuntimeEvent::Worker(WorkerEvent {
                        account,
                        request_id: action.id,
                        kind,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            ActionKind::Shutdown => break,
            _ => {} // The background worker never performs foreground mutations.
        }
    }
}
