//! Smooth scrolling and animation scheduling
use super::*;

impl App {
    /// Keep smooth scrolling at the normal animation rate, but check idle PDF
    /// conversions less often to avoid redrawing the whole TUI 100 times/sec.
    #[must_use]
    pub fn animation_frame_interval(&self) -> Duration {
        if self.attachment_picker_visible
            && self.attachment_smooth_target.is_none()
            && self
                .attachment_destination_picker
                .as_ref()
                .is_none_or(|picker| picker.smooth_target.is_none())
            && (self.attachment_preview_due.is_some()
                || !self.attachment_preview_inflight.is_empty())
        {
            return Duration::from_millis(40);
        }
        if self
            .composer
            .as_ref()
            .and_then(|composer| composer.file_picker.as_ref())
            .is_some_and(|picker| {
                picker.smooth_target.is_none()
                    && (picker.pdf_preview_due.is_some() || !picker.pdf_preview_inflight.is_empty())
            })
        {
            Duration::from_millis(60)
        } else {
            Duration::from_millis(10) // 100 FPS for smooth cursor animation.
        }
    }

    #[must_use]
    pub fn animation_active(&self) -> bool {
        if let Some(confirmation) = &self.detach_confirmation {
            return confirmation.review.active();
        }
        if let Some(confirmation) = &self.confirmation {
            return confirmation.review.active();
        }
        if let Some(modal) = &self.contacts_modal {
            if modal.confirm_delete {
                return modal.delete_review.active();
            }
            // A modal pauses animations behind it. Otherwise an existing mail
            // scroll could keep redrawing contacts at 100 FPS indefinitely.
            return modal.smooth_target.is_some();
        }
        if let Some(composer) = self.composer.as_ref()
            && composer.confirm_send
        {
            return !composer.help_visible && composer.send_preview.navigation.active();
        }
        self.composer
            .as_ref()
            .is_some_and(|composer| composer.body_smooth_target.is_some())
            || (self.attachment_picker_visible
                && (self.attachment_smooth_target.is_some()
                    || self.attachment_preview_due.is_some()
                    || !self.attachment_preview_inflight.is_empty()
                    || self
                        .attachment_destination_picker
                        .as_ref()
                        .is_some_and(|picker| picker.smooth_target.is_some())))
            || self
                .composer
                .as_ref()
                .and_then(|composer| composer.file_picker.as_ref())
                .is_some_and(|picker| {
                    picker.smooth_target.is_some()
                        || picker.pdf_preview_due.is_some()
                        || !picker.pdf_preview_inflight.is_empty()
                })
            || self.reader_navigation.smooth_target.is_some()
            || self.account().is_some_and(|account| {
                account.smooth_message_target.is_some()
                    || account.smooth_mailbox_target.is_some()
                    || account.smooth_preview_target.is_some()
            })
    }

