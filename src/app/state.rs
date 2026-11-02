use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::app::{Command, Focus};
use crate::config::Config;
use crate::model::{Capabilities, Mailbox, MessageBody, MessageFlags, MessageId, MessageSummary};
use crate::worker::{ActionKind, WorkerEvent, WorkerEventKind, WorkerHandle};

#[derive(Debug, Clone)]
pub struct StatusLine {
    pub text: String,
    pub is_error: bool,
    expires_at: Option<Instant>,
}

impl StatusLine {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            expires_at: None,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            expires_at: None,
        }
    }

    pub fn temporary(text: impl Into<String>, duration: Duration) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            expires_at: Some(Instant::now() + duration),
        }
    }

    #[must_use]
    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|deadline| Instant::now() >= deadline)
    }
}

pub struct AccountState {
    pub name: String,
    pub default_mailbox: String,
    pub capabilities: Option<Capabilities>,
    pub mailboxes: Vec<Mailbox>,
    pub mailbox_index: usize,
    pub current_mailbox: Option<String>,
    pub messages: Vec<MessageSummary>,
    pub message_index: usize,
    pub preview_scroll: u16,
    pub bodies: HashMap<MessageId, MessageBody>,
    pub pending_bodies: HashSet<MessageId>,
    pub last_scan_micros: Option<u128>,
    pub worker: WorkerHandle,
}

impl AccountState {
    #[must_use]
    pub fn selected_mailbox(&self) -> Option<&Mailbox> {
        self.mailboxes.get(self.mailbox_index)
    }

    #[must_use]
    pub fn selected_message(&self) -> Option<&MessageSummary> {
        self.messages.get(self.message_index)
    }

    #[must_use]
    pub fn selected_body(&self) -> Option<&MessageBody> {
        self.selected_message().and_then(|msg| self.bodies.get(&msg.id))
    }

    fn clamp_selection(&mut self) {
        self.mailbox_index = self
            .mailbox_index
            .min(self.mailboxes.len().saturating_sub(1));
        self.message_index = self
            .message_index
            .min(self.messages.len().saturating_sub(1));
    }

    fn request_selected_body(&mut self) {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return;
        };
        let Some(id) = self.selected_message().map(|m| m.id.clone()) else {
            return;
        };
        if self.bodies.contains_key(&id) || self.pending_bodies.contains(&id) {
            return;
        }
        if self
            .worker
            .post(ActionKind::FetchBody {
                mailbox,
                id: id.clone(),
            })
            .is_ok()
        {
            self.pending_bodies.insert(id);
        }
    }
}

pub struct App {
    pub accounts: Vec<AccountState>,
    pub account_index: usize,
    pub focus: Focus,
    pub status: StatusLine,
    pub should_quit: bool,
    pub sidebar_width: u16,
    pub message_width_percent: u16,
}

impl App {
    pub fn new(config: &Config, workers: Vec<WorkerHandle>) -> Self {
        let accounts = config
            .accounts
            .iter()
            .zip(workers)
            .map(|(config, worker)| AccountState {
                name: config.name.clone(),
                default_mailbox: config.default_mailbox.clone(),
                capabilities: None,
                mailboxes: Vec::new(),
                mailbox_index: 0,
                current_mailbox: None,
                messages: Vec::new(),
                message_index: 0,
                preview_scroll: 0,
                bodies: HashMap::new(),
                pending_bodies: HashSet::new(),
                last_scan_micros: None,
                worker,
            })
            .collect();
        Self {
            accounts,
            account_index: 0,
            focus: Focus::Messages,
            status: StatusLine::info("starting workers…"),
            should_quit: false,
            sidebar_width: config.ui.sidebar_width,
            message_width_percent: config.ui.message_width_percent,
        }
    }

    #[must_use]
    pub fn account(&self) -> Option<&AccountState> {
        self.accounts.get(self.account_index)
    }

    pub fn account_mut(&mut self) -> Option<&mut AccountState> {
        self.accounts.get_mut(self.account_index)
    }

    pub fn bootstrap(&mut self) {
        for account in &self.accounts {
            let _ = account.worker.post(ActionKind::Connect);
        }
    }

    pub fn handle_worker_event(&mut self, event: WorkerEvent) {
        let Some(account) = self.accounts.get_mut(event.account) else {
            return;
        };
        match event.kind {
            WorkerEventKind::Connected { capabilities } => {
                account.capabilities = Some(capabilities);
                let _ = account.worker.post(ActionKind::ListMailboxes);
                self.status = StatusLine::info(format!("{} connected", account.name));
            }
            WorkerEventKind::Mailboxes(mailboxes) => {
                account.mailboxes = mailboxes;
                account.mailbox_index = account
                    .mailboxes
                    .iter()
                    .position(|m| m.name == account.default_mailbox)
                    .unwrap_or(0);
                account.clamp_selection();
                if let Some(mailbox) = account.selected_mailbox() {
                    let _ = account
                        .worker
                        .post(ActionKind::OpenMailbox(mailbox.name.clone()));
                }
            }
            WorkerEventKind::MailboxOpened {
                name,
                messages,
                elapsed_micros,
            } => {
                account.current_mailbox = Some(name.clone());
                account.messages = messages;
                account.message_index = 0;
                account.preview_scroll = 0;
                account.bodies.clear();
                account.pending_bodies.clear();
                account.last_scan_micros = Some(elapsed_micros);
                account.clamp_selection();
                account.request_selected_body();
                self.status = StatusLine::temporary(
                    format!(
                        "{}: loaded {} messages from {} in {:.2} ms",
                        account.name,
                        account.messages.len(),
                        name,
                        elapsed_micros as f64 / 1000.0
                    ),
                    Duration::from_secs(4),
                );
            }
            WorkerEventKind::BodyLoaded(body) => {
                account.pending_bodies.remove(&body.id);
                account.bodies.insert(body.id.clone(), body);
            }
            WorkerEventKind::MutationComplete => {
                if let Some(name) = account.current_mailbox.clone() {
                    let _ = account.worker.post(ActionKind::OpenMailbox(name));
                    let _ = account.worker.post(ActionKind::ListMailboxes);
                }
            }
            WorkerEventKind::Error(error) => {
                self.status = StatusLine::error(format!("{}: {error}", account.name));
            }
            WorkerEventKind::Stopped => {}
        }
    }

