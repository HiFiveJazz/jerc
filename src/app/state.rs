use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Size;
use ratatui_image::{Resize, picker::Picker, protocol::Protocol};

use crate::app::{Command, Focus, HELP_ENTRIES, command_for_key};
use crate::config::Config;
use crate::mail::{parse_body, render_html_for_terminal};
use crate::outgoing::{ReplyAudience, reply_recipients, reply_subject, forward_subject, inline_forward_parts, forward_html, forward_attribution, FORWARD_SEPARATOR};
use crate::mail_image::{MailImageEvent, MailImageLoader};
use crate::model::{
    Capabilities, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId, MessageSummary,
    OutgoingAttachment, OutgoingMessage,
};
use crate::theme::{Theme, load_themes, persist_theme, persisted_theme, theme_index};
use crate::worker::{ActionKind, RuntimeEvent, WorkerEvent, WorkerEventKind, WorkerHandle};

const SMOOTH_SCROLL_ROWS: isize = 22;
const SMOOTH_MAILBOX_ROWS: isize = 5;
const MAX_COUNT_PREFIX: usize = 99_999;
const MESSAGE_SCROLL_OFF: usize = 4;
const READER_SCROLL_OFF: usize = 3;
const PAGE_PREFETCH_MIN: usize = 48;

