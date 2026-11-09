//! Attachment actions behavior.

use super::*;

impl App {
    pub(super) fn reset_received_attachment_previews(&mut self) {
        self.attachment_preview = None;
        self.attachment_preview_identity = None;
        self.attachment_preview_due = None;
        self.attachment_preview_inflight.clear();
        self.attachment_preview_cache.clear();
        // Detach any outstanding worker of an earlier email: its Arc is no
        // longer visible to this popup, even if the new email uses index 0.
        self.attachment_preview_results = Arc::new(Mutex::new(VecDeque::new()));
    }

    pub(super) fn open_attachment_picker(&mut self) {
        let Some(account) = self.account() else {
            return;
        };
        let Some(message) = account.selected_message() else {
            return;
        };
        let Some(mailbox) = account.current_mailbox.clone() else {
            return;
        };
        let id = message.id.clone();
        let validity = account.uid_validities.get(&mailbox).copied();
        let source = (self.account_index, mailbox.clone(), validity, id.clone());
        let cached = if account.unverified_mailboxes.contains(&mailbox) {
            None
        } else {
            self.attachment_cache
                .iter()
                .find(|(key, _)| key == &source)
                .map(|(_, files)| Arc::clone(files))
        };
        crate::perf::record(
            if cached.is_some() {
                "ui_attachment_cache_hit"
            } else {
                "ui_attachment_cache_miss"
            },
            None,
            Duration::ZERO,
            true,
        );
        if cached.is_none()
            && account
                .worker
                .post(ActionKind::FetchAttachments {
                    mailbox,
                    id: id.clone(),
                })
                .is_err()
        {
            self.status = StatusLine::error("mail worker busy; cannot load attachments");
            return;
        }
        self.attachment_picker_visible = true;
        self.attachment_selection = 0;
        self.attachment_files = cached.clone().unwrap_or_else(|| Arc::new(Vec::new()));
        self.attachment_marked.clear();
        self.attachment_visual_anchor = None;
        self.attachment_save_directory = None;
        self.attachment_destination_picker = None;
        self.attachment_new_folder = None;
        self.attachment_pending_count = 0;
        self.attachment_pending_g = false;
        self.attachment_smooth_target = None;
        self.attachment_source = Some(source);
        self.reset_received_attachment_previews();
        self.attachment_pending = if cached.is_some() { None } else { Some(id) };
        if cached.is_some() {
            self.queue_attachment_preview();
            self.status = StatusLine::info("attachments restored from memory");
        } else {
            self.status = StatusLine::info("loading attachments…");
        }
    }

    pub(super) fn dismiss_attachment_picker(&mut self) {
        self.help_visible = false;
        self.attachment_picker_visible = false;
        self.attachment_pending = None;
        self.attachment_files = Arc::new(Vec::new());
        self.attachment_source = None;
        self.attachment_marked.clear();
        self.attachment_visual_anchor = None;
        self.attachment_save_directory = None;
        self.attachment_destination_picker = None;
        self.attachment_new_folder = None;
        self.attachment_smooth_target = None;
        self.reset_received_attachment_previews();
    }

    pub(super) fn queue_attachment_preview(&mut self) {
        self.attachment_preview_due = None;
        if self.attachment_smooth_target.is_some() || self.attachment_destination_picker.is_some() {
            return;
        }
        let index = self.attachment_selection;
        let Some(file) = self.attachment_files.get(index) else {
            return;
        };
        if !received_attachment_is_image(file) && !received_attachment_is_pdf(file) {
            return;
        }
        if self
            .attachment_preview_cache
            .iter()
            .any(|(cached, _)| *cached == index)
            || self.attachment_preview_inflight.contains(&index)
        {
            return;
        }
        // A short debounce avoids launching Poppler for every intermediate
        // attachment while navigating rapidly. Images use a shorter delay.
        let delay = if received_attachment_is_pdf(file) {
            95
        } else {
            25
        };
        self.attachment_preview_due = Some((index, Instant::now() + Duration::from_millis(delay)));
    }

