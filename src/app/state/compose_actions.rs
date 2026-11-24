//! Compose actions behavior.

use super::*;

impl AccountState {
    fn queue_outgoing(
        &mut self,
        message: crate::outgoing::outbox::QueuedSend,
    ) -> anyhow::Result<()> {
        let store = self
            .outbox_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("offline outbox storage unavailable"))?;
        let mut next = self.outbox.clone();
        let offline_id = self.capabilities.is_none().then(|| message.id.clone());
        next.push(message);
        store.save(&next)?;
        if let Some(id) = offline_id {
            self.offline_notice_sends.insert(id);
        }
        self.outbox = next;
        Ok(())
    }

    pub(super) fn pump_outbox(&mut self) {
        if self.capabilities.is_none() {
            return;
        }
        if !self.inflight_outbox.is_empty() {
            return;
        }
        let ready = self
            .outbox
            .iter()
            .filter(|message| message.state == crate::outgoing::outbox::SendState::Queued)
            .take(1)
            .cloned()
            .collect::<Vec<_>>();
        for message in ready {
            if let Some(stored) = self
                .outbox
                .iter_mut()
                .find(|stored| stored.id == message.id)
            {
                stored.state = crate::outgoing::outbox::SendState::NeedsReview;
            }
            // Durable marker before possible SMTP submission. If sending
            // succeeds but the app crashes before the ACK, do not resend.
            if self.save_outbox().is_err() {
                if let Some(stored) = self
                    .outbox
                    .iter_mut()
                    .find(|stored| stored.id == message.id)
                {
                    stored.state = crate::outgoing::outbox::SendState::Queued;
                }
                break;
            }
            match self.worker.post(ActionKind::Send {
                message: message.message,
                sent_mailbox: message.sent_mailbox,
                copy_to: message.copy_to,
            }) {
                Ok(request_id) => {
                    self.inflight_outbox.insert(request_id, message.id);
                }
                Err(_) => {
                    if let Some(stored) = self
                        .outbox
                        .iter_mut()
                        .find(|stored| stored.id == message.id)
                    {
                        stored.state = crate::outgoing::outbox::SendState::Queued;
                    }
                    let _ = self.save_outbox();
                    break;
                }
            }
        }
    }

    /// A classified network failure occurred before delivery was attempted.
    /// Restore the Outbox entry to Queued durably before automatic replay.
    /// Other failures remain NeedsReview because the server may have accepted it.
    pub(super) fn requeue_outbox_before_submission(&mut self, request_id: u64) -> bool {
        let Some(id) = self.inflight_outbox.remove(&request_id) else {
            return false;
        };
        let Some(index) = self.outbox.iter().position(|message| message.id == id) else {
            return false;
        };
        let mut next = self.outbox.clone();
        next[index].state = crate::outgoing::outbox::SendState::Queued;
        let Some(store) = self.outbox_store.as_ref() else {
            return false;
        };
        if store.save(&next).is_err() {
            // The durable state is still NeedsReview; don't risk an automatic
            // duplicate send after a failed journal write.
            return false;
        }
        self.outbox = next;
        self.offline_notice_sends.insert(id);
        true
    }

    fn save_outbox(&self) -> anyhow::Result<()> {
        self.outbox_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("outbox storage unavailable"))?
            .save(&self.outbox)
    }

    pub(super) fn finish_outbox(
        &mut self,
        request_id: u64,
        accepted: bool,
    ) -> Option<crate::outgoing::outbox::QueuedSend> {
        let id = self.inflight_outbox.remove(&request_id)?;
        let index = self.outbox.iter().position(|message| message.id == id)?;
        if accepted {
            self.offline_notice_sends.remove(&id);
            let sent = self.outbox.remove(index);
            let _ = self.save_outbox();
            Some(sent)
        } else {
            // NeedsReview was persisted BEFORE submission. Keep it until
            // manually resolved because the relay's ACK may have been lost.
            None
        }
    }
}

impl App {
    pub(super) fn prepare_draft_resume(&mut self) {
        let Some(account) = self.account() else {
            return;
        };
        if !account.outgoing_configured {
            return;
        }
        let Some(mailbox) = account
            .selected_mailbox()
            .filter(|folder| folder.role == MailboxRole::Drafts)
            .map(|folder| folder.name.clone())
        else {
            return;
        };
        if account.cached_mailbox_unverified() {
            self.status = StatusLine::info("draft preview requires a verified server connection");
            return;
        }
        let Some(message_id) = account.selected_message().map(|m| m.id.clone()) else {
            return;
        };
        let account_index = self.account_index;
        if self.pending_draft.as_ref().is_some_and(|pending| {
            pending.account == account_index
                && pending.mailbox == mailbox
                && pending.message_id == message_id
        }) {
            return;
        }
        let cache = account.raw_cache_key(&mailbox, &message_id);
        let request_id = account
            .worker
            .post(ActionKind::FetchRawForCompose {
                mailbox: mailbox.clone(),
                id: message_id.clone(),
                cache,
            })
            .ok();
        if let Some(request_id) = request_id {
            self.pending_draft = Some(PendingDraft {
                account: account_index,
                request_id,
                mailbox,
                message_id,
                composer: None,
            });
            self.status = StatusLine::info("loading draft for preview…");
        } else {
            self.status = StatusLine::error("could not load draft: worker busy");
        }
    }

    pub(super) fn resume_server_draft(&mut self) {
        let Some(mut pending) = self.pending_draft.take() else {
            return;
        };
        let Some(mut composer) = pending.composer.take() else {
            return;
        };
        if self.account_index != pending.account {
            return;
        }
        composer.draft_source = Some(DraftSource {
            mailbox: pending.mailbox,
            id: pending.message_id,
        });
        self.close_message();
        self.composer = Some(composer);
        self.resumed_draft_account = None;
        self.status =
            StatusLine::info("editing saved draft; original preserved until replacement succeeds");
    }