/// Only a completed background poll against a known Inbox baseline can
/// trigger a new-message alert. STATUS counts are not message identities;
/// an equal count with concurrent arrival/deletion needs future UID tracking.
fn inbox_new_mail_count(from_poll: bool, known: bool, previous: usize, now: usize) -> usize {
    if from_poll && known { now.saturating_sub(previous) } else { 0 }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReaderPosition {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderSelection {
    pub start: ReaderPosition,
    pub end: ReaderPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderWordMotion {
    NextStart,
    PreviousStart,
}

impl ReaderSelection {
    #[must_use]
    pub fn columns_for_line(self, line: usize, line_len: usize) -> Option<(usize, usize)> {
        if line < self.start.line || line > self.end.line {
            return None;
        }
        let last = line_len.saturating_sub(1);
        let start = if line == self.start.line {
            self.start.column.min(last)
        } else {
            0
        };
        let end = if line == self.end.line {
            self.end.column.min(last)
        } else {
            last
        };
        (line_len > 0 && start <= end).then_some((start, end))
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComposeField {
    To,
    Cc,
    Bcc,
    Subject,
    Attachments,
    Headers,
    Body,
}

impl ComposeField {
    const fn next(self) -> Self {
        match self {
            Self::To => Self::Cc,
            Self::Cc => Self::Bcc,
            Self::Bcc => Self::Subject,
            Self::Subject => Self::Attachments,
            Self::Attachments => Self::Headers,
            Self::Headers => Self::Body,
            Self::Body => Self::To,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::To => Self::Body,
            Self::Cc => Self::To,
            Self::Bcc => Self::Cc,
            Self::Subject => Self::Bcc,
            Self::Attachments => Self::Subject,
            Self::Headers => Self::Attachments,
            Self::Body => Self::Headers,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Composer {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub attachments: String,
    pub headers: String,
    pub subject: String,
    pub body: String,
    pub field: ComposeField,
    pub mode: ComposeMode,
    pub cursor: usize,
    pub visual_anchor: Option<usize>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Original RFC 5322 message retained to reconstruct HTML/attachments
    /// for inline forwarding, including postponed drafts. Never serialized as
    /// a .eml attachment in the default forwarding workflow.
    pub forwarded_raw: Option<Vec<u8>>,
    pub sending: bool,
    pub confirm_send: bool,
}

impl Composer {
    pub fn new() -> Self {
        Self {
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            attachments: String::new(),
            headers: String::new(),
            subject: String::new(),
            body: String::new(),
            field: ComposeField::To,
            mode: ComposeMode::Insert,
            cursor: 0,
            visual_anchor: None,
            in_reply_to: None,
            references: Vec::new(),
            forwarded_raw: None,
            sending: false,
            confirm_send: false,
        }
    }

    fn reply(message: &MessageSummary, body: Option<&MessageBody>, quote: bool) -> Self {
        let subject = reply_subject(&message.subject);
        let mut references = message.references.clone();
        if let Some(id) = &message.message_id {
            if references.last() != Some(id) {
                references.push(id.clone());
            }
        }
        let quoted = quote.then(|| {
            body.map_or_else(String::new, |body| {
                // aerc-style replies quote the message's textual representation,
                // not the rich reader projection. HTML-only messages already
                // have a generated plain fallback, so this stays readable while
                // avoiding table chrome/image placeholders in mailing-list replies.
                body.display_text(false)
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }).unwrap_or_default();
        let body = if quoted.is_empty() {
            "\n\n".to_owned()
        } else {
            format!("\n\n{}\n{quoted}\n", reply_attribution(message))
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
            bcc: String::new(),
            attachments: String::new(),
            headers: String::new(),
            subject,
            body,
            field: ComposeField::Body,
            mode: ComposeMode::Insert,
            cursor,
            visual_anchor: None,
            in_reply_to: message.message_id.clone(),
            references,
            forwarded_raw: None,
            sending: false,
            confirm_send: false,
        }
    }

    fn forward(message: &MessageSummary, raw: Vec<u8>) -> anyhow::Result<Self> {
        let parts = inline_forward_parts(&raw)?;
        let subject = forward_subject(&message.subject);
        let attribution = forward_attribution(&raw)?;
        let body = format!("\n\n{attribution}\n{}\n", parts.plain);
        Ok(Self {
            to: String::new(), cc: String::new(), bcc: String::new(),
            attachments: String::new(), headers: String::new(), subject, body,
            field: ComposeField::To, mode: ComposeMode::Insert, cursor: 0,
            visual_anchor: None, in_reply_to: None, references: Vec::new(),
            forwarded_raw: Some(raw), sending: false, confirm_send: false,
        })
    }

    #[must_use]
    pub fn field_value(&self) -> &str {
        match self.field {
            ComposeField::To => &self.to,
            ComposeField::Cc => &self.cc,
            ComposeField::Bcc => &self.bcc,
            ComposeField::Subject => &self.subject,
            ComposeField::Attachments => &self.attachments,
            ComposeField::Headers => &self.headers,
            ComposeField::Body => &self.body,
        }
    }

    fn field_mut(&mut self) -> &mut String {
        match self.field {
            ComposeField::To => &mut self.to,
            ComposeField::Cc => &mut self.cc,
            ComposeField::Bcc => &mut self.bcc,
            ComposeField::Subject => &mut self.subject,
            ComposeField::Attachments => &mut self.attachments,
            ComposeField::Headers => &mut self.headers,
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
    Reply { quote: bool, audience: ReplyAudience },
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
    pub mailbox_status_inflight: Option<u64>,
    inbox_poll_inflight: Option<u64>,
    pending_inbox_refresh: bool,
    pending_mutation_labels: HashMap<u64, &'static str>,
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

    fn queue_unknown_mailbox_statuses(&mut self, priority: Option<&str>) {
        let unknown = self
            .mailboxes
            .iter()
            .filter(|mailbox| !mailbox.counts_known)
            .map(|mailbox| mailbox.name.clone())
            .collect::<Vec<_>>();

        self.mailbox_status_queue
            .retain(|name| unknown.iter().any(|candidate| candidate == name));

        if let Some(priority) = priority
            && unknown.iter().any(|name| name == priority)
        {
            self.mailbox_status_queue.retain(|name| name != priority);
            self.mailbox_status_queue.push_front(priority.to_owned());
        }

        for name in unknown {
            if !self.mailbox_status_queue.iter().any(|queued| queued == &name) {
                self.mailbox_status_queue.push_back(name);
            }
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

    fn request_seen(&mut self, id: MessageId, seen: bool) -> bool {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return false;
        };
        if self.pending_seen.contains(&id) {
            return false;
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
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone)]
enum MutationKind {
    Delete,
    Move { destination: String },
    Archive { destination: String },
    Unarchive { destination: String },
}

#[derive(Debug, Clone)]
pub struct MutationConfirmation {
    pub prompt: String,
    source: String,
    ids: Vec<MessageId>,
    kind: MutationKind,
}

struct ReaderImage {
    source: String,
    protocol: Protocol,
}

fn reader_image_target_size(
    native_width: u32,
    native_height: u32,
    html_width: Option<u32>,
    html_height: Option<u32>,
    font_width: u16,
    font_height: u16,
    max_width_cells: u16,
    max_height_cells: u16,
) -> Size {
    let native_width = native_width.max(1);
    let native_height = native_height.max(1);
    let font_width = u32::from(font_width.max(1));
    let font_height = u32::from(font_height.max(1));

    // HTML width/height are CSS pixels, while Picker::font_size() is reported
    // in terminal/device pixels. Email templates commonly ship 2x (Retina)
    // bitmaps and ask browsers to display them at half the backing resolution.
    // Treat the sender's CSS box as up to 2 device pixels per CSS pixel, capped
    // by the decoded bitmap below. This lets a 148x48 logo declared as 74x24
    // occupy its natural 148x48 device-pixel footprint instead of looking about
    // half-size in a HiDPI terminal, without ever inventing pixels for a 1x asset.
    const EMAIL_CSS_DEVICE_SCALE: u32 = 2;
    let scale_css_px = |value: u32| value.max(1).saturating_mul(EMAIL_CSS_DEVICE_SCALE);

    // Keep the decoded source at full resolution and use the scaled HTML box
    // only to choose the terminal-cell area handed to ratatui-image. The bitmap
    // itself is still resized exactly once by the graphics protocol.
    let (desired_width_px, desired_height_px) = match (html_width, html_height) {
        (Some(width), Some(height)) => (scale_css_px(width), scale_css_px(height)),
        (Some(width), None) => {
            let width = scale_css_px(width);
            let height = u32::try_from(
                (u64::from(native_height) * u64::from(width)).div_ceil(u64::from(native_width)),
            )
            .unwrap_or(u32::MAX)
            .max(1);
            (width, height)
        }
        (None, Some(height)) => {
            let height = scale_css_px(height);
            let width = u32::try_from(
                (u64::from(native_width) * u64::from(height)).div_ceil(u64::from(native_height)),
            )
            .unwrap_or(u32::MAX)
            .max(1);
            (width, height)
        }
        (None, None) => (native_width, native_height),
    };

    // Preserve the no-upscale rule. A high-DPI source can use its extra backing
    // pixels, but a true 1x/tiny bitmap is never enlarged beyond native size.
    let desired_width_px = desired_width_px.min(native_width);
    let desired_height_px = desired_height_px.min(native_height);
    let width_cells = u16::try_from(desired_width_px.div_ceil(font_width))
        .unwrap_or(u16::MAX)
        .max(1)
        .min(max_width_cells.max(1));
    let height_cells = u16::try_from(desired_height_px.div_ceil(font_height))
        .unwrap_or(u16::MAX)
        .max(1)
        .min(max_height_cells.max(1));

    Size::new(width_cells, height_cells)
}

pub struct App {
    pub accounts: Vec<AccountState>,
    pub account_index: usize,
    pub focus: Focus,
    pub status: StatusLine,
    pub should_quit: bool,
    inbox_poll_interval: Duration,
    new_mail_bell_enabled: bool,
    new_mail_bell_pending: bool,
    pub mailbox_width_percent: u16,
    pub sidebar_visible: bool,
    pub preview_split: bool,
    pub message_open: bool,
    pub reader_cursor: usize,
    pub reader_column: usize,
    pub reader_visual_anchor: Option<ReaderPosition>,
    reader_visual_linewise: bool,
    reader_smooth_target: Option<usize>,
    reader_preferred_column: Option<usize>,
    reader_pending_g: bool,
    reader_pending_inner_object: bool,
    reader_body_start: usize,
    reader_line_count: usize,
    reader_view_height: usize,
    image_picker: Option<Picker>,
    image_loader: Option<MailImageLoader>,
    reader_image: Option<ReaderImage>,
    reader_image_pending: Option<String>,
    reader_image_size_hint: Option<(Option<u32>, Option<u32>)>,
    reader_image_target_width: u16,
    reader_image_target_height: u16,
    pub reader_images_enabled: bool,
    pub show_message_ids: bool,
    pub attachment_picker_visible: bool,
    pub attachment_selection: usize,
    pub attachment_files: Vec<OutgoingAttachment>,
    attachment_pending: Option<MessageId>,
    attachment_preview: Option<Protocol>,
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
    resumed_draft_account: Option<usize>,
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
                prefer_html: true,
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
                mailbox_status_inflight: None,
                inbox_poll_inflight: None,
                pending_inbox_refresh: false,
                pending_mutation_labels: HashMap::new(),
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
            inbox_poll_interval: Duration::from_secs(config.ui.inbox_poll_seconds.clamp(10, 3600)),
            new_mail_bell_enabled: config.ui.new_mail_bell,
            new_mail_bell_pending: false,
            mailbox_width_percent: config.ui.mailbox_width_percent.clamp(20, 50),
            sidebar_visible: true,
            preview_split: false,
            message_open: false,
            reader_cursor: 0,
            reader_column: 0,
            reader_visual_anchor: None,
            reader_visual_linewise: false,
            reader_smooth_target: None,
            reader_preferred_column: None,
            reader_pending_g: false,
            reader_pending_inner_object: false,
            reader_body_start: 0,
            reader_line_count: 0,
            reader_view_height: 1,
            image_picker: None,
            image_loader: None,
            reader_image: None,
            reader_image_pending: None,
            reader_image_size_hint: None,
            reader_image_target_width: 48,
            reader_image_target_height: 18,
            reader_images_enabled: config.ui.remote_images,
            show_message_ids: false,
            attachment_picker_visible: false,
            attachment_selection: 0,
            attachment_files: Vec::new(),
            attachment_pending: None,
            attachment_preview: None,
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
            resumed_draft_account: None,
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

    #[must_use]
    pub fn inbox_poll_interval(&self) -> Duration { self.inbox_poll_interval }

    /// Poll only Inbox counts, not every message body or folder. IMAP servers
    /// without IDLE can still report new mail with one inexpensive STATUS.
    pub fn poll_inboxes(&mut self) {
        for account in &mut self.accounts {
            if account.capabilities.is_none() || account.mailboxes_loading
                || account.inbox_poll_inflight.is_some()
                || account.mailbox_status_inflight.is_some()
                || account.pending_mutations > 0
            { continue; }
            let Some(inbox) = account.mailboxes.iter()
                .find(|mailbox| mailbox.role == MailboxRole::Inbox)
                .map(|mailbox| mailbox.name.clone()) else { continue; };
            if let Ok(request) = account.worker.post(ActionKind::MailboxStatus(inbox)) {
                account.inbox_poll_inflight = Some(request);
            }
        }
    }

    /// Terminal BEL is emitted in the main runtime, never from a worker.
    pub fn take_new_mail_bell(&mut self) -> bool {
        std::mem::take(&mut self.new_mail_bell_pending)
    }

    pub fn bootstrap(&mut self) {
        for account in &mut self.accounts {
            if account.worker.post(ActionKind::Connect).is_ok() {
                account.connecting = true;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.attachment_picker_visible {
            self.handle_attachment_picker_key(key);
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            self.restore_postponed_composer();
            return;
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

    fn handle_reader_key(&mut self, key: KeyEvent) -> bool {
        if self.focus != Focus::Preview || !self.message_open {
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
            if let KeyCode::Char(delimiter) = key.code {
                if self.select_reader_inner_object(delimiter) {
                    return true;
                }
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
                self.toggle_message_ids();
                true
            }
            KeyCode::Char('z') => {
                self.open_attachment_picker();
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
            KeyCode::Char('T') => {
                self.count_prefix = None;
                self.toggle_html();
                true
            }
            KeyCode::Char('I') => {
                self.count_prefix = None;
                self.toggle_remote_images();
                true
            }
            _ => false,
        }
    }

    pub fn handle_worker_event(&mut self, event: WorkerEvent) {
        let own_addresses = self.accounts.iter()
            .filter_map(|a| a.from.parse::<lettre::message::Mailbox>().ok())
            .map(|a| a.email.to_string())
            .collect::<Vec<_>>();
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
                if let Some(name) = account
                    .selected_mailbox()
                    .map(|mailbox| mailbox.name.clone())
                {
                    account.queue_unknown_mailbox_statuses(Some(&name));
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
                if account.mailbox_status_inflight == Some(event.request_id) {
                    account.mailbox_status_inflight = None;
                }
                let from_poll = account.inbox_poll_inflight == Some(event.request_id);
                if from_poll { account.inbox_poll_inflight = None; }
                let mut new_count = 0usize;
                if let Some(mailbox) = account.mailboxes.iter_mut().find(|m| m.name == name) {
                    new_count = inbox_new_mail_count(
                        from_poll && mailbox.role == MailboxRole::Inbox,
                        mailbox.counts_known,
                        mailbox.total,
                        total,
                    );
                    mailbox.total = total;
                    mailbox.unread = unread;
                    mailbox.counts_known = true;
                }
                if new_count > 0 {
                    // A cached Inbox from another tab is now stale even if the
                    // user is browsing a different mailbox/account.
                    account.mailbox_cache.remove(&name);
                    self.status = StatusLine::temporary(format!(
                        "{} new message(s) in {}", new_count, account.name,
                    ), Duration::from_secs(6));
                    if self.new_mail_bell_enabled { self.new_mail_bell_pending = true; }
                    if account.current_mailbox.as_deref() == Some(name.as_str()) {
                        // Never replace the reader underneath someone who is
                        // in the middle of an email. Defer the page refresh
                        // until the reader closes; the count still updates.
                        if self.message_open && event.account == self.account_index
                            || account.page_inflight.is_some()
                        {
                            account.pending_inbox_refresh = true;
                        } else {
                            account.restore_message_index = Some(account.message_index);
                            let _ = account.request_mailbox_page(name, 0);
                        }
                    }
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
                let visible = account.visible_message_indices();
                let search_needs_more = !account.search_query.is_empty()
                    && visible.len() < account.message_view_height
                    && account.has_more
                    && account.page_inflight.is_none();
                let current_position = visible
                    .iter()
                    .position(|index| *index == account.message_index)
                    .unwrap_or(0);
                let remaining = visible
                    .len()
                    .saturating_sub(current_position.saturating_add(1));
                let prefetch_distance = account
                    .message_view_height
                    .saturating_mul(4)
                    .max(account.page_size)
                    .max(PAGE_PREFETCH_MIN);
                let browse_needs_more = account.search_query.is_empty()
                    && account.has_more
                    && account.page_inflight.is_none()
                    && remaining <= prefetch_distance;
                if search_needs_more || browse_needs_more {
                    let _ = account.request_next_page();
                }
                if first_page && event.account == self.account_index && !self.message_open {
                    self.visual_anchor = None;
                    self.focus = Focus::Messages;
                }
                self.status = StatusLine::temporary(
                    format!(
                        "{}: {} messages loaded from {} in {:.2} ms",
                        account.name,
                        account.messages.len(),
                        name,
                        elapsed_micros as f64 / 1000.0
                    ),
                    Duration::from_secs(4),
                );
                if self.preview_split {
                    account.request_selected_body();
                }
                if account.pending_inbox_refresh && account.page_inflight.is_none()
                    && !(self.message_open && event.account == self.account_index)
                {
                    account.pending_inbox_refresh = false;
                    let _ = account.request_mailbox_page(name.clone(), 0);
                }
                let selected_mailbox = account
                    .selected_mailbox()
                    .map(|mailbox| mailbox.name.clone());
                account.queue_unknown_mailbox_statuses(selected_mailbox.as_deref());
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::BodyLoaded(body) => {
                let id = body.id.clone();
                account.pending_bodies.remove(&id);
                account.bodies.insert(id.clone(), body);

                if (self.message_open || self.preview_split)
                    && account
                        .selected_message()
                        .is_some_and(|message| message.id == id && message.unread())
                {
                    account.request_seen(id, true);
                }
                Self::pump_mailbox_status(account);

            }
            WorkerEventKind::AttachmentsLoaded { id, attachments } => {
                if self.attachment_pending.as_ref() == Some(&id) && self.attachment_picker_visible {
                    self.attachment_files = attachments;
                    self.attachment_selection = 0;
                    self.attachment_pending = None;
                    self.status = StatusLine::info(format!("{} attachment(s) loaded", self.attachment_files.len()));
                }
            }
            WorkerEventKind::RawLoaded { id, raw } => {
                let pending = self.pending_compose.as_ref().filter(|pending| {
                    pending.account == event.account && pending.message_id == id
                });
                let result = pending.and_then(|pending| {
                    let message = account.messages.iter()
                        .find(|message| message.id == pending.message_id)?;
                    Some(match pending.intent {
                        ComposeIntent::Forward => Composer::forward(message, raw),
                        ComposeIntent::Reply { quote, audience } => {
                            (|| -> anyhow::Result<Composer> {
                                let targets = reply_recipients(&raw, audience, &own_addresses)?;
                                let body = parse_body(id, &raw)?;
                                let mut composer = Composer::reply(message, Some(&body), quote);
                                composer.to = targets.to;
                                composer.cc = targets.cc;
                                Ok(composer)
                            })()
                        }
                    })
                });
                match result {
                    Some(Ok(composer)) => {
                        self.pending_compose = None;
                        self.composer = Some(composer);
                        self.resumed_draft_account = None;
                        self.status = StatusLine::info("ready to compose reply/forward");
                    }
                    Some(Err(error)) => {
                        self.pending_compose = None;
                        self.status = StatusLine::error(format!("cannot prepare reply: {error:#}"));
                    }
                    None => {}
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
                let message = account.pending_mutation_labels.remove(&event.request_id)
                    .unwrap_or("mail updated");
                self.status = StatusLine::temporary(message, Duration::from_secs(2));
            }
            WorkerEventKind::MessageSent {
                smtp_status,
                sent_mailbox,
                sent_copy_saved,
                sent_copy_warning,
            } => {
                // A successful SMTP submission makes our mailbox snapshots stale even
                // when the provider files Sent itself (Gmail), and especially when the
                // message was addressed back to this account. Force the next visit to
                // INBOX/Sent to come from the server rather than the in-memory cache.
                let mut refresh_current = None;
                let affected = account
                    .mailboxes
                    .iter()
                    .filter(|mailbox| {
                        matches!(mailbox.role, MailboxRole::Inbox | MailboxRole::Sent)
                            || sent_mailbox
                                .as_deref()
                                .is_some_and(|name| name == mailbox.name)
                    })
                    .map(|mailbox| mailbox.name.clone())
                    .collect::<Vec<_>>();
                for name in &affected {
                    account.mailbox_cache.remove(name);
                    if let Some(mailbox) = account
                        .mailboxes
                        .iter_mut()
                        .find(|mailbox| mailbox.name == *name)
                    {
                        mailbox.counts_known = false;
                    }
                }
                account.queue_unknown_mailbox_statuses(sent_mailbox.as_deref());
                if account
                    .current_mailbox
                    .as_ref()
                    .is_some_and(|name| affected.iter().any(|affected| affected == name))
                {
                    refresh_current = account.current_mailbox.clone();
                }
                if let Some(name) = refresh_current {
                    let _ = account.request_mailbox_page(name, 0);
                }
                Self::pump_mailbox_status(account);

                if self.resumed_draft_account == Some(event.account) {
                    if let Ok(root) = Self::postponed_drafts_root() {
                        let _ = crate::outgoing::drafts::delete_draft(&root, &account.name);
                    }
                    self.resumed_draft_account = None;
                }
                if event.account == self.account_index {
                    self.composer = None;
                }
                self.status = if let Some(warning) = sent_copy_warning {
                    StatusLine::temporary(
                        format!("SMTP accepted ({smtp_status}); {warning}"),
                        Duration::from_secs(8),
                    )
                } else if sent_copy_saved {
                    StatusLine::temporary(
                        format!("SMTP accepted ({smtp_status}); saved to Sent"),
                        Duration::from_secs(5),
                    )
                } else {
                    StatusLine::temporary(
                        format!("SMTP accepted ({smtp_status}); provider handles Sent copy"),
                        Duration::from_secs(5),
                    )
                };
            }
            WorkerEventKind::Error(error) => {
                if account.inbox_poll_inflight == Some(event.request_id) {
                    // Background STATUS is best-effort: a transient server
                    // failure must not cancel an in-progress preview/compose
                    // or reset the mailbox browser state.
                    account.inbox_poll_inflight = None;
                    self.status = StatusLine::temporary(
                        format!("{}: Inbox check failed ({error})", account.name),
                        Duration::from_secs(4),
                    );
                    return;
                }
                let mutation_failed = account.pending_mutations > 0;
                let seen_failed = !account.pending_seen.is_empty();
                account.pending_mutations = 0;
                account.pending_mutation_labels.clear();
                account.inbox_poll_inflight = None;
                account.connecting = false;
                account.mailboxes_loading = false;
                account.messages_loading = false;
                account.page_inflight = None;
                if account.mailbox_status_inflight == Some(event.request_id) {
                    account.mailbox_status_inflight = None;
                }
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
                if (mutation_failed || seen_failed)
                    && let Some(name) = account.current_mailbox.clone()
                {
                    account.invalidate_current_cache();
                    let _ = account.request_mailbox_page(name, 0);
                }
                // STATUS failures are mailbox-local. Do not leave every other
                // folder stuck on an unknown count just because one provider
                // rejects STATUS for a special mailbox.
                Self::pump_mailbox_status(account);
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
            Command::Unarchive => {
                self.count_prefix = None;
                self.unarchive_selected();
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
            Command::ToggleImages => {
                self.count_prefix = None;
                self.toggle_remote_images();
            }
            Command::ToggleBell => {
                self.count_prefix = None;
                self.new_mail_bell_enabled = !self.new_mail_bell_enabled;
                self.status = StatusLine::temporary(
                    if self.new_mail_bell_enabled { "new-mail bell enabled" }
                    else { "new-mail bell muted" },
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
                if self.reader_visual_anchor.take().is_some() {
                    self.reader_visual_linewise = false;
                    self.status = StatusLine::temporary(
                        "reader visual selection cleared",
                        Duration::from_secs(1),
                    );
                } else if self.visual_anchor.take().is_some() {
                    self.status = StatusLine::temporary(
                        "visual selection cleared",
                        Duration::from_secs(1),
                    );
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
        self.reader_smooth_target.is_some() || self.account().is_some_and(|account| {
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
        if focus == Focus::Preview {
            let Some(target) = self.reader_smooth_target else {
                return;
            };
            if self.reader_line_count == 0 {
                self.reader_smooth_target = None;
                return;
            }

            let preferred = self.reader_preferred_column.unwrap_or(self.reader_column);
            if self.reader_cursor < target {
                self.reader_cursor = self.reader_cursor.saturating_add(1);
            } else if self.reader_cursor > target {
                self.reader_cursor = self.reader_cursor.saturating_sub(1);
            }
            let line_len = self.reader_line_len(self.reader_cursor);
            self.reader_column = if line_len == 0 {
                0
            } else {
                preferred.min(line_len.saturating_sub(1))
            };
            self.reader_preferred_column = Some(preferred);
            if self.reader_cursor == target {
                self.reader_smooth_target = None;
            }
            self.keep_reader_cursor_visible();
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
                    if remaining <= prefetch_distance {
                        let _ = account.request_next_page();
                    }
                    if next == target_position {
                        account.smooth_message_target = None;
                        if preview_split {
                            account.request_selected_body();
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

    fn pump_mailbox_status(account: &mut AccountState) {
        if account.mailbox_status_inflight.is_some()
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
            if let Ok(request_id) = account
                .worker
                .post(ActionKind::MailboxStatus(name.clone()))
            {
                account.mailbox_status_inflight = Some(request_id);
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
        self.reader_smooth_target = None;
        self.reader_visual_linewise = false;
        self.pending_compose = None;
        self.visual_anchor = None;
        self.confirmation = None;
        self.focus = Focus::Messages;
        self.account_index = offset_index(self.account_index, self.accounts.len(), delta);
        if let Some(account) = self.account() {
            self.status = StatusLine::temporary(
                format!("account: {}", account.name),
                Duration::from_secs(2),
            );
        }
    }

    fn move_selection(&mut self, delta: isize) {
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
            if preview_split {
                account.request_selected_body();
            }
        }

        let remaining = visible.len().saturating_sub(next.saturating_add(1));
        let prefetch_distance = account
            .message_view_height
            .saturating_mul(4)
            .max(account.page_size)
            .max(PAGE_PREFETCH_MIN);
        if delta > 0 && remaining <= prefetch_distance {
            let _ = account.request_next_page();
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
        account.queue_unknown_mailbox_statuses(Some(&name));
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
        if !body.has_html_alternative()
            || !prefer_html
            || body.html_render_width == width
        {
            return;
        }
        let Some(source) = body.html_source.clone() else {
            return;
        };
        let (html_text, html_lines, html_renderer) = render_html_for_terminal(&source, width);
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
        body_start: usize,
        line_count: usize,
        view_height: usize,
        max_scroll: u16,
    ) {
        self.reader_body_start = body_start;
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

    fn reader_display_text(&self) -> Option<&str> {
        let account = self.account()?;
        let body = account.selected_body()?;
        let show_html = body.has_html_alternative() && account.prefer_html;
        Some(body.display_text(show_html))
    }

    fn reader_line_text(&self, line: usize) -> Option<&str> {
        self.reader_display_text()?.lines().nth(line)
    }

    fn reader_line_len(&self, line: usize) -> usize {
        self.reader_line_text(line).map_or(0, |text| text.chars().count())
    }

    fn clamp_reader_column(&mut self) {
        let line_len = self.reader_line_len(self.reader_cursor);
        self.reader_column = if line_len == 0 {
            0
        } else {
            self.reader_column.min(line_len.saturating_sub(1))
        };
    }

    fn reader_first_non_blank(&self) -> usize {
        self.reader_line_text(self.reader_cursor)
            .and_then(|line| line.chars().position(|ch| !ch.is_whitespace()))
            .unwrap_or(0)
    }

    fn move_reader_cursor(&mut self, delta: isize) {
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

    fn move_reader_horizontal(&mut self, delta: isize) {
        let line_len = self.reader_line_len(self.reader_cursor);
        if line_len == 0 {
            self.reader_column = 0;
        } else {
            self.reader_column = bounded_index(self.reader_column, line_len, delta);
        }
        self.reader_preferred_column = None;
    }

    fn jump_reader_to_line(&mut self, line: usize) {
        if self.reader_line_count == 0 {
            return;
        }
        self.reader_cursor = line.min(self.reader_line_count.saturating_sub(1));
        self.clamp_reader_column();
        self.reader_preferred_column = None;
        self.keep_reader_cursor_visible();
    }

    fn move_reader_word(&mut self, motion: ReaderWordMotion) {
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

    fn move_reader_paragraph(&mut self, forward: bool) {
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
                    (0..blank).rev().find(|index| !lines[*index].trim().is_empty())
                })
                .unwrap_or(0)
        };
        self.reader_cursor = target;
        self.reader_column = self.reader_first_non_blank();
        self.reader_preferred_column = None;
        self.keep_reader_cursor_visible();
    }

    fn select_reader_inner_object(&mut self, delimiter: char) -> bool {
        if self.reader_visual_anchor.is_none() {
            return false;
        }
        let Some(line) = self.reader_line_text(self.reader_cursor).map(ToOwned::to_owned) else {
            return false;
        };
        let Some((start, end)) = inner_text_object_range(&line, self.reader_column, delimiter) else {
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

    fn keep_reader_cursor_visible(&mut self) {
        let body_start = self.reader_body_start;
        let cursor_row = body_start.saturating_add(self.reader_cursor);
        let view_height = self.reader_view_height.max(1);
        let total_rows = body_start
            .saturating_add(self.reader_line_count)
            .max(view_height);

        let reader_cursor = self.reader_cursor;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.smooth_preview_target = None;

        // When Vim/neoscroll returns to the first body line, restore the true
        // top of the message as well. The image sits between the headers and the
        // body, so leaving scrolloff anchored at `body_start` kept the cursor
        // visible while the header image remained scrolled away permanently.
        if reader_cursor == 0 {
            account.preview_scroll = 0;
            return;
        }

        let current_offset = usize::from(account.preview_scroll);
        let scrolloff = READER_SCROLL_OFF.min(view_height.saturating_sub(1) / 2);
        let max_offset = total_rows.saturating_sub(view_height);
        let upper_guard = current_offset.saturating_add(scrolloff);
        let lower_guard = current_offset
            .saturating_add(view_height.saturating_sub(scrolloff + 1));
        let mut next_offset = current_offset.min(max_offset);

        if cursor_row < upper_guard {
            next_offset = cursor_row.saturating_sub(scrolloff);
        } else if cursor_row > lower_guard {
            next_offset = cursor_row
                .saturating_add(scrolloff + 1)
                .saturating_sub(view_height)
                .min(max_offset);
        }

        account.preview_scroll = u16::try_from(next_offset)
            .unwrap_or(u16::MAX)
            .min(account.preview_max_scroll);
    }

    fn start_smooth_scroll(&mut self, delta: isize) {
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
                account.smooth_message_target = Some(bounded_index(base, visible.len(), delta));
            }
            Focus::Preview => {
                unreachable!("reader smooth scrolling is handled before borrowing account state")
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
                account.preview_scroll = if bottom { account.preview_max_scroll } else { 0 };
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
        if preview_split {
            account.request_selected_body();
        }
    }

    fn open_selection(&mut self) {
        match self.focus {
            Focus::Mailboxes | Focus::Messages => {
                if self.account().is_none_or(|account| account.selected_message().is_none()) {
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
                self.reader_body_start = 0;
                self.reader_line_count = 0;
                self.reader_image = None;
                self.reader_image_pending = None;
                self.reader_image_size_hint = None;
                self.message_open = true;
                if let Some(account) = self.account_mut() {
                    account.preview_scroll = 0;
                    account.preview_max_scroll = 0;
                    account.request_selected_body();
                    Self::mark_cached_selected_seen(account);
                }
                self.focus = Focus::Preview;
            }
            Focus::Preview => {}
        }
    }

    fn close_message(&mut self) {
        self.reader_cursor = 0;
        self.reader_column = 0;
        self.reader_preferred_column = None;
        self.reader_visual_anchor = None;
        self.reader_visual_linewise = false;
        self.reader_smooth_target = None;
        self.reader_pending_g = false;
        self.reader_pending_inner_object = false;
        self.reader_body_start = 0;
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

    fn open_reader_link(&mut self) {
        let Some(url) = self.link_under_reader_cursor() else {
            self.status = StatusLine::temporary(
                "reader cursor is not on a link",
                Duration::from_secs(1),
            );
            return;
        };
        let result = open_url(&url);
        self.status = match result {
            Ok(()) => StatusLine::temporary(format!("opened {url}"), Duration::from_secs(3)),
            Err(error) => StatusLine::error(format!("could not open link: {error}")),
        };
    }

    fn link_under_reader_cursor(&self) -> Option<String> {
        let account = self.account()?;
        let body = account.selected_body()?;
        let showing_html = body.has_html_alternative() && account.prefer_html;
        if showing_html
            && let Some(url) = body.html_link_at(self.reader_cursor, self.reader_column)
        {
            return Some(url.to_owned());
        }
        let line = body
            .display_text(showing_html)
            .lines()
            .nth(self.reader_cursor)?;
        url_at_cursor(line, self.reader_column)
    }

    fn yank_reader_selection(&mut self) {
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

        self.status = match copy_text_to_clipboard(&selected) {
            Ok(()) => {
                self.reader_visual_anchor = None;
                self.reader_visual_linewise = false;
                StatusLine::temporary("copied reader selection", Duration::from_secs(2))
            }
            Err(error) => StatusLine::error(format!("could not copy selection: {error}")),
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
        if !account.request_seen(id.clone(), seen) {
            return;
        }

        // Optimistic UI: reflect the flag immediately instead of waiting for an
        // IMAP round trip. The SeenChanged event later confirms the same value,
        // so its unread delta naturally becomes zero.
        let mut unread_delta = 0isize;
        if let Some(message) = account.messages.iter_mut().find(|message| message.id == id) {
            let was_unread = message.unread();
            message.flags.set(MessageFlags::SEEN, seen);
            unread_delta = match (was_unread, message.unread()) {
                (true, false) => -1,
                (false, true) => 1,
                _ => 0,
            };
        }
        if unread_delta != 0
            && let Some(current) = account.current_mailbox.as_deref()
            && let Some(mailbox) = account
                .mailboxes
                .iter_mut()
                .find(|mailbox| mailbox.name == current && mailbox.counts_known)
        {
            mailbox.unread = mailbox.unread.saturating_add_signed(unread_delta);
        }
    }

    fn toggle_visual(&mut self) {
        if self.focus == Focus::Preview && self.message_open {
            if self.reader_line_count == 0 {
                return;
            }
            if self.reader_visual_anchor.is_some() {
                self.reader_visual_anchor = None;
                self.reader_visual_linewise = false;
            } else {
                self.reader_visual_linewise = false;
                self.reader_visual_anchor = Some(ReaderPosition {
                    line: self.reader_cursor,
                    column: self.reader_column,
                });
            }
            self.reader_pending_inner_object = false;
            return;
        }

        if self.message_open || self.account().is_none_or(|account| account.messages.is_empty()) {
            return;
        }
        self.visual_anchor = if self.visual_anchor.is_some() {
            None
        } else {
            self.account().map(|account| account.message_index)
        };
    }

    fn toggle_reader_linewise_visual(&mut self) {
        if self.focus != Focus::Preview || !self.message_open || self.reader_line_count == 0 {
            return;
        }

        if self.reader_visual_anchor.is_some() && self.reader_visual_linewise {
            self.reader_visual_anchor = None;
            self.reader_visual_linewise = false;
        } else {
            let anchor_line = self
                .reader_visual_anchor
                .map_or(self.reader_cursor, |anchor| anchor.line);
            self.reader_visual_anchor = Some(ReaderPosition {
                line: anchor_line,
                column: 0,
            });
            self.reader_visual_linewise = true;
        }
        self.reader_pending_inner_object = false;
    }

    #[must_use]
    pub fn reader_visual_range(&self) -> Option<ReaderSelection> {
        let anchor = self.reader_visual_anchor?;
        if self.reader_visual_linewise {
            let (start_line, end_line) = if anchor.line <= self.reader_cursor {
                (anchor.line, self.reader_cursor)
            } else {
                (self.reader_cursor, anchor.line)
            };
            return Some(ReaderSelection {
                start: ReaderPosition {
                    line: start_line,
                    column: 0,
                },
                end: ReaderPosition {
                    line: end_line,
                    column: usize::MAX,
                },
            });
        }
        let cursor = ReaderPosition {
            line: self.reader_cursor,
            column: self.reader_column,
        };
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        Some(ReaderSelection { start, end })
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

    fn unarchive_selected(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() { return; }
        let Some(account) = self.account() else { return; };
        let Some(source) = account.current_mailbox.clone() else { return; };
        if !account.mailboxes.iter().any(|m| m.name == source && m.role == MailboxRole::Archive) {
            self.status = StatusLine::temporary(
                "open Archive / All Mail, then press U to restore to INBOX",
                Duration::from_secs(3),
            );
            return;
        }
        let Some(inbox) = account.mailboxes.iter()
            .find(|m| m.role == MailboxRole::Inbox && m.name != source)
            .map(|m| m.name.clone()) else {
                self.status = StatusLine::error("no INBOX available to restore archived mail");
                return;
            };
        self.start_mutation(source, ids, MutationKind::Unarchive { destination: inbox });
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
        if account.mailboxes.iter().any(|mailbox| mailbox.name == source && mailbox.role == MailboxRole::Archive) {
            self.status = StatusLine::temporary("message is already in Archive / All Mail", Duration::from_secs(2));
            return;
        }
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
        // Gmail All Mail is virtual. The IMAP UID delete/expunge used to
        // remove INBOX membership depends on the account's deletion settings,
        // so require explicit confirmation instead of hiding the risk.
        if source.eq_ignore_ascii_case("INBOX")
            && (destination.to_ascii_lowercase().starts_with("[gmail]/")
                || destination.to_ascii_lowercase().starts_with("[googlemail]/"))
        {
            self.confirmation = Some(MutationConfirmation {
                prompt: format!("Archive {} Gmail message(s)? This uses IMAP expunge; check Gmail deletion settings first. [y/N]", ids.len()),
                source, ids, kind: MutationKind::Archive { destination },
            });
        } else {
            self.start_mutation(source, ids, MutationKind::Archive { destination });
        }
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
                source: source.clone(), destination: destination.clone(), ids: ids.clone(),
            },
            MutationKind::Archive { destination } => ActionKind::ArchiveMany {
                source: source.clone(), destination: destination.clone(), ids: ids.clone(),
            },
            MutationKind::Unarchive { destination } => ActionKind::UnarchiveMany {
                source: source.clone(), destination: destination.clone(), ids: ids.clone(),
            },
        };

        let moving_to_trash = matches!(&kind, MutationKind::Move { destination }
            if self.account().is_some_and(|account| account.mailboxes.iter()
                .any(|mailbox| mailbox.name == *destination && mailbox.role == MailboxRole::Trash)));
        let (pending_label, complete_label) = match &kind {
            MutationKind::Delete => ("permanently deleting message(s)…", "message(s) permanently deleted"),
            MutationKind::Move { .. } if moving_to_trash => ("deleting message(s) to Trash…", "message(s) deleted to Trash"),
            MutationKind::Move { .. } => ("moving message(s)…", "message(s) moved"),
            MutationKind::Archive { .. } => ("archiving message(s)…", "message(s) archived"),
            MutationKind::Unarchive { .. } => ("restoring message(s) to INBOX…", "message(s) restored to INBOX"),
        };
        let started = self
            .account_mut()
            .is_some_and(|account| match account.worker.post(action) {
                Ok(request) => {
                    account.pending_mutations = account.pending_mutations.saturating_add(1);
                    account.pending_mutation_labels.insert(request, complete_label);
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
        self.status = StatusLine::info(pending_label);
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
        if removed.is_empty() { return; }
        // Gmail All Mail is a view containing both archived and INBOX messages.
        // Unarchiving adds the INBOX label without removing that All Mail row.
        if let MutationKind::Unarchive { destination } = kind
            && (source.to_ascii_lowercase().starts_with("[gmail]/")
                || source.to_ascii_lowercase().starts_with("[googlemail]/"))
        {
            account.mailbox_cache.remove(destination);
            if let Some(mailbox) = account.mailboxes.iter_mut()
                .find(|mailbox| mailbox.name == *destination && mailbox.counts_known)
            {
                mailbox.total = mailbox.total.saturating_add(removed.len());
                mailbox.unread = mailbox.unread.saturating_add(removed.iter().filter(|m| m.unread()).count());
            }
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
        if let MutationKind::Move { destination }
            | MutationKind::Archive { destination }
            | MutationKind::Unarchive { destination } = kind {
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

    fn open_original_html(&mut self) {
        let Some(html) = self
            .account()
            .and_then(AccountState::selected_body)
            .and_then(|body| body.html_source.as_deref())
            .filter(|html| !html.trim().is_empty())
            .map(str::to_owned)
        else {
            self.status = StatusLine::temporary(
                "selected message does not have an HTML original",
                Duration::from_secs(2),
            );
            return;
        };

        let directory = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("aerc-rs")
            .join("browser");
        if let Err(error) = std::fs::create_dir_all(&directory) {
            self.status = StatusLine::error(format!(
                "could not create browser preview directory: {error}"
            ));
            return;
        }
        let path = directory.join("message.html");
        if let Err(error) = std::fs::write(&path, html.as_bytes()) {
            self.status = StatusLine::error(format!(
                "could not write browser preview: {error}"
            ));
            return;
        }

        match open_url(path.to_string_lossy().as_ref()) {
            Ok(()) => {
                self.status = StatusLine::temporary(
                    "opened original HTML email in browser",
                    Duration::from_secs(3),
                );
            }
            Err(error) => {
                self.status = StatusLine::error(format!(
                    "could not open original HTML email: {error}"
                ));
            }
        }
    }

    fn toggle_html(&mut self) {
        let mode = {
            let Some(account) = self.account_mut() else {
                return;
            };
            if !account.selected_body().is_some_and(|body| {
                body.has_html_alternative() && body.has_plain_view()
            }) {
                None
            } else {
                account.prefer_html = !account.prefer_html;
                Some(account.prefer_html)
            }
        };
        let Some(prefer_html) = mode else {
            self.status = StatusLine::temporary(
                "selected message does not have both rich and plain views",
                Duration::from_secs(2),
            );
            return;
        };
        self.reader_cursor = 0;
        self.reader_column = 0;
        self.reader_preferred_column = None;
        self.reader_visual_anchor = None;
        self.reader_visual_linewise = false;
        self.reader_smooth_target = None;
        self.reader_pending_g = false;
        self.reader_pending_inner_object = false;
        self.reader_image = None;
        self.reader_image_pending = None;
        self.reader_image_size_hint = None;
        if let Some(account) = self.account_mut() {
            account.preview_scroll = 0;
            account.smooth_preview_target = None;
        }
        self.status = StatusLine::temporary(
            if prefer_html {
                "HTML mail rendering enabled"
            } else {
                "plain-text mail rendering enabled"
            },
            Duration::from_secs(2),
        );
    }

    fn toggle_message_ids(&mut self) {
        self.show_message_ids = !self.show_message_ids;
        self.status = StatusLine::temporary(
            if self.show_message_ids { "Message-ID shown" } else { "Message-ID hidden" },
            Duration::from_secs(2),
        );
    }

    fn open_attachment_picker(&mut self) {
        let Some(account) = self.account() else { return; };
        let Some(message) = account.selected_message() else { return; };
        let Some(mailbox) = account.current_mailbox.clone() else { return; };
        let id = message.id.clone();
        if account.worker.post(ActionKind::FetchAttachments { mailbox, id: id.clone() }).is_ok() {
            self.attachment_picker_visible = true;
            self.attachment_selection = 0;
            self.attachment_files.clear();
            self.attachment_preview = None;
            self.attachment_pending = Some(id);
            self.status = StatusLine::info("loading attachments…");
        } else {
            self.status = StatusLine::error("mail worker busy; cannot load attachments");
        }
    }

    fn handle_attachment_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.attachment_picker_visible = false;
                self.attachment_preview = None;
                self.attachment_pending = None;
                self.attachment_files.clear();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.attachment_files.is_empty() {
                    self.attachment_selection = (self.attachment_selection + 1)
                        .min(self.attachment_files.len() - 1);
                    self.attachment_preview = None;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.attachment_selection = self.attachment_selection.saturating_sub(1);
                self.attachment_preview = None;
            }
            KeyCode::Char('p') => self.preview_attachment(),
            KeyCode::Enter | KeyCode::Char('o') => self.open_attachment(),
            _ => {}
        }
    }

    fn preview_attachment(&mut self) {
        let Some(file) = self.attachment_files.get(self.attachment_selection) else { return; };
        let Some(picker) = &self.image_picker else {
            self.status = StatusLine::error("terminal image protocol unavailable");
            return;
        };
        match image::load_from_memory(&file.data) {
            Ok(image) => {
                let target = reader_image_target_size(image.width(), image.height(), None, None,
                    picker.font_size().width, picker.font_size().height, 70, 18);
                match picker.new_protocol(image, target, Resize::Fit(Some(image::imageops::FilterType::Lanczos3))) {
                    Ok(protocol) => self.attachment_preview = Some(protocol),
                    Err(error) => self.status = StatusLine::error(format!("attachment preview: {error}")),
                }
            }
            Err(_) => self.status = StatusLine::temporary(
                "not an image; press Enter to open with your system viewer",
                Duration::from_secs(3),
            ),
        }
    }

    #[must_use]
    pub fn attachment_preview(&self) -> Option<&Protocol> { self.attachment_preview.as_ref() }

    fn open_attachment(&mut self) {
        let Some(file) = self.attachment_files.get(self.attachment_selection) else { return; };
        match export_attachment_for_open(file) {
            Ok(path) => match open_url(&path.to_string_lossy()) {
                Ok(()) => self.status = StatusLine::temporary(
                    format!("opened {} with system viewer", file.filename), Duration::from_secs(3)),
                Err(error) => self.status = StatusLine::error(format!("could not open attachment: {error}")),
            },
            Err(error) => self.status = StatusLine::error(format!("could not export attachment: {error:#}")),
        }
    }

    pub fn configure_mail_images(
        &mut self,
        picker: Picker,
        runtime_tx: SyncSender<RuntimeEvent>,
    ) {
        self.image_picker = Some(picker);
        self.image_loader = Some(MailImageLoader::spawn(runtime_tx));
    }

    pub fn ensure_reader_image(&mut self, width: u16, height: u16) {
        if !self.reader_images_enabled || !self.message_open {
            return;
        }
        let Some(account) = self.account() else {
            return;
        };
        if !account.prefer_html {
            return;
        }
        let Some(candidate) = account
            .selected_body()
            .and_then(|body| body.html_image_candidates().into_iter().next())
        else {
            return;
        };
        let source = candidate.source;
        if self
            .reader_image
            .as_ref()
            .is_some_and(|image| image.source == source)
            || self.reader_image_pending.as_deref() == Some(source.as_str())
        {
            return;
        }
        self.reader_image = None;
        self.reader_image_target_width = width.max(1);
        self.reader_image_target_height = height.max(1);
        self.reader_image_size_hint = Some((candidate.width_px, candidate.height_px));
        if let Some(loader) = &self.image_loader
            && loader.request(source.clone()).is_ok()
        {
            self.reader_image_pending = Some(source);
            self.status = StatusLine::info("loading remote email image…");
        } else {
            self.reader_image_size_hint = None;
        }
    }

    pub fn handle_mail_image_event(&mut self, event: MailImageEvent) {
        if self.reader_image_pending.as_deref() != Some(event.source.as_str()) {
            return;
        }
        self.reader_image_pending = None;
        let size_hint = self.reader_image_size_hint.take().unwrap_or((None, None));
        let Some(picker) = &self.image_picker else {
            return;
        };
        match event.result {
            Ok(image) => {
                let font_size = picker.font_size();
                let target = reader_image_target_size(
                    image.width(),
                    image.height(),
                    size_hint.0,
                    size_hint.1,
                    font_size.width,
                    font_size.height,
                    self.reader_image_target_width,
                    self.reader_image_target_height,
                );
                match picker.new_protocol(
                    image,
                    target,
                    Resize::Fit(Some(image::imageops::FilterType::Lanczos3)),
                ) {
                    Ok(protocol) => {
                        self.reader_image = Some(ReaderImage {
                            source: event.source,
                            protocol,
                        });
                        self.status = StatusLine::info("remote email image loaded");
                    }
                    Err(error) => {
                        self.status = StatusLine::temporary(
                            format!("image render failed: {error}"),
                            Duration::from_secs(3),
                        );
                    }
                }
            }
            Err(error) => {
                if error != "tracking pixel ignored" {
                    self.status = StatusLine::temporary(error, Duration::from_secs(3));
                }
            }
        }
    }

    #[must_use]
    pub fn reader_image_protocol(&self) -> Option<&Protocol> {
        if self.reader_images_enabled {
            self.reader_image.as_ref().map(|image| &image.protocol)
        } else {
            None
        }
    }

    fn toggle_remote_images(&mut self) {
        self.reader_images_enabled = !self.reader_images_enabled;
        self.reader_image = None;
        self.reader_image_pending = None;
        self.reader_image_size_hint = None;
        self.status = StatusLine::temporary(
            if self.reader_images_enabled {
                "remote images enabled for this session; senders may observe image requests"
            } else {
                "remote images disabled"
            },
            Duration::from_secs(4),
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
        self.resumed_draft_account = None;
    }

    fn begin_reply(&mut self, quote: bool) {
        self.begin_compose_from_selected(ComposeIntent::Reply { quote, audience: ReplyAudience::Sender });
    }

    fn begin_reply_all(&mut self) {
        self.begin_compose_from_selected(ComposeIntent::Reply { quote: true, audience: ReplyAudience::All });
    }

    fn begin_reply_list(&mut self) {
        self.begin_compose_from_selected(ComposeIntent::Reply { quote: true, audience: ReplyAudience::List });
    }

    fn begin_forward(&mut self) {
        self.begin_compose_from_selected(ComposeIntent::Forward);
    }

    fn begin_compose_from_selected(&mut self, intent: ComposeIntent) {
        let account_index = self.account_index;
        if !self.account().is_some_and(|account| account.outgoing_configured) {
            self.status = StatusLine::error(
                "no outgoing SMTP transport configured; add `outgoing = ...` to this account",
            );
            return;
        }

        // Fetch the original MIME for every reply/forward. Reply targets must
        // come from the real To/Cc/Reply-To/List-Post headers, not the reader
        // projection or the sender summary. Forwarding preserves raw bytes.
        let Some((message_id, mailbox)) = self.account().and_then(|account| {
            Some((account.selected_message()?.id.clone(), account.current_mailbox.clone()?))
        }) else { return; };
        let queued = self.account().is_some_and(|account| {
            account.worker.post(ActionKind::FetchRawForCompose {
                mailbox, id: message_id.clone(),
            }).is_ok()
        });
        if queued {
            self.pending_compose = Some(PendingCompose { account: account_index, message_id, intent });
            self.status = StatusLine::info("loading original message headers…");
        } else {
            self.status = StatusLine::error("mail worker queue is busy; reply/forward not opened");
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

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.postpone_composer();
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

    fn postponed_drafts_root() -> anyhow::Result<std::path::PathBuf> {
        let root = dirs::data_local_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot find local application data directory"))?;
        Ok(root.join("aerc-rs").join("postponed"))
    }

    fn postpone_composer(&mut self) {
        let Some(account) = self.account() else { return; };
        let Some(composer) = self.composer.as_ref() else { return; };
        let result = Self::postponed_drafts_root().and_then(|root| {
            crate::outgoing::drafts::save_draft(&root, &account.name, composer)
        });
        match result {
            Ok(()) => {
                self.composer = None;
                self.resumed_draft_account = None;
                self.status = StatusLine::info("postponed locally; Ctrl-n resumes from this account");
            }
            Err(error) => self.status = StatusLine::error(format!("could not postpone: {error:#}")),
        }
    }

    fn restore_postponed_composer(&mut self) {
        let Some(account) = self.account() else { return; };
        if !account.outgoing_configured {
            self.status = StatusLine::error("configure outgoing SMTP before resuming a draft");
            return;
        }
        let result = Self::postponed_drafts_root().and_then(|root| {
            crate::outgoing::drafts::load_draft(&root, &account.name)
        });
        match result {
            Ok(Some(draft)) => {
                self.composer = Some(draft);
                self.resumed_draft_account = Some(self.account_index);
                self.status = StatusLine::info("resumed locally postponed draft");
            }
            Ok(None) => self.status = StatusLine::temporary("no postponed draft for this account", Duration::from_secs(2)),
            Err(error) => self.status = StatusLine::error(format!("cannot resume draft: {error:#}")),
        }
    }

    fn send_composer(&mut self) {
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        if composer.sending {
            return;
        }
        if composer.to.trim().is_empty() && composer.cc.trim().is_empty() && composer.bcc.trim().is_empty() {
            self.status = StatusLine::error("Add at least one To, Cc or Bcc recipient");
            return;
        }
        let mut attachments = match load_compose_attachments(&composer.attachments) {
            Ok(attachments) => attachments,
            Err(error) => {
                self.status = StatusLine::error(format!("attachment: {error:#}"));
                return;
            }
        };
        let custom_headers = match parse_compose_headers(&composer.headers) {
            Ok(headers) => headers,
            Err(error) => {
                self.status = StatusLine::error(format!("header: {error:#}"));
                return;
            }
        };
        let mut html_body = None;
        if let Some(raw) = composer.forwarded_raw.as_deref() {
            let parts = match inline_forward_parts(raw) {
                Ok(parts) => parts,
                Err(error) => {
                    self.status = StatusLine::error(format!("cannot inline-forward original email: {error:#}"));
                    return;
                }
            };
            attachments.extend(parts.attachments);
            if let Some(html) = parts.html {
                // Personal text preceding the forwarded-message marker belongs
                // above the original HTML. Preserve that HTML with its CSS.
                let personal_note = composer.body.split(FORWARD_SEPARATOR)
                    .next().unwrap_or("");
                let original_headers = composer.body.split(FORWARD_SEPARATOR)
                    .nth(1).and_then(|tail| tail.split("\n\n").next()).unwrap_or("");
                let attribution = format!("{FORWARD_SEPARATOR}{original_headers}");
                html_body = Some(forward_html(&html, &attribution, personal_note));
            }
        }
        let Some(account) = self.account() else {
            return;
        };
        let message = OutgoingMessage {
            from: account.from.clone(),
            to: composer.to.clone(),
            cc: composer.cc.clone(),
            bcc: composer.bcc.clone(),
            subject: composer.subject.clone(),
            body: composer.body.clone(),
            html_body,
            attachments,
            custom_headers,
            in_reply_to: composer.in_reply_to.clone(),
            references: composer.references.clone(),
            // The UI uses original bytes only to build inline alternatives and
            // attachments. Never send a .eml attachment for ordinary Forward.
            forwarded_raw: None,
        };
        let sent_mailbox = account
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role == MailboxRole::Sent)
            .map(|mailbox| mailbox.name.clone());
        if account
            .worker
            .post(ActionKind::Send {
                message,
                sent_mailbox,
            })
            .is_ok()
        {
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



fn parse_compose_headers(raw: &str) -> anyhow::Result<Vec<(String, String)>> {
    raw.split(';').filter(|part| !part.trim().is_empty()).map(|part| {
        let (name, value) = part.split_once(':')
            .ok_or_else(|| anyhow::anyhow!("use Header-Name: value; separated by semicolons"))?;
        Ok((name.trim().to_owned(), value.trim().to_owned()))
    }).collect()
}

fn load_compose_attachments(value: &str) -> anyhow::Result<Vec<OutgoingAttachment>> {
    let mut output = Vec::new();
    for part in value.split(';').map(str::trim).filter(|part| !part.is_empty()) {
        let path = std::path::Path::new(part);
        let metadata = std::fs::metadata(path)?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
            anyhow::bail!("attachments must be files under 16 MiB: {part}");
        }
        let filename = path.file_name().and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("attachment has no valid UTF-8 filename"))?
            .to_owned();
        let content_type = match path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
            "txt" | "patch" | "diff" | "log" | "md" => "text/plain",
            "html" | "htm" => "text/html",
            "pdf" => "application/pdf",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "eml" => "message/rfc822",
            "zip" => "application/zip",
            _ => "application/octet-stream",
        };
        output.push(OutgoingAttachment {
            filename,
            content_type: content_type.to_owned(),
            data: std::fs::read(path)?,
        });
    }
    Ok(output)
}

fn run_clipboard_command(program: &str, args: &[&str], text: &str) -> std::io::Result<()> {
    let mut child = ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{program} exited with status {status}"
        )))
    }
}

#[cfg(target_os = "windows")]
fn copy_text_to_clipboard(text: &str) -> std::io::Result<()> {
    run_clipboard_command("clip.exe", &[], text)
}

#[cfg(target_os = "macos")]
fn copy_text_to_clipboard(text: &str) -> std::io::Result<()> {
    run_clipboard_command("pbcopy", &[], text)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn copy_text_to_clipboard(text: &str) -> std::io::Result<()> {
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    let mut last_error = None;
    for (program, args) in candidates {
        match run_clipboard_command(program, args, text) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no clipboard helper found")
    }))
}

#[cfg(not(any(unix, target_os = "windows")))]
fn copy_text_to_clipboard(_text: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "clipboard copy is unsupported on this platform",
    ))
}

/// Keep the temporary path alive so external viewers can open it after the
/// TUI immediately regains control. tempfile creates files with mode 0600.
fn export_attachment_for_open(file: &OutgoingAttachment) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write as _;
    if file.data.len() > 32 * 1024 * 1024 {
        anyhow::bail!("attachment is larger than the 32 MiB open limit");
    }
    let extension = file.filename.rsplit_once('.').map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty() && extension.len() <= 12
            && extension.bytes().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    let mut file_out = tempfile::Builder::new()
        .prefix("aerc-rs-attachment-")
        .suffix(&format!(".{extension}"))
        .tempfile()?;
    file_out.write_all(&file.data)?;
    file_out.flush()?;
    let (_, path) = file_out.keep()?;
    Ok(path)
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

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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

fn is_reader_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn reader_word_target(
    text: &str,
    line: usize,
    column: usize,
    motion: ReaderWordMotion,
) -> Option<ReaderPosition> {
    let lines = text.lines().collect::<Vec<_>>();
    let current_line = lines.get(line)?;
    let chars = current_line.chars().collect::<Vec<_>>();

    match motion {
        ReaderWordMotion::NextStart => {
            let mut index = column.min(chars.len());
            if index < chars.len() && is_reader_word_char(chars[index]) {
                while index < chars.len() && is_reader_word_char(chars[index]) {
                    index += 1;
                }
            } else {
                index = index.saturating_add(1).min(chars.len());
            }
            while index < chars.len() && !is_reader_word_char(chars[index]) {
                index += 1;
            }
            if index < chars.len() {
                return Some(ReaderPosition { line, column: index });
            }
            for (next_line, text) in lines.iter().enumerate().skip(line + 1) {
                if let Some(column) = text.chars().position(is_reader_word_char) {
                    return Some(ReaderPosition {
                        line: next_line,
                        column,
                    });
                }
            }
        }
        ReaderWordMotion::PreviousStart => {
            if column > 0 && !chars.is_empty() {
                let mut index = column.saturating_sub(1).min(chars.len().saturating_sub(1));
                while index > 0 && !is_reader_word_char(chars[index]) {
                    index -= 1;
                }
                if is_reader_word_char(chars[index]) {
                    while index > 0 && is_reader_word_char(chars[index - 1]) {
                        index -= 1;
                    }
                    return Some(ReaderPosition { line, column: index });
                }
            }
            for previous_line in (0..line).rev() {
                let previous = lines[previous_line].chars().collect::<Vec<_>>();
                if let Some(mut index) = previous.iter().rposition(|ch| is_reader_word_char(*ch)) {
                    while index > 0 && is_reader_word_char(previous[index - 1]) {
                        index -= 1;
                    }
                    return Some(ReaderPosition {
                        line: previous_line,
                        column: index,
                    });
                }
            }
        }
    }
    None
}

fn delimiter_pair(delimiter: char) -> Option<(char, char)> {
    match delimiter {
        '"' => Some(('"', '"')),
        '\'' => Some(('\'', '\'')),
        '`' => Some(('`', '`')),
        '(' | ')' => Some(('(', ')')),
        '[' | ']' => Some(('[', ']')),
        '{' | '}' => Some(('{', '}')),
        '<' | '>' => Some(('<', '>')),
        _ => None,
    }
}

fn inner_text_object_range(text: &str, column: usize, delimiter: char) -> Option<(usize, usize)> {
    let (open, close) = delimiter_pair(delimiter)?;
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return None;
    }
    let cursor = column.min(chars.len().saturating_sub(1));

    let (opening, closing) = if open == close {
        let positions = chars
            .iter()
            .enumerate()
            .filter_map(|(index, ch)| (*ch == open).then_some(index))
            .collect::<Vec<_>>();
        positions
            .chunks_exact(2)
            .filter_map(|pair| {
                let start = pair[0];
                let end = pair[1];
                (start <= cursor && cursor <= end).then_some((start, end))
            })
            .min_by_key(|(start, end)| end.saturating_sub(*start))?
    } else {
        let mut stack = Vec::new();
        let mut enclosing = Vec::new();
        for (index, ch) in chars.iter().copied().enumerate() {
            if ch == open {
                stack.push(index);
            } else if ch == close
                && let Some(start) = stack.pop()
                && start <= cursor
                && cursor <= index
            {
                enclosing.push((start, index));
            }
        }
        enclosing
            .into_iter()
            .min_by_key(|(start, end)| end.saturating_sub(*start))?
    };

    if closing <= opening.saturating_add(1) {
        return None;
    }
    Some((opening + 1, closing - 1))
}

fn reader_selection_text(text: &str, selection: ReaderSelection) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() || selection.start.line >= lines.len() {
        return String::new();
    }
    let end_line = selection.end.line.min(lines.len().saturating_sub(1));
    let mut out = String::new();
    for line_index in selection.start.line..=end_line {
        if line_index > selection.start.line {
            out.push('\n');
        }
        let chars = lines[line_index].chars().collect::<Vec<_>>();
        if chars.is_empty() {
            continue;
        }
        let start = if line_index == selection.start.line {
            selection.start.column.min(chars.len().saturating_sub(1))
        } else {
            0
        };
        let end = if line_index == end_line {
            selection.end.column.min(chars.len().saturating_sub(1))
        } else {
            chars.len().saturating_sub(1)
        };
        if start <= end {
            out.extend(chars[start..=end].iter().copied());
        }
    }
    out
}

fn url_at_cursor(line: &str, column: usize) -> Option<String> {
    let chars = line.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return None;
    }
    let cursor = column.min(chars.len().saturating_sub(1));
    let mut start = cursor;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = cursor.saturating_add(1);
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    let token = chars[start..end].iter().collect::<String>();
    let token = token.trim_matches(|ch: char| {
        matches!(ch, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '\'' | '"' | ',' | ';')
    });
    let token = token.trim_end_matches(|ch: char| matches!(ch, '.' | ':' | '!' | '?'));
    (token.starts_with("https://") || token.starts_with("http://")).then(|| token.to_owned())
}

fn reply_date_label(message: &MessageSummary) -> String {
    if message.timestamp <= 0 {
        return "the previous message".to_owned();
    }
    DateTime::<Utc>::from_timestamp(message.timestamp, 0)
        .map(|utc| utc.with_timezone(&Local))
        .map_or_else(
            || "the previous message".to_owned(),
            |local| local.format("%a %b %-d, %Y at %-I:%M %p %Z").to_string(),
        )
}

fn reply_attribution(message: &MessageSummary) -> String {
    let author = if message.from_name.trim().is_empty() {
        message.from_address.trim()
    } else {
        message.from_name.trim()
    };
    format!("On {}, {} wrote:", reply_date_label(message), author)
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
        ComposeField, ComposeMode, Composer, ReaderPosition, ReaderSelection, ReaderWordMotion,
        bounded_index, inner_text_object_range, message_view_offset, offset_index,
        reader_image_target_size, reader_selection_text, reader_word_target, reply_attribution,
        url_at_cursor,
    };
    use crate::model::{MessageFlags, MessageId, MessageSummary};
    use ratatui::layout::Size;

    #[test]
    fn list_navigation_is_bounded() {
        assert_eq!(bounded_index(0, 5, -1), 0);
        assert_eq!(bounded_index(3, 5, 10), 4);
        assert_eq!(bounded_index(0, 0, 1), 0);
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
    fn reader_inner_text_objects_support_quotes_and_brackets() {
        assert_eq!(inner_text_object_range(r#"say "hello" now"#, 6, '"'), Some((5, 9)));
        assert_eq!(inner_text_object_range("call(foo[bar])", 9, ']'), Some((9, 11)));
        assert_eq!(inner_text_object_range("call(foo[bar])", 9, ')'), Some((5, 12)));
        assert_eq!(inner_text_object_range("'{value}'", 4, '\''), Some((1, 7)));
    }

    #[test]
    fn reader_selection_yanks_exact_character_range() {
        let selection = ReaderSelection {
            start: ReaderPosition { line: 0, column: 2 },
            end: ReaderPosition { line: 1, column: 2 },
        };
        assert_eq!(reader_selection_text("abcdef\nghijkl", selection), "cdef\nghi");
    }

    #[test]
    fn reader_linewise_selection_yanks_complete_lines() {
        let selection = ReaderSelection {
            start: ReaderPosition { line: 0, column: 0 },
            end: ReaderPosition {
                line: 1,
                column: usize::MAX,
            },
        };
        assert_eq!(
            reader_selection_text("abcdef\nghijkl\nmnop", selection),
            "abcdef\nghijkl"
        );
        assert_eq!(selection.columns_for_line(0, 6), Some((0, 5)));
        assert_eq!(selection.columns_for_line(1, 6), Some((0, 5)));
    }

    #[test]
    fn reader_word_motions_cross_lines() {
        let text = "one two\n\nthree";
        assert_eq!(
            reader_word_target(text, 0, 0, ReaderWordMotion::NextStart),
            Some(ReaderPosition { line: 0, column: 4 })
        );
        assert_eq!(
            reader_word_target(text, 0, 4, ReaderWordMotion::NextStart),
            Some(ReaderPosition { line: 2, column: 0 })
        );
        assert_eq!(
            reader_word_target(text, 2, 2, ReaderWordMotion::PreviousStart),
            Some(ReaderPosition { line: 2, column: 0 })
        );
    }

    #[test]
    fn reader_images_treat_html_dimensions_as_css_pixels_for_hidpi_assets() {
        // A typical 2x email logo should use its full backing bitmap instead of
        // being rendered at half-size just because HTML declares the CSS size.
        assert_eq!(
            reader_image_target_size(148, 48, Some(74), Some(24), 8, 16, 120, 40),
            Size::new(19, 3)
        );

        // A genuine 1x asset is still capped at native size and never upscaled.
        assert_eq!(
            reader_image_target_size(74, 24, Some(300), Some(100), 8, 16, 120, 40),
            Size::new(10, 2)
        );
    }

    #[test]
    fn reader_images_use_full_reader_bounds_instead_of_legacy_caps() {
        assert_eq!(
            reader_image_target_size(1200, 600, None, None, 8, 16, 120, 40),
            Size::new(120, 38)
        );
        assert_eq!(
            reader_image_target_size(2400, 1600, None, None, 8, 16, 90, 30),
            Size::new(90, 30)
        );
    }

    #[test]
    fn reader_url_lookup_uses_the_token_under_the_cursor() {
        let line = "first https://example.com/a then https://example.net/b.";
        assert_eq!(
            url_at_cursor(line, 9).as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(
            url_at_cursor(line, 40).as_deref(),
            Some("https://example.net/b")
        );
        assert_eq!(url_at_cursor(line, 0), None);
    }

    #[test]
    fn quoted_reply_attribution_matches_aerc_shape() {
        let message = MessageSummary {
            id: MessageId("reply".into()),
            message_id: Some("message@example.com".into()),
            in_reply_to: None,
            references: Vec::new(),
            from_name: "Takashi Iwai".into(),
            from_address: "tiwai@suse.de".into(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Re: test".into(),
            timestamp: 1_792_662_534,
            date_label: String::new(),
            size: 0,
            flags: MessageFlags::empty(),
            has_attachment_hint: false,
        };
        let attribution = reply_attribution(&message);
        assert!(attribution.starts_with("On "));
        assert!(attribution.contains(", Takashi Iwai wrote:"));
        assert!(!attribution.contains("previous message"));
    }

    #[test]
    fn forwarded_message_contains_gmail_attribution_and_original_recipient() {
        let raw = b"From: Yuzu Keycaps <contact@example.org>\r\nTo: Person <person@example.net>\r\nCc: Friend <friend@example.net>\r\nDate: Fri, 23 Oct 2026 14:02:00 -0700\r\nSubject: Ends Saturday\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nA great deal";
        let message = crate::mail::parse_summary(
            MessageId("forward".into()), raw, raw.len() as u64, MessageFlags::empty(),
        ).unwrap();
        let composer = Composer::forward(&message, raw.to_vec()).unwrap();
        assert!(composer.body.contains("---------- Forwarded message ---------"));
        assert!(composer.body.contains("To: Person <person@example.net>"));
        assert!(composer.body.contains("Cc: Friend <friend@example.net>"));
        assert!(composer.body.contains("A great deal"));
        assert!(!composer.body.contains("Bcc:"));
    }

    #[test]
    fn account_navigation_wraps() {
        assert_eq!(offset_index(0, 3, -1), 2);
        assert_eq!(offset_index(2, 3, 1), 0);
    }
}

#[cfg(test)]
mod inbox_poll_tests {
    use super::inbox_new_mail_count;

    #[test]
    fn only_a_completed_poll_against_a_known_inbox_triggers_alerts() {
        assert_eq!(inbox_new_mail_count(true, true, 10, 12), 2);
        assert_eq!(inbox_new_mail_count(true, true, 10, 9), 0);
        assert_eq!(inbox_new_mail_count(true, false, 0, 12), 0);
        assert_eq!(inbox_new_mail_count(false, true, 10, 12), 0);
    }
}
