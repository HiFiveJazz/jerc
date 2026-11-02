use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{Command, Focus, HELP_ENTRIES, command_for_key};
use crate::config::Config;
use crate::model::{
    Capabilities, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId, MessageSummary,
    OutgoingMessage,
};
use crate::theme::{Theme, load_themes, persist_theme, persisted_theme, theme_index};
use crate::worker::{ActionKind, WorkerEvent, WorkerEventKind, WorkerHandle};

const SMOOTH_SCROLL_ROWS: isize = 22;
const SMOOTH_MAILBOX_ROWS: isize = 5;
const MAX_COUNT_PREFIX: usize = 99_999;
const MESSAGE_SCROLL_OFF: usize = 4;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeMode {
    Normal,
    Insert,
    Visual,
}

impl ComposeMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Visual => "VISUAL",
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
    pub mode: ComposeMode,
    pub cursor: usize,
    pub visual_anchor: Option<usize>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub sending: bool,
    pub confirm_send: bool,
}

impl Composer {
    fn new() -> Self {
        Self {
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            body: String::new(),
            field: ComposeField::To,
            mode: ComposeMode::Insert,
            cursor: 0,
            visual_anchor: None,
            in_reply_to: None,
            references: Vec::new(),
            sending: false,
            confirm_send: false,
        }
    }

