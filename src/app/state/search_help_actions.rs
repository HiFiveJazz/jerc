//! Search help actions behavior.

use super::*;

impl App {
    pub(super) fn begin_search(&mut self) {
        if self.message_open {
            return;
        }
        if self.account().is_some_and(|a| a.native_view.is_some()) {
            self.exit_native_view();
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
        if self.account().is_some_and(|a| a.native_search_editing) {
            self.handle_native_search_key(key);
            return;
        }
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
                && !account
                    .search_query
                    .trim()
                    .to_ascii_lowercase()
                    .starts_with("body:")
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
        let entries = if self.attachment_picker_visible {
            ATTACHMENT_HELP_ENTRIES
        } else {
            HELP_ENTRIES
        };
        if crate::app::help_palette::handle_help_key(
            key,
            entries,
            &mut self.help_query,
            &mut self.help_selected,
            &mut self.help_scroll,
            crate::app::help_palette::HelpKeys::GLOBAL,
        ) {
            self.help_visible = false;
            // The modal Clear can erase otherwise unchanged Kitty graphics.
            self.invalidate_attachment_preview_layout();
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
        if let Some((index, position)) =
            crate::app::navigation::filtered_move(&matches, self.theme_selected, delta, true)
        {
            self.theme_selected = index;
            self.theme_index = index;
            self.theme_scroll = position.saturating_sub(4);
        }
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

impl App {
    fn native_view_available(&mut self) -> bool {
        if self.message_open {
            return false;
        }
        if self.account().is_some_and(|a| {
            a.capabilities.is_some_and(|c| c.server_search)
                && !a.connecting
                && a.current_mailbox
                    .as_ref()
                    .is_some_and(|name| a.cache_identities.contains_key(name))
                && !a.cached_mailbox_unverified()
        }) {
            true
        } else {
            self.status = StatusLine::info(
                "Server search requires a connected IMAP or JMAP mailbox; use / for cached search",
            );
            false
        }
    }

    pub(super) fn begin_native_search(&mut self) {
        if !self.native_view_available() {
            return;
        }
        self.visual_anchor = None;
        if let Some(account) = self.account_mut() {
            account.search_query.clear();
            account.search_editing = true;
            account.native_search_editing = true;
            account.smooth_message_target = None;
        }
        self.status = StatusLine::info("Server search: text, from:, subject:, is:unread");
    }

    fn handle_native_search_key(&mut self, key: KeyEvent) {
        let Some(account) = self.account_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                account.search_editing = false;
                account.native_search_editing = false;
                account.search_query.clear();
            }
            KeyCode::Enter => {
                let query = std::mem::take(&mut account.search_query);
                account.search_editing = false;
                account.native_search_editing = false;
                if !query.trim().is_empty() {
                    self.enter_native_view(crate::backend::MailboxView::Search(query));
                }
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                account.search_query.clear()
            }
            KeyCode::Backspace => {
                account.search_query.pop();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && account.search_query.len() < 4096 =>
            {
                account.search_query.push(ch);
            }
            _ => {}
        }
    }

    fn thread_server_available(&self) -> bool {
        !self.message_open
            && self.account().is_some_and(|a| {
                a.capabilities.is_some()
                    && !a.connecting
                    && a.current_mailbox.as_ref().is_some_and(|name| {
                        matches!(
                            a.cache_identities.get(name),
                            Some(CacheIdentity::Imap(_) | CacheIdentity::Jmap { .. })
                        )
                    })
                    && !a.cached_mailbox_unverified()
            })
    }

    pub(super) fn toggle_native_threads(&mut self) {
        if self.message_open {
            return;
        }
        if self.account().is_some_and(|a| a.native_view.is_some()) {
            self.exit_native_view();
        }
        self.visual_anchor = None;
        self.message_marked.clear();
        let preview = self.preview_split;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.threading_enabled = !account.threading_enabled;
        account.local_thread_view = None;
        account.expanded_threads.clear();
        account.inline_thread_requests.clear();
        account.smooth_message_target = None;
        account.last_prefetch_anchor = None;
        account.normalize_search_selection();
        if preview {
            account.request_selected_body();
        }
        self.status = StatusLine::info(if account.threading_enabled {
            "Thread grouping on"
        } else {
            "Thread grouping off"
        });
    }

    pub(super) fn open_native_thread(&mut self) {
        if self.message_open {
            return;
        }
        let online = self.thread_server_available();
        let preview = self.preview_split;
        let Some(account) = self.account_mut() else {
            return;
        };
        if !account.collapsed_threads() || account.native_view.is_some() {
            return;
        }
        if account.collapse_inline_thread() {
            if preview {
                account.request_selected_body();
            }
        } else {
            let Some(message) = account.selected_message() else {
                return;
            };
            let seed = message.id.clone();
            let native = if account.jmap {
                message.thread_id.clone()
            } else {
                Some(seed.0.clone())
            };
            let ids = account.inline_group_ids();
            if ids.len() <= 1 && (!online || native.is_none()) {
                return;
            }
            account.expanded_threads.extend(ids);
            account.last_prefetch_anchor = None;
            account.smooth_message_target = None;
            account.normalize_search_selection();
            // A single bounded page per explicit expansion. Keep this request
            // separate from the canonical page cursor and all body requests.
            if online
                && account.inline_thread_requests.len() < 4
                && let Some(name) = account.current_mailbox.clone()
                && let Some(native) = native
                && let Ok(request) = account.worker.post(ActionKind::DiscoverThread {
                    name,
                    view: crate::backend::MailboxView::Thread(native),
                    cached_ids: account
                        .canonical_messages()
                        .iter()
                        .take(cache::MAX_MESSAGES_PER_FOLDER)
                        .map(|m| m.id.clone())
                        .collect(),
                    limit: account.page_size.clamp(1, 200),
                })
            {
                account.inline_thread_requests.insert(request, seed);
            }
        }
        self.visual_anchor = None;
        self.message_marked.clear();
    }

    pub(super) fn collapse_selected_thread(&mut self) {
        let preview = self.preview_split;
        self.visual_anchor = None;
        self.message_marked.clear();
        if let Some(account) = self.account_mut()
            && account.collapse_inline_thread()
            && preview
        {
            account.request_selected_body();
        }
    }

    fn enter_native_view(&mut self, view: crate::backend::MailboxView) {
        if !self.thread_server_available() {
            return;
        }
        self.visual_anchor = None;
        self.message_marked.clear();
        let label = view.label();
        let preview_split = self.preview_split;
        let Some(account) = self.account_mut() else {
            return;
        };
        let Some(name) = account.current_mailbox.clone() else {
            return;
        };
        if account.native_view.is_none() {
            account.save_current_cache();
            account.persist_current_navigation();
        }
        account.reset_inline_threads();
        account.native_view = Some(view);
        account.local_thread_view = None;
        account.search_query.clear();
        account.search_editing = false;
        account.native_search_editing = false;
        account.messages.clear();
        account.bodies.clear();
        account.pending_bodies.clear();
        account.body_request_id = None;
        account.last_prefetch_window = None;
        account.last_prefetch_anchor = None;
        account.message_index = 0;
        account.trim_preview_bodies();
        account.message_view_offset = 0;
        account.smooth_message_target = None;
        account.page_inflight = None;
        account.page_request_id = None;
        account.has_more = false;
        account.next_page_offset = 0;
        if account.request_mailbox_page(name, 0) {
            account.normalize_search_selection();
            if preview_split {
                account.request_selected_body();
            }
            self.status = StatusLine::info(if account.messages.is_empty() {
                format!("{label}…")
            } else {
                format!("{label}: showing cached messages; checking server…")
            });
        } else {
            self.exit_native_view();
            self.status = StatusLine::error("Could not schedule native mailbox query");
        }
    }

    pub(super) fn exit_native_view(&mut self) {
        self.visual_anchor = None;
        self.message_marked.clear();
        let preview_split = self.preview_split;
        if let Some(account) = self.account_mut() {
            if account.native_view.is_none() {
                account.local_thread_view = None;
                account.search_query.clear();
                account.search_editing = false;
                account.native_search_editing = false;
                account.normalize_search_selection();
            } else {
                Self::activate_selected_mailbox(account, preview_split);
            }
        }
        self.status = StatusLine::info(if self.account().is_some_and(|a| a.threading_enabled) {
            "Grouped mailbox view"
        } else {
            "Normal mailbox view"
        });
    }
}