    /// Populate the existing composer from a CLI URI or patch. Merely opening
    /// a draft does not send, queue, or persist it.
    pub fn open_cli_compose(&mut self, draft: crate::runtime::StartupCompose) {
        let mut composer = Composer::new();
        composer.to = draft.to;
        composer.cc = draft.cc;
        composer.bcc = draft.bcc;
        composer.subject = draft.subject;
        composer.body = draft.body;
        composer.attachment_paths = draft.attachments;
        composer.field = if composer.body.is_empty() {
            ComposeField::To
        } else {
            ComposeField::Body
        };
        composer.cursor = if composer.field == ComposeField::Body {
            composer.body.chars().count()
        } else {
            composer.to.chars().count()
        };
        self.composer = Some(composer);
        self.pending_draft = None;
        self.pending_compose = None;
        self.resumed_draft_account = None;
    }

    pub(super) fn begin_compose(&mut self) {
        let Some(account) = self.account() else {
            return;
        };
        if !account.outgoing_configured {
            self.status = StatusLine::error(
                "no outgoing transport configured; add `outgoing = ...` to this account",
            );
            return;
        }
        self.composer = Some(Composer::new());
        self.pending_compose = None;
        self.pending_draft = None;
        self.resumed_draft_account = None;
    }

    pub(super) fn begin_reply(&mut self, quote: bool) {
        self.begin_compose_from_selected(ComposeIntent::Reply {
            quote,
            audience: ReplyAudience::Sender,
        });
    }

    pub(super) fn begin_reply_all(&mut self) {
        self.begin_compose_from_selected(ComposeIntent::Reply {
            quote: true,
            audience: ReplyAudience::All,
        });
    }

    pub(super) fn begin_reply_list(&mut self) {
        self.begin_compose_from_selected(ComposeIntent::Reply {
            quote: true,
            audience: ReplyAudience::List,
        });
    }

    pub(super) fn begin_forward(&mut self) {
        self.begin_compose_from_selected(ComposeIntent::Forward);
    }

    /// Open the editor without waiting for SELECT/UIDVALIDITY or a MIME fetch.
    /// The worker may read the immutable raw cache immediately; it must not
    /// fetch an unverified UID from the server.
    pub(super) fn begin_compose_from_selected(&mut self, intent: ComposeIntent) {
        let Some(account) = self.account() else {
            return;
        };
        if !account.outgoing_configured {
            self.status = StatusLine::error(
                "no outgoing transport configured; add `outgoing = ...` to this account",
            );
            return;
        }
        let Some(summary) = account.selected_message().cloned() else {
            return;
        };
        let Some(mailbox) = account.current_mailbox.clone() else {
            return;
        };
        // Keep the keystroke-to-first-frame path bounded. Large MIME text is
        // quoted by the background worker rather than formatted on the UI thread.
        let cached_body = account
            .bodies
            .get(&summary.id)
            .filter(|body| body.display_text(false).len() <= 128 * 1024)
            .cloned();
        let mut source = ComposeSource {
            cache_identity: account
                .cache_identities
                .get(&mailbox)
                .map(CacheIdentity::generation),
            mailbox,
            summary,
            intent,
            initial_body: String::new(),
        };
        let mut composer = match intent {
            ComposeIntent::Reply { quote, .. } => {
                Composer::reply(&source.summary, cached_body.as_ref(), quote)
            }
            ComposeIntent::Forward => {
                let mut editor = Composer::new();
                editor.subject = forward_subject(&source.summary.subject);
                editor.body = "\n\n".to_owned();
                editor.field = ComposeField::Body;
                editor.mode = ComposeMode::Insert;
                editor
            }
        };
        composer.original_loading = true;
        let initial_body = composer.body.clone();
        source.initial_body = initial_body.clone();
        composer.original_source = Some(source.clone());
        self.composer = Some(composer);
        self.pending_draft = None;
        self.resumed_draft_account = None;
        self.pending_compose = Some(PendingCompose {
            account: self.account_index,
            request_id: 0,
            message_id: source.summary.id.clone(),
            intent,
            mailbox: source.mailbox.clone(),
            summary: source.summary,
            cache_identity: source.cache_identity,
            cache_only: false,
            initial_body,
        });
        self.try_prepare_open_composer();
    }

