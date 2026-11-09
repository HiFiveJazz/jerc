//! Mouse input and panel interactions.

use super::*;

impl App {
    pub fn handle_mouse(&mut self, mouse: MouseEvent, width: u16, height: u16) {
        if self.setup.is_some() {
            let mut continue_clicked = false;
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                let wizard = self.setup.as_mut().expect("wizard exists");
                let (popup_width, popup_height) = wizard.popup_size(width, height);
                let popup_y = height.saturating_sub(popup_height) / 2;
                let popup_x = width.saturating_sub(popup_width) / 2;
                let header_height = if wizard.step == crate::setup::SetupStep::Identity {
                    1
                } else {
                    3
                };
                let list_y =
                    popup_y.saturating_add(1 + crate::setup::PROGRESS_ROWS + header_height);
                let list_height = usize::from(
                    popup_height.saturating_sub(5 + crate::setup::PROGRESS_ROWS + header_height),
                );
                let rows = wizard.display_rows();
                let selected_pos = rows.iter().position(|row|
                    matches!(row, crate::setup::SetupDisplayRow::Field(index) if *index == wizard.selected))
                    .unwrap_or(0);
                let first = selected_pos
                    .saturating_sub(list_height.saturating_sub(1))
                    .min(rows.len().saturating_sub(list_height));
                if mouse.column > popup_x && mouse.column < popup_x.saturating_add(popup_width) {
                    if mouse.row >= list_y
                        && usize::from(mouse.row - list_y) < list_height
                        && !wizard.authenticating
                        && !wizard.discovering
                        && let Some(crate::setup::SetupDisplayRow::Field(field)) =
                            rows.get(first + usize::from(mouse.row - list_y))
                    {
                        wizard.selected = *field;
                    }
                    // The last interior row presents the primary Continue action.
                    let button_y = if wizard.saved {
                        // Confirmation button occupies the penultimate inner row.
                        popup_y.saturating_add(popup_height).saturating_sub(3)
                    } else {
                        // Primary button occupies the last inner row.
                        popup_y.saturating_add(popup_height).saturating_sub(2)
                    };
                    let label = if wizard.saved {
                        "Open account"
                    } else if wizard.step == crate::setup::SetupStep::Identity {
                        "Continue"
                    } else if wizard.authenticated {
                        "Save and open account"
                    } else if wizard.login_failed {
                        "Retry sign-in"
                    } else {
                        "Continue to Browser Sign-In"
                    };
                    let button_width = (if wizard.saved {
                        26
                    } else {
                        (label.chars().count() as u16).saturating_add(6)
                    })
                    .min(popup_width.saturating_sub(2));
                    let button_x = popup_x.saturating_add(1).saturating_add(
                        popup_width.saturating_sub(2).saturating_sub(button_width) / 2,
                    );
                    if mouse.row == button_y
                        && mouse.column >= button_x
                        && mouse.column < button_x.saturating_add(button_width)
                        && !wizard.authenticating
                        && !wizard.discovering
                    {
                        continue_clicked = true;
                    }
                }
            }
            if continue_clicked {
                let key = if self.setup.as_ref().is_some_and(|wizard| wizard.saved) {
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
                } else {
                    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
                };
                self.handle_setup_key(key);
            }
            return;
        }
        if self.show_startup_warnings || self.remove_account_confirmation.is_some() {
            return;
        }
        // The rightmost footer bell is an action as well as a status indicator.
        // Never steal clicks from compose, confirmation, or other open dialogs.
        if height > 0
            && width >= crate::ui::NOTIFICATION_INDICATOR_WIDTH
            && mouse.row == height - 1
            && mouse.column < width
            && mouse.column >= width - crate::ui::NOTIFICATION_INDICATOR_WIDTH
            && self.composer.is_none()
            && !self.attachment_picker_visible
            && !self.help_visible
            && !self.theme_picker_visible
            && self.confirmation.is_none()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.handle_command(Command::ToggleNotifications);
            return;
        }
        if mouse.row == 0
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.composer.is_none()
            && !self.attachment_picker_visible
            && !self.help_visible
            && !self.theme_picker_visible
            && self.confirmation.is_none()
        {
            let mut left = 0usize;
            for index in 0..self.accounts.len() {
                let cells =
                    unicode_width::UnicodeWidthStr::width(self.accounts[index].name.as_str()) + 2;
                if usize::from(mouse.column) >= left && usize::from(mouse.column) < left + cells {
                    self.select_account(index);
                    return;
                }
                left += cells;
                if index + 1 < self.accounts.len() {
                    left += 2;
                }
            }
            if !self.startup_warnings.is_empty() {
                let label = format!(
                    "  ! {} config issue(s) [! to inspect]",
                    self.startup_warnings.len()
                );
                let len = unicode_width::UnicodeWidthStr::width(label.as_str());
                if (left..left + len).contains(&usize::from(mouse.column)) {
                    self.show_startup_warnings = true;
                    return;
                }
            }
        }
        if self.attachment_picker_visible {
            if self.help_visible {
                return;
            }
            if self.attachment_destination_picker.is_some() {
                let (_, _, files, _, footer) = crate::ui::attachment_destination_regions(
                    ratatui::layout::Rect::new(0, 0, width, height),
                );
                // Only the [s] Save here button is actionable; clicking
                // the Cancel instructions must never unexpectedly write files.
                if mouse.row == footer.y
                    && mouse.column >= footer.x
                    && mouse.column < footer.x.saturating_add(15)
                    && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                {
                    self.save_selected_attachments();
                    return;
                }
                let Some(picker) = self.attachment_destination_picker.as_mut() else {
                    return;
                };
                match mouse.kind {
                    MouseEventKind::ScrollDown => picker.move_selection(3),
                    MouseEventKind::ScrollUp => picker.move_selection(-3),
                    MouseEventKind::Moved | MouseEventKind::Down(MouseButton::Left)
                        if mouse.column >= files.x
                            && mouse.column < files.right()
                            && mouse.row >= files.y
                            && mouse.row < files.bottom() =>
                    {
                        let visible = usize::from(files.height);
                        let start = picker
                            .selected
                            .saturating_sub(visible / 2)
                            .min(picker.entries.len().saturating_sub(visible));
                        let next = start
                            .saturating_add(usize::from(mouse.row - files.y))
                            .min(picker.entries.len().saturating_sub(1));
                        let already_selected = next == picker.selected;
                        if !already_selected {
                            picker.selected = next;
                            picker.refresh_preview();
                        } else if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                            // Hover selects; clicking a selected folder enters it.
                            if let Some(entry) = picker.entries.get(next)
                                && entry.is_directory
                                && let Err(error) = picker.enter_directory(entry.path.clone())
                            {
                                self.status = StatusLine::error(format!("file browser: {error}"));
                            }
                        }
                    }
                    _ => {}
                }
                return;
            }
            let (_, list, _, _) =
                crate::ui::attachment_regions(ratatui::layout::Rect::new(0, 0, width, height));
            let inside_list = mouse.column >= list.x
                && mouse.column < list.x.saturating_add(list.width)
                && mouse.row > list.y
                && mouse.row < list.y.saturating_add(list.height);
            if inside_list
                && matches!(
                    mouse.kind,
                    MouseEventKind::Moved
                        | MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::Drag(MouseButton::Left)
                )
            {
                let rows = usize::from(list.height.saturating_sub(1));
                let start = attachment_list_start(
                    self.attachment_selection,
                    self.attachment_files.len(),
                    rows,
                );
                self.move_attachment_selection(
                    start.saturating_add(usize::from(mouse.row - list.y).saturating_sub(1)),
                );
            }
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.move_attachment_selection(self.attachment_selection.saturating_add(3))
                }
                MouseEventKind::ScrollUp => {
                    self.move_attachment_selection(self.attachment_selection.saturating_sub(3))
                }
                _ => {}
            }
            return;
        }
        if self.message_open {
            self.preview_resizing = false;
            let key = match mouse.kind {
                MouseEventKind::ScrollDown => Some(KeyCode::Down),
                MouseEventKind::ScrollUp => Some(KeyCode::Up),
                _ => None,
            };
            if let Some(code) = key {
                self.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            }
            self.mailbox_resizing = false;
            return;
        }
        if self.composer.is_some()
            || self.help_visible
            || self.theme_picker_visible
            || self.confirmation.is_some()
        {
            self.mailbox_resizing = false;
            self.preview_resizing = false;
            return;
        }
        let sidebar_width = if self.sidebar_visible {
            crate::ui::mailbox_column_width(width, self.mailbox_width_percent)
        } else {
            0
        };
        let separator = sidebar_width.saturating_sub(1);
        let right_width = width.saturating_sub(sidebar_width);
        let preview_separator = sidebar_width.saturating_add(right_width.saturating_sub(
            crate::ui::preview_column_width(right_width, self.preview_width_percent),
        ));
        if self.preview_split {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if mouse.row > 0
                        && mouse.row < height.saturating_sub(1)
                        && mouse.column.abs_diff(preview_separator) <= 1 =>
                {
                    self.preview_resizing = true;
                    self.mailbox_resizing = false;
                    return;
                }
                MouseEventKind::Drag(MouseButton::Left) if self.preview_resizing => {
                    self.preview_width_percent =
                        preview_percent_at(mouse.column, sidebar_width, right_width);
                    return;
                }
                MouseEventKind::Up(MouseButton::Left) if self.preview_resizing => {
                    self.preview_resizing = false;
                    self.preview_width_percent =
                        preview_percent_at(mouse.column, sidebar_width, right_width);
                    self.save_panel_width(crate::config::PanelWidth::Preview);
                    return;
                }
                _ => {}
            }
        }
        if !self.sidebar_visible {
            self.mailbox_resizing = false;
            return;
        }
        if mouse.row > 0 && mouse.row < height.saturating_sub(1) {
            let scrolling = match mouse.kind {
                MouseEventKind::ScrollDown => Some(true),
                MouseEventKind::ScrollUp => Some(false),
                _ => None,
            };
            if let Some(down) = scrolling {
                let mailbox = mouse.column < separator;
                let key = match (mailbox, down) {
                    (true, true) => KeyCode::Char('J'),
                    (true, false) => KeyCode::Char('K'),
                    (false, true) => KeyCode::Down,
                    (false, false) => KeyCode::Up,
                };
                self.handle_key(KeyEvent::new(key, KeyModifiers::NONE));
                return;
            }
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if mouse.row > 0
                    && mouse.row < height.saturating_sub(1)
                    && mouse.column.abs_diff(separator) <= 1 =>
            {
                self.mailbox_resizing = true
            }
            MouseEventKind::Drag(MouseButton::Left) if self.mailbox_resizing => {
                self.mailbox_width_percent = mouse_width_percent(mouse.column, width);
            }
            MouseEventKind::Up(MouseButton::Left) if self.mailbox_resizing => {
                self.mailbox_resizing = false;
                self.mailbox_width_percent = mouse_width_percent(mouse.column, width);
                self.save_panel_width(crate::config::PanelWidth::Mailbox);
            }
            _ => {}
        }
    }
}
