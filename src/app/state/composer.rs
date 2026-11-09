//! Composer behavior.

use super::*;

impl Composer {
    pub fn new() -> Self {
        Self {
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            attachments: String::new(),
            attachment_paths: Vec::new(),
            headers: String::new(),
            file_picker: None,
            attachment_manager_open: false,
            attachment_manager_selected: 0,
            attachment_manager_visual_anchor: None,
            attachment_manager_marked: HashSet::new(),
            removed_attachments: Vec::new(),
            advanced_headers_open: false,
            advanced_previous: None,
            subject: String::new(),
            body: String::new(),
            field: ComposeField::To,
            mode: ComposeMode::Insert,
            cursor: 0,
            visual_anchor: None,
            in_reply_to: None,
            references: Vec::new(),
            forwarded_raw: None,
            draft_raw: None,
            draft_attachment_names: Vec::new(),
            draft_source: None,
            sending: false,
            confirm_send: false,
            exit_prompt: false,
            preview_html: true,
            preview_scroll: 0,
            preview_rich_lines: None,
            preview_forwarded_attachments: Vec::new(),
            help_visible: false,
            contact_suggestion: 0,
            dismissed_contact_query: None,
            help_query: String::new(),
            help_selected: 0,
            help_scroll: 0,
            body_view_width: 72,
            body_scroll: 0,
            body_smooth_target: None,
            body_preferred_column: None,
            visual_linewise: false,
            pending_g: false,
            pending_delete: false,
            pending_yank: false,
            pending_inner: false,
            pending_find: None,
            count: None,
            undo_history: Vec::new(),
            redo_history: Vec::new(),
        }
    }

    pub(super) fn from_server_draft(id: MessageId, raw: Vec<u8>) -> anyhow::Result<Self> {
        let parsed = mail_parser::MessageParser::new()
            .parse(&raw)
            .ok_or_else(|| anyhow::anyhow!("cannot parse saved draft headers"))?;
        // Normalize the display names through the same quoting implementation
        // as autocomplete, rather than passing unquoted parentheses to Lettre.
        macro_rules! recipients_in {
            ($value:expr) => {
                $value.map(|list| {
                    list.mailboxes().filter_map(|mb| {
                        let address = mb.address()?.trim();
                        if address.is_empty() { return None; }
                        Some(crate::contacts::Contact {
                            name: mb.name().unwrap_or_default().to_owned(),
                            email: address.to_owned(),
                            saved: false,
                            favorite: false,
                        }.display())
                    }).collect::<Vec<_>>().join(", ")
                }).unwrap_or_default()
            };
        }
        let summary = crate::mail::parse_summary(
            id.clone(), &raw, raw.len() as u64, MessageFlags::empty()
        )?;
        let body = crate::mail::parse_body(id, &raw)?;
        let mut composer = Self::new();
        composer.to = recipients_in!(parsed.to());
        composer.cc = recipients_in!(parsed.cc());
        composer.bcc = recipients_in!(parsed.bcc());
        composer.subject = summary.subject;
        composer.body = body.plain_text
            .or(body.plain_fallback)
            .unwrap_or_default();
        composer.in_reply_to = summary.in_reply_to;
        composer.references = summary.references;
        // Attachments remain encoded in the original RFC5322 payload until
        // the user sends or saves. The Compose editor only modifies plain text;
        // do not re-send a stale HTML alternative with diverging content.
        if body.attachment_count > 0 {
            composer.draft_attachment_names = body.attachment_names;
            composer.draft_raw = Some(raw);
        }
        composer.field = ComposeField::Body;
        composer.mode = ComposeMode::Insert;
        composer.cursor = composer.body.chars().count();
        Ok(composer)
    }