    fn reply(
        message: &MessageSummary,
        body: Option<&MessageBody>,
        prefer_html: bool,
        quote: bool,
    ) -> Self {
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
        let quoted = quote.then(|| {
            body.map_or_else(String::new, |body| {
                body.display_text(prefer_html)
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }).unwrap_or_default();
        let body = if quoted.is_empty() {
            "\n\n".to_owned()
        } else {
            format!("\n\nOn the previous message, {} wrote:\n{quoted}\n", display_sender(message))
        };
        // Replies are composed above any quoted history, matching normal mail
        // clients and aerc's reply workflow rather than dropping the cursor below
        // the quote.
        let cursor = 0;
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
            mode: ComposeMode::Insert,
            cursor,
            visual_anchor: None,
            in_reply_to: message.message_id.clone(),
            references,
            sending: false,
            confirm_send: false,
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
            mode: ComposeMode::Insert,
            cursor: 0,
            visual_anchor: None,
            in_reply_to: None,
            references: Vec::new(),
            sending: false,
            confirm_send: false,
        }
    }

    #[must_use]
    pub fn field_value(&self) -> &str {
        match self.field {
            ComposeField::To => &self.to,
            ComposeField::Cc => &self.cc,
            ComposeField::Subject => &self.subject,
            ComposeField::Body => &self.body,
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

    fn char_count(&self) -> usize {
        self.field_value().chars().count()
    }

    fn switch_field(&mut self, field: ComposeField) {
        self.field = field;
        self.cursor = self.char_count();
        self.visual_anchor = None;
        if self.mode == ComposeMode::Visual {
            self.mode = ComposeMode::Normal;
        }
    }

    fn byte_index(text: &str, char_index: usize) -> usize {
        text.char_indices()
            .nth(char_index)
            .map_or(text.len(), |(index, _)| index)
    }

    fn insert_char(&mut self, ch: char) {
        let cursor = self.cursor.min(self.char_count());
        let byte = Self::byte_index(self.field_value(), cursor);
        self.field_mut().insert(byte, ch);
        self.cursor = cursor.saturating_add(1);
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.cursor.min(self.char_count());
        let start = end - 1;
        self.delete_range(start, end);
        self.cursor = start;
    }

    fn delete_at_cursor(&mut self) {
        let start = self.cursor.min(self.char_count());
        if start < self.char_count() {
            self.delete_range(start, start + 1);
        }
    }

    fn delete_range(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let text = self.field_value();
        let start_byte = Self::byte_index(text, start);
        let end_byte = Self::byte_index(text, end);
        self.field_mut().replace_range(start_byte..end_byte, "");
        self.cursor = start.min(self.char_count());
        self.visual_anchor = None;
    }

    fn move_horizontal(&mut self, delta: isize) {
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.char_count());
    }

    fn move_line(&mut self, delta: isize) {
        if self.field != ComposeField::Body {
            return;
        }
        let text = self.body.as_str();
        let chars: Vec<char> = text.chars().collect();
        let cursor = self.cursor.min(chars.len());
        let mut line_start = cursor;
        while line_start > 0 && chars[line_start - 1] != '\n' {
            line_start -= 1;
        }
        let col = cursor.saturating_sub(line_start);
        let target_start = if delta < 0 {
            if line_start == 0 {
                return;
            }
            let mut start = line_start - 1;
            while start > 0 && chars[start - 1] != '\n' {
                start -= 1;
            }
            start
        } else {
            let mut end = cursor;
            while end < chars.len() && chars[end] != '\n' {
                end += 1;
            }
            if end >= chars.len() {
                return;
            }
            end + 1
        };
        let mut target_end = target_start;
        while target_end < chars.len() && chars[target_end] != '\n' {
            target_end += 1;
        }
        self.cursor = target_start + col.min(target_end.saturating_sub(target_start));
    }

    fn line_start(&self) -> usize {
        if self.field != ComposeField::Body {
            return 0;
        }
        let chars: Vec<char> = self.body.chars().collect();
        let mut start = self.cursor.min(chars.len());
        while start > 0 && chars[start - 1] != '\n' {
            start -= 1;
        }
        start
    }

    fn line_end(&self) -> usize {
        if self.field != ComposeField::Body {
            return self.char_count();
        }
        let chars: Vec<char> = self.body.chars().collect();
        let mut end = self.cursor.min(chars.len());
        while end < chars.len() && chars[end] != '\n' {
            end += 1;
        }
        end
    }

    fn open_line_below(&mut self) {
        if self.field != ComposeField::Body {
            return;
        }
        let end = self.line_end();
        let byte = Self::byte_index(&self.body, end);
        self.body.insert(byte, '\n');
        self.cursor = end.saturating_add(1);
        self.visual_anchor = None;
    }

    fn open_line_above(&mut self) {
        if self.field != ComposeField::Body {
            return;
        }
        let start = self.line_start();
        let byte = Self::byte_index(&self.body, start);
        self.body.insert(byte, '\n');
        self.cursor = start;
        self.visual_anchor = None;
    }

    fn move_word(&mut self, backwards: bool) {
        let chars: Vec<char> = self.field_value().chars().collect();
        if chars.is_empty() {
            self.cursor = 0;
            return;
        }
        let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
        if backwards {
            let mut index = self.cursor.min(chars.len());
            while index > 0 && !is_word(chars[index - 1]) {
                index -= 1;
            }
            while index > 0 && is_word(chars[index - 1]) {
                index -= 1;
            }
            self.cursor = index;
        } else {
            let mut index = self.cursor.min(chars.len());
            while index < chars.len() && is_word(chars[index]) {
                index += 1;
            }
            while index < chars.len() && !is_word(chars[index]) {
                index += 1;
            }
            self.cursor = index;
        }
    }

    #[must_use]
    pub fn visual_range(&self) -> Option<(usize, usize)> {
        let anchor = self.visual_anchor?;
        let cursor = self.cursor.min(self.char_count());
        Some(if anchor <= cursor {
            (anchor, cursor.saturating_add(1).min(self.char_count()))
        } else {
            (cursor, anchor.saturating_add(1).min(self.char_count()))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeIntent {
    Reply { quote: bool },
    Forward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCompose {
    account: usize,
    message_id: MessageId,
    intent: ComposeIntent,
}

#[derive(Debug, Clone)]
struct CachedMailbox {
    messages: Vec<MessageSummary>,
    bodies: HashMap<MessageId, MessageBody>,
    message_index: usize,
    message_view_offset: usize,
    next_page_offset: usize,
    has_more: bool,
}

pub struct AccountState {
    pub name: String,
    pub default_mailbox: String,
    pub from: String,
    pub outgoing_configured: bool,
    pub capabilities: Option<Capabilities>,
    pub mailboxes: Vec<Mailbox>,
    pub mailbox_index: usize,
    pub mailbox_view_offset: usize,
    pub mailbox_view_height: usize,
    pub current_mailbox: Option<String>,
    pub messages: Vec<MessageSummary>,
    pub message_index: usize,
    pub preview_scroll: u16,
    pub message_view_offset: usize,
    pub message_view_height: usize,
    pub smooth_message_target: Option<usize>,
    pub smooth_mailbox_target: Option<usize>,
    pub smooth_preview_target: Option<u16>,
    pub preview_max_scroll: u16,
    pub prefer_html: bool,
    pub page_size: usize,
    pub next_page_offset: usize,
    pub has_more: bool,
    pub page_inflight: Option<(String, usize)>,
    mailbox_cache: HashMap<String, CachedMailbox>,
    pub search_query: String,
    pub search_editing: bool,
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
    pub pending_mutations: usize,
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
        self.keep_mailbox_cursor_visible();
        self.message_index = self
            .message_index
            .min(self.messages.len().saturating_sub(1));
    }

    fn keep_mailbox_cursor_visible(&mut self) {
        self.mailbox_view_offset = message_view_offset(
            self.mailbox_view_offset,
            self.mailbox_index,
            self.mailboxes.len(),
            self.mailbox_view_height,
        );
    }

    #[must_use]
    pub fn visible_message_indices(&self) -> Vec<usize> {
        let query = self.search_query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return (0..self.messages.len()).collect();
        }
        self.messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                let matches = message.subject.to_ascii_lowercase().contains(&query)
                    || message.from_name.to_ascii_lowercase().contains(&query)
                    || message.from_address.to_ascii_lowercase().contains(&query)
                    || message.date_label.to_ascii_lowercase().contains(&query);
                matches.then_some(index)
            })
            .collect()
    }

    #[must_use]
    pub fn visible_message_position(&self) -> Option<usize> {
        self.visible_message_indices()
            .iter()
            .position(|index| *index == self.message_index)
    }

    fn normalize_search_selection(&mut self) {
        let visible = self.visible_message_indices();
        if visible.is_empty() {
            self.message_index = 0;
            self.message_view_offset = 0;
            return;
        }
        if !visible.contains(&self.message_index) {
            self.message_index = visible[0];
        }
        self.keep_message_cursor_visible();
    }

    fn keep_message_cursor_visible(&mut self) {
        let visible = self.visible_message_indices();
        let Some(position) = visible.iter().position(|index| *index == self.message_index) else {
            self.message_view_offset = 0;
            return;
        };
        self.message_view_offset = message_view_offset(
            self.message_view_offset,
            position,
            visible.len(),
            self.message_view_height,
        );
    }

    fn save_current_cache(&mut self) {
        let Some(name) = self.current_mailbox.clone() else {
            return;
        };
        self.mailbox_cache.insert(
            name,
            CachedMailbox {
                messages: self.messages.clone(),
                bodies: self.bodies.clone(),
                message_index: self.message_index,
                message_view_offset: self.message_view_offset,
                next_page_offset: self.next_page_offset,
                has_more: self.has_more,
            },
        );
    }

    fn restore_cached_mailbox(&mut self, name: &str) -> bool {
        let Some(cached) = self.mailbox_cache.get(name).cloned() else {
            return false;
        };
        self.current_mailbox = Some(name.to_owned());
        self.messages = cached.messages;
        self.bodies = cached.bodies;
        self.message_index = cached.message_index.min(self.messages.len().saturating_sub(1));
        self.message_view_offset = cached.message_view_offset;
        self.next_page_offset = cached.next_page_offset;
        self.has_more = cached.has_more;
        self.page_inflight = None;
        self.messages_loading = false;
        self.preview_scroll = 0;
        self.preview_max_scroll = 0;
        self.pending_bodies.clear();
        self.pending_seen.clear();
        self.search_query.clear();
        self.search_editing = false;
        self.clamp_selection();
        true
    }

    fn invalidate_current_cache(&mut self) {
        if let Some(name) = self.current_mailbox.as_ref() {
            self.mailbox_cache.remove(name);
        }
    }

    fn request_mailbox_page(&mut self, name: String, offset: usize) -> bool {
        if self
            .page_inflight
            .as_ref()
            .is_some_and(|(pending_name, pending_offset)| {
                pending_name == &name && *pending_offset == offset
            })
        {
            return false;
        }
        let pending_name = name.clone();
        if self
            .worker
            .post(ActionKind::OpenMailbox {
                name,
                offset,
                limit: self.page_size,
            })
            .is_ok()
        {
            self.page_inflight = Some((pending_name, offset));
            self.messages_loading = offset == 0;
            true
        } else {
            false
        }
    }

    fn request_next_page(&mut self) -> bool {
        if !self.has_more || self.page_inflight.is_some() {
            return false;
        }
        let Some(name) = self.current_mailbox.clone() else {
            return false;
        };
        self.request_mailbox_page(name, self.next_page_offset)
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

#[derive(Debug, Clone)]
enum MutationKind {
    Delete,
    Move { destination: String },
}

#[derive(Debug, Clone)]
pub struct MutationConfirmation {
    pub prompt: String,
    source: String,
    ids: Vec<MessageId>,
    kind: MutationKind,
}

pub struct App {
    pub accounts: Vec<AccountState>,
    pub account_index: usize,
    pub focus: Focus,
    pub status: StatusLine,
    pub should_quit: bool,
    pub mailbox_width_percent: u16,
    pub sidebar_visible: bool,
    pub preview_split: bool,
    pub message_open: bool,
    pub reader_link_index: usize,
    pub help_visible: bool,
    pub help_query: String,
    pub help_selected: usize,
    pub help_scroll: usize,
    pub theme_picker_visible: bool,
    pub theme_query: String,
    pub theme_selected: usize,
    pub theme_scroll: usize,
    pub themes: Vec<Theme>,
    pub theme_index: usize,
    theme_before_picker: usize,
    pub count_prefix: Option<usize>,
    pub visual_anchor: Option<usize>,
    pub confirmation: Option<MutationConfirmation>,
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
                mailbox_view_offset: 0,
                mailbox_view_height: 1,
                current_mailbox: None,
                messages: Vec::new(),
                message_index: 0,
                preview_scroll: 0,
                message_view_offset: 0,
                message_view_height: 1,
                smooth_message_target: None,
                smooth_mailbox_target: None,
                smooth_preview_target: None,
                preview_max_scroll: 0,
                prefer_html: false,
                // Interactive pagination has its own small page size, so an old
                // `max_messages = 500` config no longer makes All Mail block on
                // five hundred headers before first paint.
                page_size: config.page_size.max(1),
                next_page_offset: 0,
                has_more: false,
                page_inflight: None,
                mailbox_cache: HashMap::new(),
                search_query: String::new(),
                search_editing: false,
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
                pending_mutations: 0,
                worker,
            })
            .collect();
        let themes = load_themes();
        let configured_theme = persisted_theme().unwrap_or_else(|| config.ui.theme.clone());
        let theme_index = theme_index(&themes, &configured_theme);
        Self {
            accounts,
            account_index: 0,
            focus: Focus::Messages,
            status: StatusLine::info("starting workers…"),
            should_quit: false,
            mailbox_width_percent: config.ui.mailbox_width_percent.clamp(20, 50),
            sidebar_visible: true,
            preview_split: false,
            message_open: false,
            reader_link_index: 0,
            help_visible: false,
            help_query: String::new(),
            help_selected: 0,
            help_scroll: 0,
            theme_picker_visible: false,
            theme_query: String::new(),
            theme_selected: theme_index,
            theme_scroll: 0,
            themes,
            theme_index,
            theme_before_picker: theme_index,
            count_prefix: None,
            visual_anchor: None,
            confirmation: None,
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
    pub fn theme(&self) -> &Theme {
        &self.themes[self.theme_index.min(self.themes.len().saturating_sub(1))]
    }

    pub fn bootstrap(&mut self) {
        for account in &mut self.accounts {
            if account.worker.post(ActionKind::Connect).is_ok() {
                account.connecting = true;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
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
        if self.message_open || self.preview_split {
            match key.code {
                KeyCode::Tab if key.modifiers.is_empty() => {
                    self.cycle_reader_link(1);
                    return;
                }
                KeyCode::BackTab => {
                    self.cycle_reader_link(-1);
                    return;
                }
                KeyCode::Char('o') if key.modifiers.is_empty() => {
                    self.open_reader_link();
                    return;
                }
                _ => {}
            }
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
                let wanted = account
                    .current_mailbox
                    .clone()
                    .unwrap_or_else(|| account.default_mailbox.clone());
                account.mailboxes = mailboxes;
                account.mailbox_index = account
                    .mailboxes
                    .iter()
                    .position(|mailbox| mailbox.name == wanted)
                    .or_else(|| {
                        account
                            .mailboxes
                            .iter()
                            .position(|mailbox| mailbox.name == account.default_mailbox)
                    })
                    .unwrap_or(0);
                account.clamp_selection();
                account.mailbox_status_queue.clear();
                if let Some((name, counts_known)) = account
                    .selected_mailbox()
                    .map(|mailbox| (mailbox.name.clone(), mailbox.counts_known))
                {
                    if !counts_known {
                        account.mailbox_status_queue.push_back(name.clone());
                    }
                    if account.current_mailbox.as_deref() != Some(name.as_str()) {
                        if !account.restore_cached_mailbox(&name) {
                            account.current_mailbox = Some(name.clone());
                            account.messages.clear();
                            account.bodies.clear();
                            account.message_index = 0;
                            account.message_view_offset = 0;
                            account.next_page_offset = 0;
                            account.has_more = false;
                            let _ = account.request_mailbox_page(name, 0);
                        }
                    } else if account.messages.is_empty() && account.page_inflight.is_none() {
                        let _ = account.request_mailbox_page(name, 0);
                    }
                }
                Self::pump_mailbox_status(account);
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
                page,
                elapsed_micros,
            } => {
                let first_page = page.offset == 0;
                let selected_name = account.selected_mailbox().map(|mailbox| mailbox.name.clone());
                if selected_name.as_deref() != Some(name.as_str()) {
                    if page.offset == 0 {
                        account.mailbox_cache.insert(
                            name,
                            CachedMailbox {
                                messages: page.messages,
                                bodies: HashMap::new(),
                                message_index: 0,
                                message_view_offset: 0,
                                next_page_offset: page.next_offset,
                                has_more: page.has_more,
                            },
                        );
                    }
                    return;
                }

                if account
                    .page_inflight
                    .as_ref()
                    .is_some_and(|(pending_name, pending_offset)| {
                        pending_name == &name && *pending_offset == page.offset
                    })
                {
                    account.page_inflight = None;
                }
                account.messages_loading = false;
                account.current_mailbox = Some(name.clone());
                if page.offset == 0 {
                    let selected_id = account.selected_message().map(|message| message.id.clone());
                    account.messages = page.messages;
                    account.message_index = selected_id
                        .and_then(|id| account.messages.iter().position(|message| message.id == id))
                        .or_else(|| account.restore_message_index.take())
                        .unwrap_or(0)
                        .min(account.messages.len().saturating_sub(1));
                    account.message_view_offset = 0;
                    let live = account
                        .messages
                        .iter()
                        .map(|message| message.id.clone())
                        .collect::<HashSet<_>>();
                    account.bodies.retain(|id, _| live.contains(id));
                } else {
                    let existing = account
                        .messages
                        .iter()
                        .map(|message| message.id.clone())
                        .collect::<HashSet<_>>();
                    account.messages.extend(
                        page.messages
                            .into_iter()
                            .filter(|message| !existing.contains(&message.id)),
                    );
                    account.messages.sort_by(|a, b| {
                        b.timestamp
                            .cmp(&a.timestamp)
                            .then_with(|| b.id.cmp(&a.id))
                    });
                }
                account.next_page_offset = page.next_offset;
                account.has_more = page.has_more;
                if first_page {
                    account.preview_scroll = 0;
                    account.preview_max_scroll = 0;
                    account.smooth_message_target = None;
                    account.smooth_preview_target = None;
                    account.pending_bodies.clear();
                    account.pending_seen.clear();
                }
                account.last_scan_micros = Some(elapsed_micros);
                account.clamp_selection();
                account.normalize_search_selection();
                let search_needs_more = !account.search_query.is_empty()
                    && account.visible_message_indices().len() < account.message_view_height
                    && account.has_more
                    && account.page_inflight.is_none();
                if search_needs_more {
                    let _ = account.request_next_page();
                }
                if first_page {
                    self.visual_anchor = None;
                    self.focus = Focus::Messages;
                }
                self.status = StatusLine::temporary(
                    format!(
                        "{}: {} messages loaded from {} in {:.2} ms{}",
                        account.name,
                        account.messages.len(),
                        name,
                        elapsed_micros as f64 / 1000.0,
                        if account.has_more { " • older mail available" } else { "" }
                    ),
                    Duration::from_secs(4),
                );
                if self.preview_split {
                    account.request_selected_body();
                }
                account.mailbox_status_queue.clear();
                if let Some((name, false)) = account
                    .selected_mailbox()
                    .map(|mailbox| (mailbox.name.clone(), mailbox.counts_known))
                {
                    account.mailbox_status_queue.push_back(name);
                }
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
                        ComposeIntent::Reply { quote } => {
                            Composer::reply(message, body, account.prefer_html, quote)
                        }
                        ComposeIntent::Forward => {
                            Composer::forward(message, body, account.prefer_html)
                        }
                    })
                });

                if (self.message_open || self.preview_split)
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
                account.pending_mutations = account.pending_mutations.saturating_sub(1);
                self.status = StatusLine::temporary("mail updated", Duration::from_secs(2));
            }
            WorkerEventKind::MessageSent => {
                if event.account == self.account_index {
                    self.composer = None;
                }
                self.status = StatusLine::temporary("message sent", Duration::from_secs(4));
            }
            WorkerEventKind::Error(error) => {
                let mutation_failed = account.pending_mutations > 0;
                account.pending_mutations = 0;
                account.connecting = false;
                account.mailboxes_loading = false;
                account.messages_loading = false;
                account.page_inflight = None;
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
                if mutation_failed
                    && let Some(name) = account.current_mailbox.clone()
                {
                    account.invalidate_current_cache();
                    let _ = account.request_mailbox_page(name, 0);
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
                if self.preview_split {
                    if let Some(account) = self.account_mut() {
                        account.request_selected_body();
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
            Command::Escape => {
                self.count_prefix = None;
                if self.visual_anchor.take().is_some() {
                    self.status = StatusLine::temporary("visual selection cleared", Duration::from_secs(1));
                } else if self.message_open {
                    self.close_message();
                } else {
                    if let Some(account) = self.account_mut() {
                        if !account.search_query.is_empty() {
                            account.search_query.clear();
                            account.search_editing = false;
                            account.normalize_search_selection();
                        }
                    }
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
            account.smooth_message_target.is_some()
                || account.smooth_mailbox_target.is_some()
                || account.smooth_preview_target.is_some()
        })
    }

    /// Advance one row of a pending smooth-scroll animation. Message targets
    /// are positions inside the filtered list; mailbox targets deliberately do
    /// not open intermediate folders, so Ctrl-Shift-j/k stays responsive.
    pub fn advance_animation(&mut self) {
        let focus = self.focus;
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
                    if next == target_position {
                        account.smooth_message_target = None;
                        if preview_split {
                            account.request_selected_body();
                        }
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
            }
        }

        if mailbox_finished {
            Self::activate_selected_mailbox(account, preview_split);
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
        if let Some(account) = self.account_mut() {
            account.save_current_cache();
        }
        self.count_prefix = None;
        self.message_open = false;
        self.pending_compose = None;
        self.visual_anchor = None;
        self.confirmation = None;
        self.focus = Focus::Messages;
        self.account_index = offset_index(self.account_index, self.accounts.len(), delta);
    }

    fn move_selection(&mut self, delta: isize) {
        let focus = self.focus;
        let preview_split = self.preview_split;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.smooth_message_target = None;
        account.smooth_preview_target = None;
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
                let next = bounded_index(current, visible.len(), delta);
                let previous = account.message_index;
                account.message_index = visible[next];
                if account.message_index != previous {
                    account.preview_scroll = 0;
                    account.keep_message_cursor_visible();
                    if preview_split {
                        account.request_selected_body();
                    }
                }
                if delta > 0 && next.saturating_add(5) >= visible.len() {
                    let _ = account.request_next_page();
                }
            }
            Focus::Preview => {
                account.preview_scroll =
                    bounded_scroll_to(account.preview_scroll, delta, account.preview_max_scroll);
            }
        }
    }

    fn move_mailbox_list(&mut self, delta: isize) {
        if self.message_open {
            return;
        }
        self.visual_anchor = None;
        let preview_split = self.preview_split;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.save_current_cache();
        account.smooth_mailbox_target = None;
        let previous = account.mailbox_index;
        account.mailbox_index = bounded_index(account.mailbox_index, account.mailboxes.len(), delta);
        if account.mailbox_index == previous {
            return;
        }
        account.keep_mailbox_cursor_visible();
        Self::activate_selected_mailbox(account, preview_split);
    }

    fn activate_selected_mailbox(account: &mut AccountState, preview_split: bool) {
        let Some(name) = account.selected_mailbox().map(|mailbox| mailbox.name.clone()) else {
            return;
        };
        account.smooth_message_target = None;
        account.smooth_preview_target = None;
        account.message_view_offset = 0;
        account.message_index = 0;
        account.search_query.clear();
        account.search_editing = false;
        account.mailbox_status_queue.clear();
        if let Some((status_name, false)) = account
            .selected_mailbox()
            .map(|mailbox| (mailbox.name.clone(), mailbox.counts_known))
        {
            account.mailbox_status_queue.push_back(status_name);
        }
        if account.restore_cached_mailbox(&name) {
            if preview_split {
                account.request_selected_body();
            }
            Self::pump_mailbox_status(account);
            return;
        }
        account.current_mailbox = Some(name.clone());
        account.messages.clear();
        account.bodies.clear();
        account.pending_bodies.clear();
        account.next_page_offset = 0;
        account.has_more = false;
        account.page_inflight = None;
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

    /// Called by the renderer after wrapping the current message. Keeping the
    /// exact visual bound in state prevents invisible overscroll.
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
                account.smooth_message_target = Some(bounded_index(base, visible.len(), delta));
            }
            Focus::Preview => {
                let base = account
                    .smooth_preview_target
                    .unwrap_or(account.preview_scroll)
                    .min(account.preview_max_scroll);
                let target = bounded_scroll_to(base, delta, account.preview_max_scroll);
                account.smooth_preview_target = (target != account.preview_scroll).then_some(target);
            }
        }
    }

    fn start_smooth_mailbox_scroll(&mut self, delta: isize) {
        if self.message_open {
            return;
        }
        self.visual_anchor = None;
        let Some(account) = self.account_mut() else {
            return;
        };
        if account.mailboxes.is_empty() {
            return;
        }
        if account.smooth_mailbox_target.is_none() {
            account.save_current_cache();
        }
        let base = account.smooth_mailbox_target.unwrap_or(account.mailbox_index);
        account.smooth_mailbox_target = Some(bounded_index(base, account.mailboxes.len(), delta));
    }

    fn jump(&mut self, bottom: bool) {
        let focus = self.focus;
        let preview_split = self.preview_split;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.smooth_message_target = None;
        account.smooth_preview_target = None;
        match focus {
            Focus::Mailboxes | Focus::Messages => {
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
                if preview_split {
                    account.request_selected_body();
                }
            }
            Focus::Preview => {
                account.preview_scroll = if bottom { account.preview_max_scroll } else { 0 };
            }
        }
    }

    fn open_selection(&mut self) {
        match self.focus {
            Focus::Mailboxes | Focus::Messages => {
                if self.account().is_none_or(|account| account.selected_message().is_none()) {
                    return;
                }
                self.visual_anchor = None;
                self.reader_link_index = 0;
                self.message_open = true;
                if let Some(account) = self.account_mut() {
                    account.preview_scroll = 0;
                    account.preview_max_scroll = 0;
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
        self.reader_link_index = 0;
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

    fn current_message_links(&self) -> Vec<String> {
        self.account()
            .and_then(AccountState::selected_body)
            .map(MessageBody::html_links)
            .unwrap_or_default()
    }

    fn cycle_reader_link(&mut self, delta: isize) {
        let links = self.current_message_links();
        if links.is_empty() {
            self.reader_link_index = 0;
            self.status = StatusLine::temporary("message has no rendered links", Duration::from_secs(1));
            return;
        }
        self.reader_link_index = offset_index(self.reader_link_index.min(links.len() - 1), links.len(), delta);
        self.status = StatusLine::temporary(
            format!("link {}/{}: {}", self.reader_link_index + 1, links.len(), links[self.reader_link_index]),
            Duration::from_secs(4),
        );
    }

    fn open_reader_link(&mut self) {
        let links = self.current_message_links();
        let Some(url) = links.get(self.reader_link_index.min(links.len().saturating_sub(1))).cloned() else {
            self.status = StatusLine::temporary("message has no rendered links", Duration::from_secs(1));
            return;
        };
        let result = open_url(&url);
        self.status = match result {
            Ok(()) => StatusLine::temporary(format!("opened {url}"), Duration::from_secs(3)),
            Err(error) => StatusLine::error(format!("could not open link: {error}")),
        };
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

    fn toggle_visual(&mut self) {
        if self.message_open || self.account().is_none_or(|account| account.messages.is_empty()) {
            return;
        }
        self.visual_anchor = if self.visual_anchor.is_some() {
            None
        } else {
            self.account().map(|account| account.message_index)
        };
    }

    #[must_use]
    pub fn visual_range(&self) -> Option<(usize, usize)> {
        let anchor_index = self.visual_anchor?;
        let account = self.account()?;
        let visible = account.visible_message_indices();
        let anchor = visible.iter().position(|index| *index == anchor_index)?;
        let current = visible
            .iter()
            .position(|index| *index == account.message_index)?;
        Some(if anchor <= current {
            (anchor, current)
        } else {
            (current, anchor)
        })
    }

    #[must_use]
    pub fn message_is_visual_selected(&self, index: usize) -> bool {
        let Some((start, end)) = self.visual_range() else {
            return false;
        };
        let Some(account) = self.account() else {
            return false;
        };
        account
            .visible_message_indices()
            .iter()
            .position(|candidate| *candidate == index)
            .is_some_and(|position| (start..=end).contains(&position))
    }

    fn selected_message_ids(&self) -> Vec<MessageId> {
        let Some(account) = self.account() else {
            return Vec::new();
        };
        if let Some((start, end)) = self.visual_range() {
            let visible = account.visible_message_indices();
            return visible[start..=end]
                .iter()
                .filter_map(|index| account.messages.get(*index))
                .map(|message| message.id.clone())
                .collect();
        }
        account
            .selected_message()
            .map(|message| vec![message.id.clone()])
            .unwrap_or_default()
    }

    fn delete_selected(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        let Some(source) = account.current_mailbox.clone() else {
            return;
        };
        let current_role = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.name == source)
            .map(|mailbox| mailbox.role.clone());
        let count = ids.len();

        if current_role == Some(MailboxRole::Trash) {
            self.confirmation = Some(MutationConfirmation {
                prompt: format!(
                    "Permanently delete {count} message{}? This cannot be undone. [y/N]",
                    if count == 1 { "" } else { "s" }
                ),
                source,
                ids,
                kind: MutationKind::Delete,
            });
            return;
        }

        if let Some(trash) = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == MailboxRole::Trash)
            .map(|mailbox| mailbox.name.clone())
        {
            self.confirmation = Some(MutationConfirmation {
                prompt: format!(
                    "Move {count} message{} to Trash? [y/N]",
                    if count == 1 { "" } else { "s" }
                ),
                source,
                ids,
                kind: MutationKind::Move { destination: trash },
            });
        } else {
            self.confirmation = Some(MutationConfirmation {
                prompt: format!(
                    "Delete {count} message{}? This server has no Trash mailbox. [y/N]",
                    if count == 1 { "" } else { "s" }
                ),
                source,
                ids,
                kind: MutationKind::Delete,
            });
        }
    }

    fn delete_selected_immediate(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        let Some(source) = account.current_mailbox.clone() else {
            return;
        };
        let current_role = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.name == source)
            .map(|mailbox| mailbox.role.clone());
        let kind = if current_role == Some(MailboxRole::Trash) {
            MutationKind::Delete
        } else if let Some(trash) = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == MailboxRole::Trash)
            .map(|mailbox| mailbox.name.clone())
        {
            MutationKind::Move { destination: trash }
        } else {
            MutationKind::Delete
        };
        self.start_mutation(source, ids, kind);
    }

    fn archive_selected(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        let Some(source) = account.current_mailbox.clone() else {
            return;
        };
        let destination = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == MailboxRole::Archive && mailbox.name != source)
            .map(|mailbox| mailbox.name.clone());
        let Some(destination) = destination else {
            self.status = StatusLine::temporary(
                "this account has no advertised Archive/All Mail mailbox",
                Duration::from_secs(3),
            );
            return;
        };
        self.start_mutation(source, ids, MutationKind::Move { destination });
    }

    fn handle_confirmation_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let Some(confirmation) = self.confirmation.take() else {
                    return;
                };
                self.start_mutation(confirmation.source, confirmation.ids, confirmation.kind);
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.confirmation = None;
                self.status = StatusLine::temporary("delete cancelled", Duration::from_secs(2));
            }
            _ => {}
        }
    }

    fn start_mutation(&mut self, source: String, ids: Vec<MessageId>, kind: MutationKind) {
        if ids.is_empty() {
            return;
        }
        let action = match &kind {
            MutationKind::Delete => ActionKind::DeleteMany {
                mailbox: source.clone(),
                ids: ids.clone(),
            },
            MutationKind::Move { destination } => ActionKind::MoveMany {
                source: source.clone(),
                destination: destination.clone(),
                ids: ids.clone(),
            },
        };

        let started = self
            .account_mut()
            .is_some_and(|account| match account.worker.post(action) {
                Ok(_) => {
                    account.pending_mutations = account.pending_mutations.saturating_add(1);
                    true
                }
                Err(_) => false,
            });
        if !started {
            self.status = StatusLine::error("mail worker queue is busy; action not started");
            return;
        }

        self.remove_messages_locally(&source, &ids, &kind);
        self.visual_anchor = None;
        if self.message_open {
            self.close_message();
        }
        self.status = StatusLine::info(match kind {
            MutationKind::Delete => "deleting message(s)…",
            MutationKind::Move { .. } => "moving message(s)…",
        });
    }

    fn remove_messages_locally(&mut self, source: &str, ids: &[MessageId], kind: &MutationKind) {
        let Some(account) = self.account_mut() else {
            return;
        };
        let selected_ids: HashSet<&MessageId> = ids.iter().collect();
        let removed: Vec<MessageSummary> = account
            .messages
            .iter()
            .filter(|message| selected_ids.contains(&message.id))
            .cloned()
            .collect();
        if removed.is_empty() {
            return;
        }
        account.messages.retain(|message| !selected_ids.contains(&message.id));
        for id in ids {
            account.bodies.remove(id);
            account.pending_bodies.remove(id);
            account.pending_seen.remove(id);
        }
        account.message_index = account
            .message_index
            .min(account.messages.len().saturating_sub(1));
        account.keep_message_cursor_visible();

        let removed_unread = removed.iter().filter(|message| message.unread()).count();
        if let Some(mailbox) = account
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == source && mailbox.counts_known)
        {
            mailbox.total = mailbox.total.saturating_sub(removed.len());
            mailbox.unread = mailbox.unread.saturating_sub(removed_unread);
        }
        if let MutationKind::Move { destination } = kind {
            // A destination cache cannot know about the moved messages without
            // a server response containing their destination UIDs. Drop only
            // that cache; the active source list stays updated locally.
            account.mailbox_cache.remove(destination);
            if let Some(mailbox) = account
                .mailboxes
                .iter_mut()
                .find(|mailbox| mailbox.name == *destination && mailbox.counts_known)
            {
                mailbox.total = mailbox.total.saturating_add(removed.len());
                mailbox.unread = mailbox.unread.saturating_add(removed_unread);
            }
        }
        account.normalize_search_selection();
    }

    fn begin_search(&mut self) {
        if self.message_open {
            return;
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

    fn handle_search_key(&mut self, key: KeyEvent) {
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
                KeyCode::Backspace if key.modifiers.is_empty() => {
                    account.search_query.pop();
                    account.normalize_search_selection();
                }
                KeyCode::Char(ch)
                    if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
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
                if account.has_more { " • search continues into older pages" } else { "" }
            )
        };
        self.status = if cleared {
            StatusLine::temporary("filter cleared", Duration::from_secs(1))
        } else {
            StatusLine::info(status)
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
        self.themes
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
        let id = self.theme().id.clone();
        if let Err(error) = persist_theme(&id) {
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
            account.invalidate_current_cache();
            account.page_inflight = None;
            account.restore_message_index = Some(account.message_index);
            let _ = account.request_mailbox_page(name, 0);
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

    fn begin_reply(&mut self, quote: bool) {
        self.begin_compose_from_selected(ComposeIntent::Reply { quote });
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
                ComposeIntent::Reply { quote } => {
                    Composer::reply(message, Some(body), account.prefer_html, quote)
                }
                ComposeIntent::Forward => {
                    Composer::forward(message, Some(body), account.prefer_html)
                }
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
                ComposeIntent::Reply { quote: true } => "loading message for quoted reply…",
                ComposeIntent::Reply { quote: false } => "loading message for reply…",
                ComposeIntent::Forward => "loading message for forward…",
            });
        } else {
            self.status = StatusLine::error("message body could not be queued for loading");
        }
    }

    fn handle_composer_key(&mut self, key: KeyEvent) {
        let sending = self.composer.as_ref().is_some_and(|composer| composer.sending);
        if sending {
            if key.code == KeyCode::Esc {
                self.status = StatusLine::temporary(
                    "send is already in progress",
                    Duration::from_secs(2),
                );
            }
            return;
        }

        if self
            .composer
            .as_ref()
            .is_some_and(|composer| composer.confirm_send)
        {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.confirm_send = false;
                    }
                    self.send_composer();
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.confirm_send = false;
                    }
                    self.status = StatusLine::temporary("send cancelled", Duration::from_secs(1));
                }
                _ => {}
            }
            return;
        }

        let Some(mode) = self.composer.as_ref().map(|composer| composer.mode) else {
            return;
        };

        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let composer = self.composer.as_mut().expect("composer checked above");
            let field = if key.code == KeyCode::Tab {
                composer.field.next()
            } else {
                composer.field.previous()
            };
            composer.switch_field(field);
            return;
        }

        match mode {
            ComposeMode::Insert => {
                let Some(composer) = self.composer.as_mut() else {
                    return;
                };
                match key.code {
                    KeyCode::Esc => {
                        composer.mode = ComposeMode::Normal;
                        composer.visual_anchor = None;
                    }
                    KeyCode::Enter if composer.field == ComposeField::Body => {
                        composer.insert_char('\n');
                    }
                    KeyCode::Enter => {
                        composer.switch_field(composer.field.next());
                    }
                    KeyCode::Backspace => composer.backspace(),
                    KeyCode::Left => composer.move_horizontal(-1),
                    KeyCode::Right => composer.move_horizontal(1),
                    KeyCode::Up => composer.move_line(-1),
                    KeyCode::Down => composer.move_line(1),
                    KeyCode::Char(ch)
                        if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        composer.insert_char(ch);
                    }
                    _ => {}
                }
            }
            ComposeMode::Normal => {
                if key.code == KeyCode::Esc {
                    self.composer = None;
                    return;
                }
                let Some(composer) = self.composer.as_mut() else {
                    return;
                };
                match key.code {
                    KeyCode::Enter => composer.confirm_send = true,
                    KeyCode::Char('i') => composer.mode = ComposeMode::Insert,
                    KeyCode::Char('a') => {
                        composer.move_horizontal(1);
                        composer.mode = ComposeMode::Insert;
                    }
                    KeyCode::Char('I') => {
                        composer.cursor = composer.line_start();
                        composer.mode = ComposeMode::Insert;
                    }
                    KeyCode::Char('A') => {
                        composer.cursor = composer.line_end();
                        composer.mode = ComposeMode::Insert;
                    }
                    KeyCode::Char('v') => {
                        composer.visual_anchor = Some(composer.cursor.min(composer.char_count()));
                        composer.mode = ComposeMode::Visual;
                    }
                    KeyCode::Char('h') | KeyCode::Left => composer.move_horizontal(-1),
                    KeyCode::Char('l') | KeyCode::Right => composer.move_horizontal(1),
                    KeyCode::Char('j') | KeyCode::Down => composer.move_line(1),
                    KeyCode::Char('k') | KeyCode::Up => composer.move_line(-1),
                    KeyCode::Char('w') => composer.move_word(false),
                    KeyCode::Char('b') => composer.move_word(true),
                    KeyCode::Char('0') | KeyCode::Home => composer.cursor = composer.line_start(),
                    KeyCode::Char('$') | KeyCode::End => composer.cursor = composer.line_end(),
                    KeyCode::Char('x') | KeyCode::Delete => composer.delete_at_cursor(),
                    KeyCode::Char('o') if composer.field == ComposeField::Body => {
                        composer.open_line_below();
                        composer.mode = ComposeMode::Insert;
                    }
                    KeyCode::Char('O') if composer.field == ComposeField::Body => {
                        composer.open_line_above();
                        composer.mode = ComposeMode::Insert;
                    }
                    _ => {}
                }
            }
            ComposeMode::Visual => {
                let Some(composer) = self.composer.as_mut() else {
                    return;
                };
                match key.code {
                    KeyCode::Esc | KeyCode::Char('v') => {
                        composer.visual_anchor = None;
                        composer.mode = ComposeMode::Normal;
                    }
                    KeyCode::Char('h') | KeyCode::Left => composer.move_horizontal(-1),
                    KeyCode::Char('l') | KeyCode::Right => composer.move_horizontal(1),
                    KeyCode::Char('j') | KeyCode::Down => composer.move_line(1),
                    KeyCode::Char('k') | KeyCode::Up => composer.move_line(-1),
                    KeyCode::Char('w') => composer.move_word(false),
                    KeyCode::Char('b') => composer.move_word(true),
                    KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => {
                        if let Some((start, end)) = composer.visual_range() {
                            composer.delete_range(start, end);
                        }
                        composer.mode = ComposeMode::Normal;
                    }
                    KeyCode::Char('c') => {
                        if let Some((start, end)) = composer.visual_range() {
                            composer.delete_range(start, end);
                        }
                        composer.mode = ComposeMode::Insert;
                    }
                    _ => {}
                }
            }
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


fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = ProcessCommand::new("rundll32.exe");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = ProcessCommand::new("open");
        command.arg(url);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = ProcessCommand::new("xdg-open");
        command.arg(url);
        command
    };
    #[cfg(not(any(unix, target_os = "windows")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "opening URLs is unsupported on this platform",
    ));

    command.spawn().map(|_| ())
}

fn message_view_offset(current_offset: usize, index: usize, len: usize, height: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let height = height.max(1).min(len);
    let max_offset = len.saturating_sub(height);
    let scrolloff = MESSAGE_SCROLL_OFF.min(height.saturating_sub(1) / 2);
    let index = index.min(len.saturating_sub(1));
    let mut offset = current_offset.min(max_offset);
    let upper_guard = offset.saturating_add(scrolloff);
    let lower_guard = offset.saturating_add(height.saturating_sub(scrolloff + 1));

    if index < upper_guard {
        offset = index.saturating_sub(scrolloff);
    } else if index > lower_guard {
        offset = index
            .saturating_add(scrolloff + 1)
            .saturating_sub(height)
            .min(max_offset);
    }
    offset.min(max_offset)
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
    use super::{
        ComposeField, ComposeMode, Composer, bounded_index, bounded_scroll, bounded_scroll_to,
        message_view_offset, offset_index,
    };

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
    fn message_view_keeps_neovim_style_scrolloff() {
        // At the bottom of a 100-message list in a 20-row viewport, moving up
        // should let the cursor rise inside the viewport before the viewport moves.
        assert_eq!(message_view_offset(80, 99, 100, 20), 80);
        assert_eq!(message_view_offset(80, 95, 100, 20), 80);
        assert_eq!(message_view_offset(80, 90, 100, 20), 80);
        assert_eq!(message_view_offset(80, 83, 100, 20), 79);

        // Likewise, moving down through the middle does not pin the cursor to
        // the bottom row; scrolling begins only inside the scrolloff guard.
        assert_eq!(message_view_offset(20, 30, 100, 20), 20);
        assert_eq!(message_view_offset(20, 36, 100, 20), 21);
    }

    #[test]
    fn composer_field_switch_keeps_labels_modal_and_exits_visual() {
        let mut composer = Composer::new();
        composer.to = "person@example.com".to_owned();
        composer.switch_field(ComposeField::To);
        composer.mode = ComposeMode::Visual;
        composer.visual_anchor = Some(0);

        composer.switch_field(ComposeField::Cc);
        assert_eq!(composer.field, ComposeField::Cc);
        assert_eq!(composer.mode, ComposeMode::Normal);
        assert_eq!(composer.visual_anchor, None);
        assert_eq!(composer.cursor, 0);
    }

    #[test]
    fn composer_normal_line_motions_follow_body_lines() {
        let mut composer = Composer::new();
        composer.body = "one\ntwo\nthree".to_owned();
        composer.switch_field(ComposeField::Body);
        composer.cursor = 6;
        assert_eq!(composer.line_start(), 4);
        assert_eq!(composer.line_end(), 7);
        composer.open_line_below();
        assert_eq!(composer.body, "one\ntwo\n\nthree");
        assert_eq!(composer.cursor, 8);
    }

    #[test]
    fn composer_visual_range_tracks_character_indices() {
        let mut composer = Composer::new();
        composer.body = "héllo".to_owned();
        composer.switch_field(ComposeField::Body);
        composer.mode = ComposeMode::Visual;
        composer.visual_anchor = Some(1);
        composer.cursor = 4;
        assert_eq!(composer.visual_range(), Some((1, 5)));
        composer.delete_range(1, 5);
        assert_eq!(composer.body, "h");
    }

    #[test]
    fn account_navigation_wraps() {
        assert_eq!(offset_index(0, 3, -1), 2);
        assert_eq!(offset_index(2, 3, 1), 0);
    }
}
