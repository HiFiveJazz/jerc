//! Mailbox navigation, viewport dimensions and account switching
use super::*;

impl App {
    pub(super) fn change_account(&mut self, delta: isize) {
        if self.accounts.is_empty() {
            return;
        }
        let target = offset_index(self.account_index, self.accounts.len(), delta);
        self.select_account(target);
    }

    pub(super) fn select_account(&mut self, index: usize) {
        if index >= self.accounts.len() || index == self.account_index {
            return;
        }
        if let Some(account) = self.account_mut() {
            // Each account retains its own live mailbox while inactive; do
            // not copy its preview bodies into an additional in-memory cache.
            account.persist_current_cache();
        }
        self.count_prefix = None;
        self.message_open = false;
        self.reader_smooth_target = None;
        self.reader_visual_linewise = false;
        self.pending_compose = None;
        self.pending_draft = None;
        self.visual_anchor = None;
        self.message_marked.clear();
        self.confirmation = None;
        self.focus = Focus::Messages;
        self.account_index = index;
        if let Some(account) = self.account_mut() {
            account.last_prefetch_window = None;
        }
        if let Some(account) = self.account() {
            self.status =
                StatusLine::temporary(format!("account: {}", account.name), Duration::from_secs(2));
        }
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        let focus = self.focus;
        if focus == Focus::Preview {
            self.move_reader_cursor(delta);
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
        let current = visible
            .iter()
            .position(|index| *index == account.message_index)
            .unwrap_or(0);
        let next = bounded_index(current, visible.len(), delta);
        let previous = account.message_index;
        account.message_index = visible[next];
        if account.message_index != previous {
            account.preview_scroll = 0;
            account.keep_message_cursor_visible();
            account.prefetch_near_cursor(delta);
            if preview_split {
                account.request_selected_body();
                Self::mark_cached_selected_seen(account);
            }
        }

        let remaining = visible.len().saturating_sub(next.saturating_add(1));
        let prefetch_distance = account
            .message_view_height
            .saturating_mul(4)
            .max(account.page_size)
            .max(PAGE_PREFETCH_MIN);
        if delta > 0 && account.remaining_for_pagination(remaining) <= prefetch_distance {
            let _ = account.request_next_page();
        }
    }

    pub(super) fn move_mailbox_list(&mut self, delta: isize) {
        if self.message_open {
            return;
        }
        let will_change = self.account().is_some_and(|account| {
            bounded_index(account.mailbox_index, account.mailboxes.len(), delta)
                != account.mailbox_index
        });
        if !will_change {
            return;
        }
        self.visual_anchor = None;
        self.message_marked.clear();
        let preview_split = self.preview_split;
        let Some(account) = self.account_mut() else {
            return;
        };
        let previous = account.mailbox_index;
        let next = bounded_index(previous, account.mailboxes.len(), delta);
        if next == previous {
            return;
        }
        account.save_current_cache();
        account.persist_current_cache();
        account.smooth_mailbox_target = None;
        account.mailbox_index = next;
        account.keep_mailbox_cursor_visible();
        Self::activate_selected_mailbox(account, preview_split);
    }

    pub(super) fn activate_selected_mailbox(account: &mut AccountState, preview_split: bool) {
        let Some(name) = account
            .selected_mailbox()
            .map(|mailbox| mailbox.name.clone())
        else {
            return;
        };
        account.smooth_message_target = None;
        account.smooth_preview_target = None;
        account.message_view_offset = 0;
        account.message_index = 0;
        account.search_query.clear();
        account.search_editing = false;
        account.native_search_editing = false;
        account.native_view = None;
        account.local_thread_view = None;
        account.reset_inline_threads();
        *account.local_thread_index.get_mut() = Default::default();
        account.queue_unknown_mailbox_statuses(Some(&name));
        if account.restore_cached_mailbox(&name) {
            // Don't trade instant rendering for stale data: refresh the first
            // page on the worker while the cached view stays on screen.
            let _ = account.request_mailbox_page(name, 0);
            account.prefetch_near_cursor(1);
            if preview_split {
                account.request_selected_body();
                Self::mark_cached_selected_seen(account);
            }
            Self::pump_mailbox_status(account);
            return;
        }
        account.current_mailbox = Some(name.clone());
        account.messages.clear();
        account.bodies.clear();
        account.pending_bodies.clear();
        account.body_request_id = None;
        account.last_prefetch_window = None;
        account.next_page_offset = 0;
        account.has_more = false;
        account.page_inflight = None;
        account.page_request_id = None;
        let _ = account.request_mailbox_page(name, 0);
    }

    pub fn set_mailbox_view_height(&mut self, height: usize) {
        let Some(account) = self.account_mut() else {
            return;
        };
        account.mailbox_view_height = height.max(1);
        account.keep_mailbox_cursor_visible();
    }

    pub fn set_message_view_height(&mut self, height: usize) {
        let Some(account) = self.account_mut() else {
            return;
        };
        account.message_view_height = height.max(1);
        account.keep_message_cursor_visible();
    }
}