    pub(super) fn reply(message: &MessageSummary, body: Option<&MessageBody>, quote: bool) -> Self {
        let subject = reply_subject(&message.subject);
        let mut references = message.references.clone();
        if let Some(id) = &message.message_id
            && references.last() != Some(id)
        {
            references.push(id.clone());
        }
        let quoted = if quote {
            body.map_or_else(String::new, |body| {
                // quoted replies use the message's textual representation,
                // not the rich reader projection. HTML-only messages already
                // have a generated plain fallback, so this stays readable while
                // avoiding table chrome/image placeholders in mailing-list replies.
                body.display_text(false)
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        } else {
            String::new()
        };
        let body = if quoted.is_empty() {
            "\n\n".to_owned()
        } else {
            format!("\n\n{}\n{quoted}\n", reply_attribution(message))
        };
        // Replies are composed above any quoted history, matching normal mail
        // clients' reply workflow rather than dropping the cursor below
        // the quote.
        let cursor = 0;
        Self {
            to: if message.from_name.is_empty() {
                message.from_address.clone()
            } else {
                crate::contacts::Contact {
                    name: message.from_name.clone(),
                    email: message.from_address.clone(),
                    saved: false,
                    favorite: false,
                }.display()
            },
            cc: String::new(),
            bcc: String::new(),
            attachments: String::new(),
            attachment_paths: Vec::new(),
            headers: String::new(),
            file_picker: None,
            attachment_manager_open: false,
            attachment_manager_selected: 0,
            attachment_manager_visual_anchor: None,
            attachment_manager_marked: HashSet::new(),
            removed_attachments: Vec::new(),
            advanced_headers_open: false,
            advanced_previous: None,
            subject,
            body,
            field: ComposeField::Body,
            mode: ComposeMode::Insert,
            cursor,
            visual_anchor: None,
            in_reply_to: message.message_id.clone(),
            references,
            forwarded_raw: None,
            draft_raw: None,
            draft_attachment_names: Vec::new(),
            draft_source: None,
            sending: false,
            confirm_send: false,
            exit_prompt: false,
            preview_html: true,
            preview_scroll: 0,
            preview_rich_lines: None,
            preview_forwarded_attachments: Vec::new(),
            help_visible: false,
            contact_suggestion: 0,
            dismissed_contact_query: None,
            help_query: String::new(),
            help_selected: 0,
            help_scroll: 0,
            body_view_width: 72,
            body_scroll: 0,
            body_smooth_target: None,
            body_preferred_column: None,
            visual_linewise: false,
            pending_g: false,
            pending_delete: false,
            pending_yank: false,
            pending_inner: false,
            pending_find: None,
            count: None,
            undo_history: Vec::new(),
            redo_history: Vec::new(),
        }
    }

    pub(super) fn forward(message: &MessageSummary, raw: Vec<u8>) -> anyhow::Result<Self> {
        let parts = inline_forward_parts(&raw)?;
        let subject = forward_subject(&message.subject);
        let attribution = forward_attribution(&raw)?;
        let body = format!("\n\n{attribution}\n{}\n", parts.plain);
        Ok(Self {
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            attachments: String::new(),
            attachment_paths: Vec::new(),
            headers: String::new(),
            file_picker: None,
            attachment_manager_open: false,
            attachment_manager_selected: 0,
            attachment_manager_visual_anchor: None,
            attachment_manager_marked: HashSet::new(),
            removed_attachments: Vec::new(),
            advanced_headers_open: false,
            advanced_previous: None,
            subject,
            body,
            field: ComposeField::To,
            mode: ComposeMode::Insert,
            cursor: 0,
            visual_anchor: None,
            in_reply_to: None,
            references: Vec::new(),
            forwarded_raw: Some(raw),
            draft_raw: None,
            draft_attachment_names: Vec::new(),
            draft_source: None,
            sending: false,
            confirm_send: false,
            exit_prompt: false,
            preview_html: true,
            preview_scroll: 0,
            preview_rich_lines: None,
            preview_forwarded_attachments: Vec::new(),
            help_visible: false,
            contact_suggestion: 0,
            dismissed_contact_query: None,
            help_query: String::new(),
            help_selected: 0,
            help_scroll: 0,
            body_view_width: 72,
            body_scroll: 0,
            body_smooth_target: None,
            body_preferred_column: None,
            visual_linewise: false,
            pending_g: false,
            pending_delete: false,
            pending_yank: false,
            pending_inner: false,
            pending_find: None,
            count: None,
            undo_history: Vec::new(),
            redo_history: Vec::new(),
        })
    }

    pub(super) fn prepare_send_preview(&mut self) -> anyhow::Result<()> {
        crate::outgoing::validate_recipients(&self.to, &self.cc, &self.bcc)?;
        // Only parse/reflow rich mail and attachment metadata upon opening the
        // preview, never while the terminal draws successive frames.
        self.preview_forwarded_attachments = self.draft_attachment_names.clone();
        self.preview_forwarded_attachments.extend(self
            .forwarded_raw
            .as_deref()
            .and_then(|raw| crate::mail::extract_attachments(raw).ok())
            .map(|parts| parts.into_iter().map(|part| part.filename).collect::<Vec<_>>())
            .unwrap_or_default());
        self.preview_rich_lines = self
            .outgoing_html_preview()?
            .map(|html| render_html_for_terminal(&html, 90).1);
        self.preview_html = true;
        self.preview_scroll = 0;
        self.confirm_send = true;
        Ok(())
    }

    /// The HTML actually used for inline forwards, if present. Normal plain
    /// messages have no HTML alternative, so T correctly shows the same text.
    pub fn outgoing_html_preview(&self) -> anyhow::Result<Option<String>> {
        let Some(raw) = self.forwarded_raw.as_deref() else {
            return Ok(None);
        };
        let parts = inline_forward_parts(raw)?;
        Ok(parts.html.map(|html| {
            let personal_note = self.body.split(FORWARD_SEPARATOR).next().unwrap_or("");
            let original_headers = self
                .body
                .split(FORWARD_SEPARATOR)
                .nth(1)
                .and_then(|tail| tail.split("\n\n").next())
                .unwrap_or("");
            let attribution = format!("{FORWARD_SEPARATOR}{original_headers}");
            forward_html(&html, &attribution, personal_note)
        }))
    }

    /// Unsaved mail must never disappear on a stray `q` or `Esc`.
    /// A reply or forward with original material is also worth protecting.
    pub fn has_unsent_content(&self) -> bool {
        [
            &self.to,
            &self.cc,
            &self.bcc,
            &self.subject,
            &self.body,
            &self.headers,
            &self.attachments,
        ]
        .iter()
        .any(|value| !value.trim().is_empty())
            || !self.attachment_paths.is_empty()
            || self.forwarded_raw.is_some()
            || self.in_reply_to.is_some()
    }

    pub fn total_attachment_count(&self) -> usize {
        self.attachment_count() + self.draft_attachment_names.len()
    }

    pub fn attachment_count(&self) -> usize {
        self.attachment_paths.len()
            + self
                .attachments
                .split(';')
                .filter(|part| !part.trim().is_empty())
                .count()
    }

    pub(super) fn remove_managed_attachment(&mut self) -> Option<PathBuf> {
        let selected = self.attachment_manager_selected;
        let (removed, index, legacy) = if selected < self.attachment_paths.len() {
            (self.attachment_paths.remove(selected), selected, false)
        } else {
            let index = selected - self.attachment_paths.len();
            let mut paths: Vec<String> = self
                .attachments
                .split(';')
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect();
            if index >= paths.len() {
                return None;
            }
            let removed = PathBuf::from(paths.remove(index));
            self.attachments = paths.join(";");
            (removed, index, true)
        };
        self.removed_attachments.push(RemovedAttachment {
            index,
            path: removed.clone(),
            legacy,
        });
        self.attachment_manager_selected = selected.min(self.attachment_count().saturating_sub(1));
        Some(removed)
    }

    pub(super) fn undo_managed_attachment(&mut self) -> Option<PathBuf> {
        let entry = self.removed_attachments.pop()?;
        if entry.legacy {
            let mut paths: Vec<String> = self
                .attachments
                .split(';')
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect();
            let index = entry.index.min(paths.len());
            paths.insert(index, entry.path.to_string_lossy().into_owned());
            self.attachments = paths.join(";");
            self.attachment_manager_selected = self.attachment_paths.len() + index;
        } else {
            let index = entry.index.min(self.attachment_paths.len());
            self.attachment_paths.insert(index, entry.path.clone());
            self.attachment_manager_selected = index;
        }
        Some(entry.path)
    }

    pub(super) fn toggle_advanced_headers(&mut self) {
        if let Some((field, mode, cursor)) = self.advanced_previous.take() {
            self.advanced_headers_open = false;
            self.field = field;
            self.mode = mode;
            self.cursor = cursor;
            self.clear_pending();
        } else {
            let prior_mode = if self.mode == ComposeMode::Visual {
                ComposeMode::Normal
            } else {
                self.mode
            };
            self.advanced_previous = Some((self.field, prior_mode, self.cursor));
            self.advanced_headers_open = true;
            self.switch_field(ComposeField::Headers);
            self.mode = ComposeMode::Insert;
        }
    }

    pub(super) fn open_file_picker(&mut self) -> anyhow::Result<()> {
        let directory = std::env::current_dir().or_else(|_| {
            dirs::home_dir().ok_or_else(|| std::io::Error::other("no home directory"))
        })?;
        self.file_picker = Some(ComposeFilePicker::open(directory)?);
        Ok(())
    }

    /// Return a short message to display in the status bar, if applicable.
    pub(super) fn file_picker_key(&mut self, key: KeyCode) -> anyhow::Result<Option<String>> {
        let Some(picker) = self.file_picker.as_mut() else {
            return Ok(None);
        };
        if picker.help_visible {
            match key {
                KeyCode::Esc | KeyCode::Char('?') if picker.help_query.is_empty() => {
                    picker.help_visible = false
                }
                KeyCode::Esc => picker.help_visible = false,
                KeyCode::Backspace => {
                    picker.help_query.pop();
                    picker.help_selected = *picker.help_matches().first().unwrap_or(&0);
                    picker.help_scroll = 0;
                }
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k')
                    if picker.help_query.is_empty() =>
                {
                    let matches = picker.help_matches();
                    if let Some(index) = matches.iter().position(|&i| i == picker.help_selected) {
                        let index = if matches!(key, KeyCode::Up | KeyCode::Char('k')) {
                            index.saturating_sub(1)
                        } else {
                            (index + 1).min(matches.len().saturating_sub(1))
                        };
                        picker.help_selected = matches[index];
                        picker.help_scroll = index.saturating_sub(5);
                    }
                }
                KeyCode::Char(ch) => {
                    picker.help_query.push(ch);
                    picker.help_selected = *picker.help_matches().first().unwrap_or(&0);
                    picker.help_scroll = 0;
                }
                _ => {}
            }
            return Ok(None);
        }
        if picker.search_query.is_some() {
            match key {
                KeyCode::Esc => picker.search_query = None,
                // Accept the highlighted match and return to normal navigation.
                // Down/Up still cycle suggestions while the search is active.
                KeyCode::Enter => picker.search_query = None,
                KeyCode::Down => picker.next_search_match(false),
                KeyCode::Up => picker.next_search_match(true),
                KeyCode::Backspace => {
                    if let Some(query) = picker.search_query.as_mut() {
                        query.pop();
                    }
                    picker.update_search_selection();
                }
                KeyCode::Tab | KeyCode::Right => {
                    if let Some(entry) = picker.entries.get(picker.selected)
                        && entry.is_directory
                    {
                        picker.enter_directory(entry.path.clone())?;
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(query) = picker.search_query.as_mut() {
                        query.push(ch);
                    }
                    picker.update_search_selection();
                }
                _ => {}
            }
            return Ok(None);
        }
        if key == KeyCode::Char('/') {
            picker.smooth_target = None;
            picker.search_query = Some(String::new());
            return Ok(None);
        }
        if key == KeyCode::Char('?') {
            picker.help_visible = true;
            picker.help_query.clear();
            picker.help_selected = 0;
            picker.help_scroll = 0;
            return Ok(None);
        }
        if matches!(key, KeyCode::Esc | KeyCode::Char('q')) {
            if picker.visual_anchor.take().is_none() {
                self.file_picker = None;
            }
            return Ok(None);
        }
        if matches!(key, KeyCode::Char('v' | 'V')) {
            if picker.visual_anchor.is_some() {
                picker.visual_anchor = None;
            } else {
                picker.visual_anchor = Some(picker.selected);
            }
            picker.pending_count = 0;
            return Ok(None);
        }
        if picker.pending_g {
            picker.pending_g = false;
            if key == KeyCode::Char('g') {
                picker.selected = 0;
                picker.refresh_preview();
                picker.pending_count = 0;
                return Ok(None);
            }
        }
        match key {
            KeyCode::Char(digit @ '1'..='9') => {
                picker.pending_count = picker
                    .pending_count
                    .saturating_mul(10)
                    .saturating_add(digit.to_digit(10).unwrap_or(1) as usize)
                    .min(99_999);
                return Ok(None);
            }
            KeyCode::Char('0') if picker.pending_count > 0 => {
                picker.pending_count = picker.pending_count.saturating_mul(10).min(99_999);
                return Ok(None);
            }
            KeyCode::Char('g') => {
                picker.pending_g = true;
                return Ok(None);
            }
            _ => {}
        }
        let count = picker.pending_count.max(1);
        picker.pending_count = 0;
        if key == KeyCode::Char('x') {
            let (start, end) = picker
                .visual_anchor
                .map(|anchor| (anchor.min(picker.selected), anchor.max(picker.selected)))
                .unwrap_or((picker.selected, picker.selected));
            let paths: Vec<PathBuf> = picker
                .entries
                .iter()
                .skip(start)
                .take(end - start + 1)
                .filter(|entry| !entry.is_directory)
                .map(|entry| entry.path.clone())
                .collect();
            let before = self.attachment_paths.len();
            self.attachment_paths.retain(|path| !paths.contains(path));
            let removed = before - self.attachment_paths.len();
            picker.visual_anchor = None;
            return Ok(Some(format!("removed {removed} attached file(s)")));
        }
        match key {
            KeyCode::Up | KeyCode::Char('k') => picker.move_selection(-(count as isize)),
            KeyCode::Down | KeyCode::Char('j') => picker.move_selection(count as isize),
            KeyCode::PageUp => picker.move_selection(-10),
            KeyCode::PageDown => picker.move_selection(10),
            KeyCode::Home => {
                picker.selected = 0;
                picker.refresh_preview();
            }
            KeyCode::End | KeyCode::Char('G') => {
                picker.selected = picker.entries.len().saturating_sub(1);
                picker.refresh_preview();
            }
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                if let Some(parent) = picker.directory.parent() {
                    picker.enter_directory(parent.to_path_buf())?;
                }
            }
            KeyCode::Char('~') => {
                if let Some(home) = dirs::home_dir() {
                    picker.enter_directory(home)?;
                }
            }
            KeyCode::Char('d') => {
                if let Some(home) = dirs::home_dir() {
                    let downloads = home.join("Downloads");
                    if downloads.is_dir() {
                        picker.enter_directory(downloads)?;
                    }
                }
            }
            KeyCode::Char('.') => {
                picker.show_hidden = !picker.show_hidden;
                picker.refresh()?;
            }
            KeyCode::Char('l') | KeyCode::Right if picker.visual_anchor.is_none() => {
                if let Some(entry) = picker.entries.get(picker.selected)
                    && entry.is_directory
                {
                    picker.enter_directory(entry.path.clone())?;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(anchor) = picker.visual_anchor {
                    let start = anchor.min(picker.selected);
                    let end = anchor.max(picker.selected);
                    // Directories are navigation-only, never attachment targets.
                    let paths: Vec<_> = picker
                        .entries
                        .iter()
                        .skip(start)
                        .take(end - start + 1)
                        .filter(|entry| !entry.is_directory)
                        .map(|entry| entry.path.clone())
                        .collect();
                    if paths.is_empty() {
                        picker.visual_anchor = None;
                        return Ok(Some("no files in visual selection".to_owned()));
                    }
                    // Space over an all-attached range detaches it. A mixed
                    // range attaches only missing files, without disturbing
                    // the files that are already attached.
                    if key == KeyCode::Char(' ')
                        && paths
                            .iter()
                            .all(|path| self.attachment_paths.contains(path))
                    {
                        self.attachment_paths.retain(|path| !paths.contains(path));
                        picker.visual_anchor = None;
                        return Ok(Some(format!(
                            "detached {} files from visual selection",
                            paths.len()
                        )));
                    }
                    // Validate *before* any mutation so one invalid file
                    // cannot leave a partially attached selection.
                    for path in &paths {
                        if !self.attachment_paths.contains(path) {
                            validate_picker_attachment(path)?;
                        }
                    }
                    let mut attached = 0;
                    for path in paths {
                        if !self.attachment_paths.contains(&path) {
                            self.attachment_paths.push(path);
                            attached += 1;
                        }
                    }
                    picker.visual_anchor = None;
                    return Ok(Some(format!(
                        "attached {attached} files from visual selection"
                    )));
                }
                if let Some(entry) = picker.entries.get(picker.selected).cloned() {
                    if entry.is_directory {
                        if key == KeyCode::Enter {
                            picker.enter_directory(entry.path)?;
                        } else {
                            let last = picker.entries.len().saturating_sub(1);
                            if picker.selected < last {
                                picker.move_selection(1);
                            }
                        }
                    } else {
                        validate_picker_attachment(&entry.path)?;
                        if let Some(index) = self
                            .attachment_paths
                            .iter()
                            .position(|existing| existing == &entry.path)
                        {
                            self.attachment_paths.remove(index);
                        } else {
                            self.attachment_paths.push(entry.path);
                        }
                        // Space is rapid multi-selection, Enter only toggles.
                        if key == KeyCode::Char(' ') {
                            let last = picker.entries.len().saturating_sub(1);
                            if picker.selected < last {
                                picker.move_selection(1);
                            }
                        }
                        return Ok(Some(format!(
                            "updated attachments: {}",
                            self.attachment_paths.len()
                        )));
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    #[must_use]
    pub fn field_value(&self) -> &str {
        match self.field {
            ComposeField::To => &self.to,
            ComposeField::Cc => &self.cc,
            ComposeField::Bcc => &self.bcc,
            ComposeField::Subject => &self.subject,
            ComposeField::Attachments => &self.attachments,
            ComposeField::Headers => &self.headers,
            ComposeField::Body => &self.body,
        }
    }

    pub(super) fn field_mut(&mut self) -> &mut String {
        match self.field {
            ComposeField::To => &mut self.to,
            ComposeField::Cc => &mut self.cc,
            ComposeField::Bcc => &mut self.bcc,
            ComposeField::Subject => &mut self.subject,
            ComposeField::Attachments => &mut self.attachments,
            ComposeField::Headers => &mut self.headers,
            ComposeField::Body => &mut self.body,
        }
    }

    pub(super) fn char_count(&self) -> usize {
        self.field_value().chars().count()
    }

    pub(super) fn switch_field(&mut self, field: ComposeField) {
        self.body_smooth_target = None;
        self.body_preferred_column = None;
        self.field = field;
        self.cursor = self.char_count();
        self.visual_anchor = None;
        if self.mode == ComposeMode::Visual {
            self.mode = ComposeMode::Normal;
        }
        self.visual_linewise = false;
        self.contact_suggestion = 0;
        self.dismissed_contact_query = None;
        self.clear_pending();
    }

    /// Normal-mode j/k on headers navigate *fields*, not characters/lines.
    /// Never leave the body using j/k: once there, motions are Vim motions.
    pub(super) fn move_header_field(&mut self, down: bool, count: usize) -> bool {
        if self.field == ComposeField::Body || self.mode == ComposeMode::Insert {
            return false;
        }
        for _ in 0..count.min(5) {
            let next = if down {
                self.field.next()
            } else {
                self.field.previous()
            };
            if !down && next == ComposeField::Body {
                break;
            }
            self.switch_field(next);
            if self.field == ComposeField::Body {
                break;
            }
        }
        true
    }

    pub(super) fn clear_pending(&mut self) {
        self.pending_g = false;
        self.pending_delete = false;
        self.pending_yank = false;
        self.pending_inner = false;
        self.pending_find = None;
        self.count = None;
    }

    pub(super) fn remember_edit(&mut self) {
        if self.undo_history.len() >= 200 {
            self.undo_history.remove(0);
        }
        self.undo_history
            .push((self.field, self.field_value().to_owned(), self.cursor));
        self.redo_history.clear();
    }

    pub(super) fn undo_edit(&mut self, redo: bool) {
        let previous = if redo {
            self.redo_history.pop()
        } else {
            self.undo_history.pop()
        };
        let Some((field, text, cursor)) = previous else {
            return;
        };
        let current = (self.field, self.field_value().to_owned(), self.cursor);
        if redo {
            self.undo_history.push(current);
        } else {
            self.redo_history.push(current);
        }
        self.field = field;
        *self.field_mut() = text;
        self.cursor = cursor.min(self.char_count());
        self.visual_anchor = None;
        self.visual_linewise = false;
        self.contact_suggestion = 0;
        self.dismissed_contact_query = None;
    }

    pub(super) fn byte_index(text: &str, char_index: usize) -> usize {
        text.char_indices()
            .nth(char_index)
            .map_or(text.len(), |(index, _)| index)
    }

    pub(super) fn insert_char(&mut self, ch: char) {
        self.body_smooth_target = None;
        self.body_preferred_column = None;
        let cursor = self.cursor.min(self.char_count());
        let byte = Self::byte_index(self.field_value(), cursor);
        self.remember_edit();
        self.field_mut().insert(byte, ch);
        self.cursor = cursor.saturating_add(1);
        self.contact_suggestion = 0;
        self.dismissed_contact_query = None;
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.cursor.min(self.char_count());
        let start = end - 1;
        self.delete_range(start, end);
        self.cursor = start;
    }

    pub(super) fn delete_at_cursor(&mut self) {
        let start = self.cursor.min(self.char_count());
        if start < self.char_count() {
            self.delete_range(start, start + 1);
        }
    }

    pub(super) fn delete_range(&mut self, start: usize, end: usize) {
        self.body_smooth_target = None;
        self.body_preferred_column = None;
        if start >= end {
            return;
        }
        let text = self.field_value();
        let start_byte = Self::byte_index(text, start);
        let end_byte = Self::byte_index(text, end);
        self.remember_edit();
        self.field_mut().replace_range(start_byte..end_byte, "");
        self.cursor = start.min(self.char_count());
        self.visual_anchor = None;
        self.contact_suggestion = 0;
        self.dismissed_contact_query = None;
    }

    pub(super) fn move_horizontal(&mut self, delta: isize) {
        self.body_smooth_target = None;
        self.body_preferred_column = None;
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.char_count());
    }

    pub(crate) fn body_rows(&self) -> Vec<ComposeBodyRow> {
        compose_body_rows(&self.body, self.body_view_width)
    }

    /// j/k navigate *displayed rows*, including soft wraps in forwarded mail.
    /// Retain the preferred display-cell column when traversing short rows.
    pub(super) fn move_line(&mut self, delta: isize) {
        if self.field != ComposeField::Body {
            return;
        }
        let rows = compose_body_rows(&self.body, self.body_view_width);
        let current = compose_body_cursor_row(&rows, self.cursor);
        let target = current.saturating_add_signed(delta).min(rows.len() - 1);
        if target == current {
            return;
        }
        let preferred = self
            .body_preferred_column
            .unwrap_or_else(|| compose_body_column(&self.body, rows[current].start, self.cursor));
        let row = rows[target];
        let mut cursor = row.start;
        let mut cells = 0usize;
        let text = self
            .body
            .chars()
            .skip(row.start)
            .take(row.end - row.start)
            .collect::<String>();
        for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(text.as_str(), true) {
            let width = unicode_width::UnicodeWidthStr::width(grapheme);
            if cells + width > preferred {
                break;
            }
            cells += width;
            cursor += grapheme.chars().count();
        }
        // A soft-wrap boundary belongs to the next row. Keep a vertical
        // motion *inside* its intended row, rather than accidentally jumping
        // forward another row (or oscillating during an animation).
        if rows
            .get(target + 1)
            .is_some_and(|next| next.start == row.end)
            && cursor == row.end
            && cursor > row.start
        {
            cursor -= 1;
        }
        self.cursor = cursor;
        self.body_preferred_column = Some(preferred);
    }

    pub(super) fn queue_body_smooth_scroll(&mut self, delta: isize) {
        if self.field != ComposeField::Body {
            return;
        }
        let rows = compose_body_rows(&self.body, self.body_view_width);
        let current = compose_body_cursor_row(&rows, self.cursor);
        let from = self.body_smooth_target.unwrap_or(current);
        let target = from.saturating_add_signed(delta).min(rows.len() - 1);
        self.body_smooth_target = (target != current).then_some(target);
    }

    pub(super) fn advance_body_smooth_scroll(&mut self) {
        let Some(target) = self.body_smooth_target else {
            return;
        };
        let rows = compose_body_rows(&self.body, self.body_view_width);
        let current = compose_body_cursor_row(&rows, self.cursor);
        if current == target {
            self.body_smooth_target = None;
            return;
        }
        self.move_line(if target > current { 1 } else { -1 });
        if compose_body_cursor_row(&rows, self.cursor) == target {
            self.body_smooth_target = None;
        }
    }

    pub(super) fn line_start(&self) -> usize {
        if self.field != ComposeField::Body {
            return 0;
        }
        let chars: Vec<char> = self.body.chars().collect();
        let mut start = self.cursor.min(chars.len());
        while start > 0 && chars[start - 1] != '\n' {
            start -= 1;
        }
        start
    }

    pub(super) fn line_end(&self) -> usize {
        if self.field != ComposeField::Body {
            return self.char_count();
        }
        let chars: Vec<char> = self.body.chars().collect();
        let mut end = self.cursor.min(chars.len());
        while end < chars.len() && chars[end] != '\n' {
            end += 1;
        }
        end
    }

    pub(super) fn open_line_below(&mut self) {
        if self.field != ComposeField::Body {
            return;
        }
        let end = self.line_end();
        let byte = Self::byte_index(&self.body, end);
        self.remember_edit();
        self.body.insert(byte, '\n');
        self.cursor = end.saturating_add(1);
        self.visual_anchor = None;
    }

    pub(super) fn open_line_above(&mut self) {
        if self.field != ComposeField::Body {
            return;
        }
        let start = self.line_start();
        let byte = Self::byte_index(&self.body, start);
        self.remember_edit();
        self.body.insert(byte, '\n');
        self.cursor = start;
        self.visual_anchor = None;
    }

    pub(super) fn move_word_kind(&mut self, backwards: bool, whole: bool) {
        let chars: Vec<char> = self.field_value().chars().collect();
        let belongs = |ch: char| {
            if whole {
                !ch.is_whitespace()
            } else {
                ch.is_alphanumeric() || ch == '_'
            }
        };
        if backwards {
            let mut i = self.cursor.min(chars.len());
            while i > 0 && !belongs(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && belongs(chars[i - 1]) {
                i -= 1;
            }
            self.cursor = i;
        } else {
            let mut i = self.cursor.min(chars.len());
            while i < chars.len() && belongs(chars[i]) {
                i += 1;
            }
            while i < chars.len() && !belongs(chars[i]) {
                i += 1;
            }
            self.cursor = i;
        }
    }

    pub(super) fn move_word_end(&mut self, whole: bool) {
        let chars: Vec<char> = self.field_value().chars().collect();
        if chars.is_empty() {
            self.cursor = 0;
            return;
        }
        let belongs = |ch: char| {
            if whole {
                !ch.is_whitespace()
            } else {
                ch.is_alphanumeric() || ch == '_'
            }
        };
        let mut i = self.cursor.min(chars.len().saturating_sub(1));
        if belongs(chars[i]) {
            i = (i + 1).min(chars.len().saturating_sub(1));
        }
        while i < chars.len().saturating_sub(1) && !belongs(chars[i]) {
            i += 1;
        }
        while i < chars.len().saturating_sub(1) && belongs(chars[i + 1]) {
            i += 1;
        }
        self.cursor = i;
    }

    pub(super) fn first_nonblank(&self) -> usize {
        let chars: Vec<char> = self.field_value().chars().collect();
        let mut i = self.line_start();
        while i < chars.len() && chars[i] != '\n' && matches!(chars[i], ' ' | '\t') {
            i += 1;
        }
        i
    }

    pub(super) fn move_paragraph(&mut self, forward: bool) {
        if self.field != ComposeField::Body {
            return;
        }
        let chars: Vec<char> = self.body.chars().collect();
        let mut i = self.cursor.min(chars.len());
        if forward {
            while i < chars.len() {
                if chars[i] == '\n' && chars.get(i + 1) == Some(&'\n') {
                    self.cursor = (i + 2).min(chars.len());
                    return;
                }
                i += 1;
            }
            self.cursor = chars.len();
        } else {
            i = i.saturating_sub(1);
            while i > 0 {
                if chars[i - 1] == '\n' && chars[i] == '\n' {
                    self.cursor = i + 1;
                    return;
                }
                i -= 1;
            }
            self.cursor = 0;
        }
    }

    pub(super) fn delete_current_lines(&mut self, count: usize) {
        if self.field != ComposeField::Body {
            self.delete_range(0, self.char_count());
            return;
        }
        let start = self.line_start();
        let chars: Vec<char> = self.body.chars().collect();
        let mut end = start;
        for _ in 0..count {
            while end < chars.len() && chars[end] != '\n' {
                end += 1;
            }
            if end < chars.len() {
                end += 1;
            }
        }
        // Last line without newline still counts as a line.
        if start == end && start > 0 {
            self.delete_range(start - 1, end);
        } else {
            self.delete_range(start, end);
        }
        self.cursor = start.min(self.char_count());
    }

    pub(super) fn clear_current_line(&mut self) {
        if self.field != ComposeField::Body {
            self.delete_range(0, self.char_count());
        } else {
            // Unlike dd, Ctrl-W leaves the newline in place.
            let start = self.line_start();
            let end = self.line_end();
            self.delete_range(start, end);
            self.cursor = start;
        }
        self.mode = ComposeMode::Insert;
        self.visual_anchor = None;
        self.clear_pending();
    }

    pub(super) fn delete_words(&mut self, count: usize) {
        let start = self.cursor;
        for _ in 0..count {
            self.move_word_kind(false, false);
        }
        self.delete_range(start, self.cursor);
        self.cursor = start.min(self.char_count());
    }

    pub(super) fn find_char(&mut self, ch: char, forward: bool, count: usize) {
        let chars: Vec<char> = self.field_value().chars().collect();
        let mut position = self.cursor.min(chars.len());
        let start = self.line_start();
        let end = self.line_end();
        for _ in 0..count {
            let match_index = if forward {
                (position.saturating_add(1)..end).find(|&index| chars[index] == ch)
            } else {
                (start..position).rev().find(|&index| chars[index] == ch)
            };
            let Some(index) = match_index else { break };
            position = index;
        }
        self.cursor = position;
    }

    pub(super) fn select_inner_object(&mut self, delimiter: char) {
        // Reuse the reader's tested matching algorithm: it understands nested
        // delimiters and paired quotes, unlike a first-open/first-close scan.
        if let Some((start, end)) =
            inner_text_object_range(self.field_value(), self.cursor, delimiter)
        {
            self.visual_anchor = Some(start);
            self.cursor = end;
            self.visual_linewise = false;
            self.mode = ComposeMode::Visual;
        }
    }

    pub(super) fn apply_motion(&mut self, code: KeyCode, count: usize) -> bool {
        self.body_smooth_target = None;
        if !matches!(code, KeyCode::Char('j' | 'k') | KeyCode::Up | KeyCode::Down) {
            self.body_preferred_column = None;
        }
        match code {
            KeyCode::Char('h') | KeyCode::Left => self.move_horizontal(-(count as isize)),
            KeyCode::Char('l') | KeyCode::Right => self.move_horizontal(count as isize),
            KeyCode::Char('j') | KeyCode::Down => {
                for _ in 0..count {
                    self.move_line(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                for _ in 0..count {
                    self.move_line(-1);
                }
            }
            KeyCode::Char('w') => {
                for _ in 0..count {
                    self.move_word_kind(false, false);
                }
            }
            KeyCode::Char('W') => {
                for _ in 0..count {
                    self.move_word_kind(false, true);
                }
            }
            KeyCode::Char('b') => {
                for _ in 0..count {
                    self.move_word_kind(true, false);
                }
            }
            KeyCode::Char('B') => {
                for _ in 0..count {
                    self.move_word_kind(true, true);
                }
            }
            KeyCode::Char('e') => {
                for _ in 0..count {
                    self.move_word_end(false);
                }
            }
            KeyCode::Char('E') => {
                for _ in 0..count {
                    self.move_word_end(true);
                }
            }
            KeyCode::Char('0') | KeyCode::Home | KeyCode::Char('H') => {
                self.cursor = self.line_start()
            }
            KeyCode::Char('^') => self.cursor = self.first_nonblank(),
            KeyCode::Char('$') | KeyCode::End | KeyCode::Char('L') => self.cursor = self.line_end(),
            KeyCode::Char('G') => self.cursor = self.char_count(),
            KeyCode::Char('{') => {
                for _ in 0..count {
                    self.move_paragraph(false);
                }
            }
            KeyCode::Char('}') => {
                for _ in 0..count {
                    self.move_paragraph(true);
                }
            }
            _ => return false,
        }
        true
    }

    pub fn compose_help_matches(&self) -> Vec<usize> {
        let query = self.help_query.trim().to_lowercase();
        crate::app::COMPOSE_HELP_ENTRIES
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                (query.is_empty()
                    || format!("{} {} {}", entry.category, entry.key, entry.description)
                        .to_lowercase()
                        .contains(&query))
                .then_some(index)
            })
            .collect()
    }

    pub fn visual_range(&self) -> Option<(usize, usize)> {
        let anchor = self.visual_anchor?;
        let cursor = self.cursor.min(self.char_count());
        // To/Cc/Bcc/Subject are single-line editors: V selects the entire
        // field, even when the caret is just past its final character.
        if self.visual_linewise && self.field != ComposeField::Body {
            return Some((0, self.char_count()));
        }
        if self.visual_linewise && self.field == ComposeField::Body {
            let chars: Vec<char> = self.body.chars().collect();
            let mut start = anchor.min(cursor);
            while start > 0 && chars[start - 1] != '\n' {
                start -= 1;
            }
            let mut end = anchor.max(cursor);
            while end < chars.len() && chars[end] != '\n' {
                end += 1;
            }
            if end < chars.len() {
                end += 1;
            }
            return Some((start, end));
        }
        Some(if anchor <= cursor {
            (anchor, cursor.saturating_add(1).min(self.char_count()))
        } else {
            (cursor, anchor.saturating_add(1).min(self.char_count()))
        })
    }

    /// Copy a Visual character range or entire lines, without touching the OS clipboard.
    pub(super) fn yank_visual(&self) -> Option<YankRegister> {
        let (start, end) = self.visual_range()?;
        let text = self.field_value();
        Some(YankRegister {
            text: text[Self::byte_index(text, start)..Self::byte_index(text, end)].to_owned(),
            linewise: self.visual_linewise && self.field == ComposeField::Body,
        })
    }

    pub(super) fn yank_lines(&self, count: usize) -> YankRegister {
        if self.field != ComposeField::Body {
            return YankRegister {
                text: self.field_value().to_owned(),
                linewise: false,
            };
        }
        let chars = self.body.chars().collect::<Vec<_>>();
        let start = self.line_start();
        let mut end = start;
        for _ in 0..count {
            while end < chars.len() && chars[end] != '\n' {
                end += 1;
            }
            if end < chars.len() {
                end += 1;
            }
        }
        YankRegister {
            text: self.body[Self::byte_index(&self.body, start)..Self::byte_index(&self.body, end)]
                .to_owned(),
            linewise: true,
        }
    }

    pub(super) fn yank_to_line_end(&self) -> YankRegister {
        let start = self.cursor.min(self.char_count());
        let end = self.line_end();
        let text = self.field_value();
        YankRegister {
            text: text[Self::byte_index(text, start)..Self::byte_index(text, end)].to_owned(),
            linewise: false,
        }
    }

    /// Replace a range as one undoable edit; all indices are Unicode scalar offsets.
    fn replace_with_text(&mut self, start: usize, end: usize, text: &str, cursor: usize) {
        let len = self.char_count();
        let start = start.min(len);
        let end = end.min(len).max(start);
        if start == end && text.is_empty() {
            return;
        }
        self.body_smooth_target = None;
        self.body_preferred_column = None;
        let first = Self::byte_index(self.field_value(), start);
        let last = Self::byte_index(self.field_value(), end);
        self.remember_edit();
        self.field_mut().replace_range(first..last, text);
        self.cursor = cursor.min(self.char_count());
        self.visual_anchor = None;
        self.visual_linewise = false;
        self.contact_suggestion = 0;
        self.dismissed_contact_query = None;
    }

    /// Vim's p/P: characterwise after/before cursor, linewise below/above line.
    pub(super) fn paste_register(&mut self, register: &YankRegister, after: bool, count: usize) {
        if register.text.is_empty() && !register.linewise {
            return;
        }
        let count = count.clamp(1, MAX_COUNT_PREFIX);
        if register.linewise && self.field == ComposeField::Body {
            let content = register.text.strip_suffix('\n').unwrap_or(&register.text);
            // Repeating a linewise register repeats complete logical lines.
            let repeated = std::iter::repeat_n(content, count).collect::<Vec<_>>().join("\n");
            if after {
                let end = self.line_end();
                let has_newline = self.body.chars().nth(end) == Some('\n');
                if self.body.is_empty() {
                    self.replace_with_text(0, 0, &repeated, 0);
                } else if has_newline {
                    let insert = format!("{repeated}\n");
                    self.replace_with_text(end + 1, end + 1, &insert, end + 1);
                } else {
                    let insert = format!("\n{repeated}");
                    self.replace_with_text(end, end, &insert, end + 1);
                }
            } else {
                let start = self.line_start();
                let insert = if self.body.is_empty() {
                    repeated
                } else {
                    format!("{repeated}\n")
                };
                self.replace_with_text(start, start, &insert, start);
            }
        } else {
            let text = if self.field == ComposeField::Body {
                register.text.repeat(count)
            } else {
                register.text.replace(['\r', '\n'], " ").repeat(count)
            };
            let position = self.cursor.min(self.char_count());
            let at = if after && position < self.char_count() {
                position + 1
            } else {
                position
            };
            let last_char = at + text.chars().count().saturating_sub(1);
            self.replace_with_text(at, at, &text, last_char);
        }
    }

    /// Visual p replaces the selected text without overwriting the yank register.
    pub(super) fn paste_over_visual(&mut self, register: &YankRegister) {
        let Some((start, end)) = self.visual_range() else {
            return;
        };
        let mut replacement = if self.field == ComposeField::Body {
            register.text.clone()
        } else {
            register.text.replace(['\r', '\n'], " ")
        };
        if self.visual_linewise
            && self.field == ComposeField::Body
            && self.body.chars().nth(end.saturating_sub(1)) == Some('\n')
            && !replacement.ends_with('\n')
        {
            replacement.push('\n');
        }
        self.replace_with_text(start, end, &replacement, start);
        self.mode = ComposeMode::Normal;
    }
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}
