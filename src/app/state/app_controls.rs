//! Refresh and numeric command-prefix handling
use super::*;

impl App {
    pub(super) fn refresh(&mut self) {
        let Some(account) = self.account_mut() else {
            return;
        };
        // JMAP refresh is read-only and already in progress. Do not abandon
        // its correlated response just to enqueue the same refresh again.
        if account.jmap
            && (account.connecting
                || account.mailboxes_loading
                || account
                    .page_inflight
                    .as_ref()
                    .is_some_and(|(_, offset)| *offset == 0))
        {
            return;
        }
        // A reconnect precedes all SELECTs. Keeping the old cached page on
        // screen avoids throwing away offline mail when the network is down.
        // Supersede any older page response, but retain cached rows/bodies.
        account.page_inflight = None;
        account.page_request_id = None;
        account.messages_loading = false;
        account.restore_message_index = Some(account.message_index);
        if account.request_connect() {
            self.status = StatusLine::info("reconnecting to mail server…");
        }
    }

    pub(super) fn push_count_digit(&mut self, digit: u8) {
        if digit == 0 && self.count_prefix.is_none() {
            return;
        }
        let next = self
            .count_prefix
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(usize::from(digit))
            .min(MAX_COUNT_PREFIX);
        self.count_prefix = Some(next);
    }

    pub(super) fn take_count(&mut self, default: usize) -> usize {
        self.count_prefix.take().unwrap_or(default).max(1)
    }
}
