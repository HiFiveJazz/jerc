//! Mailbox discovery, synchronization and STATUS reconciliation.

use super::*;

impl App {
    pub(super) fn handle_mailbox_event(&mut self, mut event: WorkerEvent) {
        // A query can enrich a canonical snapshot only after verifying the
        // request, folder and generation. It cannot establish a generation.
        event.kind = match event.kind {
            WorkerEventKind::MailboxQueried {
                name,
                page,
                identity,
                memberships,
                elapsed_micros,
            } => {
                let Some(account) = self.accounts.get_mut(event.account) else {
                    return;
                };
                if account.native_view.is_none()
                    || account.page_request_id != Some(event.request_id)
                    || account.current_mailbox.as_deref() != Some(name.as_str())
                {
                    return;
                }
                if account.cache_identities.get(&name).is_some_and(|old| {
                    identity
                        .as_ref()
                        .is_some_and(|new| old.same_generation(new))
                }) && let Some(cached) = account.mailbox_cache.get_mut(&name)
                    && cached.identity.as_ref().is_some_and(|old| {
                        identity
                            .as_ref()
                            .is_some_and(|new| old.same_generation(new))
                    })
                    && crate::threading::merge_native_ids(&mut cached.messages, &memberships)
                    && let Some(writer) = &account.cache_writer
                {
                    let snapshot = CachedMailbox {
                        thread_messages: cached.thread_messages.clone(),
                        messages: cached.messages.clone(),
                        bodies: cache::persistable_bodies_near_compact(
                            &cached.messages,
                            &cached.bodies,
                            cached.message_index,
                        ),
                        message_index: cached.message_index,
                        message_view_offset: cached.message_view_offset,
                        next_page_offset: cached.next_page_offset,
                        has_more: cached.has_more,
                        identity: cached.identity.clone(),
                        uid_next: cached.uid_next,
                    };
                    writer.send(cache::Change::Folder(name.clone(), snapshot));
                }
                WorkerEventKind::MailboxOpened {
                    name,
                    page,
                    identity,
                    uid_next: None,
                    elapsed_micros,
                }
            }
            other => other,
        };
        // Only parse account identities when an event needs contact/recipient matching.
        let own_addresses = if matches!(&event.kind, WorkerEventKind::MailboxOpened { .. }) {
            self.accounts
                .iter()
                .filter_map(|a| a.from.parse::<lettre::message::Mailbox>().ok())
                .map(|a| a.email.to_string())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let Some(account) = self.accounts.get_mut(event.account) else {
            return;
        };
        match event.kind {
            WorkerEventKind::ThreadDiscoveryFailed => {
                account.inline_thread_requests.remove(&event.request_id);
                // Optional read-only discovery must not disconnect the account.
            }
            WorkerEventKind::ThreadDiscovered {
                name,
                page,
                identity,
                memberships,
            } => {
                let Some(seed) = account.inline_thread_requests.remove(&event.request_id) else {
                    return;
                };
                if account.native_view.is_some()
                    || !account.threading_enabled
                    || account.current_mailbox.as_deref() != Some(name.as_str())
                    || account.cached_mailbox_unverified()
                    || account.capabilities.is_none()
                    || page.offset != 0
                    || !account.expanded_threads.contains(&seed)
                    || !account.cache_identities.get(&name).is_some_and(|old| {
                        identity
                            .as_ref()
                            .is_some_and(|new| old.same_generation(new))
                    })
                {
                    return;
                }
                let Some(seed_message) = account.messages.iter().find(|m| m.id == seed) else {
                    return;
                };
                let native = memberships
                    .get(&seed)
                    .or(seed_message.thread_id.as_ref())
                    .cloned();
                let Some(native) = native else {
                    return;
                };
                // Preserve optimistic flags and all existing rows. Only native
                // identity hints can modify a canonical summary here.
                let selected = account.selected_message().map(|m| m.id.clone());
                crate::threading::merge_native_ids(&mut account.messages, &memberships);
                let mut existing: HashSet<_> =
                    account.messages.iter().map(|m| m.id.clone()).collect();
                let remaining = cache::MAX_MESSAGES_PER_FOLDER
                    .saturating_sub(account.inline_thread_extra.len());
                let Some(identity) = identity else {
                    return;
                };
                for message in page
                    .messages
                    .into_iter()
                    .filter(|m| m.thread_id.as_ref() == Some(&native) && identity.accepts(&m.id))
                    .filter(|m| existing.insert(m.id.clone()))
                    .take(remaining)
                {
                    account.inline_thread_extra.insert(message.id.clone());
                    account.messages.push(message);
                }
                // Keep selected rows anchored by ID, even if authoritative
                // membership corrects the local grouping.
                account.overlay_deferred_mutations();
                account.overlay_deferred_seen();
                let live: HashSet<_> = account.messages.iter().map(|m| &m.id).collect();
                account.inline_thread_extra.retain(|id| live.contains(id));
                if let Some(index) =
                    selected.and_then(|id| account.messages.iter().position(|m| m.id == id))
                {
                    account.message_index = index;
                    if self.message_open && event.account == self.account_index {
                        account
                            .expanded_threads
                            .insert(account.messages[index].id.clone());
                    }
                }
                account.smooth_message_target = None;
                account.last_prefetch_anchor = None;
                account.normalize_search_selection();
                account.persist_current_cache();
                if event.account == self.account_index {
                    if page.has_more {
                        self.status =
                            StatusLine::info("Conversation partially cached (bounded lookup)");
                    }
                    self.visual_anchor = None;
                    if self.preview_split || self.message_open {
                        account.request_selected_body();
                    }
                }
            }
            WorkerEventKind::Connected { capabilities } => {
                // A delayed response from an abandoned connection must not
                // overwrite the new session's state.
                if account.connect_request_id != Some(event.request_id) {
                    return;
                }
                account.connecting = false;
                account.connect_request_id = None;
                account.capabilities = Some(capabilities);
                // Get the mailbox list before replaying offline work. If the
                // worker queue cannot accept LIST, retry via reconnect later.
                if !account.request_mailbox_list() {
                    account.mark_disconnected();
                    self.status = StatusLine::temporary_error(
                        format!("{}: could not schedule mailbox discovery", account.name),
                        Duration::from_secs(6),
                    );
                    return;
                }
                self.status = StatusLine::info(format!("{} connected", account.name));
            }
            WorkerEventKind::Mailboxes(mailboxes) => {
                // LIST may complete after a newer reconnect/refresh. Never let
                // an old snapshot replace mailbox names or counts.
                if account.mailbox_list_request_id != Some(event.request_id) {
                    account.is_abandoned_list_request(event.request_id);
                    return;
                }
                let from_push = std::mem::take(&mut account.push_list_inflight);
                account.mailbox_list_request_id = None;
                account.mailboxes_loading = false;
                account.sync_progress_at = None;
                // A matching, completed LIST on a connected account proves
                // recovery. Do not clear a newer incident if another worker
                // already marked the account disconnected in the meantime.
                if account.capabilities.is_some() {
                    account.offline_incident_active = false;
                }
                let wanted = account
                    .current_mailbox
                    .clone()
                    .unwrap_or_else(|| account.default_mailbox.clone());
                let discovered: Vec<Mailbox> = mailboxes
                    .into_iter()
                    .filter(|mailbox| {
                        !crate::config::mailbox_is_excluded(
                            &mailbox.name,
                            &account.exclude_mailboxes,
                        )
                    })
                    .collect();
                // Initial discovery establishes a baseline. Only a push-driven
                // list can infer arrivals from the previous live Inbox count.
                let new_inbox = if from_push {
                    account
                        .mailboxes
                        .iter()
                        .find(|mailbox| mailbox.role == MailboxRole::Inbox)
                        .and_then(|old| {
                            discovered
                                .iter()
                                .find(|new| new.name == old.name)
                                .map(|new| {
                                    (
                                        new.name.clone(),
                                        inbox_new_mail_count(
                                            true,
                                            old.counts_known && new.counts_known,
                                            old.total,
                                            new.total,
                                        ),
                                    )
                                })
                        })
                } else {
                    None
                };
                account.reconcile_mailbox_list(discovered);
                if let Some((name, new_count)) = new_inbox.filter(|(_, count)| *count > 0) {
                    if self.new_mail_notifications_enabled {
                        self.new_mail_since_launch =
                            self.new_mail_since_launch.saturating_add(new_count);
                        if self.new_mail_bell_enabled {
                            self.new_mail_bell_pending = true;
                        }
                        if account
                            .worker
                            .post(ActionKind::NewMailPreview {
                                mailbox: name,
                                count: new_count,
                            })
                            .is_err()
                        {
                            self.new_mail_notices.push_back(NewMailNotice {
                                account: account.name.clone(),
                                count: new_count,
                                sender: String::new(),
                                subject: String::new(),
                                excerpt: String::new(),
                            });
                        }
                    }
                    self.status = StatusLine::temporary(
                        format!("{} new message(s) in {}", new_count, account.name),
                        Duration::from_secs(6),
                    );
                }
                account.persist_mailboxes();
                account.mailbox_index = account
                    .mailboxes
                    .iter()
                    .position(|mailbox| mailbox.name == wanted)
                    .or_else(|| {
                        account
                            .mailboxes
                            .iter()
                            .position(|mailbox| mailbox.name == account.default_mailbox)
                    })
                    .unwrap_or(0);
                account.clamp_selection();
                if account.mailboxes.is_empty() {
                    account.current_mailbox = None;
                    account.native_view = None;
                    account.local_thread_view = None;
                    account.reset_inline_threads();
                    account.messages.clear();
                    account.bodies.clear();
                }
                if let Some(name) = account
                    .selected_mailbox()
                    .map(|mailbox| mailbox.name.clone())
                {
                    account.queue_unknown_mailbox_statuses(Some(&name));
                    if account.current_mailbox.as_deref() != Some(name.as_str())
                        && !account.restore_cached_mailbox(&name)
                    {
                        account.current_mailbox = Some(name.clone());
                        account.native_view = None;
                        account.local_thread_view = None;
                        account.reset_inline_threads();
                        account.messages.clear();
                        account.bodies.clear();
                        account.message_index = 0;
                        account.message_view_offset = 0;
                        account.next_page_offset = 0;
                        account.has_more = false;
                    }
                    // Cached rows remain visible while the first-page refresh
                    // runs on the interactive IMAP worker.
                    if from_push
                        && (account.page_inflight.is_some()
                            || (self.message_open && event.account == self.account_index))
                    {
                        account.pending_inbox_refresh = true;
                    } else if account.page_inflight.is_none() {
                        account.pending_inbox_refresh = false;
                        let _ = account.request_mailbox_page(name, 0);
                    }
                }
                Self::pump_mailbox_status(account);
                account.plan_mailbox_warming();
                account.pump_mailbox_warming();
                account.pump_deferred_mutations();
                account.pump_outbox();
            }
            WorkerEventKind::AccountChanged => {
                // One follow-up for an entire burst, including events received
                // while a previous mailbox list or message page is in flight.
                account.pending_account_refresh = true;
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::MailboxChanged { name } => {
                if account.current_mailbox.as_deref() == Some(name.as_str()) {
                    account.pending_inbox_refresh = true;
                }
                // An IDLE wakeup during another STATUS is not expendable:
                // retain one refresh per mailbox until the current request ends.
                account.queue_status_refresh(name.clone());
                if account.current_mailbox.as_deref() != Some(name.as_str()) {
                    account.forget_cached_folder(&name);
                    account.rewarm_mailbox(&name);
                }
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::MailboxStatus {
                name,
                total,
                unread,
            } => {
                let Some(request) = account.status_requests.finish(event.request_id) else {
                    // Never apply an old/out-of-order count result to the UI.
                    return;
                };
                if request.name != name {
                    Self::pump_mailbox_status(account);
                    return;
                }
                let from_poll = request.reason == StatusReason::Refresh;
                let mut new_count = 0usize;
                let mut changed = false;
                if let Some(mailbox) = account.mailboxes.iter_mut().find(|m| m.name == name) {
                    changed = mailbox.counts_known
                        && (mailbox.total != total || mailbox.unread != unread);
                    new_count = inbox_new_mail_count(
                        from_poll && mailbox.role == MailboxRole::Inbox,
                        mailbox.counts_known,
                        mailbox.total,
                        total,
                    );
                    mailbox.total = total;
                    mailbox.unread = unread;
                    mailbox.counts_known = true;
                }
                if new_count > 0 {
                    if self.new_mail_notifications_enabled {
                        self.new_mail_since_launch =
                            self.new_mail_since_launch.saturating_add(new_count);
                    }
                    // Keep the UID-keyed header cache on new arrivals. The
                    // next SELECT validates UIDVALIDITY and obtains a current
                    // UID+FLAGS page before we reuse any old summary. Dropping
                    // the cache here would force full header downloads on
                    // precisely the refreshes that benefit from delta fetches.
                    self.status = StatusLine::temporary(
                        format!("{} new message(s) in {}", new_count, account.name,),
                        Duration::from_secs(6),
                    );
                    if self.new_mail_bell_enabled && self.new_mail_notifications_enabled {
                        self.new_mail_bell_pending = true;
                    }
                    if self.new_mail_notifications_enabled {
                        // Use the existing background IMAP session; do not block
                        // rendering, fetch on the foreground session, or change \Seen.
                        if account
                            .worker
                            .post(ActionKind::NewMailPreview {
                                mailbox: name.clone(),
                                count: new_count,
                            })
                            .is_err()
                        {
                            self.new_mail_notices.push_back(NewMailNotice {
                                account: account.name.clone(),
                                count: new_count,
                                sender: String::new(),
                                subject: String::new(),
                                excerpt: String::new(),
                            });
                        }
                    }
                    if account.current_mailbox.as_deref() == Some(name.as_str()) {
                        // Never replace the reader underneath someone who is
                        // in the middle of an email. Defer the page refresh
                        // until the reader closes; the count still updates.
                        if self.message_open && event.account == self.account_index
                            || account.page_inflight.is_some()
                        {
                            account.pending_inbox_refresh = true;
                        } else {
                            account.pending_inbox_refresh = false;
                            account.restore_message_index = Some(account.message_index);
                            let _ = account.request_mailbox_page(name.clone(), 0);
                        }
                    }
                }
                if (changed || (from_poll && account.pending_inbox_refresh))
                    && new_count == 0
                    && account.current_mailbox.as_deref() == Some(name.as_str())
                {
                    if account.page_inflight.is_some()
                        || (self.message_open && event.account == self.account_index)
                    {
                        account.pending_inbox_refresh = true;
                    } else {
                        account.pending_inbox_refresh = false;
                        let _ = account.request_mailbox_page(name.clone(), 0);
                    }
                }
                if changed
                    && account.current_mailbox.as_deref() != Some(name.as_str())
                    && account.mailbox_cache.contains_key(&name)
                {
                    account.rewarm_mailbox(&name);
                }
                account.persist_mailboxes();
                account.pump_deferred_mutations();
                account.pump_outbox();
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::NewMailPreview {
                count,
                sender,
                subject,
                excerpt,
            } => {
                if self.new_mail_notifications_enabled {
                    self.new_mail_notices.push_back(NewMailNotice {
                        account: account.name.clone(),
                        count,
                        sender,
                        subject,
                        excerpt,
                    });
                }
            }
            WorkerEventKind::MailboxWarmed {
                name,
                page,
                identity,
                uid_next,
            } => {
                if account.mailbox_warm_request.as_ref() != Some(&(event.request_id, name.clone()))
                {
                    return; // superseded by a mutation or newer LIST
                }
                account.mailbox_warm_request = None;
                if account.current_mailbox.as_deref() != Some(name.as_str()) {
                    account.accept_warmed_mailbox(name, page, identity, uid_next);
                }
                account.pump_mailbox_warming();
            }
            WorkerEventKind::MailboxWarmFailed { name } => {
                if account.mailbox_warm_request.as_ref() == Some(&(event.request_id, name)) {
                    account.mailbox_warm_request = None;
                    // A failed optional preload is not an account failure.
                    // Stop the series until the next LIST/explicit refresh.
                    account.mailbox_warm_queue.clear();
                }
            }
            WorkerEventKind::MailboxOpened {
                name,
                mut page,
                identity,
                uid_next,
                elapsed_micros,
            } => {
                // A newer folder/page request supersedes this result even if
                // both requests happen to name the same mailbox.
                if account.page_request_id != Some(event.request_id) {
                    crate::perf::record_request(
                        "ui_mailbox_stale",
                        event.request_id,
                        None,
                        Duration::ZERO,
                        true,
                    );
                    return;
                }
                account.page_request_id = None;
                account.sync_progress_at = None;
                // A successful foreground mailbox refresh also proves recovery
                // when the account was already connected before the failure.
                // Never clear an incident using a stale/offline page event.
                if account.capabilities.is_some() {
                    account.offline_incident_active = false;
                }
                if let Some(view) = account.native_view.as_ref() {
                    let label = view.label();
                    // Query results never replace the canonical cache or its checkpoint.
                    account.page_inflight = None;
                    account.messages_loading = false;
                    if account.current_mailbox.as_deref() != Some(name.as_str()) {
                        return;
                    }
                    if !account.cache_identities.get(&name).is_some_and(|old| {
                        identity
                            .as_ref()
                            .is_some_and(|new| old.same_generation(new))
                    }) {
                        // A temporary view cannot establish a new generation.
                        account.forget_cached_folder(&name);
                        Self::activate_selected_mailbox(account, false);
                        return;
                    }
                    account.unverified_mailboxes.remove(&name);
                    if page.offset == 0 {
                        let selected = account.selected_message().map(|m| m.id.clone());
                        // A cached reply can be opened while native discovery
                        // is in flight. Never switch its reader to another
                        // email if the first server page omits that reply.
                        if self.message_open
                            && event.account == self.account_index
                            && account.local_thread_view.is_some()
                            && selected
                                .as_ref()
                                .is_some_and(|id| !page.messages.iter().any(|m| &m.id == id))
                        {
                            account.pending_inbox_refresh = true;
                            return;
                        }
                        account.messages = page.messages;
                        account.message_index = selected
                            .and_then(|id| account.messages.iter().position(|m| m.id == id))
                            .unwrap_or(0);
                        account.message_view_offset = 0;
                    } else {
                        let existing: HashSet<_> =
                            account.messages.iter().map(|m| m.id.clone()).collect();
                        account.messages.extend(
                            page.messages
                                .into_iter()
                                .filter(|m| !existing.contains(&m.id)),
                        );
                    }
                    account.next_page_offset = page.next_offset;
                    account.has_more = page.has_more && page.next_offset > page.offset;
                    let live: HashSet<_> = account.messages.iter().map(|m| m.id.clone()).collect();
                    account.bodies.retain(|id, _| live.contains(id));
                    account.overlay_deferred_mutations();
                    account.overlay_deferred_seen();
                    account.clamp_selection();
                    account.normalize_search_selection();
                    self.status = StatusLine::info(format!(
                        "{label}: {} messages loaded (Esc to exit)",
                        account.messages.len()
                    ));
                    if account.pending_inbox_refresh
                        && !(self.message_open && event.account == self.account_index)
                    {
                        account.pending_inbox_refresh = false;
                        let _ = account.request_mailbox_page(name, 0);
                    } else if account.messages.is_empty() && account.has_more {
                        let _ = account.request_next_page();
                    }
                    if event.account == self.account_index
                        && (self.preview_split || self.message_open)
                    {
                        account.request_selected_body();
                    }
                    Self::pump_mailbox_status(account);
                    return;
                }
                let first_page = page.offset == 0;
                let supplemental_compatible =
                    match (account.cache_identities.get(&name), identity.as_ref()) {
                        (Some(CacheIdentity::Imap(old)), Some(CacheIdentity::Imap(new))) => {
                            old == new
                        }
                        (
                            Some(CacheIdentity::Jmap {
                                email_state: Some(old),
                                ..
                            }),
                            Some(CacheIdentity::Jmap {
                                email_state: Some(new),
                                ..
                            }),
                        ) => old == new,
                        _ => false,
                    };
                let generation_changed = account.cache_identities.get(&name).is_some_and(|old| {
                    identity
                        .as_ref()
                        .is_none_or(|new| !old.same_generation(new))
                });
                if generation_changed && !first_page {
                    // A page from a different server generation cannot be
                    // appended to the old cache. Reconcile from offset zero.
                    account.page_inflight = None;
                    account.messages_loading = false;
                    let _ = account.request_mailbox_page(name, 0);
                    return;
                }
                let mut identity = identity;
                if !first_page
                    && let Some(new) = identity.as_mut()
                    && matches!(new, CacheIdentity::Jmap { .. })
                    && account.cache_identities.get(&name) != Some(&*new)
                {
                    // A multi-page snapshot spans multiple Email states;
                    // keep it readable but don't reuse its mutable summaries.
                    new.invalidate_checkpoint();
                }
                if generation_changed {
                    // Cached UIDs may point to entirely different emails after
                    // a server UIDVALIDITY reset: drop all old derived data.
                    account.forget_cached_folder(&name);
                    account.reset_inline_threads();
                    account.bodies.clear();
                }
                if let Some(identity) = &identity {
                    account
                        .cache_identities
                        .insert(name.clone(), identity.clone());
                } else {
                    account.cache_identities.remove(&name);
                }
                if let Some(next) = uid_next {
                    account.uid_nexts.insert(name.clone(), next);
                } else {
                    account.uid_nexts.remove(&name);
                }
                account.unverified_mailboxes.remove(&name);
                if first_page {
                    let before = account.deferred_seen.len();
                    account.deferred_seen.retain(|entry| {
                        entry.mailbox != name
                            || Some(entry.validity)
                                == identity.as_ref().and_then(CacheIdentity::uid_validity)
                    });
                    if account.deferred_seen.len() != before {
                        account.persist_deferred_seen();
                    }
                }
                let selected_name = account
                    .selected_mailbox()
                    .map(|mailbox| mailbox.name.clone());
                if selected_name.as_deref() != Some(name.as_str()) {
                    account.page_inflight = None;
                    account.messages_loading = false;
                    if page.offset == 0 {
                        account.remember_mailbox(
                            name,
                            CachedMailbox {
                                thread_messages: Vec::new(),
                                messages: page.messages,
                                bodies: HashMap::new(),
                                message_index: 0,
                                message_view_offset: 0,
                                next_page_offset: page.next_offset,
                                has_more: page.has_more,
                                identity,
                                uid_next,
                            },
                        );
                    }
                    return;
                }

                if account
                    .page_inflight
                    .as_ref()
                    .is_some_and(|(pending_name, pending_offset)| {
                        pending_name == &name && *pending_offset == page.offset
                    })
                {
                    account.page_inflight = None;
                }
                account.messages_loading = false;
                crate::perf::record_request(
                    "ui_mailbox_applied",
                    event.request_id,
                    None,
                    Duration::ZERO,
                    true,
                );
                let same_folder = account.current_mailbox.as_deref() == Some(name.as_str());
                account.current_mailbox = Some(name.clone());
                if page.offset == 0 {
                    if self.message_open
                        && event.account == self.account_index
                        && !generation_changed
                        && account.selected_message().is_some_and(|message| {
                            account.inline_thread_extra.contains(&message.id)
                                && !page.messages.iter().any(|row| row.id == message.id)
                        })
                    {
                        account.pending_inbox_refresh = true;
                        return;
                    }
                    if same_folder
                        && !generation_changed
                        && matches!(identity, Some(CacheIdentity::Imap(_)))
                    {
                        let old_ids = account
                            .messages
                            .iter()
                            .filter_map(|m| {
                                let id = m.thread_id.as_ref().filter(|id| {
                                    id.starts_with("gmail:") || id.starts_with("imap-ref:")
                                })?;
                                Some((m.id.clone(), id.clone()))
                            })
                            .collect();
                        crate::threading::merge_native_ids(&mut page.messages, &old_ids);
                    }
                    let previous_index = account.message_index;
                    let previous_offset = account.message_view_offset;
                    let selected_id = (!generation_changed)
                        .then(|| account.selected_message().map(|message| message.id.clone()))
                        .flatten();
                    // Keep discovered replies distinct from the canonical page.
                    // A complete server page or changed generation invalidates
                    // absent replies; a partial page cannot prove deletion.
                    let mut incoming: HashSet<_> =
                        page.messages.iter().map(|m| m.id.clone()).collect();
                    let extras = if same_folder
                        && !generation_changed
                        && supplemental_compatible
                        && page.has_more
                        && account.native_view.is_none()
                    {
                        account
                            .messages
                            .iter()
                            .filter(|m| {
                                account.inline_thread_extra.contains(&m.id)
                                    && incoming.insert(m.id.clone())
                            })
                            .cloned()
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    account.inline_thread_extra = extras.iter().map(|m| m.id.clone()).collect();
                    account.inline_thread_requests.clear();
                    account.messages = page.messages;
                    account.messages.extend(extras);
                    account.message_index = selected_id
                        .and_then(|id| account.messages.iter().position(|message| message.id == id))
                        .or_else(|| account.restore_message_index.take())
                        .unwrap_or(0)
                        .min(account.messages.len().saturating_sub(1));
                    // Reconcile rows in place. A background arrival should not
                    // jump the user's viewport to the top of the folder.
                    account.message_view_offset = account
                        .message_index
                        .saturating_sub(previous_index.saturating_sub(previous_offset))
                        .min(account.messages.len().saturating_sub(1));
                    let live = account
                        .messages
                        .iter()
                        .map(|message| message.id.clone())
                        .collect::<HashSet<_>>();
                    account.expanded_threads.retain(|id| live.contains(id));
                    account.bodies.retain(|id, _| live.contains(id));
                } else {
                    let selected_id = account.selected_message().map(|message| message.id.clone());
                    // Normal IMAP FETCH does not include the extension IDs
                    // learned by native threading. Keep those hints on promotion.
                    if matches!(identity, Some(CacheIdentity::Imap(_))) {
                        let hints = account
                            .messages
                            .iter()
                            .filter(|m| account.inline_thread_extra.contains(&m.id))
                            .filter_map(|m| Some((m.id.clone(), m.thread_id.clone()?)))
                            .collect();
                        crate::threading::merge_native_ids(&mut page.messages, &hints);
                    }
                    let incoming: HashSet<_> = page.messages.iter().map(|m| m.id.clone()).collect();
                    account.messages.retain(|m| {
                        !account.inline_thread_extra.contains(&m.id) || !incoming.contains(&m.id)
                    });
                    account
                        .inline_thread_extra
                        .retain(|id| !incoming.contains(id));
                    let existing: HashSet<_> =
                        account.messages.iter().map(|m| m.id.clone()).collect();
                    account.messages.extend(
                        page.messages
                            .into_iter()
                            .filter(|m| !existing.contains(&m.id)),
                    );
                    account.messages.sort_by(|a, b| {
                        account
                            .inline_thread_extra
                            .contains(&a.id)
                            .cmp(&account.inline_thread_extra.contains(&b.id))
                            .then_with(|| b.timestamp.cmp(&a.timestamp))
                            .then_with(|| b.id.cmp(&a.id))
                    });
                    if let Some(index) = selected_id.as_ref().and_then(|id| {
                        account
                            .messages
                            .iter()
                            .position(|message| &message.id == id)
                    }) {
                        account.message_index = index;
                    }
                }
                account.next_page_offset = page.next_offset;
                account.has_more = page.has_more;
                account.overlay_deferred_mutations();
                // Keep an open reader attached to its exact message if a new
                // reply becomes the representative of the same conversation.
                if self.message_open
                    && event.account == self.account_index
                    && account.native_view.is_none()
                    && account.collapsed_threads()
                    && !account
                        .visible_message_indices()
                        .contains(&account.message_index)
                    && let Some(message) = account.messages.get(account.message_index)
                {
                    account.expanded_threads.insert(message.id.clone());
                }
                if !account.pending_seen.is_empty() || !account.pending_mutation_labels.is_empty() {
                    account.invalidate_sync_checkpoint(&name);
                }
                if first_page {
                    // Don't tear down an open offline reader when its cached
                    // mailbox finishes background reconciliation.
                    if !self.message_open || event.account != self.account_index {
                        account.preview_scroll = 0;
                        account.preview_max_scroll = 0;
                    }
                    account.smooth_message_target = None;
                    account.smooth_preview_target = None;
                    // A verified same-folder refresh can finish while the
                    // priority worker fetches the selected body. Keep the
                    // request if its UID survived this fresh page; otherwise
                    // invalidate it (especially after UIDVALIDITY changes).
                    let still_selected = !generation_changed
                        && account.body_request_id.is_some()
                        && account.pending_bodies.len() == 1
                        && account
                            .selected_message()
                            .is_some_and(|message| account.pending_bodies.contains(&message.id));
                    if !still_selected {
                        account.pending_bodies.clear();
                        account.body_request_id = None;
                    }
                    // Preserve live Seen request IDs across regular page
                    // refreshes. Clearing them would make valid ACKs look
                    // stale and cause duplicate journal replay.
                    if generation_changed {
                        account.pending_seen.clear();
                    }
                }
                account.last_scan_micros = Some(elapsed_micros);
                // Learn only lightweight envelope addresses already fetched
                // for normal browsing. Never scan bodies or junk folders.
                let mailbox_role = account
                    .mailboxes
                    .iter()
                    .find(|m| m.name == name)
                    .map(|m| &m.role);
                let is_sent = matches!(mailbox_role, Some(&MailboxRole::Sent));
                let is_inbox = matches!(mailbox_role, Some(&MailboxRole::Inbox));
                if ((is_inbox && self.contacts_learn_inbox)
                    || (is_sent && self.contacts_learn_sent))
                    && self.contacts_writable
                {
                    let mut changed = false;
                    for message in account.messages.iter().take(200) {
                        if is_inbox
                            && !own_addresses
                                .iter()
                                .any(|own| own.eq_ignore_ascii_case(&message.from_address))
                        {
                            changed |= self
                                .contacts
                                .upsert(&message.from_name, &message.from_address, false)
                                .unwrap_or(false);
                        }
                        if is_sent {
                            for (person, address) in recipients(&message.to)
                                .into_iter()
                                .chain(recipients(&message.cc))
                            {
                                if !own_addresses
                                    .iter()
                                    .any(|own| own.eq_ignore_ascii_case(&address))
                                {
                                    changed |= self
                                        .contacts
                                        .upsert(&person, &address, false)
                                        .unwrap_or(false);
                                }
                            }
                        }
                    }
                    if changed {
                        self.contacts_dirty = true;
                        if self.contacts_save_due.is_none() {
                            self.contacts_save_due = Some(Instant::now() + Duration::from_secs(3));
                        }
                        if let Some(modal) = self.contacts_modal.as_mut() {
                            modal.matches = self.contacts.filtered(&modal.query);
                            modal.ensure_visible(modal.matches.len());
                        }
                    }
                }
                account.overlay_deferred_seen();
                account.pump_deferred_seen();
                account.persist_current_cache();
                account.clamp_selection();
                account.normalize_search_selection();
                let visible = account.visible_message_indices();
                let search_needs_more = !account.search_query.is_empty()
                    && visible.len() < account.message_view_height
                    && account.has_more
                    && account.page_inflight.is_none();
                let current_position = visible
                    .iter()
                    .position(|index| *index == account.message_index)
                    .unwrap_or(0);
                let remaining = visible
                    .len()
                    .saturating_sub(current_position.saturating_add(1));
                let prefetch_distance = account
                    .message_view_height
                    .saturating_mul(4)
                    .max(account.page_size)
                    .max(PAGE_PREFETCH_MIN);
                // A single collapsed row may represent many cached messages.
                // Base automatic prefetch on canonical distance in that case,
                // so a long thread cannot trigger an unbounded page scan.
                let browse_remaining = account.remaining_for_pagination(remaining);
                let browse_needs_more = account.search_query.is_empty()
                    && account.has_more
                    && account.page_inflight.is_none()
                    && browse_remaining <= prefetch_distance;
                if search_needs_more || browse_needs_more {
                    let _ = account.request_next_page();
                }
                if first_page && event.account == self.account_index && !self.message_open {
                    self.visual_anchor = None;
                    self.focus = Focus::Messages;
                }
                self.status = StatusLine::temporary(
                    format!(
                        "{}: {} messages loaded from {} in {:.2} ms",
                        account.name,
                        account.messages.len(),
                        name,
                        elapsed_micros as f64 / 1000.0
                    ),
                    Duration::from_secs(4),
                );
                account.last_prefetch_window = None;
                if event.account == self.account_index {
                    account.prefetch_near_cursor(1);
                }
                // If the reader opened a cached *header* before SELECT, the
                // missing body should start loading automatically once the
                // mailbox generation is verified. No close/reopen required.
                if (self.preview_split || self.message_open) && event.account == self.account_index
                {
                    account.request_selected_body();
                    Self::mark_cached_selected_seen(account);
                }
                if account.pending_inbox_refresh
                    && account.page_inflight.is_none()
                    && !(self.message_open && event.account == self.account_index)
                {
                    account.pending_inbox_refresh = false;
                    let _ = account.request_mailbox_page(name.clone(), 0);
                }
                let selected_mailbox = account
                    .selected_mailbox()
                    .map(|mailbox| mailbox.name.clone());
                account.queue_unknown_mailbox_statuses(selected_mailbox.as_deref());
                account.pump_deferred_mutations();
                account.pump_outbox();
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::MailboxOpenFailed { name, error } => {
                if account.page_request_id != Some(event.request_id) {
                    crate::perf::record_request(
                        "ui_mailbox_stale",
                        event.request_id,
                        None,
                        Duration::ZERO,
                        false,
                    );
                    return;
                }
                account.page_request_id = None;
                account.page_inflight = None;
                account.messages_loading = false;
                account.sync_progress_at = None;
                let thread_fallback =
                    account.native_view.is_some() && account.local_thread_view.is_some();
                if error.kind.is_connection_failure() {
                    account.mark_disconnected();
                } else if thread_fallback {
                    account.restore_thread_fallback();
                    if error.kind == crate::backend::ErrorKind::StaleState {
                        account.unverified_mailboxes.insert(name.clone());
                        let _ = account.request_mailbox_page(name.clone(), 0);
                    }
                }
                Self::pump_mailbox_status(account);
                if thread_fallback {
                    self.status = StatusLine::info(
                        "Native threading unavailable; showing cached conversations (loaded messages only)",
                    );
                    return;
                }
                self.status = StatusLine::temporary_error(
                    format!("{}: could not open {name}: {error}", account.name),
                    Duration::from_secs(8),
                );
            }
            _ => unreachable!("worker event routed to wrong handler"),
        }
    }
}
