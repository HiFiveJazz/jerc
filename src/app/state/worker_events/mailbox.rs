//! Mailbox discovery, synchronization and STATUS reconciliation.

use super::*;

impl App {
    pub(super) fn handle_mailbox_event(&mut self, event: WorkerEvent) {
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
            WorkerEventKind::Connected { capabilities } => {
                account.connecting = false;
                account.capabilities = Some(capabilities);
                if account.worker.post(ActionKind::ListMailboxes).is_ok() {
                    account.mailboxes_loading = true;
                }
                self.status = StatusLine::info(format!("{} connected", account.name));
            }
            WorkerEventKind::Mailboxes(mailboxes) => {
                account.mailboxes_loading = false;
                let wanted = account
                    .current_mailbox
                    .clone()
                    .unwrap_or_else(|| account.default_mailbox.clone());
                account.mailboxes = mailboxes
                    .into_iter()
                    .filter(|mailbox| {
                        !crate::config::mailbox_is_excluded(
                            &mailbox.name,
                            &account.exclude_mailboxes,
                        )
                    })
                    .collect();
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
                        account.messages.clear();
                        account.bodies.clear();
                        account.message_index = 0;
                        account.message_view_offset = 0;
                        account.next_page_offset = 0;
                        account.has_more = false;
                    }
                    // Cached rows remain visible while the first-page refresh
                    // runs on the interactive IMAP worker.
                    if account.page_inflight.is_none() {
                        let _ = account.request_mailbox_page(name, 0);
                    }
                }
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::MailboxChanged { name } => {
                if account.current_mailbox.as_deref() == Some(name.as_str()) {
                    account.pending_inbox_refresh = true;
                }
                // An IDLE wakeup during another STATUS is not expendable:
                // retain one refresh per mailbox until the current request ends.
                account.queue_status_refresh(name);
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
                account.persist_mailboxes();
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
            WorkerEventKind::MailboxOpened {
                name,
                page,
                uid_validity,
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
                let first_page = page.offset == 0;
                let previous_validity = account.uid_validities.get(&name).copied();
                let generation_changed =
                    first_page && previous_validity.is_some() && previous_validity != uid_validity;
                if generation_changed {
                    // Cached UIDs may point to entirely different emails after
                    // a server UIDVALIDITY reset: drop all old derived data.
                    account.forget_cached_folder(&name);
                    account.bodies.clear();
                }
                if let Some(validity) = uid_validity {
                    account.uid_validities.insert(name.clone(), validity);
                } else {
                    account.uid_validities.remove(&name);
                }
                if let Some(next) = uid_next {
                    account.uid_nexts.insert(name.clone(), next);
                } else {
                    account.uid_nexts.remove(&name);
                }
                account.unverified_mailboxes.remove(&name);
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
                                messages: page.messages,
                                bodies: HashMap::new(),
                                message_index: 0,
                                message_view_offset: 0,
                                next_page_offset: page.next_offset,
                                has_more: page.has_more,
                                uid_validity,
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
                account.current_mailbox = Some(name.clone());
                if page.offset == 0 {
                    let selected_id = (!generation_changed)
                        .then(|| account.selected_message().map(|message| message.id.clone()))
                        .flatten();
                    account.messages = page.messages;
                    account.message_index = selected_id
                        .and_then(|id| account.messages.iter().position(|message| message.id == id))
                        .or_else(|| account.restore_message_index.take())
                        .unwrap_or(0)
                        .min(account.messages.len().saturating_sub(1));
                    account.message_view_offset = 0;
                    let live = account
                        .messages
                        .iter()
                        .map(|message| message.id.clone())
                        .collect::<HashSet<_>>();
                    account.bodies.retain(|id, _| live.contains(id));
                } else {
                    let existing = account
                        .messages
                        .iter()
                        .map(|message| message.id.clone())
                        .collect::<HashSet<_>>();
                    account.messages.extend(
                        page.messages
                            .into_iter()
                            .filter(|message| !existing.contains(&message.id)),
                    );
                    account.messages.sort_by(|a, b| {
                        b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id))
                    });
                }
                account.next_page_offset = page.next_offset;
                account.has_more = page.has_more;
                if first_page {
                    account.preview_scroll = 0;
                    account.preview_max_scroll = 0;
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
                    account.pending_seen.clear();
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
                let browse_needs_more = account.search_query.is_empty()
                    && account.has_more
                    && account.page_inflight.is_none()
                    && remaining <= prefetch_distance;
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
                if self.preview_split && event.account == self.account_index {
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
                Self::pump_mailbox_status(account);
                self.status =
                    StatusLine::error(format!("{}: could not open {name}: {error}", account.name));
            }
            _ => unreachable!("worker event routed to wrong handler"),
        }
    }
}
