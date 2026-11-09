//! Search help actions behavior.

use super::*;

impl App {
    pub(super) fn begin_search(&mut self) {
        if self.message_open {
            return;
        }
        self.visual_anchor = None;
        let query = {
            let Some(account) = self.account_mut() else {
                return;
            };
            account.search_editing = true;
            account.smooth_message_target = None;
            account.normalize_search_selection();
            account.search_query.clone()
        };
        self.status = StatusLine::info(format!("/{query}"));
    }

    pub(super) fn handle_search_key(&mut self, key: KeyEvent) {
        let mut cleared = false;
        let status = {
            let Some(account) = self.account_mut() else {
                return;
            };
            match key.code {
                KeyCode::Esc => {
                    account.search_query.clear();
                    account.search_editing = false;
                    account.normalize_search_selection();
                    cleared = true;
                }
                KeyCode::Enter => {
                    account.search_editing = false;
                    account.normalize_search_selection();
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    account.search_query.clear();
                    account.normalize_search_selection();
                }
                KeyCode::Backspace if key.modifiers.is_empty() => {
                    account.search_query.pop();
                    account.normalize_search_selection();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    account.search_query.push(ch);
                    account.normalize_search_selection();
                }
                _ => return,
            }
            let count = account.visible_message_indices().len();
            // Search is progressive: pull older pages only until there are enough
            // visible matches to fill the current viewport (or the mailbox ends).
            // This searches beyond the first page without eagerly downloading all
            // of a huge All Mail folder for a broad query.
            if !account.search_query.is_empty()
                && count < account.message_view_height
                && account.has_more
                && account.page_inflight.is_none()
            {
                let _ = account.request_next_page();
            }
            format!(
                "/{} • {count} match{}{}",
                account.search_query,
                if count == 1 { "" } else { "es" },
                if account.has_more {
                    " • search continues into older pages"
                } else {
                    ""
                }
            )
        };
        self.status = if cleared {
            StatusLine::temporary("filter cleared", Duration::from_secs(1))
        } else {
            StatusLine::info(status)
        };
    }

    pub(super) fn open_theme_picker(&mut self) {
        self.theme_before_picker = self.theme_index;
        self.theme_picker_visible = true;
        self.theme_query.clear();
        self.theme_selected = self.theme_index;
        self.theme_scroll = 0;
    }

    pub(super) fn open_help(&mut self) {
        self.help_visible = true;
        self.help_query.clear();
        self.help_selected = 0;
        self.help_scroll = 0;
    }

    #[must_use]
    pub fn help_matches(&self) -> Vec<usize> {
        let entries = if self.attachment_picker_visible {
            ATTACHMENT_HELP_ENTRIES
        } else {
            HELP_ENTRIES
        };
        ranked_help_matches(entries, &self.help_query)
    }

    #[must_use]
    pub fn theme_matches(&self) -> Vec<usize> {
        let query = self.theme_query.trim().to_ascii_lowercase();
        self.themes
            .iter()
            .enumerate()
            .filter(|(_, theme)| {
                query.is_empty() || theme.name.to_ascii_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub(super) fn handle_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') if key.modifiers.is_empty() => {
                self.help_visible = false;
                // Kitty graphics can be erased by a modal's Clear even when the
                // attachment selection and geometry have not changed.
                self.invalidate_attachment_preview_layout();
            }
            KeyCode::Down if key.modifiers.is_empty() => {
                self.move_help_selection(1);
            }
            KeyCode::Up if key.modifiers.is_empty() => {
                self.move_help_selection(-1);
            }
            KeyCode::Home if key.modifiers.is_empty() => {
                if let Some(first) = self.help_matches().first().copied() {
                    self.help_selected = first;
                    self.help_scroll = 0;
                }
            }
            KeyCode::End if key.modifiers.is_empty() => {
                let matches = self.help_matches();
                if let Some(last) = matches.last().copied() {
                    self.help_selected = last;
                    self.help_scroll = matches.len().saturating_sub(7);
                }
            }
            KeyCode::Backspace if key.modifiers.is_empty() => {
                self.help_query.pop();
                self.reset_help_selection();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.help_query.push(ch);
                self.reset_help_selection();
            }
            _ => {}
        }
    }

    pub(super) fn handle_theme_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.cancel_theme_picker(),
            KeyCode::Enter => self.commit_theme_picker(),
            KeyCode::Down if key.modifiers.is_empty() => {
                self.move_theme_selection(1);
            }
            KeyCode::Up if key.modifiers.is_empty() => {
                self.move_theme_selection(-1);
            }
            KeyCode::Home if key.modifiers.is_empty() => {
                if let Some(first) = self.theme_matches().first().copied() {
                    self.theme_selected = first;
                    self.theme_index = first;
                    self.theme_scroll = 0;
                }
            }
            KeyCode::End if key.modifiers.is_empty() => {
                let matches = self.theme_matches();
                if let Some(last) = matches.last().copied() {
                    self.theme_selected = last;
                    self.theme_index = last;
                    self.theme_scroll = matches.len().saturating_sub(7);
                }
            }
            KeyCode::Backspace if key.modifiers.is_empty() => {
                self.theme_query.pop();
                self.reset_theme_selection();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.theme_query.push(ch);
                self.reset_theme_selection();
            }
            _ => {}
        }
    }

    pub(super) fn reset_help_selection(&mut self) {
        let matches = self.help_matches();
        self.help_selected = matches.first().copied().unwrap_or(0);
        self.help_scroll = 0;
    }

    pub(super) fn move_help_selection(&mut self, delta: isize) {
        let matches = self.help_matches();
        if matches.is_empty() {
            return;
        }
        let position = matches
            .iter()
            .position(|index| *index == self.help_selected)
            .unwrap_or(0);
        let next = offset_index(position, matches.len(), delta);
        self.help_selected = matches[next];
        self.help_scroll = next.saturating_sub(6);
    }

    pub(super) fn reset_theme_selection(&mut self) {
        let matches = self.theme_matches();
        if let Some(index) = matches.first().copied() {
            self.theme_selected = index;
            self.theme_index = index;
        }
        self.theme_scroll = 0;
    }

    pub(super) fn move_theme_selection(&mut self, delta: isize) {
        let matches = self.theme_matches();
        if matches.is_empty() {
            return;
        }
        let position = matches
            .iter()
            .position(|index| *index == self.theme_selected)
            .unwrap_or(0);
        let next = offset_index(position, matches.len(), delta);
        self.theme_selected = matches[next];
        self.theme_index = self.theme_selected;
        self.theme_scroll = next.saturating_sub(4);
    }

    pub(super) fn commit_theme_picker(&mut self) {
        self.theme_picker_visible = false;
        self.theme_query.clear();
        let id = self.theme().id.clone();
        if let Some(path) = self.theme_config_path.as_deref() {
            if let Err(error) = crate::config::save_ui_theme(path, &id) {
                self.status = StatusLine::error(format!("could not save [ui] theme: {error:#}"));
            } else {
                self.status =
                    StatusLine::temporary("theme saved in config.toml", Duration::from_secs(2));
            }
        } else {
            self.status = StatusLine::temporary(
                "theme changed for this session (set [ui] theme in config.toml to persist)",
                Duration::from_secs(4),
            );
        }
    }

    pub(super) fn cancel_theme_picker(&mut self) {
        self.theme_index = self.theme_before_picker;
        self.theme_selected = self.theme_before_picker;
        self.theme_query.clear();
        self.theme_picker_visible = false;
    }
}