    /// Re-attempt an unresolved source after an authoritative mailbox page.
    /// A generation change *must never* cause a fetch against a recycled UID.
    pub(super) fn try_prepare_open_composer(&mut self) {
        let Some(pending) = self.pending_compose.as_ref() else {
            return;
        };
        if pending.request_id != 0 || self.composer.as_ref().is_none_or(|c| !c.original_loading) {
            return;
        }
        let Some(account) = self.accounts.get(pending.account) else {
            return;
        };
        let verified = !account.unverified_mailboxes.contains(&pending.mailbox)
            && account.capabilities.is_some();
        let current_identity = account.cache_identities.get(&pending.mailbox);
        if verified
            && pending.cache_identity.as_ref().is_some_and(|old| {
                current_identity.is_none_or(|current| !old.same_generation(current))
            })
        {
            self.pending_compose = None;
            self.status = StatusLine::error(
                "original mailbox identity changed; refusing to fetch a recycled message ID",
            );
            return;
        }
        if verified
            && current_identity.is_some_and(|identity| !identity.accepts(&pending.message_id))
        {
            self.pending_compose = None;
            self.status = StatusLine::error(
                "original message ID is not valid for the verified mailbox; fetch refused",
            );
            return;
        }
        let cache = current_identity
            .filter(|identity| identity.accepts(&pending.message_id))
            .and_then(|identity| {
                account
                    .raw_store
                    .as_ref()
                    .map(|store| (store.dir.clone(), identity.generation()))
            });
        // The first attempt always tries the local raw MIME cache, even when
        // offline. After a cache miss, only verified online fetches may retry.
        let cache_only = !verified;
        if cache_only && (cache.is_none() || pending.cache_only) {
            self.status = StatusLine::info(
                "composer open; original MIME pending mailbox verification (editing is available)",
            );
            return;
        }
        let own_addresses = self
            .accounts
            .iter()
            .filter_map(|a| a.from.parse::<lettre::message::Mailbox>().ok())
            .map(|a| a.email.to_string())
            .collect::<Vec<_>>();
        let action = ActionKind::PrepareCompose {
            mailbox: pending.mailbox.clone(),
            id: pending.message_id.clone(),
            summary: pending.summary.clone(),
            intent: pending.intent,
            own_addresses,
            cache,
            cache_only,
        };
        match account.worker.post(action) {
            Ok(request_id) => {
                if let Some(pending) = self.pending_compose.as_mut() {
                    pending.request_id = request_id;
                    pending.cache_only = cache_only;
                }
                self.status = StatusLine::info(if cache_only {
                    "composer open; loading original MIME from local cache…"
                } else {
                    "composer open; loading original MIME in background…"
                });
            }
            Err(_) => {
                self.status = StatusLine::error(
                    "composer open, but MIME worker is busy; retry after mailbox refresh",
                )
            }
        }
    }

    /// Safely merge a prepared original without replacing text already typed
    /// in the immediately opened editor.
    pub(super) fn merge_prepared_compose(
        current: &mut Composer,
        prepared: Composer,
        source: &PendingCompose,
    ) {
        if !current.original_loading {
            return;
        }
        match source.intent {
            ComposeIntent::Forward => {
                Self::append_prepared_original(
                    &mut current.body,
                    &source.initial_body,
                    &prepared.body,
                );
                current.forwarded_raw = prepared.forwarded_raw;
            }
            ComposeIntent::Reply { quote, .. } => {
                let initial_to = Composer::reply(&source.summary, None, quote).to;
                // Recipients typed by the user must win over late Reply-To/CC
                // parsing, but an untouched default should be corrected.
                if current.to == initial_to {
                    current.to = prepared.to;
                }
                if current.cc.is_empty() {
                    current.cc = prepared.cc;
                }
                if quote {
                    current.reply_html_quote = prepared.reply_html_quote;
                    Self::append_prepared_original(
                        &mut current.body,
                        &source.initial_body,
                        &prepared.body,
                    );
                }
            }
        }
        current.original_loading = false;
        current.original_source = None;
    }

    fn append_prepared_original(edited: &mut String, initial: &str, prepared: &str) {
        if edited == initial {
            *edited = prepared.to_owned();
        } else if initial == "\n\n" {
            // The editor's cursor continues to refer to user-authored text.
            // Put the quoted/forwarded original *after* those edits.
            if !edited.ends_with('\n') {
                edited.push('\n');
            }
            edited.push('\n');
            edited.push_str(prepared.trim_start_matches('\n'));
        }
        // When the cached body already contained the quoted text and the
        // user edited it, do not insert a second copy or clobber those edits.
    }

