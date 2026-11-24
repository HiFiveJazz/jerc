//! Request execution, queue compaction and message/thread discovery.
use super::helpers::*;
use super::*;

pub(super) fn run_worker(
    account: usize,
    generation: u64,
    active_generation: Arc<AtomicU64>,
    backend: &mut dyn MailBackend,
    outgoing: &mut Option<Box<dyn MailSender>>,
    receiver: Receiver<Action>,
    runtime_tx: SyncSender<RuntimeEvent>,
) {
    let mut pending = VecDeque::<Action>::new();

    loop {
        // Drain the current queue BEFORE choosing an action. Previously a
        // stale OpenMailbox already in `pending` could execute before newer
        // folder requests that arrived in the channel during a slow SELECT.
        if pending.is_empty() {
            match receiver.recv() {
                Ok(action) => pending.push_back(action),
                Err(_) => break,
            }
        }
        while let Ok(action) = receiver.try_recv() {
            pending.push_back(action);
        }
        compact_navigation_queue(&mut pending);
        // The old socket may return long after a replacement was started.
        // Drop its queued read-only work rather than issuing stale requests.
        if active_generation.load(Ordering::Acquire) != generation {
            break;
        }
        let Some(action) = pending.pop_front() else {
            continue;
        };
        if matches!(&action.kind, ActionKind::Shutdown) {
            break;
        }

        // A blocked IMAP operation can delay subsequent queued work even while
        // the UI is responsive. Record queue time separately from execution.
        let queue_wait = action.queued_at.elapsed();
        let operation = match &action.kind {
            ActionKind::Connect => "worker_connect",
            ActionKind::ListMailboxes => "worker_list_mailboxes",
            ActionKind::MailboxStatus(_) | ActionKind::InboxStatus { .. } => {
                "worker_mailbox_status"
            }
            ActionKind::WakePriority => "worker_wake_priority",
            ActionKind::WatchMailboxChanged(_) | ActionKind::WatchAccountChanged => {
                "worker_idle_changed"
            }
            ActionKind::NewMailPreview { .. } => "worker_new_mail_preview",
            ActionKind::OpenMailbox { .. } | ActionKind::QueryMailbox { .. } => {
                "worker_open_mailbox"
            }
            ActionKind::DiscoverThread { .. } => "worker_discover_thread",
            ActionKind::FetchBody { .. } => "worker_fetch_body",
            ActionKind::PrefetchBodies { .. } => "worker_prefetch_bodies",
            ActionKind::WarmMailbox { .. } => "worker_warm_mailbox",
            ActionKind::FetchRawForCompose { .. } => "worker_fetch_compose",
            ActionKind::PrepareCompose { .. } => "worker_prepare_compose",
            ActionKind::FetchAttachments { .. } => "worker_fetch_attachments",
            ActionKind::RemoteAttachment { .. } => "worker_remote_attachment",
            ActionKind::ExportRaw { .. } => "worker_export_raw",
            ActionKind::SetSeen { .. } => "worker_set_seen",
            ActionKind::Delete { .. } | ActionKind::DeleteMany { .. } => "worker_delete",
            ActionKind::Move { .. } | ActionKind::MoveMany { .. } => "worker_move",
            ActionKind::ArchiveMany { .. } | ActionKind::UnarchiveMany { .. } => "worker_archive",
            ActionKind::Send { .. } => "worker_send",
            ActionKind::SaveDraft { .. } => "worker_save_draft",
            ActionKind::Shutdown => "worker_shutdown",
        };
        let started = Instant::now();
        let request_id = action.id;
        if operation == "worker_open_mailbox" || operation == "worker_fetch_body" {
            crate::perf::record_request(
                "worker_start",
                request_id,
                Some(queue_wait),
                started.elapsed(),
                true,
            );
        }
        let event = match action.kind {
            ActionKind::Connect => backend
                .connect()
                .map(|()| WorkerEventKind::Connected {
                    capabilities: backend.capabilities(),
                }),
            ActionKind::ListMailboxes => backend.list_mailboxes().map(WorkerEventKind::Mailboxes),
            ActionKind::NewMailPreview { mailbox, count, id } => {
                Ok(new_mail_preview(backend, &mailbox, count, id))
            }
            ActionKind::WatchMailboxChanged(name) => Ok(WorkerEventKind::MailboxChanged { name }),
            ActionKind::WatchAccountChanged => Ok(WorkerEventKind::AccountChanged),
            ActionKind::MailboxStatus(name) => backend.mailbox_status(&name).map(|(total, unread)| {
                WorkerEventKind::MailboxStatus {
                    name,
                    total,
                    unread,
                    inbox: None,
                }
            }),
            ActionKind::InboxStatus { name, previous } => backend.inbox_status(&name, previous.as_ref()).map(|((total, unread), inbox)| {
                WorkerEventKind::MailboxStatus { name, total, unread, inbox: Some(inbox) }
            }),
            ActionKind::OpenMailbox { name, offset, limit, hint } => {
                let start = Instant::now();
                Ok(match backend.open_mailbox_with_hint(&name, offset, limit, hint) {
                    Ok(page) => WorkerEventKind::MailboxOpened {
                        identity: backend.mailbox_cache_identity(&name),
                        uid_next: backend.mailbox_uid_next(&name),
                        name, page, elapsed_micros: start.elapsed().as_micros(),
                    },
                    Err(error) => WorkerEventKind::MailboxOpenFailed {
                        name, error: WorkerError::from_error(error),
                    },
                })
            }
            ActionKind::QueryMailbox { name, view, cached_ids, offset, limit } => {
                Ok(match backend.query_mailbox(&name, &view, offset, limit) {
                    Ok(page) => {
                        let mut memberships = backend.cached_thread_memberships(&cached_ids);
                        // JMAP already includes its native thread IDs in each summary.
                        memberships.extend(page.messages.iter().filter_map(|message| Some((message.id.clone(), message.thread_id.clone()?))));
                        WorkerEventKind::MailboxQueried {
                            identity: backend.mailbox_cache_identity(&name),
                            name, page, memberships,
                            elapsed_micros: started.elapsed().as_micros(),
                        }
                    },
                    Err(error) => WorkerEventKind::MailboxOpenFailed { name, error: WorkerError::from_error(error) },
                })
            }
            ActionKind::DiscoverThread { name, view, cached_ids, limit } => Ok(discover_thread(backend, name, &view, &cached_ids, limit)),
            ActionKind::FetchBody { mailbox, id, html_width } => Ok(match backend.fetch_body(&mailbox, &id) {
                Ok(mut body) => {
                    if let Some(width) = html_width {
                        crate::mail::layout_message_html(&mut body, width);
                    }
                    WorkerEventKind::BodyLoaded(body)
                },
                Err(error) => WorkerEventKind::BodyLoadFailed { error: WorkerError::from_error(error) },
            }),
            ActionKind::PrefetchBodies { .. } | ActionKind::WarmMailbox { .. } => unreachable!("prefetch requires background worker"),
            ActionKind::FetchRawForCompose { mailbox, id, cache } => {
                Ok(WorkerEventKind::DraftPrepared {
                    id: id.clone(),
                    result: fetch_raw_cached(backend, &mailbox, &id, cache.as_ref())
                        .and_then(|raw| crate::app::Composer::from_server_draft(id, raw))
                        .map(Box::new)
                        .map_err(|error| format!("{error:#}")),
                })
            },
            ActionKind::PrepareCompose { mailbox, id, summary, intent, own_addresses, cache, cache_only } => {
                Ok(WorkerEventKind::ComposePrepared {
                    id: id.clone(),
                    result: (if cache_only {
                        cached_compose_raw(&mailbox, &id, cache.as_ref())
                    } else {
                        fetch_raw_cached(backend, &mailbox, &id, cache.as_ref())
                    })
                        .and_then(|raw| crate::app::Composer::prepare_original(
                            &summary, id, raw, intent, &own_addresses,
                        ))
                        .map(Box::new)
                        .map_err(|error| format!("{error:#}")),
                })
            },
            ActionKind::FetchAttachments { mailbox, id, cache } =>
                attachment_event(backend, &mailbox, &id, cache.as_ref()),
            ActionKind::RemoteAttachment { mailbox, job } => Ok(WorkerEventKind::RemoteAttachmentCompleted(
                download_attachment_job(backend, &mailbox, job).map_err(|e| format!("{e:#}")))),
            ActionKind::ExportRaw { mailbox, id, path } =>
                write_export_stream(&path, |output| backend.export_raw_into(&mailbox, &id, output))
                    .map(|()| WorkerEventKind::MessageExported { path }),
            ActionKind::SetSeen { mailbox, id, seen } => backend
                .set_seen(&mailbox, &id, seen)
                .map(|()| WorkerEventKind::SeenChanged { id, seen }),
            ActionKind::Delete { mailbox, id } => backend
                .delete(&mailbox, &id)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::DeleteMany { mailbox, ids } => backend
                .delete_many(&mailbox, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::Move { source, destination, id } => backend
                .move_to(&source, &destination, &id)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::MoveMany { source, destination, ids } => backend
                .move_many(&source, &destination, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::ArchiveMany { source, destination, ids } => backend
                .archive_many(&source, &destination, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::UnarchiveMany { source, destination, ids } => backend
                .unarchive_many(&source, &destination, &ids)
                .map(|()| WorkerEventKind::MutationComplete),
            ActionKind::SaveDraft { message, mailbox } => {
                // Save failures must not invalidate unrelated mailbox state.
                // A local recovery copy is already on disk at this point.
                Ok(match crate::outgoing::build_draft(&message, &message.from)
                    .and_then(|raw| backend.append_draft(&mailbox, &raw)) {
                    Ok(()) => WorkerEventKind::DraftSaved { mailbox },
                    Err(error) => WorkerEventKind::DraftSaveFailed {
                        mailbox, error: WorkerError::from_error(error),
                    },
                })
            }
            ActionKind::Send {
                message,
                sent_mailbox,
                copy_to,
            } => (|| -> anyhow::Result<WorkerEventKind> {
                if let Some(receipt) = backend.submit_message(&message, sent_mailbox.as_deref(), copy_to.as_deref())? {
                    return Ok(WorkerEventKind::MessageSent {
                        smtp_status: "JMAP".to_owned(),
                        sent_mailbox: receipt.sent_mailbox,
                        affected_mailboxes: receipt.affected_mailboxes,
                        sent_copy_saved: receipt.sent_copy_saved,
                        sent_copy_warning: receipt.sent_copy_warning,
                        sent_copy_disabled: copy_to.as_ref().is_some_and(Vec::is_empty),
                    });
                }
                match outgoing.as_deref_mut() {
                    Some(sender) => sender.send(&message).map(|receipt| {
                        let mut sent_copy_saved = false;
                        let mut sent_copy_warning = None;
                        let targets = match copy_to.as_ref() {
                            Some(folders) => folders.clone(),
                            None if receipt.server_files_sent_copy => Vec::new(),
                            None => sent_mailbox.clone().into_iter().collect(),
                        };
                        for mailbox in &targets {
                            match backend.append_message(mailbox, &receipt.raw_message, true) {
                                Ok(()) => sent_copy_saved = true,
                                Err(_error) => {
                                    // SMTP already succeeded; never retry the send
                                    // merely because a server-side Sent APPEND fails.
                                    sent_copy_warning = Some(format!(
                                        "SMTP accepted the message, but saving a Sent copy to {mailbox:?} failed"
                                    ));
                                }
                            }
                        }
                        if copy_to.is_none() && !receipt.server_files_sent_copy && targets.is_empty() {
                            sent_copy_warning = Some("message submitted, but no Sent mailbox was advertised".to_owned());
                        }
                        WorkerEventKind::MessageSent {
                            smtp_status: receipt.smtp_status,
                            sent_mailbox,
                            affected_mailboxes: targets,
                            sent_copy_saved,
                            sent_copy_warning,
                            sent_copy_disabled: copy_to.as_ref().is_some_and(Vec::is_empty),
                        }
                    }),
                    None => Err(anyhow::anyhow!(
                        "no outgoing transport configured for this account"
                    )),
                }
            })(),
            ActionKind::WakePriority => unreachable!("priority wake belongs to status worker"),
            ActionKind::Shutdown => unreachable!(),
        }
        .unwrap_or_else(|err| WorkerEventKind::Error(WorkerError::from_error(err)));

        crate::perf::record(
            operation,
            Some(queue_wait),
            started.elapsed(),
            !matches!(
                &event,
                WorkerEventKind::Error(_)
                    | WorkerEventKind::DraftSaveFailed { .. }
                    | WorkerEventKind::MailboxOpenFailed { .. }
                    | WorkerEventKind::BodyLoadFailed { .. }
            ),
        );

        if operation == "worker_open_mailbox" || operation == "worker_fetch_body" {
            crate::perf::record_request(
                "worker_complete",
                request_id,
                Some(queue_wait),
                started.elapsed(),
                !matches!(
                    &event,
                    WorkerEventKind::Error(_)
                        | WorkerEventKind::MailboxOpenFailed { .. }
                        | WorkerEventKind::BodyLoadFailed { .. }
                ),
            );
        }
        // Retired generations never publish stale mailbox results/errors.
        // Every production foreground event remains tagged through the UI
        // event queue, including results sent immediately before rotation.
        let should_publish = active_generation.load(Ordering::Acquire) == generation;
        if should_publish
            && runtime_tx
                .send(RuntimeEvent::ForegroundWorker {
                    generation,
                    event: WorkerEvent {
                        account,
                        request_id,
                        kind: event,
                    },
                })
                .is_err()
        {
            break;
        }
    }
}

/// Drop obsolete navigation, preserving the relative order of mutations and
/// other side effects. The newest mailbox open wins; a newer open invalidates
/// earlier body fetches as well. An in-flight IMAP request can't be canceled.
pub(super) fn compact_navigation_queue(pending: &mut VecDeque<Action>) {
    let latest_mailbox = pending
        .iter()
        .rev()
        .find_map(|action| match &action.kind {
            ActionKind::OpenMailbox { name, .. } | ActionKind::QueryMailbox { name, .. } => {
                Some(name.as_str())
            }
            _ => None,
        })
        .map(str::to_owned);
    let mut seen_open = false;
    let mut seen_body = false;
    let mut kept = VecDeque::with_capacity(pending.len());
    while let Some(action) = pending.pop_back() {
        match &action.kind {
            ActionKind::OpenMailbox { .. } | ActionKind::QueryMailbox { .. } if seen_open => {
                continue;
            }
            ActionKind::OpenMailbox { .. } | ActionKind::QueryMailbox { .. } => {
                seen_open = true;
                seen_body = true;
            }
            ActionKind::FetchBody { mailbox, .. }
                if seen_body
                    || latest_mailbox
                        .as_deref()
                        .is_some_and(|name| name != mailbox) =>
            {
                continue;
            }
            ActionKind::FetchBody { .. } => seen_body = true,
            _ => {}
        }
        kept.push_front(action);
    }
    *pending = kept;
}

/// Choose the newest prefetch window, but after a small body burst give an
/// awaiting unread-count update priority. Returns None only for an empty queue.
pub(super) fn pop_background_action(
    pending: &mut VecDeque<Action>,
    bodies_since_status: usize,
) -> Option<Action> {
    // Explicit requests (selected bodies and attachments) outrank both STATUS
    // and speculative prefetch. Keep every attachment request; only an old
    // selected-body request is safely superseded by a newer body request.
    if let Some(index) = pending.iter().position(|action| {
        matches!(
            &action.kind,
            ActionKind::PrepareCompose { .. } | ActionKind::FetchRawForCompose { .. }
        )
    }) {
        return pending.remove(index);
    }
    if let Some(index) = pending.iter().position(|action| {
        matches!(
            &action.kind,
            ActionKind::FetchBody { .. }
                | ActionKind::FetchAttachments { .. }
                | ActionKind::RemoteAttachment { .. }
        )
    }) {
        let chosen = pending.remove(index);
        if chosen
            .as_ref()
            .is_some_and(|action| matches!(&action.kind, ActionKind::FetchBody { .. }))
        {
            // The latest requested reader body should win if navigation raced.
            if let Some(newer) = pending
                .iter()
                .rposition(|action| matches!(&action.kind, ActionKind::FetchBody { .. }))
            {
                let latest = pending.remove(newer);
                pending.retain(|action| !matches!(&action.kind, ActionKind::FetchBody { .. }));
                return latest;
            }
        }
        return chosen;
    }
    let status_index = (bodies_since_status >= 4)
        .then(|| {
            pending.iter().position(|action| {
                matches!(
                    &action.kind,
                    ActionKind::MailboxStatus(_) | ActionKind::InboxStatus { .. }
                )
            })
        })
        .flatten();
    if let Some(index) = status_index {
        return pending.remove(index);
    }
    let prefetch_index = pending
        .iter()
        .rposition(|action| matches!(&action.kind, ActionKind::PrefetchBodies { .. }));
    if let Some(index) = prefetch_index {
        let newest = pending.remove(index);
        pending.retain(|action| !matches!(&action.kind, ActionKind::PrefetchBodies { .. }));
        newest
    } else if let Some(index) = pending
        .iter()
        .position(|action| !matches!(&action.kind, ActionKind::WarmMailbox { .. }))
    {
        pending.remove(index)
    } else {
        pending.pop_front()
    }
}

pub(super) fn discover_thread(
    backend: &mut dyn MailBackend,
    name: String,
    view: &crate::backend::MailboxView,
    cached_ids: &[MessageId],
    limit: usize,
) -> WorkerEventKind {
    match backend.query_mailbox(&name, view, 0, limit) {
        Ok(mut page) => {
            let identity = backend.mailbox_cache_identity(&name);
            let mut memberships = backend.cached_thread_memberships(cached_ids);
            // An explicit expansion may fetch several bounded pages, but
            // never monopolize the worker for an unbounded conversation.
            let mut seen: HashSet<_> = page.messages.iter().map(|m| m.id.clone()).collect();
            for _ in 0..3 {
                if !page.has_more || page.messages.len() >= crate::cache::MAX_MESSAGES_PER_FOLDER {
                    break;
                }
                let offset = page.next_offset;
                let remaining = crate::cache::MAX_MESSAGES_PER_FOLDER - page.messages.len();
                let Ok(next) =
                    backend.query_mailbox(&name, view, offset, limit.max(1).min(remaining))
                else {
                    break;
                };
                if next.offset != offset
                    || next.next_offset <= offset
                    || !identity.as_ref().is_some_and(|old| {
                        backend
                            .mailbox_cache_identity(&name)
                            .as_ref()
                            .is_some_and(|new| old.same_generation(new))
                    })
                    || next.messages.iter().any(|m| seen.contains(&m.id))
                {
                    break;
                }
                page.next_offset = next.next_offset;
                page.has_more = next.has_more;
                for message in next.messages {
                    if seen.insert(message.id.clone()) {
                        page.messages.push(message);
                    }
                }
                memberships.extend(backend.cached_thread_memberships(cached_ids));
            }
            memberships.extend(
                page.messages
                    .iter()
                    .filter_map(|message| Some((message.id.clone(), message.thread_id.clone()?))),
            );
            WorkerEventKind::ThreadDiscovered {
                identity,
                name,
                page,
                memberships,
            }
        }
        Err(_) => WorkerEventKind::ThreadDiscoveryFailed,
    }
}
