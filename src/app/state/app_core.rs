//! Core account access and startup-facing state
use super::*;

impl App {
    #[must_use]
    pub fn draft_resume_prompt_open(&self) -> bool {
        self.pending_draft
            .as_ref()
            .is_some_and(|pending| pending.composer.is_some())
    }

    #[must_use]
    pub fn account(&self) -> Option<&AccountState> {
        self.accounts.get(self.account_index)
    }

    pub fn account_mut(&mut self) -> Option<&mut AccountState> {
        self.accounts.get_mut(self.account_index)
    }

    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.themes[self.theme_index.min(self.themes.len().saturating_sub(1))]
    }

    /// The active account's initial mailbox/connection work runs independently
    /// from the cached reader. Keep this a *display* signal, not a UI gate.
    #[must_use]
    pub fn syncing_with_server(&self) -> bool {
        self.account()
            .is_some_and(AccountState::syncing_with_server)
    }

    #[must_use]
    pub fn sync_spinner(&self) -> &'static str {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        FRAMES[self.sync_spinner_frame % FRAMES.len()]
    }

    pub fn advance_sync_spinner(&mut self) {
        self.sync_spinner_frame = self.sync_spinner_frame.wrapping_add(1);
    }

    #[must_use]
    pub fn inbox_poll_interval(&self) -> Duration {
        self.inbox_poll_interval
    }

    /// Terminal BEL is emitted in the main runtime, never from a worker.
    pub fn take_new_mail_bell(&mut self) -> bool {
        std::mem::take(&mut self.new_mail_bell_pending)
    }

    /// Only known count increases trigger notifications; never notify on initial sync.
    pub fn take_new_mail_notice(&mut self) -> Option<NewMailNotice> {
        self.new_mail_notices.pop_front()
    }

    /// Desktop banners and their associated sound; independent of the terminal BEL setting.
    pub fn notifications_enabled(&self) -> bool {
        self.new_mail_notifications_enabled
    }

    pub fn notification_icon_style(&self) -> crate::config::NotificationIconStyle {
        self.notification_icon_style
    }

    /// Count only arrivals detected during this session; older unread mail is
    /// not incorrectly presented as a new notification.
    pub fn new_mail_count(&self) -> usize {
        self.new_mail_since_launch
    }

    pub fn new_mail_sound_name(&self) -> &str {
        &self.new_mail_sound_name
    }
    pub fn new_mail_sound_file(&self) -> Option<&str> {
        self.new_mail_sound_file.as_deref()
    }

    pub fn bootstrap(&mut self) {
        self.reconnect_offline_accounts();
    }
}