    pub fn handle_command(&mut self, command: Command) {
        if self.status.expired() {
            self.status = StatusLine::info("ready");
        }
        match command {
            Command::Quit => self.should_quit = true,
            Command::FocusNext => self.focus = self.focus.next(),
            Command::FocusPrevious => self.focus = self.focus.previous(),
            Command::NextAccount => self.change_account(1),
            Command::PreviousAccount => self.change_account(-1),
            Command::Down => self.move_selection(1),
            Command::Up => self.move_selection(-1),
            Command::PageDown => self.move_selection(10),
            Command::PageUp => self.move_selection(-10),
            Command::Top => self.jump(false),
            Command::Bottom => self.jump(true),
            Command::Open => self.open_selection(),
            Command::ToggleSeen => self.toggle_seen(),
            Command::Refresh => self.refresh(),
            Command::None => {}
        }
    }

    fn change_account(&mut self, delta: isize) {
        if self.accounts.is_empty() {
            return;
        }
        self.account_index = offset_index(self.account_index, self.accounts.len(), delta);
    }

    fn move_selection(&mut self, delta: isize) {
        let focus = self.focus;
        let Some(account) = self.account_mut() else {
            return;
        };
        match focus {
            Focus::Mailboxes => {
                account.mailbox_index = bounded_index(account.mailbox_index, account.mailboxes.len(), delta);
            }
            Focus::Messages => {
                let previous = account.message_index;
                account.message_index =
                    bounded_index(account.message_index, account.messages.len(), delta);
                if account.message_index != previous {
                    account.preview_scroll = 0;
                }
                account.request_selected_body();
            }
            Focus::Preview => {
                account.preview_scroll = bounded_scroll(account.preview_scroll, delta);
            }
        }
    }

    fn jump(&mut self, bottom: bool) {
        let focus = self.focus;
        let Some(account) = self.account_mut() else {
            return;
        };
        match focus {
            Focus::Mailboxes => {
                account.mailbox_index = if bottom {
                    account.mailboxes.len().saturating_sub(1)
                } else {
                    0
                };
            }
            Focus::Messages => {
                account.message_index = if bottom {
                    account.messages.len().saturating_sub(1)
                } else {
                    0
                };
                account.preview_scroll = 0;
                account.request_selected_body();
            }
            Focus::Preview => {
                account.preview_scroll = if bottom { u16::MAX } else { 0 };
            }
        }
    }

    fn open_selection(&mut self) {
        match self.focus {
            Focus::Mailboxes => {
                let Some(account) = self.account_mut() else {
                    return;
                };
                let Some(name) = account.selected_mailbox().map(|m| m.name.clone()) else {
                    return;
                };
                let _ = account.worker.post(ActionKind::OpenMailbox(name));
                self.focus = Focus::Messages;
            }
            Focus::Messages => {
                if let Some(account) = self.account_mut() {
                    account.preview_scroll = 0;
                    account.request_selected_body();
                }
                self.focus = Focus::Preview;
            }
            Focus::Preview => self.focus = Focus::Messages,
        }
    }

    fn toggle_seen(&mut self) {
        let Some(account) = self.account_mut() else {
            return;
        };
        let Some(mailbox) = account.current_mailbox.clone() else {
            return;
        };
        let Some(message) = account.selected_message() else {
            return;
        };
        let seen = !message.flags.contains(MessageFlags::SEEN);
        let _ = account.worker.post(ActionKind::SetSeen {
            mailbox,
            id: message.id.clone(),
            seen,
        });
    }

    fn refresh(&mut self) {
        let Some(account) = self.account_mut() else {
            return;
        };
        let _ = account.worker.post(ActionKind::ListMailboxes);
        if let Some(name) = account.current_mailbox.clone() {
            let _ = account.worker.post(ActionKind::OpenMailbox(name));
        }
    }
}

fn bounded_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    current.saturating_add_signed(delta).min(len - 1)
}

fn bounded_scroll(current: u16, delta: isize) -> u16 {
    if delta >= 0 {
        current.saturating_add(u16::try_from(delta).unwrap_or(u16::MAX))
    } else {
        current.saturating_sub(u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX))
    }
}

fn offset_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as isize;
    ((current as isize + delta).rem_euclid(len)) as usize
}

#[cfg(test)]
mod tests {
    use super::{bounded_index, bounded_scroll, offset_index};

    #[test]
    fn list_navigation_is_bounded() {
        assert_eq!(bounded_index(0, 5, -1), 0);
        assert_eq!(bounded_index(3, 5, 10), 4);
        assert_eq!(bounded_index(0, 0, 1), 0);
    }

    #[test]
    fn preview_scroll_saturates() {
        assert_eq!(bounded_scroll(0, -1), 0);
        assert_eq!(bounded_scroll(10, 5), 15);
        assert_eq!(bounded_scroll(u16::MAX - 1, 10), u16::MAX);
    }

    #[test]
    fn account_navigation_wraps() {
        assert_eq!(offset_index(0, 3, -1), 2);
        assert_eq!(offset_index(2, 3, 1), 0);
    }
}
