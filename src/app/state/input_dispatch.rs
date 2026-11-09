//! Top-level keyboard and command routing.

use super::*;

impl App {
    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.setup.is_some() {
            self.handle_setup_key(key);
            return;
        }
        if key.modifiers.contains(KeyModifiers::ALT)
            && key.code == KeyCode::Char('c')
            && self.contacts_modal.is_none()
            && self.confirmation.is_none()
            && self.setup.is_none()
            && !self.attachment_picker_visible
            && !self.help_visible
            && !self.theme_picker_visible
            && !self.show_startup_warnings
            && self.remove_account_confirmation.is_none()
            && self.composer.as_ref().is_none_or(|c| {
                !c.sending
                    && !c.confirm_send
                    && !c.exit_prompt
                    && !c.help_visible
                    && !c.attachment_manager_open
                    && c.file_picker.is_none()
                    && !c.advanced_headers_open
            })
        {
            self.open_contacts();
            return;
        }
        if self.contacts_modal.is_some() {
            self.handle_contacts_key(key);
            return;
        }
        if self.draft_resume_prompt_open() {
            match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.resume_server_draft(),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.pending_draft = None;
                    self.status = StatusLine::info("draft preview kept open");
                }
                _ => {}
            }
            return;
        }
        // Account actions share Alt shortcuts. Ctrl-a remains available for
        // the compose attachment picker, not the account setup wizard.
        if key.modifiers == KeyModifiers::ALT
            && key.code == KeyCode::Char('a')
            && self.composer.is_none()
            && self.confirmation.is_none()
            && !self.help_visible
            && !self.theme_picker_visible
            && !self.attachment_picker_visible
        {
            self.setup = Some(SetupWizard::new(false));
            return;
        }
        if self.show_startup_warnings {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('!')
            ) {
                self.show_startup_warnings = false;
            }
            return;
        }
        if !self.startup_warnings.is_empty()
            && key.code == KeyCode::Char('!')
            && self.composer.is_none()
            && !self.attachment_picker_visible
            && !self.help_visible
            && !self.theme_picker_visible
            && self.confirmation.is_none()
        {
            self.show_startup_warnings = true;
            return;
        }
        if self.attachment_picker_visible {
            self.handle_attachment_picker_key(key);
            return;
        }
        if self.composer.is_none()
            && !self.message_open
            && self.preview_split
            && key
                .modifiers
                .contains(KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            let adjustment = match key.code {
                KeyCode::Char('h') | KeyCode::Char('H') => Some(3_i32),
                KeyCode::Char('l') | KeyCode::Char('L') => Some(-3_i32),
                _ => None,
            };
            if let Some(adjustment) = adjustment {
                self.preview_width_percent =
                    (i32::from(self.preview_width_percent) + adjustment).clamp(25, 75) as u16;
                self.save_panel_width(crate::config::PanelWidth::Preview);
                return;
            }
        }
        if self.composer.is_none()
            && !self.message_open
            && self.sidebar_visible
            && key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
        {
            let adjustment = match key.code {
                KeyCode::Char('h') => Some(-2_i32),
                KeyCode::Char('l') => Some(2_i32),
                _ => None,
            };
            if let Some(adjustment) = adjustment {
                self.mailbox_width_percent =
                    (i32::from(self.mailbox_width_percent) + adjustment).clamp(12, 50) as u16;
                self.save_panel_width(crate::config::PanelWidth::Mailbox);
                return;
            }
        }
        if self.remove_account_confirmation.is_some() {
            self.handle_remove_account_confirmation_key(key);
            return;
        }
        if self.confirmation.is_some() {
            self.handle_confirmation_key(key);
            return;
        }
        if self.composer.is_some() {
            self.handle_composer_key(key);
            return;
        }
        if self.theme_picker_visible {
            self.handle_theme_picker_key(key);
            return;
        }
        if self.help_visible {
            self.handle_help_key(key);
            return;
        }
        if self.account().is_some_and(|account| account.search_editing) {
            self.handle_search_key(key);
            return;
        }
        // Alt-d removes the currently selected *local account*, never a remote
        // IMAP folder. Only available from the mailbox browser, with a prompt.
        if key.modifiers.contains(KeyModifiers::ALT)
            && key.code == KeyCode::Char('d')
            && !self.message_open
            && self.sidebar_visible
        {
            if let Some(account) = self.account() {
                self.remove_account_confirmation = Some(account.name.clone());
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            self.restore_postponed_composer();
            return;
        }
        // Global account shortcuts work in the reader as well as the mailbox
        // list, but must not steal input from compose or modal dialogs.
        if key.modifiers.contains(KeyModifiers::ALT) {
            let command = command_for_key(key);
            if matches!(
                command,
                Command::NextAccount | Command::PreviousAccount | Command::SelectAccount(_)
            ) {
                self.handle_command(command);
                return;
            }
        }
        if self.message_open && self.handle_reader_key(key) {
            return;
        }
        if self.preview_split {
            match key.code {
                KeyCode::Char('o') if key.modifiers.is_empty() => {
                    self.open_reader_link();
                    return;
                }
                KeyCode::Char('O') if key.modifiers.is_empty() => {
                    self.open_original_html();
                    return;
                }
                _ => {}
            }
        }
        self.handle_command(command_for_key(key));
    }

    pub fn handle_command(&mut self, command: Command) {
        if self.status.expired() {
            self.status = StatusLine::info("ready");
        }

        match command {
            Command::Quit => self.should_quit = true,
            Command::NextAccount => self.change_account(1),
            Command::PreviousAccount => self.change_account(-1),
            Command::SelectAccount(index) => self.select_account(index),
            Command::Down => {
                let count = self.take_count(1);
                self.move_selection(isize::try_from(count).unwrap_or(isize::MAX));
            }
            Command::Up => {
                let count = self.take_count(1);
                self.move_selection(-isize::try_from(count).unwrap_or(isize::MAX));
            }
            Command::MailboxDown => {
                let count = self.take_count(1);
                self.move_mailbox_list(isize::try_from(count).unwrap_or(isize::MAX));
            }
            Command::MailboxUp => {
                let count = self.take_count(1);
                self.move_mailbox_list(-isize::try_from(count).unwrap_or(isize::MAX));
            }
            Command::SmoothMailboxDown => {
                self.count_prefix = None;
                self.start_smooth_mailbox_scroll(SMOOTH_MAILBOX_ROWS);
            }
            Command::SmoothMailboxUp => {
                self.count_prefix = None;
                self.start_smooth_mailbox_scroll(-SMOOTH_MAILBOX_ROWS);
            }
            Command::SmoothDown => {
                self.count_prefix = None;
                self.start_smooth_scroll(SMOOTH_SCROLL_ROWS);
            }
            Command::SmoothUp => {
                self.count_prefix = None;
                self.start_smooth_scroll(-SMOOTH_SCROLL_ROWS);
            }
            Command::PageDown => {
                let count = self.take_count(10);
                self.move_selection(isize::try_from(count).unwrap_or(isize::MAX));
            }
            Command::PageUp => {
                let count = self.take_count(10);
                self.move_selection(-isize::try_from(count).unwrap_or(isize::MAX));
            }
            Command::Top => {
                self.count_prefix = None;
                self.jump(false);
            }
            Command::Bottom => {
                self.count_prefix = None;
                self.jump(true);
            }
            Command::Open => {
                self.count_prefix = None;
                self.open_selection();
            }
            Command::ToggleVisual => {
                self.count_prefix = None;
                self.toggle_visual();
            }
            Command::ToggleMark => {
                self.count_prefix = None;
                if self.message_open {
                    // Preserve the old reader Space navigation behavior.
                    self.move_selection(10);
                } else {
                    self.toggle_message_mark();
                }
            }
            Command::ToggleSeen => {
                self.count_prefix = None;
                self.toggle_seen();
            }
            Command::Delete => {
                self.count_prefix = None;
                self.delete_selected();
            }
            Command::DeleteImmediate => {
                self.count_prefix = None;
                self.delete_selected_immediate();
            }
            Command::Archive => {
                self.count_prefix = None;
                self.archive_selected();
            }
            Command::Unarchive => {
                self.count_prefix = None;
                self.unarchive_selected();
            }
            Command::ToggleSpam => {
                self.count_prefix = None;
                self.toggle_spam_selected();
            }
            Command::Attachments => {
                self.count_prefix = None;
                self.open_attachment_picker();
            }
            Command::Search => {
                self.count_prefix = None;
                self.begin_search();
            }
            Command::ToggleSidebar => {
                self.count_prefix = None;
                self.sidebar_visible = !self.sidebar_visible;
            }
            Command::TogglePreviewSplit => {
                self.count_prefix = None;
                self.preview_split = !self.preview_split;
                let preview_split_enabled = self.preview_split;
                if let Some(account) = self.account_mut() {
                    account.last_prefetch_window = None;
                    if preview_split_enabled {
                        account.request_selected_body();
                        account.prefetch_near_cursor(1);
                        Self::mark_cached_selected_seen(account);
                    }
                }
            }
            Command::Refresh => {
                self.count_prefix = None;
                self.refresh();
            }
            Command::ToggleThemePicker => {
                self.count_prefix = None;
                self.open_theme_picker();
            }
            Command::ExportRaw => {
                self.count_prefix = None;
                self.export_selected_raw();
            }
            Command::ToggleHelp => {
                self.count_prefix = None;
                self.open_help();
            }
            Command::ToggleHtml => {
                self.count_prefix = None;
                self.toggle_html();
            }
            Command::ToggleQuotedDiff => {
                self.count_prefix = None;
                self.quoted_diff_highlighting = !self.quoted_diff_highlighting;
                self.status = StatusLine::temporary(
                    if self.quoted_diff_highlighting {
                        "quoted Git diff colors enabled"
                    } else {
                        "conversation quote colors enabled"
                    },
                    Duration::from_secs(2),
                );
            }
            Command::ToggleImages => {
                self.count_prefix = None;
                self.toggle_remote_images();
            }
            Command::ToggleNotifications => {
                self.count_prefix = None;
                self.new_mail_notifications_enabled = !self.new_mail_notifications_enabled;
                if !self.new_mail_notifications_enabled {
                    // Prevent previously queued messages from appearing after mute.
                    self.new_mail_notices.clear();
                    self.new_mail_bell_pending = false;
                    self.new_mail_since_launch = 0;
                }
                self.status = StatusLine::temporary(
                    if self.new_mail_notifications_enabled {
                        "new-mail notifications enabled"
                    } else {
                        "new-mail notifications muted"
                    },
                    Duration::from_secs(2),
                );
            }
            Command::ToggleBell => {
                self.count_prefix = None;
                self.new_mail_bell_enabled = !self.new_mail_bell_enabled;
                self.status = StatusLine::temporary(
                    if self.new_mail_bell_enabled {
                        "new-mail bell enabled"
                    } else {
                        "new-mail bell muted"
                    },
                    Duration::from_secs(2),
                );
            }
            Command::OpenOriginal => {
                self.count_prefix = None;
                self.open_original_html();
            }
            Command::Yank => {
                self.count_prefix = None;
                self.yank_reader_selection();
            }
            Command::Compose => {
                self.count_prefix = None;
                self.begin_compose();
            }
            Command::Reply => {
                self.count_prefix = None;
                self.begin_reply(false);
            }
            Command::ReplyQuoted => {
                self.count_prefix = None;
                self.begin_reply(true);
            }
            Command::Forward => {
                self.count_prefix = None;
                self.begin_forward();
            }
            Command::ReplyAll => {
                self.count_prefix = None;
                self.begin_reply_all();
            }
            Command::ReplyList => {
                self.count_prefix = None;
                self.begin_reply_list();
            }
            Command::Escape => {
                self.count_prefix = None;
                let had_selection = self.reader_visual_anchor.take().is_some()
                    | self.visual_anchor.take().is_some()
                    | !self.message_marked.is_empty();
                self.message_marked.clear();
                self.reader_visual_linewise = false;
                if had_selection {
                    self.status =
                        StatusLine::temporary("selection cleared", Duration::from_secs(1));
                } else if self.message_open {
                    self.close_message();
                } else {
                    if let Some(account) = self.account_mut()
                        && !account.search_query.is_empty()
                    {
                        account.search_query.clear();
                        account.search_editing = false;
                        account.normalize_search_selection();
                    }
                    self.focus = Focus::Messages;
                }
            }
            Command::Digit(digit) => self.push_count_digit(digit),
            Command::None => {}
        }
    }
}