    pub(super) fn advance_attachment_preview(&mut self) {
        let completed = if let Ok(mut results) = self.attachment_preview_results.lock() {
            results.drain(..).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut selected_completed = false;
        for (index, result) in completed {
            self.attachment_preview_inflight.remove(&index);
            self.attachment_preview_cache
                .retain(|(cached, _)| *cached != index);
            self.attachment_preview_cache.push_back((index, result));
            while self.attachment_preview_cache.len() > 8 {
                self.attachment_preview_cache.pop_front();
            }
            selected_completed |= index == self.attachment_selection;
        }
        if selected_completed {
            self.attachment_preview_identity = None;
        }
        if self.attachment_smooth_target.is_some() || self.attachment_destination_picker.is_some() {
            return;
        }
        let Some((index, due)) = self.attachment_preview_due else {
            return;
        };
        if Instant::now() < due || !self.attachment_preview_inflight.is_empty() {
            return;
        }
        self.attachment_preview_due = None;
        if self.attachment_files.get(index).is_none() {
            return;
        }
        // The MIME buffers are shared with the worker, not copied per image.
        let files = Arc::clone(&self.attachment_files);
        let queue = Arc::clone(&self.attachment_preview_results);
        match std::thread::Builder::new()
            .name("jerc-attachment-preview".into())
            .spawn(move || {
                let start = Instant::now();
                let result = decode_received_attachment_preview(&files[index]);
                crate::perf::record(
                    "attachment_preview_decode",
                    None,
                    start.elapsed(),
                    result.is_ok(),
                );
                if let Ok(mut pending) = queue.lock() {
                    pending.push_back((index, result));
                }
            }) {
            Ok(_handle) => {
                self.attachment_preview_inflight.insert(index);
            }
            Err(error) => {
                self.attachment_preview_cache
                    .push_back((index, Err(format!("preview worker: {error}"))));
                self.attachment_preview_identity = None;
            }
        }
    }

    pub(super) fn move_attachment_selection(&mut self, new_selection: usize) {
        if self.attachment_files.is_empty() {
            return;
        }
        let next = new_selection.min(self.attachment_files.len() - 1);
        if next != self.attachment_selection {
            self.attachment_selection = next;
            self.attachment_preview = None;
            self.attachment_preview_identity = None;
            self.queue_attachment_preview();
        }
    }

    pub(super) fn handle_attachment_destination_key(&mut self, key: KeyEvent) {
        // The destination explorer owns its own searchable help. In particular,
        // pressing Esc here must close help rather than cancel the save dialog.
        if let Some(picker) = self.attachment_destination_picker.as_mut()
            && picker.help_visible
        {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {
                    picker.help_visible = false;
                    picker.help_query.clear();
                }
                KeyCode::Backspace => {
                    picker.help_query.pop();
                    picker.help_selected = ranked_help_matches(
                        DESTINATION_HELP_ENTRIES,
                        &picker.help_query,
                    )
                    .first()
                    .copied()
                    .unwrap_or(0);
                    picker.help_scroll = 0;
                }
                KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End
                    if key.modifiers.is_empty() =>
                {
                    let matches = ranked_help_matches(
                        DESTINATION_HELP_ENTRIES,
                        &picker.help_query,
                    );
                    if let Some(position) = matches.iter().position(|&i| i == picker.help_selected)
                    {
                        let next = match key.code {
                            KeyCode::Up => position.saturating_sub(1),
                            KeyCode::Down => (position + 1).min(matches.len() - 1),
                            KeyCode::Home => 0,
                            KeyCode::End => matches.len() - 1,
                            _ => position,
                        };
                        picker.help_selected = matches[next];
                        picker.help_scroll = next.saturating_sub(5);
                    } else if let Some(&first) = matches.first() {
                        picker.help_selected = first;
                        picker.help_scroll = 0;
                    }
                }
                KeyCode::Char(ch)
                    if !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                {
                    picker.help_query.push(ch);
                    picker.help_selected = ranked_help_matches(
                        DESTINATION_HELP_ENTRIES,
                        &picker.help_query,
                    )
                    .first()
                    .copied()
                    .unwrap_or(0);
                    picker.help_scroll = 0;
                }
                _ => {}
            }
            return;
        }
        if self.attachment_new_folder.is_none()
            && key.code == KeyCode::Char('?')
            && !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
        {
            if let Some(picker) = self.attachment_destination_picker.as_mut() {
                picker.help_visible = true;
                picker.help_query.clear();
                picker.help_selected = 0;
                picker.help_scroll = 0;
            }
            return;
        }
        if self.attachment_new_folder.is_some() {
            match key.code {
                KeyCode::Esc => self.attachment_new_folder = None,
                KeyCode::Backspace => {
                    if let Some(name) = &mut self.attachment_new_folder {
                        name.pop();
                    }
                }
                KeyCode::Enter => {
                    let name = self.attachment_new_folder.take().unwrap_or_default();
                    if name.is_empty()
                        || name == "."
                        || name == ".."
                        || name.contains('/')
                        || name.contains('\\')
                        || name.chars().any(char::is_control)
                    {
                        self.status = StatusLine::error("enter a single safe folder name");
                    } else if let Some(picker) = &mut self.attachment_destination_picker {
                        let path = picker.directory.join(name);
                        let result = (|| -> anyhow::Result<()> {
                            std::fs::create_dir(&path)?;
                            // Keep navigation consistent and refresh the current directory.
                            picker.enter_directory(path)
                        })();
                        if let Err(error) = result {
                            self.status =
                                StatusLine::error(format!("could not create folder: {error}"));
                        }
                    }
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if let Some(name) = &mut self.attachment_new_folder {
                        name.push(ch);
                    }
                }
                _ => {}
            }
            return;
        }
        // Modal-wide actions must run before borrowing the picker mutably.
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.attachment_destination_picker = None;
                self.queue_attachment_preview();
                return;
            }
            KeyCode::Char('s') => {
                self.save_selected_attachments();
                return;
            }
            KeyCode::Char('n') => {
                self.attachment_new_folder = Some(String::new());
                return;
            }
            _ => {}
        }
        let Some(picker) = self.attachment_destination_picker.as_mut() else {
            return;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('j' | 'k'))
        {
            let delta = if key.code == KeyCode::Char('j') {
                SMOOTH_SCROLL_ROWS
            } else {
                -SMOOTH_SCROLL_ROWS
            };
            picker.queue_smooth_scroll(delta);
            return;
        }
        if picker.pending_g {
            picker.pending_g = false;
            if key.code == KeyCode::Char('g') {
                picker.selected = 0;
                picker.pending_count = 0;
                picker.refresh_preview();
                return;
            }
        }
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                picker.pending_count = picker
                    .pending_count
                    .saturating_mul(10)
                    .saturating_add(d.to_digit(10).unwrap_or(1) as usize)
                    .min(99_999);
                return;
            }
            KeyCode::Char('0') if picker.pending_count > 0 => {
                picker.pending_count = picker.pending_count.saturating_mul(10).min(99_999);
                return;
            }
            KeyCode::Char('g') => {
                picker.pending_g = true;
                return;
            }
            _ => {}
        }
        let count = picker.pending_count.max(1);
        let explicit = picker.pending_count > 0;
        picker.pending_count = 0;
        let result = match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                picker.move_selection(count as isize);
                Ok(())
            }
            KeyCode::Char('k') | KeyCode::Up => {
                picker.move_selection(-(count as isize));
                Ok(())
            }
            KeyCode::PageDown => {
                picker.move_selection(12);
                Ok(())
            }
            KeyCode::PageUp => {
                picker.move_selection(-12);
                Ok(())
            }
            KeyCode::Char('G') | KeyCode::End => {
                picker.selected = if explicit {
                    count
                        .saturating_sub(1)
                        .min(picker.entries.len().saturating_sub(1))
                } else {
                    picker.entries.len().saturating_sub(1)
                };
                picker.refresh_preview();
                Ok(())
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                if let Some(parent) = picker.directory.parent().map(Path::to_path_buf) {
                    picker.enter_directory(parent)
                } else {
                    Ok(())
                }
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                if let Some(entry) = picker.entries.get(picker.selected) {
                    if entry.is_directory {
                        picker.enter_directory(entry.path.clone())
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }
            KeyCode::Char('~') => {
                if let Some(home) = dirs::home_dir() {
                    picker.enter_directory(home)
                } else {
                    Ok(())
                }
            }
            KeyCode::Char('.') => {
                picker.show_hidden = !picker.show_hidden;
                picker.refresh()
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.status = StatusLine::error(format!("file browser: {error}"));
        }
    }

    pub(super) fn handle_attachment_picker_key(&mut self, key: KeyEvent) {
        if self.help_visible {
            self.handle_help_key(key);
            return;
        }
        if self.attachment_destination_picker.is_none()
            && key.code == KeyCode::Char('?')
            && !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
        {
            self.open_help();
            return;
        }
        if self.attachment_destination_picker.is_some() {
            self.handle_attachment_destination_key(key);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('j' | 'k'))
        {
            let delta = if key.code == KeyCode::Char('j') {
                SMOOTH_SCROLL_ROWS
            } else {
                -SMOOTH_SCROLL_ROWS
            };
            let target = self
                .attachment_selection
                .saturating_add_signed(delta)
                .min(self.attachment_files.len().saturating_sub(1));
            self.attachment_smooth_target = (target != self.attachment_selection).then_some(target);
            self.attachment_preview_due = None;
            return;
        }
        if self.attachment_pending_g {
            self.attachment_pending_g = false;
            if key.code == KeyCode::Char('g') {
                self.attachment_smooth_target = None;
                self.move_attachment_selection(0);
                self.attachment_pending_count = 0;
                return;
            }
        }
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                self.attachment_pending_count = self
                    .attachment_pending_count
                    .saturating_mul(10)
                    .saturating_add(d.to_digit(10).unwrap_or(1) as usize)
                    .min(99_999);
                return;
            }
            KeyCode::Char('0') if self.attachment_pending_count > 0 => {
                self.attachment_pending_count =
                    self.attachment_pending_count.saturating_mul(10).min(99_999);
                return;
            }
            KeyCode::Char('g') => {
                self.attachment_pending_g = true;
                return;
            }
            _ => {}
        }
        let count = self.attachment_pending_count.max(1);
        let explicit = self.attachment_pending_count > 0;
        self.attachment_pending_count = 0;
        self.attachment_smooth_target = None;
        match key.code {
            KeyCode::Esc
                if self.attachment_visual_anchor.is_some()
                    || !self.attachment_marked.is_empty() =>
            {
                self.attachment_visual_anchor = None;
                self.attachment_marked.clear();
            }
            KeyCode::Esc | KeyCode::Char('q') => self.dismiss_attachment_picker(),
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_attachment_selection(self.attachment_selection.saturating_add(count))
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_attachment_selection(self.attachment_selection.saturating_sub(count))
            }
            KeyCode::Char('G') => self.move_attachment_selection(if explicit {
                count.saturating_sub(1)
            } else {
                self.attachment_files.len().saturating_sub(1)
            }),
            KeyCode::PageDown => {
                self.move_attachment_selection(self.attachment_selection.saturating_add(12))
            }
            KeyCode::PageUp => {
                self.move_attachment_selection(self.attachment_selection.saturating_sub(12))
            }
            KeyCode::Char('v') | KeyCode::Char('V') => {
                if self.attachment_visual_anchor.is_some() {
                    self.attachment_visual_anchor = None;
                } else if !self.attachment_files.is_empty() {
                    self.attachment_visual_anchor = Some(self.attachment_selection);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(anchor) = self.attachment_visual_anchor.take() {
                    let first = anchor.min(self.attachment_selection);
                    let last = anchor.max(self.attachment_selection).min(self.attachment_files.len().saturating_sub(1));
                    if !self.attachment_files.is_empty() {
                        let indices = (first..=last).collect::<Vec<_>>();
                        crate::app::vim::toggle_group(&mut self.attachment_marked, &indices);
                    }
                } else if !self.attachment_files.is_empty() {
                    let index = self.attachment_selection;
                    crate::app::vim::toggle_group(&mut self.attachment_marked, &[index]);
                    self.move_attachment_selection(index + 1);
                }
            }
            KeyCode::Char('a') => {
                if self.attachment_marked.len() == self.attachment_files.len() {
                    self.attachment_marked.clear();
                } else {
                    self.attachment_marked = (0..self.attachment_files.len()).collect();
                }
            }
            KeyCode::Char('p') => self.attachment_preview_identity = None,
            KeyCode::Enter | KeyCode::Char('o') => self.open_attachment(),
            KeyCode::Char('s') => self.prompt_attachment_save(),
            _ => {}
        }
    }

    /// Invalidate the terminal-side protocol, not the decoded thumbnail cache.
    /// Resize events and closing `?` can clear Kitty graphics independently
    /// of their selection/size identity. The next draw re-uploads the image.
    pub fn invalidate_attachment_preview_layout(&mut self) {
        if self.attachment_picker_visible {
            self.attachment_preview_identity = None;
            self.attachment_preview = None;
        }
    }

    /// Create terminal graphics only from decoded, cached thumbnails. Decoding
    /// images and spawning pdftoppm happen on a worker, never on the 100 FPS
    /// event/render thread.
    pub fn ensure_attachment_preview(&mut self, width: u16, height: u16) {
        if !self.attachment_picker_visible || width == 0 || height == 0 {
            return;
        }
        let identity = (self.attachment_selection, width, height);
        if self.attachment_preview_identity == Some(identity) {
            return;
        }
        self.attachment_preview_identity = Some(identity);
        self.attachment_preview = None;
        let Some((_, Ok(image))) = self
            .attachment_preview_cache
            .iter()
            .find(|(cached, _)| *cached == self.attachment_selection)
        else {
            return;
        };
        let Some(picker) = &self.image_picker else {
            return;
        };
        let target = reader_image_target_size(
            (image.width(), image.height()),
            (None, None),
            (picker.font_size().width, picker.font_size().height),
            (width, height),
        );
        self.attachment_preview = picker
            .new_protocol(
                image.clone(),
                target,
                Resize::Fit(Some(image::imageops::FilterType::Lanczos3)),
            )
            .ok();
    }

    pub fn attachment_preview_decoded(&self) -> bool {
        self.attachment_preview_cache
            .iter()
            .any(|(index, result)| *index == self.attachment_selection && result.is_ok())
    }

    pub fn attachment_preview_error(&self) -> Option<&str> {
        self.attachment_preview_cache
            .iter()
            .find(|(index, _)| *index == self.attachment_selection)
            .and_then(|(_, result)| result.as_ref().err().map(String::as_str))
    }

    /// All rows the attachment picker currently displays as selected.
    /// Visual selections are actionable without an extra Space-to-mark step.
    pub(crate) fn selected_attachment_indices(&self) -> Vec<usize> {
        let all = (0..self.attachment_files.len()).collect::<Vec<_>>();
        let mut indices = crate::app::vim::selected(
            &all, &all, self.attachment_selection,
            self.attachment_visual_anchor, &self.attachment_marked,
        );
        indices.sort_unstable();
        indices
    }

    pub(super) fn prompt_attachment_save(&mut self) {
        if self.attachment_files.is_empty() {
            return;
        }
        let directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        match ComposeFilePicker::open_directory_only(directory) {
            Ok(picker) => {
                self.attachment_preview_due = None;
                self.attachment_destination_picker = Some(picker);
                self.attachment_new_folder = None;
            }
            Err(error) => {
                self.status = StatusLine::error(format!("cannot open destination picker: {error}"))
            }
        }
    }

    pub(super) fn save_selected_attachments(&mut self) {
        let Some(picker) = &self.attachment_destination_picker else {
            return;
        };
        let directory = picker.directory.clone();
        if !directory.is_dir() {
            self.status = StatusLine::error(format!(
                "not an existing directory: {}",
                directory.display()
            ));
            return;
        }
        let indices = self.selected_attachment_indices();
        let mut saved = 0;
        let mut first_error = None;
        for index in indices {
            if let Some(file) = self.attachment_files.get(index) {
                match save_attachment_without_overwrite(&directory, file, index) {
                    Ok(_) => saved += 1,
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error.to_string());
                        }
                    }
                }
            }
        }
        self.attachment_destination_picker = None;
        if let Some(error) = first_error {
            self.status = StatusLine::error(format!("saved {saved} file(s), error: {error}"));
        } else {
            self.status = StatusLine::temporary(
                format!("saved {saved} attachment(s) to {}", directory.display()),
                Duration::from_secs(5),
            );
            self.attachment_marked.clear();
            self.attachment_visual_anchor = None;
        }
        self.queue_attachment_preview();
    }

    pub fn attachment_pending_loading(&self) -> bool {
        self.attachment_pending.is_some()
    }
}
