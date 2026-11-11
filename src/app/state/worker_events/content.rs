//! Body, attachment and MIME payload completions.

use super::*;

impl App {
    pub(super) fn handle_content_event(&mut self, event: WorkerEvent) {
        // Only parse account identities when an event needs contact/recipient matching.
        let own_addresses = if matches!(&event.kind, WorkerEventKind::RawLoaded { .. }) {
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
            WorkerEventKind::BodyLoadFailed { error } => {
                if account.body_request_id != Some(event.request_id) {
                    return;
                }
                account.body_request_id = None;
                account.pending_bodies.clear();
                Self::pump_mailbox_status(account);
                self.status =
                    StatusLine::error(format!("{}: could not load email: {error}", account.name));
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
                account.persist_body(&body);
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
                account.persist_body(&body);
                account.bodies.insert(id.clone(), body);
                account.trim_preview_bodies();
                if event.account == self.account_index
                    && (self.message_open || self.preview_split)
                    && account
                        .selected_message()
                        .is_some_and(|message| message.id == id && message.unread())
                {
                    account.request_seen(id, true);
                }
                // Do not serialize the entire mailbox for every opportunistic
                // body. The regular cache checkpoint saves it when needed.
            }
            WorkerEventKind::AttachmentsLoaded {
                mailbox,
                id,
                attachments,
            } => {
                let expected =
                    self.attachment_source
                        .as_ref()
                        .is_some_and(|(account, folder, _, wanted)| {
                            *account == event.account && folder == &mailbox && wanted == &id
                        });
                if expected
                    && self.attachment_pending.as_ref() == Some(&id)
                    && self.attachment_pending_request_id == Some(event.request_id)
                {
                    self.attachment_pending_request_id = None;
                    // The user may have selected another email while this MIME
                    // extraction ran. Never pop up the old message's files.
                    if event.account != self.account_index
                        || account.current_mailbox.as_deref() != Some(mailbox.as_str())
                        || !account
                            .selected_message()
                            .is_some_and(|message| message.id == id)
                    {
                        self.attachment_pending = None;
                        self.attachment_source = None;
                        return;
                    }
                    if attachments.is_empty() {
                        self.attachment_pending = None;
                        self.attachment_source = None;
                        self.status = StatusLine::info("no attachments to preview");
                        return;
                    }
                    self.attachment_picker_visible = true;
                    self.attachment_files = Arc::new(attachments);
                    if let Some(source) = self
                        .attachment_source
                        .as_ref()
                        .filter(|(account, _, _, message)| {
                            *account == event.account && message == &id
                        })
                        .cloned()
                    {
                        let total_bytes = self
                            .attachment_files
                            .iter()
                            .map(|file| file.data.len())
                            .sum::<usize>();
                        if total_bytes <= 12 * 1024 * 1024 {
                            self.attachment_cache.retain(|(key, _)| key != &source);
                            self.attachment_cache
                                .push_back((source, Arc::clone(&self.attachment_files)));
                            while self.attachment_cache.len() > 6
                                || self
                                    .attachment_cache
                                    .iter()
                                    .flat_map(|(_, files)| files.iter())
                                    .map(|file| file.data.len())
                                    .sum::<usize>()
                                    > 24 * 1024 * 1024
                            {
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
                    self.status = StatusLine::info(format!(
                        "{} attachment(s) loaded",
                        self.attachment_files.len()
                    ));
                }
            }
            WorkerEventKind::RawLoaded { id, raw } => {
                if self.pending_draft.as_ref().is_some_and(|pending| {
                    pending.account == event.account && pending.message_id == id
                }) {
                    match Composer::from_server_draft(id.clone(), raw) {
                        Ok(composer) => {
                            if let Some(pending) = self.pending_draft.as_mut() {
                                pending.composer = Some(composer);
                            }
                            self.status =
                                StatusLine::info("Draft preview ready: [Y]es edit / [N]o preview");
                        }
                        Err(error) => {
                            self.pending_draft = None;
                            self.status =
                                StatusLine::error(format!("cannot open draft: {error:#}"));
                        }
                    }
                    return;
                }
                let pending = self
                    .pending_compose
                    .as_ref()
                    .filter(|pending| pending.account == event.account && pending.message_id == id);
                let result = pending.and_then(|pending| {
                    let message = account
                        .messages
                        .iter()
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
            _ => unreachable!("worker event routed to wrong handler"),
        }
    }
}
