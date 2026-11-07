//! Worker events behavior.

use super::*;

impl App {
    pub fn handle_worker_event(&mut self, event: WorkerEvent) {
        let own_addresses = self.accounts.iter()
            .filter_map(|a| a.from.parse::<lettre::message::Mailbox>().ok())
            .map(|a| a.email.to_string())
            .collect::<Vec<_>>();
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
                account.mailboxes = mailboxes.into_iter()
                    .filter(|mailbox| !crate::config::mailbox_is_excluded(
                        &mailbox.name, &account.exclude_mailboxes)).collect();
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
            WorkerEventKind::MailboxStatus {
                name,
                total,
                unread,
            } => {
                if account.mailbox_status_inflight == Some(event.request_id) {
                    account.mailbox_status_inflight = None;
                }
                let from_poll = account.inbox_poll_inflight == Some(event.request_id);
                if from_poll { account.inbox_poll_inflight = None; }
                let mut new_count = 0usize;
                if let Some(mailbox) = account.mailboxes.iter_mut().find(|m| m.name == name) {
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
                        self.new_mail_since_launch = self.new_mail_since_launch.saturating_add(new_count);
                    }
                    // Keep the UID-keyed header cache on new arrivals. The
                    // next SELECT validates UIDVALIDITY and obtains a current
                    // UID+FLAGS page before we reuse any old summary. Dropping
                    // the cache here would force full header downloads on
                    // precisely the refreshes that benefit from delta fetches.
                    self.status = StatusLine::temporary(format!(
                        "{} new message(s) in {}", new_count, account.name,
                    ), Duration::from_secs(6));
                    if self.new_mail_bell_enabled && self.new_mail_notifications_enabled {
                        self.new_mail_bell_pending = true;
                    }
                    if self.new_mail_notifications_enabled {
                        // Use the existing background IMAP session; do not block
                        // rendering, fetch on the foreground session, or change \Seen.
                        if account.worker.post(ActionKind::NewMailPreview {
                            mailbox: name.clone(), count: new_count,
                        }).is_err() {
                            self.new_mail_notices.push_back(NewMailNotice {
                                account: account.name.clone(), count: new_count,
                                sender: String::new(), subject: String::new(), excerpt: String::new(),
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
                            account.restore_message_index = Some(account.message_index);
                            let _ = account.request_mailbox_page(name, 0);
                        }
                    }
                }
                account.persist_mailboxes();
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::NewMailPreview { count, sender, subject, excerpt } => {
                if self.new_mail_notifications_enabled {
                    self.new_mail_notices.push_back(NewMailNotice {
                        account: account.name.clone(), count, sender, subject, excerpt,
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
                    crate::perf::record_request("ui_mailbox_stale", event.request_id, None, Duration::ZERO, true);
                    return;
                }
                account.page_request_id = None;
                let first_page = page.offset == 0;
                let previous_validity = account.uid_validities.get(&name).copied();
                let generation_changed = first_page && previous_validity.is_some()
                    && previous_validity != uid_validity;
                if generation_changed {
                    // Cached UIDs may point to entirely different emails after
                    // a server UIDVALIDITY reset: drop all old derived data.
                    account.mailbox_cache.remove(&name);
                    account.remove_persistent_folder(&name);
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
                let selected_name = account.selected_mailbox().map(|mailbox| mailbox.name.clone());
                if selected_name.as_deref() != Some(name.as_str()) {
                    account.page_inflight = None;
                    account.messages_loading = false;
                    if page.offset == 0 {
                        account.mailbox_cache.insert(
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
                crate::perf::record_request("ui_mailbox_applied", event.request_id, None, Duration::ZERO, true);
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
                        b.timestamp
                            .cmp(&a.timestamp)
                            .then_with(|| b.id.cmp(&a.id))
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
                        && account.selected_message().is_some_and(|message|
                            account.pending_bodies.contains(&message.id));
                    if !still_selected {
                        account.pending_bodies.clear();
                        account.body_request_id = None;
                    }
                    account.pending_seen.clear();
                }
                account.last_scan_micros = Some(elapsed_micros);
                // Learn only lightweight envelope addresses already fetched
                // for normal browsing. Never scan bodies or junk folders.
                let mailbox_role = account.mailboxes.iter()
                    .find(|m| m.name == name).map(|m| &m.role);
                let is_sent = matches!(mailbox_role, Some(&MailboxRole::Sent));
                let is_inbox = matches!(mailbox_role, Some(&MailboxRole::Inbox));
                if ((is_inbox && self.contacts_learn_inbox) || (is_sent && self.contacts_learn_sent))
                    && self.contacts_writable {
                    let mut changed = false;
                    for message in account.messages.iter().take(200) {
                        if is_inbox && !own_addresses.iter().any(|own|
                            own.eq_ignore_ascii_case(&message.from_address)) {
                            changed |= self.contacts.upsert(&message.from_name, &message.from_address, false)
                                .unwrap_or(false);
                        }
                        if is_sent {
                            for (person, address) in recipients(&message.to).into_iter()
                                .chain(recipients(&message.cc)) {
                                if !own_addresses.iter().any(|own| own.eq_ignore_ascii_case(&address)) {
                                    changed |= self.contacts.upsert(&person, &address, false).unwrap_or(false);
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
                if event.account == self.account_index { account.prefetch_near_cursor(1); }
                if self.preview_split && event.account == self.account_index {
                    account.request_selected_body();
                    Self::mark_cached_selected_seen(account);
                }
                if account.pending_inbox_refresh && account.page_inflight.is_none()
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
                    crate::perf::record_request("ui_mailbox_stale", event.request_id, None, Duration::ZERO, false);
                    return;
                }
                account.page_request_id = None;
                account.page_inflight = None;
                account.messages_loading = false;
                self.status = StatusLine::error(format!("{}: could not open {name}: {error}", account.name));
            }
            WorkerEventKind::BodyLoadFailed { error } => {
                if account.body_request_id != Some(event.request_id) { return; }
                account.body_request_id = None;
                account.pending_bodies.clear();
                self.status = StatusLine::error(format!("{}: could not load email: {error}", account.name));
            }
            WorkerEventKind::BodyLoaded(body) => {
                if account.body_request_id != Some(event.request_id) {
                    return;
                }
                account.body_request_id = None;
                let id = body.id.clone();
                account.pending_bodies.remove(&id);
                if account.bodies.contains_key(&id) {
                    crate::perf::record("ui_selected_body_duplicate", None, Duration::ZERO, true);
                }
                account.bodies.insert(id.clone(), body);
                account.trim_preview_bodies();
                account.persist_current_cache();

                if (self.message_open || self.preview_split)
                    && account
                        .selected_message()
                        .is_some_and(|message| message.id == id && message.unread())
                {
                    account.request_seen(id, true);
                }
                Self::pump_mailbox_status(account);

            }
            WorkerEventKind::BodyPrefetched { mailbox, body } => {
                // The background connection can finish after a folder change,
                // UIDVALIDITY reset, expunge, or account switch. Never apply a
                // body unless it still belongs to the verified visible folder.
                let id = body.id.clone();
                if account.current_mailbox.as_deref() != Some(mailbox.as_str())
                    || account.unverified_mailboxes.contains(&mailbox)
                    || !account.messages.iter().any(|message| message.id == id)
                {
                    crate::perf::record("ui_prefetch_discarded", None, Duration::ZERO, true);
                    return;
                }
                if account.bodies.contains_key(&id) {
                    crate::perf::record("ui_prefetch_duplicate", None, Duration::ZERO, true);
                    return;
                }
                crate::perf::record("ui_prefetch_applied", None, Duration::ZERO, true);
                account.bodies.insert(id.clone(), body);
                account.trim_preview_bodies();
                if event.account == self.account_index && (self.message_open || self.preview_split)
                    && account.selected_message().is_some_and(|message| message.id == id && message.unread())
                {
                    account.request_seen(id, true);
                }
                // Do not serialize the entire mailbox for every opportunistic
                // body. The regular cache checkpoint saves it when needed.
            }
            WorkerEventKind::AttachmentsLoaded { mailbox, id, attachments } => {
                let expected = self.attachment_source.as_ref().is_some_and(|(account, folder, _, wanted)|
                    *account == event.account && folder == &mailbox && wanted == &id);
                if expected && self.attachment_pending.as_ref() == Some(&id) && self.attachment_picker_visible {
                    self.attachment_files = Arc::new(attachments);
                    if let Some(source) = self.attachment_source.as_ref()
                        .filter(|(account, _, _, message)| *account == event.account && message == &id).cloned() {
                        let total_bytes = self.attachment_files.iter().map(|file| file.data.len()).sum::<usize>();
                        if total_bytes <= 12 * 1024 * 1024 {
                            self.attachment_cache.retain(|(key, _)| key != &source);
                            self.attachment_cache.push_back((source, Arc::clone(&self.attachment_files)));
                            while self.attachment_cache.len() > 6 || self.attachment_cache.iter()
                                .flat_map(|(_, files)| files.iter()).map(|file| file.data.len()).sum::<usize>()
                                > 24 * 1024 * 1024 {
                                self.attachment_cache.pop_front();
                            }
                        }
                    }
                    self.attachment_selection = 0;
                    self.attachment_marked.clear();
                    self.attachment_visual_anchor = None;
                    self.attachment_preview = None;
                    self.attachment_preview_identity = None;
                    self.attachment_pending = None;
                    self.queue_attachment_preview();
                    self.status = StatusLine::info(format!("{} attachment(s) loaded", self.attachment_files.len()));
                }
            }
            WorkerEventKind::RawLoaded { id, raw } => {
                let pending = self.pending_compose.as_ref().filter(|pending| {
                    pending.account == event.account && pending.message_id == id
                });
                let result = pending.and_then(|pending| {
                    let message = account.messages.iter()
                        .find(|message| message.id == pending.message_id)?;
                    Some(match pending.intent {
                        ComposeIntent::Forward => Composer::forward(message, raw),
                        ComposeIntent::Reply { quote, audience } => {
                            (|| -> anyhow::Result<Composer> {
                                let targets = reply_recipients(&raw, audience, &own_addresses)?;
                                let body = parse_body(id, &raw)?;
                                let mut composer = Composer::reply(message, Some(&body), quote);
                                composer.to = targets.to;
                                composer.cc = targets.cc;
                                Ok(composer)
                            })()
                        }
                    })
                });
                match result {
                    Some(Ok(composer)) => {
                        self.pending_compose = None;
                        self.composer = Some(composer);
                        self.resumed_draft_account = None;
                        self.status = StatusLine::info("ready to compose reply/forward");
                    }
                    Some(Err(error)) => {
                        self.pending_compose = None;
                        self.status = StatusLine::error(format!("cannot prepare reply: {error:#}"));
                    }
                    None => {}
                }
            }
            WorkerEventKind::MessageExported { path } => {
                self.status = StatusLine::temporary(
                    format!("raw message exported to {}", path.display()),
                    Duration::from_secs(8),
                );
            }
            WorkerEventKind::SeenChanged { id, seen } => {
                account.pending_seen.remove(&id);
                let mut unread_delta = 0isize;
                if let Some(message) = account.messages.iter_mut().find(|message| message.id == id) {
                    let was_unread = message.unread();
                    message.flags.set(MessageFlags::SEEN, seen);
                    let is_unread = message.unread();
                    unread_delta = match (was_unread, is_unread) {
                        (true, false) => -1,
                        (false, true) => 1,
                        _ => 0,
                    };
                }
                if unread_delta != 0 {
                    if let Some(current) = account.current_mailbox.as_deref() {
                        if let Some(mailbox) = account
                            .mailboxes
                            .iter_mut()
                            .find(|mailbox| mailbox.name == current && mailbox.counts_known)
                        {
                            mailbox.unread = mailbox.unread.saturating_add_signed(unread_delta);
                        }
                    }
                }
                account.persist_current_cache();
                account.persist_mailboxes();
            }
            WorkerEventKind::MutationComplete => {
                account.pending_mutations = account.pending_mutations.saturating_sub(1);
                let message = account.pending_mutation_labels.remove(&event.request_id)
                    .unwrap_or("mail updated");
                self.status = StatusLine::temporary(message, Duration::from_secs(2));
            }
            WorkerEventKind::DraftSaveFailed { mailbox, error } => {
                self.status = StatusLine::error(format!(
                    "saved locally only: uploading to {mailbox} failed ({error}); Ctrl+n resumes"));
            }
            WorkerEventKind::DraftSaved { mailbox } => {
                // Refresh cached Drafts on the next visit, or immediately if
                // the user is already browsing that folder.
                account.mailbox_cache.remove(&mailbox);
                account.remove_persistent_folder(&mailbox);
                if let Some(folder) = account.mailboxes.iter_mut()
                    .find(|folder| folder.name == mailbox) {
                    folder.counts_known = false;
                }
                if account.current_mailbox.as_deref() == Some(mailbox.as_str()) {
                    let _ = account.request_mailbox_page(mailbox.clone(), 0);
                }
                self.status = StatusLine::info(format!(
                    "draft saved in {mailbox}; Ctrl+n resumes editing local recovery copy"));
            }
            WorkerEventKind::MessageSent {
                smtp_status,
                sent_mailbox,
                sent_copy_saved,
                sent_copy_warning,
                sent_copy_disabled,
            } => {
                if let Some(composer) = &self.composer {
                    let mut changed = false;
                    for field in [&composer.to, &composer.cc, &composer.bcc] {
                        for (name, address) in recipients(field) {
                            if !own_addresses.iter().any(|own| own.eq_ignore_ascii_case(&address)) {
                                changed |= self.contacts.upsert(&name, &address, false).unwrap_or(false);
                            }
                        }
                    }
                    if changed {
                        self.contacts_dirty = true;
                        if self.contacts_save_due.is_none() {
                            self.contacts_save_due = Some(Instant::now() + Duration::from_secs(3));
                        }
                    }
                }
                // A successful SMTP submission makes our mailbox snapshots stale even
                // when the provider files Sent itself (Gmail), and especially when the
                // message was addressed back to this account. Force the next visit to
                // INBOX/Sent to come from the server rather than the in-memory cache.
                let mut refresh_current = None;
                let affected = account
                    .mailboxes
                    .iter()
                    .filter(|mailbox| {
                        matches!(mailbox.role, MailboxRole::Inbox | MailboxRole::Sent)
                            || sent_mailbox
                                .as_deref()
                                .is_some_and(|name| name == mailbox.name)
                    })
                    .map(|mailbox| mailbox.name.clone())
                    .collect::<Vec<_>>();
                for name in &affected {
                    account.mailbox_cache.remove(name);
                    account.remove_persistent_folder(name);
                    if let Some(mailbox) = account
                        .mailboxes
                        .iter_mut()
                        .find(|mailbox| mailbox.name == *name)
                    {
                        mailbox.counts_known = false;
                    }
                }
                account.queue_unknown_mailbox_statuses(sent_mailbox.as_deref());
                if account
                    .current_mailbox
                    .as_ref()
                    .is_some_and(|name| affected.iter().any(|affected| affected == name))
                {
                    refresh_current = account.current_mailbox.clone();
                }
                if let Some(name) = refresh_current {
                    let _ = account.request_mailbox_page(name, 0);
                }
                Self::pump_mailbox_status(account);

                if self.resumed_draft_account == Some(event.account) {
                    if let Ok(root) = Self::postponed_drafts_root() {
                        let _ = crate::outgoing::drafts::delete_draft(&root, &account.name);
                    }
                    self.resumed_draft_account = None;
                }
                if event.account == self.account_index {
                    self.composer = None;
                }
                self.status = if let Some(warning) = sent_copy_warning {
                    StatusLine::temporary(
                        format!("SMTP accepted ({smtp_status}); {warning}"),
                        Duration::from_secs(8),
                    )
                } else if sent_copy_disabled {
                    StatusLine::temporary(format!("Message accepted ({smtp_status}); Sent copy disabled"), Duration::from_secs(5))
                } else if sent_copy_saved {
                    StatusLine::temporary(
                        format!("SMTP accepted ({smtp_status}); saved to Sent"),
                        Duration::from_secs(5),
                    )
                } else {
                    StatusLine::temporary(
                        format!("SMTP accepted ({smtp_status}); provider handles Sent copy"),
                        Duration::from_secs(5),
                    )
                };
            }
            WorkerEventKind::Error(error) => {
                if account.mailbox_status_inflight == Some(event.request_id) {
                    account.mailbox_status_inflight = None;
                    // Background count lookup failure is nonfatal: keep the
                    // current mailbox, reader, and interactive loading alive.
                    return;
                }
                if account.inbox_poll_inflight == Some(event.request_id) {
                    // Background STATUS is best-effort: a transient server
                    // failure must not cancel an in-progress preview/compose
                    // or reset the mailbox browser state.
                    account.inbox_poll_inflight = None;
                    self.status = StatusLine::temporary(
                        format!("{}: Inbox check failed ({error})", account.name),
                        Duration::from_secs(4),
                    );
                    return;
                }
                let mutation_failed = account.pending_mutations > 0;
                let seen_failed = !account.pending_seen.is_empty();
                account.pending_mutations = 0;
                account.pending_mutation_labels.clear();
                account.inbox_poll_inflight = None;
                account.connecting = false;
                account.mailboxes_loading = false;
                // A failure in an unrelated mutation/compose request must not
                // cancel a currently loading mailbox or email. Navigation has
                // dedicated error variants above and is correlated by ID.
                if account.page_request_id.is_none() {
                    account.messages_loading = false;
                    account.page_inflight = None;
                }
                if account.mailbox_status_inflight == Some(event.request_id) {
                    account.mailbox_status_inflight = None;
                }
                if account.body_request_id.is_none() {
                    account.pending_bodies.clear();
                }
                account.pending_seen.clear();
                if self
                    .pending_compose
                    .as_ref()
                    .is_some_and(|pending| pending.account == event.account)
                {
                    self.pending_compose = None;
                }
                if event.account == self.account_index {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.sending = false;
                    }
                }
                if (mutation_failed || seen_failed)
                    && let Some(name) = account.current_mailbox.clone()
                {
                    account.invalidate_current_cache();
                    let _ = account.request_mailbox_page(name, 0);
                }
                // STATUS failures are mailbox-local. Do not leave every other
                // folder stuck on an unknown count just because one provider
                // rejects STATUS for a special mailbox.
                Self::pump_mailbox_status(account);
                self.status = StatusLine::error(format!("{}: {error}", account.name));
            }
            WorkerEventKind::Stopped => {}
        }
    }

}