    pub(super) fn handle_composer_key(&mut self, key: KeyEvent) {
        let suggestions = self.contact_suggestions();
        if !suggestions.is_empty() {
            if key.code == KeyCode::Enter {
                let selected = self
                    .composer
                    .as_ref()
                    .map_or(0, |composer| composer.contact_suggestion);
                let id = suggestions[selected.min(suggestions.len() - 1)];
                self.accept_contact_suggestion(id);
                return;
            }
            if let Some(composer) = self.composer.as_mut() {
                match key.code {
                    KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        composer.contact_suggestion =
                            (composer.contact_suggestion + 1) % suggestions.len();
                        return;
                    }
                    KeyCode::Char('p' | 'k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        composer.contact_suggestion =
                            (composer.contact_suggestion + suggestions.len() - 1)
                                % suggestions.len();
                        return;
                    }
                    KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        composer.contact_suggestion =
                            (composer.contact_suggestion + 1) % suggestions.len();
                        return;
                    }
                    KeyCode::Down => {
                        composer.contact_suggestion =
                            (composer.contact_suggestion + 1) % suggestions.len();
                        return;
                    }
                    KeyCode::Up => {
                        composer.contact_suggestion =
                            (composer.contact_suggestion + suggestions.len() - 1)
                                % suggestions.len();
                        return;
                    }
                    KeyCode::Esc => {
                        composer.dismissed_contact_query =
                            Some(recipient_query(composer.field_value(), composer.cursor));
                        // One Escape should dismiss completion AND leave Insert
                        // mode, so the next y/yy/p key is a Vim operation,
                        // not literal text typed into an address field.
                        composer.mode = ComposeMode::Normal;
                        composer.visual_anchor = None;
                        composer.visual_linewise = false;
                        composer.clear_pending();
                        return;
                    }
                    _ => {
                        // A keystroke that edits the query will reset the popup
                        // on the next frame, without any per-keystroke I/O.
                        composer.contact_suggestion = 0;
                    }
                }
            }
        }
        if self.detach_confirmation.is_some() {
            self.handle_detach_confirmation_key(key);
            return;
        }
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        if composer.sending {
            if key.code == KeyCode::Esc {
                self.status =
                    StatusLine::temporary("send is already in progress", Duration::from_secs(2));
            }
            return;
        }
        if composer.exit_prompt {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.exit_prompt = false;
                    }
                    self.postpone_composer();
                }
                KeyCode::Char('d') | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.composer = None;
                    self.pending_compose = None;
                    self.resumed_draft_account = None;
                    self.status =
                        StatusLine::temporary("unsent email discarded", Duration::from_secs(2));
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.exit_prompt = false;
                    }
                }
                _ => {}
            }
            return;
        }
        if composer.help_visible {
            let composer = self.composer.as_mut().expect("composer exists");
            if crate::app::help_palette::handle_help_key(
                key,
                composer.compose_help_entries(),
                &mut composer.help_query,
                &mut composer.help_selected,
                &mut composer.help_scroll,
                crate::app::help_palette::HelpKeys::TEXT_MODAL,
            ) {
                composer.help_visible = false;
            }
            return;
        }
        if composer.confirm_send {
            let composer = self.composer.as_mut().expect("composer exists");
            match key.code {
                KeyCode::Enter if composer.send_preview.navigation.visual_anchor.is_some() => {}
                KeyCode::Esc if composer.send_preview.navigation.visual_anchor.is_some() => {
                    composer.clear_preview_visual();
                }
                KeyCode::Char('v' | 'V') => {
                    composer.toggle_preview_visual(key.code == KeyCode::Char('V'));
                }
                KeyCode::Char('y') => {
                    if let Some(selection) = composer.preview_visual_range() {
                        let text =
                            reader_selection_text(&composer.send_preview.layout.text, selection);
                        let linewise = composer.send_preview.navigation.visual_linewise;
                        composer.clear_preview_visual();
                        if let Some(status) =
                            record_yank(&mut self.yank_register, YankRegister { text, linewise })
                        {
                            self.status = status;
                        }
                    }
                }
                KeyCode::Enter => {
                    composer.close_send_preview();
                    self.send_composer();
                }
                KeyCode::Esc => {
                    composer.close_send_preview();
                }
                KeyCode::Char('?') => {
                    composer.send_preview.clear_pending_motion();
                    composer.help_visible = true;
                    composer.help_query.clear();
                    composer.help_selected = 0;
                    composer.help_scroll = 0;
                }
                KeyCode::Char('t') => {
                    composer.send_preview.toggle_mode();
                }
                KeyCode::Char('C') => {
                    self.quoted_diff_highlighting = !self.quoted_diff_highlighting;
                    composer.send_preview.invalidate_layout();
                }
                KeyCode::Char('O') => self.open_compose_browser_preview(),
                _ => composer.handle_preview_motion(key),
            }
            return;
        }
        if composer.attachment_manager_open {
            let composer = self.composer.as_mut().expect("composer exists");
            match key.code {
                KeyCode::Esc
                    if composer.attachment_manager_visual_anchor.is_some()
                        || !composer.attachment_manager_marked.is_empty() =>
                {
                    composer.attachment_manager_visual_anchor = None;
                    composer.attachment_manager_marked.clear();
                }
                KeyCode::Esc | KeyCode::Char('q') => composer.attachment_manager_open = false,
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    composer.attachment_manager_open = false;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    composer.attachment_manager_selected = composer
                        .attachment_manager_selected
                        .saturating_add(1)
                        .min(composer.attachment_count().saturating_sub(1))
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    composer.attachment_manager_selected =
                        composer.attachment_manager_selected.saturating_sub(1)
                }
                KeyCode::Char('?') => {
                    composer.help_visible = true;
                    composer.help_query.clear();
                    composer.help_selected = 0;
                    composer.help_scroll = 0;
                }
                KeyCode::Char('v' | 'V') => {
                    if composer.attachment_manager_visual_anchor.take().is_none() {
                        composer.attachment_manager_visual_anchor =
                            Some(composer.attachment_manager_selected);
                    }
                }
                KeyCode::Char(' ') => {
                    if let Some(anchor) = composer.attachment_manager_visual_anchor.take() {
                        let (first, last) = (
                            anchor.min(composer.attachment_manager_selected),
                            anchor.max(composer.attachment_manager_selected),
                        );
                        let indices = (first..=last).collect::<Vec<_>>();
                        crate::app::vim::toggle_group(
                            &mut composer.attachment_manager_marked,
                            &indices,
                        );
                    } else {
                        let index = composer.attachment_manager_selected;
                        crate::app::vim::toggle_group(
                            &mut composer.attachment_manager_marked,
                            &[index],
                        );
                        composer.attachment_manager_selected = index
                            .saturating_add(1)
                            .min(composer.attachment_count().saturating_sub(1));
                    }
                }
                KeyCode::Char('x' | 'd') | KeyCode::Delete | KeyCode::Backspace => {
                    let files = composer
                        .attachment_paths
                        .iter()
                        .cloned()
                        .chain(
                            composer
                                .attachments
                                .split(';')
                                .map(str::trim)
                                .filter(|part| !part.is_empty())
                                .map(PathBuf::from),
                        )
                        .collect::<Vec<_>>();
                    let selected = composer.attachment_manager_selected;
                    let anchor = composer.attachment_manager_visual_anchor;
                    let paths = files
                        .into_iter()
                        .enumerate()
                        .filter(|(index, _)| {
                            composer.attachment_manager_marked.contains(index)
                                || anchor.is_some_and(|anchor| {
                                    (anchor.min(selected)..=anchor.max(selected)).contains(index)
                                })
                                || (composer.attachment_manager_marked.is_empty()
                                    && anchor.is_none()
                                    && *index == selected)
                        })
                        .map(|(_, path)| path)
                        .collect::<Vec<_>>();
                    if !paths.is_empty() {
                        self.detach_confirmation = Some(DetachConfirmation {
                            paths,
                            review: ReviewScroll::default(),
                        });
                    }
                }
                KeyCode::Char('u') => {
                    if let Some(path) = composer.undo_managed_attachment() {
                        self.status = StatusLine::temporary(
                            format!("reattached {}", path.display()),
                            Duration::from_secs(2),
                        );
                    }
                }
                _ => {}
            }
            return;
        }
        if composer.file_picker.is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
                self.composer.as_mut().expect("composer exists").file_picker = None;
                return;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let picker = self
                    .composer
                    .as_mut()
                    .and_then(|composer| composer.file_picker.as_mut());
                if let Some(picker) = picker {
                    match key.code {
                        KeyCode::Char('j') => picker.queue_smooth_scroll(SMOOTH_SCROLL_ROWS),
                        KeyCode::Char('k') => picker.queue_smooth_scroll(-SMOOTH_SCROLL_ROWS),
                        _ => {}
                    }
                }
                return;
            }
            if key.code == KeyCode::Char('x') {
                let composer = self.composer.as_ref().expect("composer exists");
                if let Some(picker) = composer.file_picker.as_ref() {
                    let (first, last) = picker
                        .visual_anchor
                        .map(|anchor| (anchor.min(picker.selected), anchor.max(picker.selected)))
                        .unwrap_or((picker.selected, picker.selected));
                    let paths = picker
                        .entries
                        .iter()
                        .skip(first)
                        .take(last - first + 1)
                        .filter(|entry| !entry.is_directory)
                        .map(|entry| entry.path.clone())
                        .filter(|path| {
                            composer.attachment_paths.contains(path)
                                || composer
                                    .attachments
                                    .split(';')
                                    .map(str::trim)
                                    .any(|part| Path::new(part) == path.as_path())
                        })
                        .collect::<Vec<_>>();
                    if !paths.is_empty() {
                        self.detach_confirmation = Some(DetachConfirmation {
                            paths,
                            review: ReviewScroll::default(),
                        });
                        return;
                    }
                }
            }
            match self
                .composer
                .as_mut()
                .expect("composer exists")
                .file_picker_key(key.code)
            {
                Ok(Some(message)) => {
                    self.status = StatusLine::temporary(message, Duration::from_secs(2))
                }
                Ok(None) => {}
                Err(error) => self.status = StatusLine::error(format!("file picker: {error:#}")),
            }
            return;
        }
        if composer.advanced_headers_open {
            let composer = self.composer.as_mut().expect("composer exists");
            match key.code {
                KeyCode::Esc | KeyCode::Enter => composer.toggle_advanced_headers(),
                KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    composer.toggle_advanced_headers()
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    composer.clear_current_line();
                }
                KeyCode::Backspace => composer.backspace(),
                KeyCode::Delete => composer.delete_at_cursor(),
                KeyCode::Left => composer.move_horizontal(-1),
                KeyCode::Right => composer.move_horizontal(1),
                KeyCode::Home => composer.cursor = 0,
                KeyCode::End => composer.cursor = composer.char_count(),
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    composer.insert_char(ch)
                }
                _ => {}
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            let composer = self.composer.as_mut().expect("composer exists");
            composer.attachment_manager_open = true;
            composer.attachment_manager_selected = 0;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            if let Err(error) = self
                .composer
                .as_mut()
                .expect("composer exists")
                .open_file_picker()
            {
                self.status = StatusLine::error(format!("file picker: {error:#}"));
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.composer
                .as_mut()
                .expect("composer exists")
                .toggle_advanced_headers();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.postpone_composer();
            return;
        }
        // Match the reader's C toggle while editing quoted replies. In Insert
        // mode uppercase C remains literal text, as in a regular Vim buffer.
        if key.code == KeyCode::Char('C')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self
                .composer
                .as_ref()
                .is_some_and(|composer| composer.mode != ComposeMode::Insert)
        {
            self.quoted_diff_highlighting = !self.quoted_diff_highlighting;
            self.status = StatusLine::info(if self.quoted_diff_highlighting {
                "quoted Git diff colors enabled"
            } else {
                "conversation quote colors enabled"
            });
            return;
        }
        let composer = self.composer.as_mut().expect("composer exists");
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                // Ctrl-W clears the current logical body line or the entire
                // single-line header field, consistently in every Compose mode.
                KeyCode::Char('w') => composer.clear_current_line(),
                KeyCode::Char('r') => composer.undo_edit(true),
                KeyCode::Char('j') if composer.field == ComposeField::Body => {
                    composer.queue_body_smooth_scroll(SMOOTH_SCROLL_ROWS);
                }
                KeyCode::Char('k') if composer.field == ComposeField::Body => {
                    composer.queue_body_smooth_scroll(-SMOOTH_SCROLL_ROWS);
                }
                _ => {}
            }
            return;
        }
        // Headers use Tab for field navigation even while typing. The body
        // keeps its literal Insert-mode tab for prose and patches.
        if (composer.mode == ComposeMode::Normal
            || (composer.mode == ComposeMode::Insert && composer.field != ComposeField::Body))
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
        {
            let next = if key.code == KeyCode::Tab {
                composer.field.next()
            } else {
                composer.field.previous()
            };
            composer.switch_field(next);
            return;
        }
        if composer.mode == ComposeMode::Insert {
            match key.code {
                KeyCode::Esc => {
                    composer.mode = ComposeMode::Normal;
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                }
                KeyCode::Tab if composer.field == ComposeField::Body => {
                    composer.insert_char('\t');
                }
                KeyCode::Enter if composer.field == ComposeField::Body => {
                    composer.insert_char('\n')
                }
                KeyCode::Enter => composer.switch_field(composer.field.next()),
                KeyCode::Backspace => composer.backspace(),
                KeyCode::Delete => composer.delete_at_cursor(),
                KeyCode::Left => composer.move_horizontal(-1),
                KeyCode::Right => composer.move_horizontal(1),
                KeyCode::Up if composer.field != ComposeField::Body => {
                    let previous = composer.field.previous();
                    if previous != ComposeField::Body {
                        composer.switch_field(previous);
                    }
                }
                KeyCode::Down if composer.field != ComposeField::Body => {
                    composer.switch_field(composer.field.next())
                }
                KeyCode::Up => composer.move_line(-1),
                KeyCode::Down => composer.move_line(1),
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::ALT) => {
                    composer.insert_char(ch)
                }
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('?') {
            composer.help_visible = true;
            composer.help_query.clear();
            composer.help_selected = 0;
            composer.help_scroll = 0;
            composer.clear_pending();
            return;
        }

        // One shared pending-command state machine: no overlapping pending
        // delete/yank/find flags, and count parsing is shared with Contacts.
        if let Some(prefix) = composer.vim_sequence.take_command() {
            use crate::app::vim::VimPrefix;
            match prefix {
                VimPrefix::FindForward | VimPrefix::FindBackward => {
                    if let KeyCode::Char(ch) = key.code {
                        let count = composer.vim_sequence.take_count();
                        composer.find_char(ch, prefix == VimPrefix::FindForward, count);
                    }
                    return;
                }
                VimPrefix::Inner => {
                    if let KeyCode::Char(delim) = key.code {
                        composer.select_inner_object(delim);
                    }
                    return;
                }
                VimPrefix::Goto => {
                    if key.code == KeyCode::Char('g') {
                        composer.cursor = 0;
                        composer.vim_sequence.discard_count();
                        return;
                    }
                    // Historically, an unmatched g is ignored and the next
                    // key still gets processed as a normal command.
                }
                VimPrefix::Yank => {
                    let count = composer.vim_sequence.take_count();
                    let copied = match key.code {
                        KeyCode::Char('y') => Some(composer.yank_lines(count)),
                        KeyCode::Char('$') => Some(composer.yank_to_line_end()),
                        _ => None,
                    };
                    if let Some(register) = copied
                        && let Some(status) = record_yank(&mut self.yank_register, register)
                    {
                        self.status = status;
                    }
                    return;
                }
                VimPrefix::Delete => {
                    let count = composer.vim_sequence.take_count();
                    match key.code {
                        KeyCode::Char('d') => composer.delete_current_lines(count),
                        KeyCode::Char('w') => composer.delete_words(count),
                        KeyCode::Char('$') => {
                            let end = composer.line_end();
                            composer.delete_range(composer.cursor, end);
                        }
                        _ => {}
                    }
                    return;
                }
            }
        }
        if let KeyCode::Char(digit) = key.code
            && composer.vim_sequence.push_digit(digit, MAX_COUNT_PREFIX)
        {
            return;
        }
        if key.code == KeyCode::Char('g') {
            composer
                .vim_sequence
                .start(crate::app::vim::VimPrefix::Goto);
            return;
        }
        if matches!(key.code, KeyCode::Char('f' | 'F')) {
            composer
                .vim_sequence
                .start(if key.code == KeyCode::Char('f') {
                    crate::app::vim::VimPrefix::FindForward
                } else {
                    crate::app::vim::VimPrefix::FindBackward
                });
            return;
        }
        let count = composer.vim_sequence.take_count();
        // Header navigation is separate from body Vim motions.
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if composer.move_header_field(true, count) => return,
            KeyCode::Char('k') | KeyCode::Up if composer.move_header_field(false, count) => return,
            _ => {}
        }
        if composer.apply_motion(key.code, count) {
            return;
        }
        match composer.mode {
            ComposeMode::Normal => match key.code {
                KeyCode::Enter => {
                    if let Err(error) = composer.prepare_send_preview() {
                        self.status =
                            StatusLine::error(format!("cannot preview outgoing email: {error:#}"));
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    if composer.has_unsent_content() {
                        composer.exit_prompt = true;
                    } else {
                        self.composer = None;
                        self.pending_compose = None;
                    }
                }
                KeyCode::Char('i') => composer.mode = ComposeMode::Insert,
                KeyCode::Char('a') => {
                    // Vim's append stops at the end of the *current* line;
                    // horizontal() can otherwise step across its newline.
                    composer.cursor = composer.cursor.saturating_add(1).min(composer.line_end());
                    composer.mode = ComposeMode::Insert;
                }
                KeyCode::Char('I') => {
                    composer.cursor = composer.first_nonblank();
                    composer.mode = ComposeMode::Insert;
                }
                KeyCode::Char('A') => {
                    composer.cursor = composer.line_end();
                    composer.mode = ComposeMode::Insert;
                }
                KeyCode::Char('v') => {
                    // Switching header fields places the caret after the last
                    // character. Vim Visual mode must still select that last
                    // character rather than starting with an empty range.
                    let anchor =
                        if composer.field != ComposeField::Body && composer.char_count() > 0 {
                            composer.cursor.min(composer.char_count() - 1)
                        } else {
                            composer.cursor.min(composer.char_count())
                        };
                    composer.cursor = anchor;
                    composer.visual_anchor = Some(anchor);
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Visual;
                }
                KeyCode::Char('V') => {
                    composer.visual_anchor = Some(composer.cursor.min(composer.char_count()));
                    composer.visual_linewise = true;
                    composer.mode = ComposeMode::Visual;
                }
                KeyCode::Char('u') => composer.undo_edit(false),
                KeyCode::Char('y') => {
                    composer
                        .vim_sequence
                        .start(crate::app::vim::VimPrefix::Yank);
                    composer.vim_sequence.set_count(count);
                }
                KeyCode::Char('Y') => {
                    if let Some(status) =
                        record_yank(&mut self.yank_register, composer.yank_lines(count))
                    {
                        self.status = status;
                    }
                }
                KeyCode::Char('p' | 'P') => {
                    if let Some(register) = &self.yank_register {
                        composer.paste_register(register, key.code == KeyCode::Char('p'), count);
                    }
                }
                KeyCode::Char('x') => {
                    let start = composer.cursor.min(composer.char_count());
                    // Vim's x does not join lines when the cursor reaches a newline.
                    let end = start.saturating_add(count).min(composer.line_end());
                    let cut = crate::app::vim::text_range(composer.field_value(), start, end)
                        .map(str::to_owned);
                    if let Some(cut) = cut {
                        composer.delete_range(start, end);
                        if let Some(status) = record_cut(&mut self.yank_register, cut, false) {
                            self.status = status;
                        }
                    }
                }
                KeyCode::Delete => {
                    for _ in 0..count {
                        composer.delete_at_cursor();
                    }
                }
                KeyCode::Char('D') => {
                    let end = composer.line_end();
                    composer.delete_range(composer.cursor, end);
                }
                KeyCode::Char('d') => {
                    composer
                        .vim_sequence
                        .start(crate::app::vim::VimPrefix::Delete);
                    composer.vim_sequence.set_count(count);
                }
                KeyCode::Char('o') if composer.field == ComposeField::Body => {
                    composer.open_line_below();
                    composer.mode = ComposeMode::Insert;
                }
                KeyCode::Char('O') if composer.field == ComposeField::Body => {
                    composer.open_line_above();
                    composer.mode = ComposeMode::Insert;
                }
                _ => {}
            },
            ComposeMode::Visual => match key.code {
                KeyCode::Esc | KeyCode::Char('v') => {
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Normal;
                }
                KeyCode::Char('V') => composer.visual_linewise = !composer.visual_linewise,
                KeyCode::Char('i') => composer
                    .vim_sequence
                    .start(crate::app::vim::VimPrefix::Inner),
                KeyCode::Char('y') => {
                    if let Some(register) = composer.yank_visual()
                        && let Some(status) = record_yank(&mut self.yank_register, register)
                    {
                        self.status = status;
                    }
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Normal;
                }
                KeyCode::Char('p' | 'P') => {
                    if let Some(register) = &self.yank_register {
                        composer.paste_over_visual(register);
                    }
                }
                KeyCode::Char('x') => {
                    if let Some((start, end)) = composer.visual_range() {
                        let linewise =
                            composer.visual_linewise && composer.field == ComposeField::Body;
                        let cut = crate::app::vim::text_range(composer.field_value(), start, end)
                            .map(str::to_owned);
                        composer.delete_range(start, end);
                        if let Some(cut) = cut
                            && let Some(status) = record_cut(&mut self.yank_register, cut, linewise)
                        {
                            self.status = status;
                        }
                    }
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Normal;
                }
                KeyCode::Char('d') | KeyCode::Delete => {
                    if let Some((start, end)) = composer.visual_range() {
                        composer.delete_range(start, end);
                    }
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Normal;
                }
                KeyCode::Char('c') => {
                    if let Some((start, end)) = composer.visual_range() {
                        composer.delete_range(start, end);
                    }
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Insert;
                }
                _ => {}
            },
            ComposeMode::Insert => {}
        }
    }

    pub(super) fn open_compose_browser_preview(&mut self) {
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        let html = match composer.outgoing_html_preview() {
            Ok(Some(html)) => html,
            Ok(None) => composer.plain_review_html(),
            Err(error) => {
                self.status = StatusLine::error(format!("cannot preview HTML: {error:#}"));
                return;
            }
        };
        let result = (|| -> anyhow::Result<()> {
            let directory = dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("jerc")
                .join("browser");
            std::fs::create_dir_all(&directory)?;
            let path = directory.join("compose-preview.html");
            std::fs::write(&path, html.as_bytes())?;
            open_url(path.to_string_lossy().as_ref())?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.status = StatusLine::temporary(
                    "opened outgoing email preview in browser",
                    Duration::from_secs(2),
                )
            }
            Err(error) => self.status = StatusLine::error(format!("browser preview: {error:#}")),
        }
    }

    pub(super) fn postponed_drafts_root() -> anyhow::Result<std::path::PathBuf> {
        let root = dirs::data_local_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot find local application data directory"))?;
        Ok(root.join("jerc").join("postponed"))
    }

    pub(super) fn postpone_composer(&mut self) {
        let Some(account) = self.account() else {
            return;
        };
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        // Always write a local recovery copy first. If IMAP is offline or the
        // Drafts APPEND fails, Ctrl+N can still recover the unsent message.
        let result = Self::postponed_drafts_root()
            .and_then(|root| crate::outgoing::drafts::save_draft(&root, &account.name, composer));
        if let Err(error) = result {
            self.status = StatusLine::error(format!("could not save draft: {error:#}"));
            return;
        }
        if composer.original_loading {
            self.composer = None;
            self.pending_compose = None;
            self.resumed_draft_account = None;
            self.status =
                StatusLine::info("saved locally; original MIME still missing (Ctrl+n to resume)");
            return;
        }
        let draft_mailbox = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == MailboxRole::Drafts)
            .map(|mailbox| mailbox.name.clone());
        let draft_request = if let Some(mailbox) = draft_mailbox {
            let message = match outgoing_from_composer(composer, &account.from) {
                Ok(message) => message,
                Err(error) => {
                    self.status = StatusLine::error(format!(
                        "saved local recovery copy, but could not prepare IMAP draft: {error:#}"
                    ));
                    return;
                }
            };
            match self.account().and_then(|account| {
                account
                    .worker
                    .post(ActionKind::SaveDraft { message, mailbox })
                    .ok()
            }) {
                Some(request_id) => Some(request_id),
                None => {
                    self.status = StatusLine::error(
                        "saved local recovery copy; Drafts worker is busy (Ctrl+n resumes)",
                    );
                    return;
                }
            }
        } else {
            None
        };
        if let (Some(request_id), Some(source)) = (draft_request, composer.draft_source.clone()) {
            self.pending_draft_replacements
                .insert((self.account_index, request_id), source);
        }
        self.composer = None;
        self.pending_compose = None;
        self.resumed_draft_account = None;
        self.status = if draft_request.is_some() {
            StatusLine::info("saved local recovery copy; saving to Drafts folder…")
        } else {
            StatusLine::error("saved locally only: no Drafts mailbox found; Ctrl+n resumes")
        };
    }

    pub(super) fn restore_postponed_composer(&mut self) {
        let Some(account) = self.account() else {
            return;
        };
        if !account.outgoing_configured {
            self.status =
                StatusLine::error("configure an outgoing transport before resuming a draft");
            return;
        }
        let result = Self::postponed_drafts_root()
            .and_then(|root| crate::outgoing::drafts::load_draft(&root, &account.name));
        match result {
            Ok(Some(draft)) => {
                let source = draft.original_source.clone();
                self.composer = Some(draft);
                self.resumed_draft_account = Some(self.account_index);
                self.pending_compose = source.map(|source| PendingCompose {
                    account: self.account_index,
                    request_id: 0,
                    message_id: source.summary.id.clone(),
                    intent: source.intent,
                    mailbox: source.mailbox,
                    summary: source.summary,
                    cache_identity: source.cache_identity,
                    cache_only: false,
                    initial_body: source.initial_body,
                });
                if self.pending_compose.is_some() {
                    self.try_prepare_open_composer();
                } else {
                    self.status = StatusLine::info("resumed locally postponed draft");
                }
            }
            Ok(None) => {
                self.status = StatusLine::temporary(
                    "no postponed draft for this account",
                    Duration::from_secs(2),
                )
            }
            Err(error) => {
                self.status = StatusLine::error(format!("cannot resume draft: {error:#}"))
            }
        }
    }

    pub(super) fn send_composer(&mut self) {
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        if composer.sending {
            return;
        }
        if composer.original_loading {
            self.status = StatusLine::error(
                "original MIME/recipients not ready; wait for cached or verified source",
            );
            return;
        }
        if composer.to.trim().is_empty()
            && composer.cc.trim().is_empty()
            && composer.bcc.trim().is_empty()
        {
            self.status = StatusLine::error("Add at least one To, Cc or Bcc recipient");
            return;
        }
        if let Err(error) =
            crate::outgoing::validate_recipients(&composer.to, &composer.cc, &composer.bcc)
        {
            self.status = StatusLine::error(format!("invalid recipient: {error:#}"));
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        let message = match outgoing_from_composer(composer, &account.from) {
            Ok(message) => message,
            Err(error) => {
                self.status = StatusLine::error(format!("cannot prepare email: {error:#}"));
                return;
            }
        };
        let copy_to = crate::outgoing::sent_copy_targets(account.copy_to.as_deref());
        let sent_mailbox = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == MailboxRole::Sent)
            .map(|mailbox| mailbox.name.clone());
        // Validate the entire wire format BEFORE putting a send on the worker:
        // headers, MIME, attachments, and SMTP envelope must all be buildable.
        // Invalid mail remains editable and is never submitted to the relay.
        if let Err(error) = crate::outgoing::build_outgoing(&message, &account.from) {
            self.status = StatusLine::error(format!("cannot prepare outgoing email: {error:#}"));
            return;
        }
        if account.capabilities.is_none() {
            let draft_source = composer
                .draft_source
                .as_ref()
                .map(|source| (source.mailbox.clone(), source.id.clone()));
            let queued = match crate::outgoing::outbox::QueuedSend::new(
                message,
                sent_mailbox,
                copy_to,
                draft_source,
            ) {
                Ok(queued) => queued,
                Err(error) => {
                    self.status =
                        StatusLine::error(format!("cannot queue offline email: {error:#}"));
                    return;
                }
            };
            if let Err(error) = self
                .account_mut()
                .expect("account exists")
                .queue_outgoing(queued)
            {
                self.status = StatusLine::error(format!("cannot queue offline email: {error:#}"));
                return;
            }
            self.composer = None;
            self.pending_compose = None;
            self.resumed_draft_account = None;
            self.status = StatusLine::info("message saved in offline Outbox; sends on reconnect");
            return;
        }
        match account.worker.post(ActionKind::Send {
            message,
            sent_mailbox,
            copy_to,
        }) {
            Ok(request_id) => {
                self.pending_send = Some((self.account_index, request_id));
                if let Some(composer) = self.composer.as_mut() {
                    composer.sending = true;
                }
                self.status = StatusLine::info("sending message…");
            }
            Err(_) => {
                // An enqueue failure definitely did not send; retain the draft.
                self.status = StatusLine::error("mail worker queue is busy; message was not sent");
            }
        }
    }
}