    /// Advance one row of a pending smooth-scroll animation. Message targets
    /// are positions inside the filtered list; mailbox targets deliberately do
    /// not open intermediate folders, so Ctrl-Shift-j/k stays responsive.
    pub fn advance_animation(&mut self) {
        if let Some(confirmation) = self.detach_confirmation.as_mut() {
            confirmation.review.advance();
            return;
        }
        if let Some(confirmation) = self.confirmation.as_mut() {
            confirmation.review.advance();
            return;
        }
        if let Some(modal) = self.contacts_modal.as_mut() {
            if modal.confirm_delete {
                modal.delete_review.advance();
                return;
            }
            let length = modal.matches.len();
            modal.advance(length);
            return;
        }
        if let Some(composer) = self.composer.as_mut()
            && composer.confirm_send
        {
            if !composer.help_visible {
                composer.send_preview.advance();
            }
            return;
        }
        if let Some(composer) = self.composer.as_mut()
            && composer.body_smooth_target.is_some()
        {
            composer.advance_body_smooth_scroll();
            return;
        }
        if self.attachment_picker_visible {
            if let Some(picker) = &mut self.attachment_destination_picker {
                if picker.smooth_target.is_some() {
                    picker.advance_smooth_scroll();
                }
            } else if let Some(target) = self.attachment_smooth_target {
                let next = self.attachment_selection.saturating_add_signed(
                    if target > self.attachment_selection {
                        1
                    } else {
                        -1
                    },
                );
                // Move the visual cursor, but don't start a decode at every
                // intermediate row. Render only the destination thumbnail.
                self.attachment_selection = next;
                self.attachment_preview = None;
                self.attachment_preview_identity = None;
                if next == target {
                    self.attachment_smooth_target = None;
                    self.queue_attachment_preview();
                }
            }
            self.advance_attachment_preview();
            return;
        }
        if let Some(picker) = self
            .composer
            .as_mut()
            .and_then(|composer| composer.file_picker.as_mut())
        {
            // Advance one row and check PDF completions on each scheduled frame.
            // Neither operation waits for the PDF renderer. If the picker is
            // idle, let animations beneath the popup settle normally.
            if picker.smooth_target.is_some()
                || picker.pdf_preview_due.is_some()
                || !picker.pdf_preview_inflight.is_empty()
            {
                if picker.smooth_target.is_some() {
                    picker.advance_smooth_scroll();
                }
                picker.advance_pdf_preview();
                return;
            }
        }
        let focus = self.focus;
        if focus == Focus::Preview {
            let mut navigation = self.reader_navigation;
            let moved = navigation.advance(
                self.reader_display_text().unwrap_or_default(),
                self.reader_line_count,
            );
            self.reader_navigation = navigation;
            if moved {
                self.keep_reader_cursor_visible();
            }
            return;
        }

        let preview_split = self.preview_split;
        let mut mailbox_finished = false;
        let Some(account) = self.account_mut() else {
            return;
        };

        if let Some(target) = account.smooth_mailbox_target {
            if account.mailbox_index < target {
                account.mailbox_index += 1;
            } else if account.mailbox_index > target {
                account.mailbox_index = account.mailbox_index.saturating_sub(1);
            }
            account.keep_mailbox_cursor_visible();
            if account.mailbox_index == target {
                account.smooth_mailbox_target = None;
                mailbox_finished = true;
            }
        } else {
            match focus {
                Focus::Messages | Focus::Mailboxes => {
                    let Some(target_position) = account.smooth_message_target else {
                        return;
                    };
                    let visible = account.visible_message_indices();
                    if visible.is_empty() {
                        account.smooth_message_target = None;
                        return;
                    }
                    let current = visible
                        .iter()
                        .position(|index| *index == account.message_index)
                        .unwrap_or(0);
                    let next = if current < target_position {
                        current + 1
                    } else if current > target_position {
                        current.saturating_sub(1)
                    } else {
                        current
                    };
                    account.message_index = visible[next.min(visible.len() - 1)];
                    account.preview_scroll = 0;
                    account.keep_message_cursor_visible();
                    let remaining = visible.len().saturating_sub(next.saturating_add(1));
                    let prefetch_distance = account
                        .message_view_height
                        .saturating_mul(4)
                        .max(account.page_size);
                    if account.remaining_for_pagination(remaining) <= prefetch_distance {
                        let _ = account.request_next_page();
                    }
                    // Large windows already cover intermediate rows. Do not
                    // interrupt an active IMAP batch every few animation frames.
                    if next % PREFETCH_WINDOW_REARM_ROWS == 0 || next == target_position {
                        account.prefetch_near_cursor(if target_position >= current {
                            1
                        } else {
                            -1
                        });
                    }
                    if next == target_position {
                        account.smooth_message_target = None;
                        if preview_split {
                            account.request_selected_body();
                            Self::mark_cached_selected_seen(account);
                        }
                    }
                }
                Focus::Preview => {
                    unreachable!("reader animation is handled before borrowing account state")
                }
            }
        }

        if mailbox_finished {
            Self::activate_selected_mailbox(account, preview_split);
        }
    }
}
