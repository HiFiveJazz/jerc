use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{Command, Focus, HELP_ENTRIES, command_for_key};
use crate::config::Config;
use crate::model::{
    Capabilities, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId, MessageSummary,
    OutgoingMessage,
};
use crate::theme::{Theme, THEMES, persist_theme, persisted_theme, theme_index};
use crate::worker::{ActionKind, WorkerEvent, WorkerEventKind, WorkerHandle};

const SMOOTH_SCROLL_ROWS: isize = 22;
const MAX_COUNT_PREFIX: usize = 99_999;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    To,
    Cc,
    Subject,
    Body,
}

impl ComposeField {
    const fn next(self) -> Self {
        match self {
            Self::To => Self::Cc,
            Self::Cc => Self::Subject,
            Self::Subject => Self::Body,
            Self::Body => Self::To,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::To => Self::Body,
            Self::Cc => Self::To,
            Self::Subject => Self::Cc,
            Self::Body => Self::Subject,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Composer {
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub body: String,
    pub field: ComposeField,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub sending: bool,
}

impl Composer {
    fn new() -> Self {
        Self {
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            body: String::new(),
            field: ComposeField::To,
            in_reply_to: None,
            references: Vec::new(),
            sending: false,
        }
    }

    fn reply(message: &MessageSummary, body: Option<&MessageBody>, prefer_html: bool) -> Self {
        let subject = if message.subject.to_ascii_lowercase().starts_with("re:") {
            message.subject.clone()
        } else {
            format!("Re: {}", message.subject)
        };
        let mut references = message.references.clone();
        if let Some(id) = &message.message_id {
            if references.last() != Some(id) {
                references.push(id.clone());
            }
        }
        let quoted = body.map_or_else(String::new, |body| {
            body.display_text(prefer_html)
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        });
        let body = if quoted.is_empty() {
            "\n\n".to_owned()
        } else {
            format!("\n\nOn the previous message, {} wrote:\n{quoted}\n", display_sender(message))
        };
        Self {
            to: if message.from_name.is_empty() {
                message.from_address.clone()
            } else {
                format!("{} <{}>", message.from_name, message.from_address)
            },
            cc: String::new(),
            subject,
            body,
            field: ComposeField::Body,
            in_reply_to: message.message_id.clone(),
            references,
            sending: false,
        }
    }

    fn forward(message: &MessageSummary, body: Option<&MessageBody>, prefer_html: bool) -> Self {
        let subject = if message.subject.to_ascii_lowercase().starts_with("fwd:") {
            message.subject.clone()
        } else {
            format!("Fwd: {}", message.subject)
        };
        let forwarded = body.map_or_else(String::new, |body| {
            body.display_text(prefer_html).to_owned()
        });
        let body = if forwarded.trim().is_empty() {
            "\n\n---------- Forwarded message ----------\n".to_owned()
        } else {
            format!(
                "\n\n---------- Forwarded message ----------\nFrom: {}\nSubject: {}\n\n{}\n",
                display_sender(message),
                message.subject,
                forwarded
            )
        };
        Self {
            to: String::new(),
            cc: String::new(),
            subject,
            body,
            field: ComposeField::To,
            in_reply_to: None,
            references: Vec::new(),
            sending: false,
        }
    }

    fn field_mut(&mut self) -> &mut String {
        match self.field {
            ComposeField::To => &mut self.to,
            ComposeField::Cc => &mut self.cc,
            ComposeField::Subject => &mut self.subject,
            ComposeField::Body => &mut self.body,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeIntent {
    Reply,
    Forward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCompose {
    account: usize,
    message_id: MessageId,
    intent: ComposeIntent,
}

pub struct AccountState {
    pub name: String,
    pub default_mailbox: String,
    pub from: String,
    pub outgoing_configured: bool,
    pub capabilities: Option<Capabilities>,
    pub mailboxes: Vec<Mailbox>,
    pub mailbox_index: usize,
    pub current_mailbox: Option<String>,
    pub messages: Vec<MessageSummary>,
    pub message_index: usize,
    pub preview_scroll: u16,
    pub smooth_message_target: Option<usize>,
    pub smooth_preview_target: Option<u16>,
    pub preview_max_scroll: u16,
    pub prefer_html: bool,
    pub bodies: HashMap<MessageId, MessageBody>,
    pub pending_bodies: HashSet<MessageId>,
    pub pending_seen: HashSet<MessageId>,
    pub mailbox_status_queue: VecDeque<String>,
    pub mailbox_status_inflight: bool,
    pub connecting: bool,
    pub mailboxes_loading: bool,
    pub messages_loading: bool,
    pub last_scan_micros: Option<u128>,
    pub restore_message_index: Option<usize>,
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
        self.selected_message()
            .and_then(|msg| self.bodies.get(&msg.id))
    }

    #[must_use]
    pub fn is_loading_body(&self) -> bool {
        self.selected_message()
            .is_some_and(|message| self.pending_bodies.contains(&message.id))
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
            // Only the newest requested reader body matters. The worker coalesces
            // queued body fetches to the newest opened selection; mirror that here so
            // discarded requests cannot leave a permanent "loading" marker.
            self.pending_bodies.clear();
            self.pending_bodies.insert(id);
        }
    }

    fn request_seen(&mut self, id: MessageId, seen: bool) {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return;
        };
        if self.pending_seen.contains(&id) {
            return;
        }
        if self
            .worker
            .post(ActionKind::SetSeen {
                mailbox,
                id: id.clone(),
                seen,
            })
            .is_ok()
        {
            self.pending_seen.insert(id);
        }
    }
}

pub struct App {
    pub accounts: Vec<AccountState>,
    pub account_index: usize,
    pub focus: Focus,
    pub status: StatusLine,
    pub should_quit: bool,
    pub mailbox_width_percent: u16,
    pub message_open: bool,
    pub help_visible: bool,
    pub help_query: String,
    pub help_selected: usize,
    pub help_scroll: usize,
    pub theme_picker_visible: bool,
    pub theme_query: String,
    pub theme_selected: usize,
    pub theme_scroll: usize,
    pub theme_index: usize,
    theme_before_picker: usize,
    pub count_prefix: Option<usize>,
    pub composer: Option<Composer>,
    pending_compose: Option<PendingCompose>,
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
                from: config.from.clone().unwrap_or_default(),
                outgoing_configured: config.outgoing.is_some(),
                capabilities: None,
                mailboxes: Vec::new(),
                mailbox_index: 0,
                current_mailbox: None,
                messages: Vec::new(),
                message_index: 0,
                preview_scroll: 0,
                smooth_message_target: None,
                smooth_preview_target: None,
                preview_max_scroll: 0,
                prefer_html: false,
                bodies: HashMap::new(),
                pending_bodies: HashSet::new(),
                pending_seen: HashSet::new(),
                mailbox_status_queue: VecDeque::new(),
                mailbox_status_inflight: false,
                connecting: false,
                mailboxes_loading: false,
                messages_loading: false,
                last_scan_micros: None,
                restore_message_index: None,
                worker,
            })
            .collect();
        let configured_theme = persisted_theme().unwrap_or_else(|| config.ui.theme.clone());
        let theme_index = theme_index(&configured_theme);
        Self {
            accounts,
            account_index: 0,
            focus: Focus::Messages,
            status: StatusLine::info("starting workers…"),
            should_quit: false,
            mailbox_width_percent: config.ui.mailbox_width_percent.clamp(20, 50),
            message_open: false,
            help_visible: false,
            help_query: String::new(),
            help_selected: 0,
            help_scroll: 0,
            theme_picker_visible: false,
            theme_query: String::new(),
            theme_selected: theme_index,
            theme_scroll: 0,
            theme_index,
            theme_before_picker: theme_index,
            count_prefix: None,
            composer: None,
            pending_compose: None,
        }
    }

    #[must_use]
    pub fn account(&self) -> Option<&AccountState> {
        self.accounts.get(self.account_index)
    }

    pub fn account_mut(&mut self) -> Option<&mut AccountState> {
        self.accounts.get_mut(self.account_index)
    }

    #[must_use]
    pub fn theme(&self) -> &'static Theme {
        &THEMES[self.theme_index.min(THEMES.len().saturating_sub(1))]
    }

