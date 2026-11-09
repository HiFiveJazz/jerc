//! Reader actions behavior.

use super::*;

impl App {
    pub(super) fn handle_reader_key(&mut self, key: KeyEvent) -> bool {
        if self.focus != Focus::Preview || !self.message_open {
            return false;
        }
        // Esc must reach the shared selection-clearing command even when a
        // multi-key Vim motion (g or an inner text object) is pending.
        if key.code == KeyCode::Esc {
            self.reader_pending_inner_object = false;
            self.reader_pending_g = false;
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
        {
            return match key.code {
                KeyCode::Char('j') => {
                    self.count_prefix = None;
                    self.start_smooth_scroll(SMOOTH_SCROLL_ROWS);
                    true
                }
                KeyCode::Char('k') => {
                    self.count_prefix = None;
                    self.start_smooth_scroll(-SMOOTH_SCROLL_ROWS);
                    true
                }
                KeyCode::Char('d') | KeyCode::Char('u') => {
                    let distance = isize::try_from(
                        self.reader_view_height
                            .saturating_div(2)
                            .max(1)
                            .saturating_mul(self.take_count(1)),
                    )
                    .unwrap_or(isize::MAX);
                    self.move_reader_cursor(if key.code == KeyCode::Char('d') {
                        distance
                    } else {
                        -distance
                    });
                    true
                }
                _ => false,
            };
        }

        // Any ordinary reader motion/action interrupts a pending neoscroll.
        // The next key should always operate from the cursor line currently on
        // screen rather than allowing an old smooth target to keep advancing.
        self.reader_smooth_target = None;

        if self.reader_pending_inner_object {
            self.reader_pending_inner_object = false;
            if let KeyCode::Char(delimiter) = key.code
                && self.select_reader_inner_object(delimiter)
            {
                return true;
            }
            self.status = StatusLine::temporary(
                "no enclosing quote/bracket at reader cursor",
                Duration::from_secs(2),
            );
            return true;
        }

        if self.reader_pending_g {
            self.reader_pending_g = false;
            if matches!(key.code, KeyCode::Char('g')) {
                let target = self.take_count(1).saturating_sub(1);
                self.jump_reader_to_line(target);
                return true;
            }
        }

        match key.code {
            KeyCode::Esc => false,
            KeyCode::Char('o') => {
                self.open_reader_link();
                true
            }
            KeyCode::Char('O') => {
                self.open_original_html();
                true
            }
            KeyCode::Char('m') => {
                self.toggle_header_details();
                true
            }
            KeyCode::Char('0') if self.count_prefix.is_none() => {
                self.reader_column = 0;
                self.reader_preferred_column = None;
                true
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() => {
                self.push_count_digit(ch.to_digit(10).unwrap_or_default() as u8);
                true
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let count = isize::try_from(self.take_count(1)).unwrap_or(isize::MAX);
                self.move_reader_cursor(count);
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let count = isize::try_from(self.take_count(1)).unwrap_or(isize::MAX);
                self.move_reader_cursor(-count);
                true
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let count = isize::try_from(self.take_count(1)).unwrap_or(isize::MAX);
                self.move_reader_horizontal(-count);
                true
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let count = isize::try_from(self.take_count(1)).unwrap_or(isize::MAX);
                self.move_reader_horizontal(count);
                true
            }
            KeyCode::Char('w') => {
                let count = self.take_count(1);
                for _ in 0..count {
                    self.move_reader_word(ReaderWordMotion::NextStart);
                }
                true
            }
            KeyCode::Char('b') => {
                let count = self.take_count(1);
                for _ in 0..count {
                    self.move_reader_word(ReaderWordMotion::PreviousStart);
                }
                true
            }
            KeyCode::Char('e') => {
                self.count_prefix = None;
                self.export_selected_raw();
                true
            }
            KeyCode::Char('^') => {
                self.reader_column = self.reader_first_non_blank();
                self.reader_preferred_column = None;
                true
            }
            KeyCode::Char('$') | KeyCode::End => {
                self.reader_column = self.reader_line_len(self.reader_cursor).saturating_sub(1);
                self.reader_preferred_column = None;
                true
            }
            KeyCode::Home => {
                self.reader_column = 0;
                self.reader_preferred_column = None;
                true
            }
            KeyCode::Char('H') => {
                self.reader_column = 0;
                self.reader_preferred_column = None;
                true
            }
            KeyCode::Char('L') => {
                self.reader_column = self.reader_line_len(self.reader_cursor).saturating_sub(1);
                self.reader_preferred_column = None;
                true
            }
            KeyCode::Char('g') => {
                self.reader_pending_g = true;
                true
            }
            KeyCode::Char('G') => {
                if let Some(count) = self.count_prefix.take() {
                    self.jump_reader_to_line(count.saturating_sub(1));
                } else {
                    self.jump_reader_to_line(self.reader_line_count.saturating_sub(1));
                }
                true
            }
            KeyCode::Char('{') => {
                let count = self.take_count(1);
                for _ in 0..count {
                    self.move_reader_paragraph(false);
                }
                true
            }
            KeyCode::Char('}') => {
                let count = self.take_count(1);
                for _ in 0..count {
                    self.move_reader_paragraph(true);
                }
                true
            }
            KeyCode::Char('v') => {
                self.count_prefix = None;
                self.toggle_visual();
                true
            }
            KeyCode::Char('V') => {
                self.count_prefix = None;
                self.toggle_reader_linewise_visual();
                true
            }
            KeyCode::Char('i') if self.reader_visual_anchor.is_some() => {
                self.reader_visual_linewise = false;
                self.reader_pending_inner_object = true;
                self.status = StatusLine::temporary(
                    "inner text object: quote or bracket",
                    Duration::from_secs(2),
                );
                true
            }
            KeyCode::Char('y') if self.reader_visual_anchor.is_some() => {
                self.count_prefix = None;
                self.yank_reader_selection();
                true
            }
            KeyCode::Char('d' | 'c' | 'x' | 'a') if self.reader_visual_anchor.is_some() => {
                self.status = StatusLine::temporary(
                    "reader is read-only; press y to copy the visual selection",
                    Duration::from_secs(2),
                );
                true
            }
            KeyCode::Char('t') => {
                self.count_prefix = None;
                self.toggle_html();
                true
            }
            KeyCode::Char('i') if self.reader_visual_anchor.is_none() => {
                self.count_prefix = None;
                self.toggle_remote_images();
                true
            }
            _ => false,
        }
    }
}

impl App {
    /// True only after a Vim cursor motion. Measuring the wrapped cursor row
    /// on every idle redraw would be expensive for large HTML emails.
    pub fn reader_needs_scroll_follow(&self) -> bool {
        self.reader_scroll_follow_pending
    }

    /// Called by the renderer after laying out the current message. Keeping the
    /// reader cursor and scroll bounds together makes normal/visual reader
    /// motions deterministic even when the message body is shorter than the
    /// viewport.
    pub fn ensure_reader_html_layout(&mut self, width: usize) {
        let width = width.max(20);
        let Some(account) = self.account_mut() else {
            return;
        };
        let Some(message_id) = account.selected_message().map(|message| message.id.clone()) else {
            return;
        };
        let prefer_html = account.prefer_html;
        let Some(body) = account.bodies.get_mut(&message_id) else {
            return;
        };
        if !body.has_html_alternative() || !prefer_html || body.html_render_width == width {
            return;
        }
        // Only the derived layout is replaced. Borrow the MIME HTML source
        // instead of cloning potentially megabytes of markup on each reflow.
        let Some(source) = body.html_source.as_deref() else {
            return;
        };
        let (html_text, html_lines, html_renderer) = render_html_for_terminal(source, width);
        body.html_text = Some(html_text);
        body.html_lines = Some(html_lines);
        body.html_renderer = Some(html_renderer);
        body.html_render_width = width;
    }

    /// Called by the renderer after laying out the current message. Keeping the
    /// reader cursor and scroll bounds together makes normal/visual reader
    /// motions deterministic even when the message body is shorter than the
    /// viewport.
    pub fn set_reader_layout(
        &mut self,
        line_count: usize,
        view_height: usize,
        max_scroll: u16,
        cursor_visual_row: Option<usize>,
    ) {
        self.reader_line_count = line_count;
        self.reader_view_height = view_height.max(1);
        if line_count == 0 {
            self.reader_cursor = 0;
            self.reader_column = 0;
            self.reader_visual_anchor = None;
            self.reader_visual_linewise = false;
            self.reader_smooth_target = None;
        } else {
            self.reader_cursor = self.reader_cursor.min(line_count.saturating_sub(1));
            self.reader_smooth_target = self
                .reader_smooth_target
                .map(|target| target.min(line_count.saturating_sub(1)))
                .filter(|target| *target != self.reader_cursor);
            if let Some(anchor) = self.reader_visual_anchor {
                let line = anchor.line.min(line_count.saturating_sub(1));
                let line_len = self.reader_line_len(line);
                self.reader_visual_anchor = Some(ReaderPosition {
                    line,
                    column: if line_len == 0 {
                        0
                    } else {
                        anchor.column.min(line_len.saturating_sub(1))
                    },
                });
            }
            self.clamp_reader_column();
        }

        let follow_cursor = self.reader_scroll_follow_pending && line_count > 0;
        self.reader_scroll_follow_pending = false;
        let cursor_at_start = self.reader_cursor == 0;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.preview_max_scroll = max_scroll;
        account.preview_scroll = account.preview_scroll.min(max_scroll);
        account.smooth_preview_target = account
            .smooth_preview_target
            .map(|target| target.min(max_scroll))
            .filter(|target| *target != account.preview_scroll);
        if follow_cursor {
            if cursor_at_start {
                // Keep the real header and hero image visible at the top.
                account.preview_scroll = 0;
            } else if let Some(row) = cursor_visual_row {
                account.preview_scroll = u16::try_from(reader_scroll_for_visual_cursor(
                    usize::from(account.preview_scroll),
                    row,
                    view_height.max(1),
                    usize::from(max_scroll),
                ))
                .unwrap_or(u16::MAX);
            }
        }
    }

    pub(super) fn reader_display_text(&self) -> Option<&str> {
        let account = self.account()?;
        let body = account.selected_body()?;
        let show_html = body.has_html_alternative() && account.prefer_html;
        Some(body.display_text(show_html))
    }

    pub(super) fn reader_line_text(&self, line: usize) -> Option<&str> {
        self.reader_display_text()?.lines().nth(line)
    }

    pub(super) fn reader_line_len(&self, line: usize) -> usize {
        self.reader_line_text(line)
            .map_or(0, |text| text.chars().count())
    }

    pub(super) fn clamp_reader_column(&mut self) {
        let line_len = self.reader_line_len(self.reader_cursor);
        self.reader_column = if line_len == 0 {
            0
        } else {
            self.reader_column.min(line_len.saturating_sub(1))
        };
    }

    pub(super) fn reader_first_non_blank(&self) -> usize {
        self.reader_line_text(self.reader_cursor)
            .and_then(|line| line.chars().position(|ch| !ch.is_whitespace()))
            .unwrap_or(0)
    }

    pub(super) fn move_reader_cursor(&mut self, delta: isize) {
        if self.reader_line_count == 0 {
            return;
        }

        let preferred = self.reader_preferred_column.unwrap_or(self.reader_column);
        self.reader_cursor = bounded_index(self.reader_cursor, self.reader_line_count, delta);
        let line_len = self.reader_line_len(self.reader_cursor);
        self.reader_column = if line_len == 0 {
            0
        } else {
            preferred.min(line_len.saturating_sub(1))
        };
        self.reader_preferred_column = Some(preferred);
        self.keep_reader_cursor_visible();
    }

    pub(super) fn move_reader_horizontal(&mut self, delta: isize) {
        let line_len = self.reader_line_len(self.reader_cursor);
        if line_len == 0 {
            self.reader_column = 0;
        } else {
            self.reader_column = bounded_index(self.reader_column, line_len, delta);
        }
        self.reader_preferred_column = None;
    }

    pub(super) fn jump_reader_to_line(&mut self, line: usize) {
        if self.reader_line_count == 0 {
            return;
        }
        self.reader_cursor = line.min(self.reader_line_count.saturating_sub(1));
        self.clamp_reader_column();
        self.reader_preferred_column = None;
        self.keep_reader_cursor_visible();
    }

    pub(super) fn move_reader_word(&mut self, motion: ReaderWordMotion) {
        let Some(text) = self.reader_display_text() else {
            return;
        };
        let Some(target) = reader_word_target(text, self.reader_cursor, self.reader_column, motion)
        else {
            return;
        };
        self.reader_cursor = target.line.min(self.reader_line_count.saturating_sub(1));
        self.reader_column = target.column;
        self.reader_preferred_column = None;
        self.keep_reader_cursor_visible();
    }

    pub(super) fn move_reader_paragraph(&mut self, forward: bool) {
        let Some(text) = self.reader_display_text() else {
            return;
        };
        let lines = text.lines().collect::<Vec<_>>();
        if lines.is_empty() {
            return;
        }
        let current = self.reader_cursor.min(lines.len().saturating_sub(1));
        let target = if forward {
            (current + 1..lines.len())
                .find(|index| lines[*index].trim().is_empty())
                .and_then(|blank| {
                    (blank + 1..lines.len()).find(|index| !lines[*index].trim().is_empty())
                })
                .unwrap_or(lines.len().saturating_sub(1))
        } else {
            (0..current)
                .rev()
                .find(|index| lines[*index].trim().is_empty())
                .and_then(|blank| {
                    (0..blank)
                        .rev()
                        .find(|index| !lines[*index].trim().is_empty())
                })
                .unwrap_or(0)
        };
        self.reader_cursor = target;
        self.reader_column = self.reader_first_non_blank();
        self.reader_preferred_column = None;
        self.keep_reader_cursor_visible();
    }

    pub(super) fn select_reader_inner_object(&mut self, delimiter: char) -> bool {
        if self.reader_visual_anchor.is_none() {
            return false;
        }
        let Some(line) = self
            .reader_line_text(self.reader_cursor)
            .map(ToOwned::to_owned)
        else {
            return false;
        };
        let Some((start, end)) = inner_text_object_range(&line, self.reader_column, delimiter)
        else {
            return false;
        };
        self.reader_visual_anchor = Some(ReaderPosition {
            line: self.reader_cursor,
            column: start,
        });
        self.reader_column = end;
        self.reader_preferred_column = None;
        true
    }

    pub(super) fn keep_reader_cursor_visible(&mut self) {
        // The renderer knows the actual wrapped row; logical body-line numbers
        // do not. Defer scrolloff until it has measured the current paragraph.
        self.reader_scroll_follow_pending = true;
        if let Some(account) = self.account_mut() {
            account.smooth_preview_target = None;
        }
    }

    pub(super) fn start_smooth_scroll(&mut self, delta: isize) {
        let focus = self.focus;
        if focus == Focus::Preview {
            if self.reader_line_count == 0 {
                return;
            }
            let base = self.reader_smooth_target.unwrap_or(self.reader_cursor);
            let target = bounded_index(base, self.reader_line_count, delta);
            self.reader_smooth_target = (target != self.reader_cursor).then_some(target);
            if let Some(account) = self.account_mut() {
                account.smooth_preview_target = None;
            }
            return;
        }

        let Some(account) = self.account_mut() else {
            return;
        };
        match focus {
            Focus::Messages | Focus::Mailboxes => {
                let visible = account.visible_message_indices();
                if visible.is_empty() {
                    return;
                }
                let current = visible
                    .iter()
                    .position(|index| *index == account.message_index)
                    .unwrap_or(0);
                let base = account.smooth_message_target.unwrap_or(current);
                let target = bounded_index(base, visible.len(), delta);
                account.smooth_message_target = Some(target);
                if target != current {
                    // Warm the entire travel corridor instead of repeatedly
                    // canceling batches aimed at the moving destination.
                    account.prefetch_for_scroll(current, target);
                }
            }
            Focus::Preview => {
                unreachable!("reader smooth scrolling is handled before borrowing account state")
            }
        }
    }

    pub(super) fn start_smooth_mailbox_scroll(&mut self, delta: isize) {
        if self.message_open {
            return;
        }
        self.visual_anchor = None;
        self.message_marked.clear();
        let Some(account) = self.account_mut() else {
            return;
        };
        if account.mailboxes.is_empty() {
            return;
        }
        if account.smooth_mailbox_target.is_none() {
            account.save_current_cache();
        }
        let base = account
            .smooth_mailbox_target
            .unwrap_or(account.mailbox_index);
        account.smooth_mailbox_target = Some(bounded_index(base, account.mailboxes.len(), delta));
    }

    pub(super) fn jump(&mut self, bottom: bool) {
        let focus = self.focus;
        if focus == Focus::Preview {
            self.reader_cursor = if bottom {
                self.reader_line_count.saturating_sub(1)
            } else {
                0
            };
            self.reader_column = 0;
            self.reader_preferred_column = None;
            self.reader_visual_anchor = None;
            self.reader_pending_g = false;
            self.reader_pending_inner_object = false;
            if let Some(account) = self.account_mut() {
                account.smooth_preview_target = None;
                account.preview_scroll = if bottom {
                    account.preview_max_scroll
                } else {
                    0
                };
            }
            return;
        }

        self.reader_cursor = 0;
        self.reader_column = 0;
        self.reader_preferred_column = None;
        self.reader_visual_anchor = None;
        self.reader_pending_g = false;
        self.reader_pending_inner_object = false;
        let preview_split = self.preview_split;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.smooth_message_target = None;
        account.smooth_preview_target = None;
        let visible = account.visible_message_indices();
        if visible.is_empty() {
            return;
        }
        account.message_index = if bottom {
            *visible.last().unwrap_or(&visible[0])
        } else {
            visible[0]
        };
        account.preview_scroll = 0;
        account.keep_message_cursor_visible();
        if bottom {
            let _ = account.request_next_page();
        }
        account.prefetch_near_cursor(if bottom { -1 } else { 1 });
        if preview_split {
            account.request_selected_body();
            Self::mark_cached_selected_seen(account);
        }
    }

    pub(super) fn open_selection(&mut self) {
        match self.focus {
            Focus::Mailboxes | Focus::Messages => {
                if self
                    .account()
                    .is_none_or(|account| account.selected_message().is_none())
                {
                    return;
                }
                self.visual_anchor = None;
                self.reader_cursor = 0;
                self.reader_column = 0;
                self.reader_preferred_column = None;
                self.reader_visual_anchor = None;
                self.reader_visual_linewise = false;
                self.reader_smooth_target = None;
                self.reader_pending_g = false;
                self.reader_pending_inner_object = false;
                self.reader_line_count = 0;
                self.reader_image = None;
                self.reader_image_pending = None;
                self.reader_image_size_hint = None;
                self.message_open = true;
                if let Some(account) = self.account_mut() {
                    account.preview_scroll = 0;
                    account.preview_max_scroll = 0;
                    account.request_selected_body();
                    account.prefetch_near_cursor(1);
                    Self::mark_cached_selected_seen(account);
                }
                self.focus = Focus::Preview;
                // A Drafts item opens in the ordinary reader as a preview.
                // Fetch exact MIME in parallel, then offer [Y]es/[N]o once
                // it is available; selecting No leaves the preview untouched.
                self.prepare_draft_resume();
            }
            Focus::Preview => {}
        }
    }

    pub(super) fn close_message(&mut self) {
        self.pending_draft = None;
        self.reader_cursor = 0;
        self.reader_column = 0;
        self.reader_preferred_column = None;
        self.reader_visual_anchor = None;
        self.reader_visual_linewise = false;
        self.reader_smooth_target = None;
        self.reader_pending_g = false;
        self.reader_pending_inner_object = false;
        self.reader_line_count = 0;
        self.reader_image = None;
        self.reader_image_pending = None;
        self.reader_image_size_hint = None;
        self.message_open = false;
        self.focus = Focus::Messages;
        if let Some(account) = self.account_mut() {
            account.smooth_preview_target = None;
            account.preview_scroll = 0;
            if account.pending_inbox_refresh && account.page_inflight.is_none() {
                account.pending_inbox_refresh = false;
                if let Some(name) = account.current_mailbox.clone() {
                    account.restore_message_index = Some(account.message_index);
                    let _ = account.request_mailbox_page(name, 0);
                }
            }
        }
    }

    pub(super) fn mark_cached_selected_seen(account: &mut AccountState) {
        let Some(id) = account
            .selected_message()
            .filter(|message| message.unread() && account.bodies.contains_key(&message.id))
            .map(|message| message.id.clone())
        else {
            return;
        };
        account.request_seen(id, true);
    }

    pub(super) fn open_reader_link(&mut self) {
        let Some(url) = self.link_under_reader_cursor() else {
            self.status =
                StatusLine::temporary("reader cursor is not on a link", Duration::from_secs(1));
            return;
        };
        let result = open_url(&url);
        self.status = match result {
            Ok(()) => StatusLine::temporary(format!("opened {url}"), Duration::from_secs(3)),
            Err(error) => StatusLine::error(format!("could not open link: {error}")),
        };
    }

    pub(super) fn link_under_reader_cursor(&self) -> Option<String> {
        let account = self.account()?;
        let body = account.selected_body()?;
        let showing_html = body.has_html_alternative() && account.prefer_html;
        if showing_html && let Some(url) = body.html_link_at(self.reader_cursor, self.reader_column)
        {
            return Some(url.to_owned());
        }
        let line = body
            .display_text(showing_html)
            .lines()
            .nth(self.reader_cursor)?;
        url_at_cursor(line, self.reader_column)
    }

    pub(super) fn yank_reader_selection(&mut self) {
        if self.focus != Focus::Preview || !self.message_open {
            return;
        }
        let Some(selection) = self.reader_visual_range() else {
            self.status = StatusLine::temporary(
                "press v and select reader text before yanking",
                Duration::from_secs(2),
            );
            return;
        };
        let Some(text) = self.reader_display_text() else {
            return;
        };
        let selected = reader_selection_text(text, selection);
        if selected.is_empty() && selection.start == selection.end {
            return;
        }

        // Always retain a private in-process register, even without wl-copy,
        // xclip, or another clipboard helper. This can be pasted into Compose.
        self.yank_register = Some(YankRegister {
            text: selected.clone(),
            linewise: self.reader_visual_linewise,
        });
        self.reader_visual_anchor = None;
        self.reader_visual_linewise = false;
        self.status = match copy_text_to_clipboard(&selected) {
            Ok(()) => StatusLine::temporary("copied reader selection", Duration::from_secs(2)),
            Err(_) => StatusLine::temporary(
                "copied to internal register (system clipboard unavailable)",
                Duration::from_secs(3),
            ),
        };
    }
}