    pub fn bootstrap(&mut self) {
        for account in &mut self.accounts {
            if account.worker.post(ActionKind::Connect).is_ok() {
                account.connecting = true;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
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
        self.handle_command(command_for_key(key));
    }

    pub fn handle_worker_event(&mut self, event: WorkerEvent) {
        let Some(account) = self.accounts.get_mut(event.account) else {
            return;
        };
        match event.kind {
            WorkerEventKind::Connected { capabilities } => {
                account.connecting = false;
                account.capabilities = Some(capabilities);
                if account.worker.post(ActionKind::ListMailboxes).is_ok() {
                    account.mailboxes_loading = true;
                }
                self.status = StatusLine::info(format!("{} connected", account.name));
            }
            WorkerEventKind::Mailboxes(mailboxes) => {
                account.mailboxes_loading = false;
                account.mailboxes = mailboxes;
                account.mailbox_index = account
                    .mailboxes
                    .iter()
                    .position(|m| m.name == account.default_mailbox)
                    .unwrap_or(0);
                account.clamp_selection();
                account.mailbox_status_queue = account
                    .mailboxes
                    .iter()
                    .filter(|mailbox| !mailbox.counts_known)
                    .map(|mailbox| mailbox.name.clone())
                    .collect();
                if let Some(mailbox) = account.selected_mailbox() {
                    let name = mailbox.name.clone();
                    if account.worker.post(ActionKind::OpenMailbox(name)).is_ok() {
                        account.messages_loading = true;
                    }
                } else {
                    Self::pump_mailbox_status(account);
                }
            }
            WorkerEventKind::MailboxStatus {
                name,
                total,
                unread,
            } => {
                account.mailbox_status_inflight = false;
                if let Some(mailbox) = account.mailboxes.iter_mut().find(|m| m.name == name) {
                    mailbox.total = total;
                    mailbox.unread = unread;
                    mailbox.counts_known = true;
                }
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::MailboxOpened {
                name,
                messages,
                elapsed_micros,
            } => {
                account.messages_loading = false;
                account.current_mailbox = Some(name.clone());
                account.messages = messages;
                account.message_index = account
                    .restore_message_index
                    .take()
                    .unwrap_or(0)
                    .min(account.messages.len().saturating_sub(1));
                account.preview_scroll = 0;
                account.preview_max_scroll = 0;
                account.smooth_message_target = None;
                account.smooth_preview_target = None;
                account.bodies.clear();
                account.pending_bodies.clear();
                account.pending_seen.clear();
                account.last_scan_micros = Some(elapsed_micros);
                account.clamp_selection();
                self.message_open = false;
                self.focus = Focus::Messages;
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
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::BodyLoaded(body) => {
                let id = body.id.clone();
                account.pending_bodies.remove(&id);
                account.bodies.insert(id.clone(), body);

                let pending = self.pending_compose.as_ref().filter(|pending| {
                    pending.account == event.account && pending.message_id == id
                });
                let composer = pending.and_then(|pending| {
                    let message = account
                        .messages
                        .iter()
                        .find(|message| message.id == pending.message_id)?;
                    let body = account.bodies.get(&pending.message_id);
                    Some(match pending.intent {
                        ComposeIntent::Reply => {
                            Composer::reply(message, body, account.prefer_html)
                        }
                        ComposeIntent::Forward => {
                            Composer::forward(message, body, account.prefer_html)
                        }
                    })
                });

                if self.message_open
                    && account
                        .selected_message()
                        .is_some_and(|message| message.id == id && message.unread())
                {
                    account.request_seen(id, true);
                }
                Self::pump_mailbox_status(account);

                if let Some(composer) = composer {
                    self.pending_compose = None;
                    self.composer = Some(composer);
                    self.status = StatusLine::info("ready to compose");
                }
            }
            WorkerEventKind::MessageExported { path } => {
                self.status = StatusLine::temporary(
                    format!("raw message exported to {}", path.display()),
                    Duration::from_secs(8),
                );
            }
            WorkerEventKind::SeenChanged { id, seen } => {
                account.pending_seen.remove(&id);
                let mut unread_delta = 0isize;
                if let Some(message) = account.messages.iter_mut().find(|message| message.id == id) {
                    let was_unread = message.unread();
                    message.flags.set(MessageFlags::SEEN, seen);
                    let is_unread = message.unread();
                    unread_delta = match (was_unread, is_unread) {
                        (true, false) => -1,
                        (false, true) => 1,
                        _ => 0,
                    };
                }
                if unread_delta != 0 {
                    if let Some(current) = account.current_mailbox.as_deref() {
                        if let Some(mailbox) = account
                            .mailboxes
                            .iter_mut()
                            .find(|mailbox| mailbox.name == current && mailbox.counts_known)
                        {
                            mailbox.unread = mailbox.unread.saturating_add_signed(unread_delta);
                        }
                    }
                }
            }
            WorkerEventKind::MutationComplete => {
                if let Some(name) = account.current_mailbox.clone() {
                    if account.worker.post(ActionKind::OpenMailbox(name)).is_ok() {
                        account.messages_loading = true;
                    }
                    if account.worker.post(ActionKind::ListMailboxes).is_ok() {
                        account.mailboxes_loading = true;
                    }
                }
            }
            WorkerEventKind::MessageSent => {
                if event.account == self.account_index {
                    self.composer = None;
                }
                self.status = StatusLine::temporary("message sent", Duration::from_secs(4));
            }
            WorkerEventKind::Error(error) => {
                account.connecting = false;
                account.mailboxes_loading = false;
                account.messages_loading = false;
                account.mailbox_status_inflight = false;
                account.pending_bodies.clear();
                account.pending_seen.clear();
                if self
                    .pending_compose
                    .as_ref()
                    .is_some_and(|pending| pending.account == event.account)
                {
                    self.pending_compose = None;
                }
                if event.account == self.account_index {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.sending = false;
                    }
                }
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
            Command::FocusNext => {
                self.count_prefix = None;
                self.focus = self.focus.next(self.message_open);
            }
            Command::FocusPrevious => {
                self.count_prefix = None;
                self.focus = self.focus.previous(self.message_open);
            }
            Command::NextAccount => self.change_account(1),
            Command::PreviousAccount => self.change_account(-1),
            Command::Down => {
                let count = self.take_count(1);
                self.move_selection(isize::try_from(count).unwrap_or(isize::MAX));
            }
            Command::Up => {
                let count = self.take_count(1);
                self.move_selection(-isize::try_from(count).unwrap_or(isize::MAX));
            }
            Command::MessageDown => {
                let count = self.take_count(1);
                self.move_message_list(isize::try_from(count).unwrap_or(isize::MAX));
            }
            Command::MessageUp => {
                let count = self.take_count(1);
                self.move_message_list(-isize::try_from(count).unwrap_or(isize::MAX));
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
            Command::ToggleSeen => {
                self.count_prefix = None;
                self.toggle_seen();
            }
            Command::Delete => {
                self.count_prefix = None;
                self.delete_selected();
            }
            Command::Archive => {
                self.count_prefix = None;
                self.archive_selected();
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
            Command::Compose => {
                self.count_prefix = None;
                self.begin_compose();
            }
            Command::Reply => {
                self.count_prefix = None;
                self.begin_reply();
            }
            Command::Forward => {
                self.count_prefix = None;
                self.begin_forward();
            }
            Command::Escape => {
                self.count_prefix = None;
                if self.message_open {
                    self.close_message();
                } else {
                    self.focus = Focus::Messages;
                }
            }
            Command::Digit(digit) => self.push_count_digit(digit),
            Command::None => {}
        }
    }

    #[must_use]
    pub fn animation_active(&self) -> bool {
        self.account().is_some_and(|account| {
            account.smooth_message_target.is_some() || account.smooth_preview_target.is_some()
        })
    }

    /// Advance one visual row of a pending Ctrl-j/Ctrl-k animation. Additional
    /// key presses adjust the target rather than queueing independent animations,
    /// so reversing direction is immediate.
    pub fn advance_animation(&mut self) {
        let focus = self.focus;
        let Some(account) = self.account_mut() else {
            return;
        };
        match focus {
            Focus::Messages => {
                let Some(target) = account.smooth_message_target else {
                    return;
                };
                if account.message_index < target {
                    account.message_index += 1;
                } else if account.message_index > target {
                    account.message_index = account.message_index.saturating_sub(1);
                }
                account.preview_scroll = 0;
                if account.message_index == target {
                    account.smooth_message_target = None;
                }
            }
            Focus::Preview => {
                let Some(target) = account.smooth_preview_target else {
                    return;
                };
                if account.preview_scroll < target {
                    account.preview_scroll = account.preview_scroll.saturating_add(1);
                } else if account.preview_scroll > target {
                    account.preview_scroll = account.preview_scroll.saturating_sub(1);
                }
                if account.preview_scroll == target {
                    account.smooth_preview_target = None;
                }
            }
            Focus::Mailboxes => account.smooth_message_target = None,
        }
    }

    fn pump_mailbox_status(account: &mut AccountState) {
        if account.mailbox_status_inflight
            || account.messages_loading
            || !account.pending_bodies.is_empty()
        {
            return;
        }
        while let Some(name) = account.mailbox_status_queue.pop_front() {
            if account
                .mailboxes
                .iter()
                .find(|mailbox| mailbox.name == name)
                .is_some_and(|mailbox| mailbox.counts_known)
            {
                continue;
            }
            if account
                .worker
                .post(ActionKind::MailboxStatus(name.clone()))
                .is_ok()
            {
                account.mailbox_status_inflight = true;
            }
            break;
        }
    }

    fn change_account(&mut self, delta: isize) {
        if self.accounts.is_empty() {
            return;
        }
        self.count_prefix = None;
        self.message_open = false;
        self.pending_compose = None;
        self.focus = Focus::Messages;
        self.account_index = offset_index(self.account_index, self.accounts.len(), delta);
    }

    fn move_selection(&mut self, delta: isize) {
        let focus = self.focus;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.smooth_message_target = None;
        account.smooth_preview_target = None;
        match focus {
            Focus::Mailboxes => {
                account.mailbox_index =
                    bounded_index(account.mailbox_index, account.mailboxes.len(), delta);
            }
            Focus::Messages => {
                let previous = account.message_index;
                account.message_index =
                    bounded_index(account.message_index, account.messages.len(), delta);
                if account.message_index != previous {
                    account.preview_scroll = 0;
                }
            }
            Focus::Preview => {
                account.preview_scroll = bounded_scroll_to(account.preview_scroll, delta, account.preview_max_scroll);
            }
        }
    }

    fn move_message_list(&mut self, delta: isize) {
        if self.message_open {
            return;
        }
        let Some(account) = self.account_mut() else {
            return;
        };
        account.smooth_message_target = None;
        let previous = account.message_index;
        account.message_index = bounded_index(account.message_index, account.messages.len(), delta);
        if account.message_index != previous {
            account.preview_scroll = 0;
        }
    }

    /// Called by the renderer after wrapping the current message. Keeping the
    /// exact visual bound in state prevents invisible overscroll, which used to
    /// make Ctrl-k appear delayed after Ctrl-j reached the bottom of a message.
    pub fn set_preview_max_scroll(&mut self, max_scroll: u16) {
        let Some(account) = self.account_mut() else {
            return;
        };
        account.preview_max_scroll = max_scroll;
        account.preview_scroll = account.preview_scroll.min(max_scroll);
        account.smooth_preview_target = account
            .smooth_preview_target
            .map(|target| target.min(max_scroll))
            .filter(|target| *target != account.preview_scroll);
    }

    fn start_smooth_scroll(&mut self, delta: isize) {
        let focus = self.focus;
        let Some(account) = self.account_mut() else {
            return;
        };
        match focus {
            Focus::Messages => {
                if account.messages.is_empty() {
                    return;
                }
                let base = account
                    .smooth_message_target
                    .unwrap_or(account.message_index);
                account.smooth_message_target = Some(bounded_index(base, account.messages.len(), delta));
            }
            Focus::Preview => {
                let base = account
                    .smooth_preview_target
                    .unwrap_or(account.preview_scroll)
                    .min(account.preview_max_scroll);
                let target = bounded_scroll_to(base, delta, account.preview_max_scroll);
                account.smooth_preview_target = (target != account.preview_scroll).then_some(target);
            }
            Focus::Mailboxes => {
                account.mailbox_index =
                    bounded_index(account.mailbox_index, account.mailboxes.len(), delta.signum());
            }
        }
    }

    fn jump(&mut self, bottom: bool) {
        let focus = self.focus;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.smooth_message_target = None;
        account.smooth_preview_target = None;
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
            }
            Focus::Preview => {
                account.preview_scroll = if bottom { account.preview_max_scroll } else { 0 };
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
                if account.worker.post(ActionKind::OpenMailbox(name)).is_ok() {
                    account.messages_loading = true;
                }
                self.message_open = false;
                self.focus = Focus::Messages;
            }
            Focus::Messages => {
                if self.account().is_none_or(|account| account.selected_message().is_none()) {
                    return;
                }
                self.message_open = true;
                if let Some(account) = self.account_mut() {
                    account.preview_scroll = 0;
                    account.preview_max_scroll = 0;
                    // Prefer the sender's text/plain alternative for each newly
                    // opened message. HTML-only mail still falls back to the
                    // rendered HTML body automatically; `h` is per-reader opt-in.
                    account.prefer_html = false;
                    account.request_selected_body();
                    Self::mark_cached_selected_seen(account);
                }
                self.focus = Focus::Preview;
            }
            Focus::Preview => {}
        }
    }

    fn close_message(&mut self) {
        self.message_open = false;
        self.focus = Focus::Messages;
        if let Some(account) = self.account_mut() {
            account.smooth_preview_target = None;
            account.preview_scroll = 0;
        }
    }

    fn mark_cached_selected_seen(account: &mut AccountState) {
        let Some(id) = account
            .selected_message()
            .filter(|message| message.unread() && account.bodies.contains_key(&message.id))
            .map(|message| message.id.clone())
        else {
            return;
        };
        account.request_seen(id, true);
    }

    fn toggle_seen(&mut self) {
        let Some(account) = self.account_mut() else {
            return;
        };
        let Some((id, seen)) = account
            .selected_message()
            .map(|message| (message.id.clone(), !message.flags.contains(MessageFlags::SEEN)))
        else {
            return;
        };
        account.request_seen(id, seen);
    }

    fn delete_selected(&mut self) {
        let result = {
            let Some(account) = self.accounts.get_mut(self.account_index) else {
                return;
            };
            let Some(source) = account.current_mailbox.clone() else {
                return;
            };
            let Some(id) = account.selected_message().map(|message| message.id.clone()) else {
                return;
            };
            let current_role = account
                .mailboxes
                .iter()
                .find(|mailbox| mailbox.name == source)
                .map(|mailbox| mailbox.role.clone());

            let action = if current_role == Some(MailboxRole::Trash) {
                ActionKind::Delete {
                    mailbox: source,
                    id,
                }
            } else if let Some(trash) = account
                .mailboxes
                .iter()
                .find(|mailbox| mailbox.role == MailboxRole::Trash)
                .map(|mailbox| mailbox.name.clone())
            {
                ActionKind::Move {
                    source,
                    destination: trash,
                    id,
                }
            } else {
                // Some servers do not advertise a Trash role. Preserve useful
                // delete semantics instead of making the key silently do nothing.
                ActionKind::Delete {
                    mailbox: source,
                    id,
                }
            };

            account.restore_message_index = Some(account.message_index);
            match account.worker.post(action) {
                Ok(_) => true,
                Err(_) => {
                    account.restore_message_index = None;
                    false
                }
            }
        };

        if result && self.message_open {
            self.close_message();
        }
        self.status = if result {
            StatusLine::info("deleting message…")
        } else {
            StatusLine::error("mail worker queue is busy; delete not started")
        };
    }

    fn archive_selected(&mut self) {
        let prepared = self.account().and_then(|account| {
            let source = account.current_mailbox.clone()?;
            let id = account.selected_message()?.id.clone();
            let destination = account
                .mailboxes
                .iter()
                .find(|mailbox| mailbox.role == MailboxRole::Archive && mailbox.name != source)
                .map(|mailbox| mailbox.name.clone());
            Some((source, destination, id, account.message_index))
        });
        let Some((source, destination, id, restore_index)) = prepared else {
            return;
        };
        let Some(destination) = destination else {
            self.status = StatusLine::temporary(
                "this account has no advertised Archive/All Mail mailbox",
                Duration::from_secs(3),
            );
            return;
        };

        let started = {
            let Some(account) = self.accounts.get_mut(self.account_index) else {
                return;
            };
            account.restore_message_index = Some(restore_index);
            match account.worker.post(ActionKind::Move {
                source,
                destination,
                id,
            }) {
                Ok(_) => true,
                Err(_) => {
                    account.restore_message_index = None;
                    false
                }
            }
        };

        if started && self.message_open {
            self.close_message();
        }
        self.status = if started {
            StatusLine::info("archiving message…")
        } else {
            StatusLine::error("mail worker queue is busy; archive not started")
        };
    }

    fn open_theme_picker(&mut self) {
        self.theme_before_picker = self.theme_index;
        self.theme_picker_visible = true;
        self.theme_query.clear();
        self.theme_selected = self.theme_index;
        self.theme_scroll = 0;
    }

    fn open_help(&mut self) {
        self.help_visible = true;
        self.help_query.clear();
        self.help_selected = 0;
        self.help_scroll = 0;
    }

    #[must_use]
    pub fn help_matches(&self) -> Vec<usize> {
        let query = self.help_query.trim().to_ascii_lowercase();
        HELP_ENTRIES
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.is_empty()
                    || entry.category.to_ascii_lowercase().contains(&query)
                    || entry.key.to_ascii_lowercase().contains(&query)
                    || entry.description.to_ascii_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    #[must_use]
    pub fn theme_matches(&self) -> Vec<usize> {
        let query = self.theme_query.trim().to_ascii_lowercase();
        THEMES
            .iter()
            .enumerate()
            .filter(|(_, theme)| query.is_empty() || theme.name.to_ascii_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') if key.modifiers.is_empty() => {
                self.help_visible = false;
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
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.help_query.push(ch);
                self.reset_help_selection();
            }
            _ => {}
        }
    }

    fn handle_theme_picker_key(&mut self, key: KeyEvent) {
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
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.theme_query.push(ch);
                self.reset_theme_selection();
            }
            _ => {}
        }
    }

    fn reset_help_selection(&mut self) {
        let matches = self.help_matches();
        self.help_selected = matches.first().copied().unwrap_or(0);
        self.help_scroll = 0;
    }

    fn move_help_selection(&mut self, delta: isize) {
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

    fn reset_theme_selection(&mut self) {
        let matches = self.theme_matches();
        if let Some(index) = matches.first().copied() {
            self.theme_selected = index;
            self.theme_index = index;
        }
        self.theme_scroll = 0;
    }

    fn move_theme_selection(&mut self, delta: isize) {
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

    fn commit_theme_picker(&mut self) {
        self.theme_picker_visible = false;
        self.theme_query.clear();
        let name = self.theme().name;
        if let Err(error) = persist_theme(name) {
            self.status = StatusLine::error(format!("could not persist theme: {error:#}"));
        } else {
            self.status = StatusLine::temporary("theme saved", Duration::from_secs(2));
        }
    }

    fn cancel_theme_picker(&mut self) {
        self.theme_index = self.theme_before_picker;
        self.theme_selected = self.theme_before_picker;
        self.theme_query.clear();
        self.theme_picker_visible = false;
    }

    fn export_selected_raw(&mut self) {
        let path = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("aerc-rs")
            .join("debug-message.eml");
        let Some(account) = self.account_mut() else {
            return;
        };
        let Some(mailbox) = account.current_mailbox.clone() else {
            return;
        };
        let Some(id) = account.selected_message().map(|message| message.id.clone()) else {
            return;
        };
        match account.worker.post(ActionKind::ExportRaw { mailbox, id, path }) {
            Ok(_) => self.status = StatusLine::info("exporting raw message…"),
            Err(_) => {
                self.status = StatusLine::error("mail worker queue is busy; export not started");
            }
        }
    }

    fn toggle_html(&mut self) {
        let mode = {
            let Some(account) = self.account_mut() else {
                return;
            };
            if !account
                .selected_body()
                .is_some_and(MessageBody::has_html_alternative)
            {
                None
            } else {
                account.prefer_html = !account.prefer_html;
                Some(account.prefer_html)
            }
        };
        let Some(prefer_html) = mode else {
            self.status = StatusLine::temporary(
                "selected message has no HTML alternative",
                Duration::from_secs(2),
            );
            return;
        };
        self.status = StatusLine::temporary(
            if prefer_html {
                "HTML mail rendering enabled"
            } else {
                "plain-text mail rendering enabled"
            },
            Duration::from_secs(2),
        );
    }

    fn refresh(&mut self) {
        let Some(account) = self.account_mut() else {
            return;
        };
        if account.worker.post(ActionKind::ListMailboxes).is_ok() {
            account.mailboxes_loading = true;
        }
        if let Some(name) = account.current_mailbox.clone() {
            if account.worker.post(ActionKind::OpenMailbox(name)).is_ok() {
                account.messages_loading = true;
            }
        }
    }

    fn begin_compose(&mut self) {
        let Some(account) = self.account() else {
            return;
        };
        if !account.outgoing_configured {
            self.status = StatusLine::error(
                "no outgoing SMTP transport configured; add `outgoing = ...` to this account",
            );
            return;
        }
        self.composer = Some(Composer::new());
    }

    fn begin_reply(&mut self) {
        self.begin_compose_from_selected(ComposeIntent::Reply);
    }

    fn begin_forward(&mut self) {
        self.begin_compose_from_selected(ComposeIntent::Forward);
    }

    fn begin_compose_from_selected(&mut self, intent: ComposeIntent) {
        let account_index = self.account_index;
        let prepared = self.account().and_then(|account| {
            if !account.outgoing_configured {
                return None;
            }
            let message = account.selected_message()?;
            let body = account.selected_body();
            let composer = body.map(|body| match intent {
                ComposeIntent::Reply => Composer::reply(message, Some(body), account.prefer_html),
                ComposeIntent::Forward => Composer::forward(message, Some(body), account.prefer_html),
            });
            Some((message.id.clone(), composer))
        });

        if !self.account().is_some_and(|account| account.outgoing_configured) {
            self.status = StatusLine::error(
                "no outgoing SMTP transport configured; add `outgoing = ...` to this account",
            );
            return;
        }

        let Some((message_id, composer)) = prepared else {
            return;
        };
        if let Some(composer) = composer {
            self.composer = Some(composer);
            return;
        }

        let started = self.account_mut().is_some_and(|account| {
            account.request_selected_body();
            account.pending_bodies.contains(&message_id)
        });
        if started {
            self.pending_compose = Some(PendingCompose {
                account: account_index,
                message_id,
                intent,
            });
            self.status = StatusLine::info(match intent {
                ComposeIntent::Reply => "loading message for reply…",
                ComposeIntent::Forward => "loading message for forward…",
            });
        } else {
            self.status = StatusLine::error("message body could not be queued for loading");
        }
    }

    fn handle_composer_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.send_composer();
            return;
        }
        let sending = self.composer.as_ref().is_some_and(|composer| composer.sending);
        if key.code == KeyCode::Esc {
            if sending {
                self.status = StatusLine::temporary(
                    "send is already in progress",
                    Duration::from_secs(2),
                );
            } else {
                self.composer = None;
            }
            return;
        }
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        if composer.sending {
            return;
        }
        match key.code {
            KeyCode::Tab => composer.field = composer.field.next(),
            KeyCode::BackTab => composer.field = composer.field.previous(),
            KeyCode::Enter if composer.field != ComposeField::Body => {
                composer.field = composer.field.next();
            }
            KeyCode::Enter => composer.body.push('\n'),
            KeyCode::Backspace => {
                composer.field_mut().pop();
            }
            KeyCode::Char(ch)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                composer.field_mut().push(ch);
            }
            _ => {}
        }
    }

    fn send_composer(&mut self) {
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        if composer.sending {
            return;
        }
        if composer.to.trim().is_empty() {
            self.status = StatusLine::error("To cannot be empty");
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        let message = OutgoingMessage {
            from: account.from.clone(),
            to: composer.to.clone(),
            cc: composer.cc.clone(),
            subject: composer.subject.clone(),
            body: composer.body.clone(),
            in_reply_to: composer.in_reply_to.clone(),
            references: composer.references.clone(),
        };
        if account.worker.post(ActionKind::Send(message)).is_ok() {
            if let Some(composer) = self.composer.as_mut() {
                composer.sending = true;
            }
            self.status = StatusLine::info("sending message…");
        } else {
            self.status = StatusLine::error("mail worker queue is busy; message was not sent");
        }
    }

    fn push_count_digit(&mut self, digit: u8) {
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

    fn take_count(&mut self, default: usize) -> usize {
        self.count_prefix.take().unwrap_or(default).max(1)
    }
}

fn display_sender(message: &MessageSummary) -> String {
    if message.from_name.is_empty() {
        message.from_address.clone()
    } else {
        message.from_name.clone()
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

fn bounded_scroll_to(current: u16, delta: isize, max: u16) -> u16 {
    bounded_scroll(current.min(max), delta).min(max)
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
    use super::{bounded_index, bounded_scroll, bounded_scroll_to, offset_index};

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
    fn preview_scroll_never_moves_beyond_rendered_bottom() {
        assert_eq!(bounded_scroll_to(90, 22, 100), 100);
        assert_eq!(bounded_scroll_to(u16::MAX, -1, 100), 99);
        assert_eq!(bounded_scroll_to(0, -22, 100), 0);
    }

    #[test]
    fn account_navigation_wraps() {
        assert_eq!(offset_index(0, 3, -1), 2);
        assert_eq!(offset_index(2, 3, 1), 0);
    }
}
