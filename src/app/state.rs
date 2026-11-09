use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Size;
use ratatui::widgets::BorderType;
use ratatui_image::{Resize, picker::Picker, protocol::Protocol};

use crate::app::{
    ATTACHMENT_HELP_ENTRIES, CONTACT_HELP_ENTRIES, Command, Focus, HELP_ENTRIES, command_for_key,
    ranked_help_matches,
};
use crate::backend::MailboxSyncHint;
use crate::cache::{self, CachedMailbox};
use crate::config::{Config, UiBorderStyle, VisualSelectionStyle};
use crate::contacts::{
    ContactBook, ContactEdit, ContactsModal, recipient_query, recipients, replace_recipient,
    replace_recipients,
};
use crate::mail::{parse_body, render_html_for_terminal};
use crate::mail_image::{MailImageEvent, MailImageLoader};
use crate::model::{
    Capabilities, HtmlLine, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId,
    MessageSummary, OutgoingAttachment, OutgoingMessage,
};
use crate::outgoing::{
    FORWARD_SEPARATOR, ReplyAudience, forward_attribution, forward_html, forward_subject,
    inline_forward_parts, reply_recipients, reply_subject,
};
use crate::setup::{SetupWizard, WizardAction};
use crate::theme::{Theme, load_themes, theme_index};
use crate::worker::{ActionKind, RuntimeEvent, WorkerEvent, WorkerEventKind, WorkerHandle};

// Large feature handlers live alongside the state model. Keeping the types
// here preserves the public API and serialized draft representation.
mod attachment_actions;
mod compose_actions;
mod composer;
mod contacts;
mod file_picker;
mod mail_actions;
mod reader_actions;
mod search_help_actions;
mod worker_events;

const SMOOTH_SCROLL_ROWS: isize = 22;
const SMOOTH_MAILBOX_ROWS: isize = SMOOTH_SCROLL_ROWS;
const MAX_COUNT_PREFIX: usize = 99_999;
const MESSAGE_SCROLL_OFF: usize = 4;
const READER_SCROLL_OFF: usize = 3;
const PAGE_PREFETCH_MIN: usize = 48;
// Ordinary navigation needs a small working set. Ctrl-j/k temporarily expands
// the window around its destination instead of eagerly fetching 42 bodies for
// every selected row. PEEK never changes the read state.
const BODY_PREFETCH_FORWARD: usize = 12;
const BODY_PREFETCH_BACKWARD: usize = 6;
const BODY_PREFETCH_FAST_FORWARD: usize = 22;
const BODY_PREFETCH_FAST_BACKWARD: usize = 8;
const PREFETCH_WINDOW_REARM_ROWS: usize = 12;
const MAX_CACHED_PREVIEW_BODIES: usize = 80;
const MAX_CACHED_PREVIEW_BYTES: usize = 48 * 1024 * 1024;
// Avoid silently downloading giant attachments simply because the cursor is
// near a message. The user can still fetch any large message explicitly.
const MAX_PREFETCH_MESSAGE_BYTES: u64 = 1024 * 1024;
const MAX_PREFETCH_WINDOW_RAW_BYTES: u64 = 8 * 1024 * 1024;

fn preview_body_bytes(body: &MessageBody) -> usize {
    let text = [
        body.plain_text.as_ref(),
        body.plain_fallback.as_ref(),
        body.html_source.as_ref(),
        body.html_text.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(String::len)
    .sum::<usize>();
    let rich = body.html_lines.as_ref().map_or(0, |lines| {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| {
                span.text.len()
                    + span.style.link.as_ref().map_or(0, String::len)
                    + span.style.image.as_ref().map_or(0, String::len)
            })
            .sum::<usize>()
    });
    text.saturating_add(rich)
}

fn preview_neighbor_positions(
    position: usize,
    count: usize,
    direction: isize,
    fast: bool,
) -> Vec<usize> {
    if position >= count {
        return Vec::new();
    }
    let (forward, backward) = if fast {
        (BODY_PREFETCH_FAST_FORWARD, BODY_PREFETCH_FAST_BACKWARD)
    } else {
        (BODY_PREFETCH_FORWARD, BODY_PREFETCH_BACKWARD)
    };
    let (ahead, behind) = if direction < 0 {
        (backward, forward)
    } else {
        (forward, backward)
    };
    let after = (1..=ahead)
        .filter_map(|distance| position.checked_add(distance))
        .filter(|index| *index < count);
    let before = (1..=behind).filter_map(|distance| position.checked_sub(distance));
    if direction < 0 {
        before.chain(after).collect()
    } else {
        after.chain(before).collect()
    }
}

/// Put imminent rows first, but also schedule the destination early enough
/// that a long neoscroll has a chance to arrive on a warm body. Never preload
/// unrelated rows between a far-away destination and the current viewport.
fn preview_travel_positions(position: usize, destination: usize, count: usize) -> Vec<usize> {
    if position >= count || destination >= count {
        return Vec::new();
    }
    let direction = if destination < position { -1 } else { 1 };
    let mut positions = vec![position];
    for distance in 1..=3 {
        let next = if direction < 0 {
            position.checked_sub(distance)
        } else {
            position.checked_add(distance).filter(|p| *p < count)
        };
        if let Some(next) = next {
            positions.push(next);
        }
    }
    positions.push(destination);
    let distance = position.abs_diff(destination);
    if distance > 4 {
        // Also warm the row just before arrival, and the next row if the
        // animation continues after another Ctrl-j/k press.
        let before = if direction < 0 {
            destination.checked_add(1)
        } else {
            destination.checked_sub(1)
        };
        let after = if direction < 0 {
            destination.checked_sub(1)
        } else {
            destination.checked_add(1)
        };
        positions.extend(before.into_iter().chain(after).filter(|p| *p < count));
    }
    positions.extend(preview_neighbor_positions(
        destination,
        count,
        direction,
        true,
    ));
    let mut seen = HashSet::new();
    positions.into_iter().filter(|p| seen.insert(*p)).collect()
}

/// Only a completed background poll against a known Inbox baseline can
/// trigger a new-message alert. STATUS counts are not message identities;
/// an equal count with concurrent arrival/deletion needs future UID tracking.
fn inbox_new_mail_count(from_poll: bool, known: bool, previous: usize, now: usize) -> usize {
    if from_poll && known {
        now.saturating_sub(previous)
    } else {
        0
    }
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

    pub(crate) fn error(text: impl Into<String>) -> Self {
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
        self.expires_at
            .is_some_and(|deadline| Instant::now() >= deadline)
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
            Self::Subject | Self::Attachments | Self::Headers => Self::Body,
            Self::Body => Self::To,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::To => Self::Body,
            Self::Cc => Self::To,
            Self::Bcc => Self::Cc,
            Self::Subject => Self::Bcc,
            Self::Attachments | Self::Headers | Self::Body => Self::Subject,
        }
    }
}

/// A displayed Compose body row. Soft wraps share the same logical line;
/// source offsets always refer to characters in the *unmodified* draft.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ComposeBodyRow {
    pub start: usize,
    pub end: usize,
    pub line_start: usize,
    pub logical_line: usize,
    pub continuation: bool,
}

/// Use the same column width for rendering and Vim motions. The four cells
/// reserved for line numbers are excluded by the caller.
fn compose_body_rows(body: &str, width: usize) -> Vec<ComposeBodyRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    let mut line_start = 0;
    let mut logical_line = 0;
    let mut cells = 0usize;
    let mut continuation = false;
    let mut position = 0;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(body, true) {
        if grapheme.ends_with('\n') {
            let trailing = grapheme.chars().count().saturating_sub(1);
            rows.push(ComposeBodyRow {
                start,
                end: position + trailing,
                line_start,
                logical_line,
                continuation,
            });
            position += grapheme.chars().count();
            start = position;
            line_start = position;
            logical_line += 1;
            cells = 0;
            continuation = false;
            continue;
        }
        let char_width = unicode_width::UnicodeWidthStr::width(grapheme).min(width);
        if cells > 0 && cells + char_width > width {
            rows.push(ComposeBodyRow {
                start,
                end: position,
                line_start,
                logical_line,
                continuation,
            });
            start = position;
            cells = 0;
            continuation = true;
        }
        cells += char_width;
        position += grapheme.chars().count();
    }
    rows.push(ComposeBodyRow {
        start,
        end: position,
        line_start,
        logical_line,
        continuation,
    });
    rows
}

fn compose_body_cursor_row(rows: &[ComposeBodyRow], cursor: usize) -> usize {
    // At a soft-wrap boundary the cursor belongs to the next visual row.
    rows.iter()
        .rposition(|row| row.start <= cursor)
        .unwrap_or(0)
}

fn compose_body_column(body: &str, start: usize, cursor: usize) -> usize {
    let text = body
        .chars()
        .skip(start)
        .take(cursor.saturating_sub(start))
        .collect::<String>();
    unicode_width::UnicodeWidthStr::width(text.as_str())
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

/// A local directory picker. The state is intentionally not stored in drafts;
/// only files the user actually selected are persisted.
#[derive(Debug, Clone)]
pub struct ComposeFileEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_directory: bool,
}

#[derive(Debug, Clone)]
pub struct ComposeFilePicker {
    pub directory: PathBuf,
    pub entries: Vec<ComposeFileEntry>,
    pub selected: usize,
    pub show_hidden: bool,
    /// Save-to-directory mode: never read or decode file contents.
    pub directory_only: bool,
    /// Adjacent panes are cached on movement, never loaded by the renderer.
    pub parent_entries: Vec<ComposeFileEntry>,
    pub parent_selected: usize,
    pub preview: ComposeFilePreview,
    pub pending_count: usize,
    pub pending_g: bool,
    /// One-step-per-frame Ctrl-j/k motion, using the existing animation clock.
    pub smooth_target: Option<usize>,
    /// Inclusive visual selection anchored at the line where v was pressed.
    pub visual_anchor: Option<usize>,
    pub help_visible: bool,
    pub help_query: String,
    pub help_selected: usize,
    pub help_scroll: usize,
    /// Incremental search of the current directory (Enter accepts highlighted row).
    pub search_query: Option<String>,
    /// Pending PDF conversion is debounced so scrolling past PDFs starts no process.
    pdf_preview_due: Option<(PathBuf, Instant)>,
    /// No more than two PDF converters may run simultaneously.
    pdf_preview_inflight: HashSet<PathBuf>,
    /// Worker threads only put completed conversions here; never touch UI state.
    pdf_preview_results: Arc<Mutex<VecDeque<PdfPreviewResult>>>,
    /// Small in-memory cache prevents repeated PDF conversion on cursor reversals.
    pdf_preview_cache: VecDeque<(PathBuf, Result<Vec<u8>, String>)>,
}

const PDF_PREVIEW_DEBOUNCE: Duration = Duration::from_millis(140);
const PDF_PREVIEW_CONCURRENCY: usize = 2;
const PDF_PREVIEW_CACHE_SIZE: usize = 6;

#[derive(Debug, Clone)]
pub enum ComposeFilePreview {
    Empty,
    Directory(Vec<ComposeFileEntry>),
    Loading(&'static str),
    Text { lines: Vec<String>, truncated: bool },
    Patch(crate::patch_preview::PatchPreview),
    Binary(u64),
    Image(Vec<u8>),
    Error(String),
}

fn compose_read_dir(directory: &Path, show_hidden: bool) -> anyhow::Result<Vec<ComposeFileEntry>> {
    let mut entries = Vec::new();
    for item in std::fs::read_dir(directory)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let path = item.path();
        let is_directory = path.is_dir();
        if is_directory || path.is_file() {
            entries.push(ComposeFileEntry {
                path,
                name,
                is_directory,
            });
        }
    }
    entries.sort_by(|a, b| {
        b.is_directory
            .cmp(&a.is_directory)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

/// Render a small first-page JPEG via Poppler in a *background worker*.
/// JPEG thumbnails are much cheaper to encode/decode than full-resolution PNGs.
/// The TUI thread must never call this function (or wait on its child process).
fn compose_pdf_thumbnail(path: &Path) -> anyhow::Result<Vec<u8>> {
    let temp = tempfile::tempdir()?;
    let output = temp.path().join("preview");
    let mut child = ProcessCommand::new("pdftoppm")
        .args([
            "-f",
            "1",
            "-l",
            "1",
            "-singlefile",
            "-scale-to",
            "850",
            "-jpeg",
            "-jpegopt",
            "quality=70",
        ])
        .arg(path)
        .arg(&output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| anyhow::anyhow!("PDF preview requires Poppler's pdftoppm: {error}"))?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                anyhow::bail!("PDF preview could not render page one");
            }
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("PDF preview timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let rendered = output.with_extension("jpg");
    let metadata = std::fs::metadata(&rendered)?;
    if metadata.len() > 16 * 1024 * 1024 {
        anyhow::bail!("PDF thumbnail too large");
    }
    Ok(std::fs::read(rendered)?)
}

fn compose_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["png", "jpg", "jpeg", "gif", "webp"]
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
}

fn validate_picker_attachment(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
        anyhow::bail!(
            "attachment must be a file of at most 16 MiB: {}",
            path.display()
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct RemovedAttachment {
    index: usize,
    path: PathBuf,
    legacy: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Composer {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    /// Legacy semicolon-separated paths, kept for compatibility with old drafts.
    #[serde(default)]
    pub attachments: String,
    /// File-picker selections (supports spaces and semicolons in filenames).
    #[serde(default)]
    pub attachment_paths: Vec<PathBuf>,
    pub headers: String,
    #[serde(skip)]
    pub file_picker: Option<ComposeFilePicker>,
    #[serde(skip)]
    pub attachment_manager_open: bool,
    #[serde(skip)]
    pub attachment_manager_selected: usize,
    #[serde(skip)]
    pub attachment_manager_visual_anchor: Option<usize>,
    #[serde(skip)]
    pub attachment_manager_marked: HashSet<usize>,
    /// Undo stack for detach operations in Ctrl+D; deliberately independent of text undo.
    #[serde(skip)]
    removed_attachments: Vec<RemovedAttachment>,
    #[serde(skip)]
    pub advanced_headers_open: bool,
    #[serde(skip)]
    advanced_previous: Option<(ComposeField, ComposeMode, usize)>,
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
    /// Prompt before discarding a nonempty unsent message. Not stored in drafts.
    #[serde(skip)]
    pub exit_prompt: bool,
    /// Rich vs plain rendering in pre-send review; ephemeral, not draft content.
    #[serde(skip)]
    pub preview_html: bool,
    #[serde(skip)]
    pub preview_scroll: usize,
    #[serde(skip)]
    pub preview_rich_lines: Option<Vec<HtmlLine>>,
    #[serde(skip)]
    pub preview_forwarded_attachments: Vec<String>,
    #[serde(skip)]
    pub help_visible: bool,
    #[serde(skip)]
    pub contact_suggestion: usize,
    #[serde(skip)]
    pub dismissed_contact_query: Option<String>,
    #[serde(skip)]
    pub help_query: String,
    #[serde(skip)]
    pub help_selected: usize,
    #[serde(skip)]
    pub help_scroll: usize,
    /// Ephemeral viewport state shared by Compose body rendering and motions.
    #[serde(skip)]
    pub body_view_width: usize,
    #[serde(skip)]
    pub body_scroll: usize,
    #[serde(skip)]
    pub body_smooth_target: Option<usize>,
    #[serde(skip)]
    body_preferred_column: Option<usize>,
    #[serde(skip)]
    pub visual_linewise: bool,
    #[serde(skip)]
    pending_g: bool,
    #[serde(skip)]
    pending_delete: bool,
    #[serde(skip)]
    pending_inner: bool,
    #[serde(skip)]
    pending_find: Option<bool>,
    #[serde(skip)]
    count: Option<usize>,
    #[serde(skip)]
    undo_history: Vec<(ComposeField, String, usize)>,
    #[serde(skip)]
    redo_history: Vec<(ComposeField, String, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeIntent {
    Reply {
        quote: bool,
        audience: ReplyAudience,
    },
    Forward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCompose {
    account: usize,
    message_id: MessageId,
    intent: ComposeIntent,
}

/// Match a query as an ordered, case-insensitive subsequence. Consecutive
/// characters, word boundaries and exact substrings rank ahead of scattered
/// matches; no external search dependency or indexing is required for a page.
fn mail_fuzzy_score(field: &str, query_lower: &str) -> Option<i64> {
    let lower = field.to_lowercase();
    let hay = lower.chars().collect::<Vec<_>>();
    let needle = query_lower.chars().collect::<Vec<_>>();
    if needle.is_empty() {
        return Some(0);
    }
    let mut at = 0usize;
    let mut previous = None;
    let mut score = 0i64;
    for wanted in needle {
        let position = (at..hay.len()).find(|&index| hay[index] == wanted)?;
        score += 10;
        if position == 0 || !hay[position - 1].is_alphanumeric() {
            score += 9;
        }
        if let Some(prev) = previous {
            if position == prev + 1 {
                score += 12;
            } else {
                score -= i64::try_from(position - prev - 1).unwrap_or(100).min(12);
            }
        }
        previous = Some(position);
        at = position + 1;
    }
    if lower.contains(query_lower) {
        score += 80;
    }
    score -= i64::try_from(hay.len() / 20).unwrap_or(10).min(10);
    Some(score)
}

pub struct AccountState {
    pub name: String,
    pub default_mailbox: String,
    pub exclude_mailboxes: Vec<String>,
    pub from: String,
    pub outgoing_configured: bool,
    pub copy_to: Option<String>,
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
    page_request_id: Option<u64>,
    body_request_id: Option<u64>,
    last_prefetch_window: Option<(String, Vec<MessageId>)>,
    last_prefetch_anchor: Option<(String, usize, isize)>,
    mailbox_cache: HashMap<String, CachedMailbox>,
    cache_writer: Option<cache::Writer>,
    uid_validities: HashMap<String, u32>,
    uid_nexts: HashMap<String, u32>,
    unverified_mailboxes: HashSet<String>,
    pub search_query: String,
    pub search_editing: bool,
    pub search_mode: crate::config::MailSearchMode,
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
        let query = self.search_query.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.messages.len()).collect();
        }
        let mut matches = self
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                let fields = [
                    message.subject.as_str(),
                    message.from_name.as_str(),
                    message.from_address.as_str(),
                    message.date_label.as_str(),
                ];
                match self.search_mode {
                    crate::config::MailSearchMode::Strict => fields
                        .iter()
                        .any(|field| field.to_lowercase().contains(&query))
                        .then_some((index, 0i64)),
                    crate::config::MailSearchMode::Fuzzy => fields
                        .iter()
                        .filter_map(|field| mail_fuzzy_score(field, &query))
                        .max()
                        .map(|score| (index, score)),
                }
            })
            .collect::<Vec<_>>();
        if self.search_mode == crate::config::MailSearchMode::Fuzzy {
            // Stable for ties; keep server date order whenever scores agree.
            matches.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        }
        matches.into_iter().map(|(index, _)| index).collect()
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
        let Some(position) = visible
            .iter()
            .position(|index| *index == self.message_index)
        else {
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
        let uid_validity = self.uid_validities.get(&name).copied();
        let uid_next = self.uid_nexts.get(&name).copied();
        self.mailbox_cache.insert(
            name,
            CachedMailbox {
                messages: self.messages.clone(),
                bodies: self.bodies.clone(),
                message_index: self.message_index,
                message_view_offset: self.message_view_offset,
                next_page_offset: self.next_page_offset,
                has_more: self.has_more,
                uid_validity,
                uid_next,
            },
        );
    }

    fn persist_mailboxes(&self) {
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::Mailboxes(self.mailboxes.clone()));
        }
    }

    fn persist_current_cache(&mut self) {
        let Some(name) = self.current_mailbox.as_ref() else {
            return;
        };
        let Some(writer) = &self.cache_writer else {
            return;
        };
        // Don't clone 80 warm preview bodies into the in-session cache and
        // then clone them *again* for a disk writer that can only keep 12.
        // In-session snapshots are captured separately when changing folders.
        let Some(uid_validity) = self.uid_validities.get(name).copied() else {
            return;
        };
        if self.unverified_mailboxes.contains(name) {
            return;
        }
        let truncated = self.messages.len() > cache::MAX_MESSAGES_PER_FOLDER;
        writer.send(cache::Change::Folder(
            name.clone(),
            CachedMailbox {
                messages: self
                    .messages
                    .iter()
                    .take(cache::MAX_MESSAGES_PER_FOLDER)
                    .cloned()
                    .collect(),
                bodies: cache::persistable_bodies(&self.messages, &self.bodies),
                message_index: self.message_index,
                message_view_offset: self.message_view_offset,
                next_page_offset: if truncated {
                    cache::MAX_MESSAGES_PER_FOLDER
                } else {
                    self.next_page_offset
                },
                has_more: truncated || self.has_more,
                uid_validity: Some(uid_validity),
                uid_next: self.uid_nexts.get(name).copied(),
            },
        ));
    }

    fn remove_persistent_folder(&self, name: &str) {
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::RemoveFolder(name.to_owned()));
        }
    }

    fn restore_cached_mailbox(&mut self, name: &str) -> bool {
        // Once selected, the live vectors own the folder. Moving them avoids
        // duplicating potentially megabytes of HTML previews on every switch.
        let Some(cached) = self.mailbox_cache.remove(name) else {
            return false;
        };
        self.current_mailbox = Some(name.to_owned());
        self.messages = cached.messages;
        self.bodies = cached.bodies;
        self.message_index = cached
            .message_index
            .min(self.messages.len().saturating_sub(1));
        self.message_view_offset = cached.message_view_offset;
        self.next_page_offset = cached.next_page_offset;
        self.has_more = cached.has_more;
        self.page_inflight = None;
        self.page_request_id = None;
        self.body_request_id = None;
        self.last_prefetch_window = None;
        self.messages_loading = false;
        self.preview_scroll = 0;
        self.preview_max_scroll = 0;
        self.pending_bodies.clear();
        self.pending_seen.clear();
        self.search_query.clear();
        self.search_editing = false;
        self.clamp_selection();
        // This precedes the network refresh. It identifies the moment cached
        // rows become available to the UI, not the terminal's paint latency.
        crate::perf::record("ui_cache_restore", None, Duration::ZERO, true);
        true
    }

    fn invalidate_current_cache(&mut self) {
        if let Some(name) = self.current_mailbox.as_ref() {
            self.mailbox_cache.remove(name);
            self.remove_persistent_folder(name);
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
            if !self
                .mailbox_status_queue
                .iter()
                .any(|queued| queued == &name)
            {
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
        // Give the worker only cached headers (never MIME bodies). It must
        // validate UIDVALIDITY with the server before reusing even one row.
        let hint = if offset != 0 {
            None
        } else if self.current_mailbox.as_deref() == Some(name.as_str()) {
            // The active folder is no longer duplicated in mailbox_cache.
            // Use its most recent live headers for IMAP incremental sync.
            self.uid_validities
                .get(&name)
                .copied()
                .map(|uid_validity| MailboxSyncHint {
                    uid_validity,
                    uid_next: self.uid_nexts.get(&name).copied(),
                    messages: self.messages.clone(),
                })
        } else {
            self.mailbox_cache.get(&name).and_then(|folder| {
                folder.uid_validity.map(|uid_validity| MailboxSyncHint {
                    uid_validity,
                    uid_next: folder.uid_next,
                    messages: folder.messages.clone(),
                })
            })
        };
        if let Ok(request_id) = self.worker.post(ActionKind::OpenMailbox {
            name,
            offset,
            limit: self.page_size,
            hint,
        }) {
            self.page_inflight = Some((pending_name, offset));
            self.page_request_id = Some(request_id);
            self.messages_loading = offset == 0;
            crate::perf::record_request(
                "ui_mailbox_request",
                request_id,
                None,
                Duration::ZERO,
                true,
            );
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
        if self.bodies.contains_key(&id) {
            crate::perf::record("ui_preview_cache_hit", None, Duration::ZERO, true);
            return;
        }
        if self.pending_bodies.contains(&id) || self.unverified_mailboxes.contains(&mailbox) {
            return;
        }
        if let Ok(request_id) = self.worker.post(ActionKind::FetchBody {
            mailbox,
            id: id.clone(),
        }) {
            crate::perf::record("ui_preview_cache_miss", None, Duration::ZERO, true);
            self.body_request_id = Some(request_id);
            // Only the newest requested reader body matters. The worker coalesces
            // queued body fetches to the newest opened selection; mirror that here so
            // discarded requests cannot leave a permanent "loading" marker.
            self.pending_bodies.clear();
            self.pending_bodies.insert(id);
        }
    }

    /// Queue only nearby, missing preview bodies. A superseding window is
    /// coalesced by the background worker; the foreground always wins.
    fn prefetch_near_cursor(&mut self, direction: isize) {
        let visible = self.visible_message_indices();
        let Some(position) = visible
            .iter()
            .position(|index| *index == self.message_index)
        else {
            return;
        };
        self.prefetch_around_position(position, direction);
    }

    fn prefetch_around_position(&mut self, position: usize, direction: isize) {
        self.prefetch_window(position, direction, None);
    }

    fn prefetch_for_scroll(&mut self, position: usize, destination: usize) {
        if position == destination {
            return;
        }
        self.prefetch_window(
            position,
            if destination < position { -1 } else { 1 },
            Some(destination),
        );
    }

    fn prefetch_window(&mut self, position: usize, direction: isize, destination: Option<usize>) {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return;
        };
        if self.unverified_mailboxes.contains(&mailbox) {
            return;
        }
        let visible = self.visible_message_indices();
        if position >= visible.len() {
            return;
        }
        let direction = if direction < 0 { -1 } else { 1 };
        let destination = destination.filter(|target| *target < visible.len());
        let anchor = destination.unwrap_or(position);
        // Keep the existing window while it covers the moving cursor; a new
        // window every animation frame would cancel almost every useful fetch.
        if self.last_prefetch_window.is_some()
            && self
                .last_prefetch_anchor
                .as_ref()
                .is_some_and(|(name, last, dir)| {
                    name == &mailbox
                        && *dir == direction
                        && last.abs_diff(anchor) < PREFETCH_WINDOW_REARM_ROWS
                })
        {
            return;
        }
        let positions = if let Some(target) = destination {
            preview_travel_positions(position, target, visible.len())
        } else {
            std::iter::once(position)
                .chain(preview_neighbor_positions(
                    position,
                    visible.len(),
                    direction,
                    false,
                ))
                .collect::<Vec<_>>()
        };
        let mut raw_bytes = 0u64;
        let ids = positions
            .into_iter()
            .filter_map(|position| self.messages.get(visible[position]))
            .filter(|message| message.size > 0 && message.size <= MAX_PREFETCH_MESSAGE_BYTES)
            .filter(|message| {
                !self.bodies.contains_key(&message.id) && !self.pending_bodies.contains(&message.id)
            })
            .filter(|message| {
                if raw_bytes.saturating_add(message.size) > MAX_PREFETCH_WINDOW_RAW_BYTES {
                    false
                } else {
                    raw_bytes += message.size;
                    true
                }
            })
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            self.last_prefetch_window = None;
            self.last_prefetch_anchor = None;
            return;
        }
        let window = (mailbox.clone(), ids.clone());
        if self.last_prefetch_window.as_ref() == Some(&window) {
            return;
        }
        if self
            .worker
            .post(ActionKind::PrefetchBodies {
                mailbox: mailbox.clone(),
                ids,
            })
            .is_ok()
        {
            crate::perf::record("ui_prefetch_window_submitted", None, Duration::ZERO, true);
            self.last_prefetch_window = Some(window);
            self.last_prefetch_anchor = Some((mailbox, anchor, direction));
        }
    }

    /// Bound preloading by both message count and actual decoded-text size.
    /// A large HTML newsletter may have much more resident data than its raw
    /// RFC822 size suggests. Keep the current message even if it exceeds budget.
    fn trim_preview_bodies(&mut self) {
        let bytes = self.bodies.values().map(preview_body_bytes).sum::<usize>();
        if self.bodies.len() <= MAX_CACHED_PREVIEW_BODIES && bytes <= MAX_CACHED_PREVIEW_BYTES {
            return;
        }
        let mut ranked = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| self.bodies.contains_key(&message.id))
            .map(|(position, message)| (position.abs_diff(self.message_index), message.id.clone()))
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(distance, _)| *distance);
        let mut used = 0usize;
        let mut keep = HashSet::new();
        for (_, id) in ranked {
            let Some(body) = self.bodies.get(&id) else {
                continue;
            };
            let weight = preview_body_bytes(body);
            if keep.len() >= MAX_CACHED_PREVIEW_BODIES
                || (!keep.is_empty() && used.saturating_add(weight) > MAX_CACHED_PREVIEW_BYTES)
            {
                continue;
            }
            used = used.saturating_add(weight);
            keep.insert(id);
        }
        self.bodies.retain(|id, _| keep.contains(id));
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

/// One-row-per-frame, 22-row target scrolling shared by delete previews.
#[derive(Debug, Clone, Default)]
pub struct ReviewScroll {
    pub position: usize,
    target: Option<usize>,
}

impl ReviewScroll {
    pub fn queue(&mut self, count: usize, delta: isize) {
        let origin = self.target.unwrap_or(self.position);
        let next = origin
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
        self.target = (next != self.position).then_some(next);
    }

    pub fn active(&self) -> bool {
        self.target.is_some()
    }

    pub fn advance(&mut self) {
        if let Some(target) = self.target {
            self.position = self
                .position
                .saturating_add_signed(if target > self.position { 1 } else { -1 });
            if self.position == target {
                self.target = None;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct MutationConfirmation {
    pub prompt: String,
    pub items: Vec<String>,
    pub review: ReviewScroll,
    source: String,
    ids: Vec<MessageId>,
    kind: MutationKind,
}

/// An attachment is only detached after the user accepts the review.
#[derive(Debug, Clone)]
pub struct DetachConfirmation {
    pub paths: Vec<PathBuf>,
    pub review: ReviewScroll,
}

type PdfPreviewResult = (PathBuf, Result<Vec<u8>, String>);
type AttachmentCacheEntry = (
    (usize, String, Option<u32>, MessageId),
    Arc<Vec<OutgoingAttachment>>,
);
type AttachmentPreviewResult = (usize, Result<image::DynamicImage, String>);

struct ReaderImage {
    source: String,
    protocol: Protocol,
}

fn reader_image_target_size(
    native: (u32, u32),
    html: (Option<u32>, Option<u32>),
    font: (u16, u16),
    max_cells: (u16, u16),
) -> Size {
    let (native_width, native_height) = native;
    let (html_width, html_height) = html;
    let (font_width, font_height) = font;
    let (max_width_cells, max_height_cells) = max_cells;
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

#[derive(Debug, Clone)]
pub struct NewMailNotice {
    pub account: String,
    pub count: usize,
    pub sender: String,
    pub subject: String,
    pub excerpt: String,
}

pub struct App {
    pub accounts: Vec<AccountState>,
    pub account_index: usize,
    pub focus: Focus,
    pub status: StatusLine,
    pub startup_warnings: Vec<String>,
    pub show_startup_warnings: bool,
    pub should_quit: bool,
    /// In-app first-run or add-account setup, modal over normal mail UI.
    pub setup: Option<SetupWizard>,
    pub remove_account_confirmation: Option<String>,
    pub setup_config_path: PathBuf,
    pub setup_reload_requested: bool,
    setup_sender: Option<SyncSender<RuntimeEvent>>,
    inbox_poll_interval: Duration,
    new_mail_bell_enabled: bool,
    new_mail_bell_pending: bool,
    new_mail_notifications_enabled: bool,
    notification_icon_style: crate::config::NotificationIconStyle,
    new_mail_since_launch: usize,
    new_mail_sound_name: String,
    new_mail_sound_file: Option<String>,
    new_mail_notices: VecDeque<NewMailNotice>,
    pub mailbox_width_percent: u16,
    pub mailbox_resizing: bool,
    pub preview_width_percent: u16,
    pub preview_resizing: bool,
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
    reader_line_count: usize,
    reader_view_height: usize,
    /// Follow the Vim cursor after the renderer measures wrapped terminal rows.
    /// Mouse/independent preview scrolling should not snap back on idle redraws.
    reader_scroll_follow_pending: bool,
    image_picker: Option<Picker>,
    image_loader: Option<MailImageLoader>,
    reader_image: Option<ReaderImage>,
    reader_image_pending: Option<String>,
    reader_image_size_hint: Option<(Option<u32>, Option<u32>)>,
    reader_image_target_width: u16,
    reader_image_target_height: u16,
    pub reader_images_enabled: bool,
    pub show_header_details: bool,
    theme_config_path: Option<PathBuf>,
    pub attachment_picker_visible: bool,
    pub attachment_selection: usize,
    pub attachment_files: Arc<Vec<OutgoingAttachment>>,
    // A small per-session MIME cache makes reopening a received attachment
    // browser instantaneous instead of refetching the entire raw email.
    // Include UIDVALIDITY to avoid reusing a recycled IMAP UID.
    attachment_source: Option<(usize, String, Option<u32>, MessageId)>,
    attachment_cache: VecDeque<AttachmentCacheEntry>,
    pub attachment_marked: HashSet<usize>,
    pub attachment_visual_anchor: Option<usize>,
    pub attachment_save_directory: Option<String>, // legacy path field, no longer used by save UI
    /// Reuse the exact three-column local picker from compose for save destinations.
    pub attachment_destination_picker: Option<ComposeFilePicker>,
    pub attachment_new_folder: Option<String>,
    attachment_pending_count: usize,
    attachment_pending_g: bool,
    attachment_smooth_target: Option<usize>,
    attachment_preview_due: Option<(usize, Instant)>,
    attachment_preview_inflight: HashSet<usize>,
    attachment_preview_results: Arc<Mutex<VecDeque<AttachmentPreviewResult>>>,
    attachment_preview_cache: VecDeque<(usize, Result<image::DynamicImage, String>)>,
    attachment_pending: Option<MessageId>,
    attachment_preview: Option<Protocol>,
    attachment_preview_identity: Option<(usize, u16, u16)>,
    compose_file_preview_image: Option<Protocol>,
    compose_file_preview_identity: Option<(PathBuf, u16, u16)>,
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
    /// Session-only marks for the active account/mailbox. Stable IDs survive
    /// sorting, searching, and pagination; changing folders clears them.
    pub message_marked: HashSet<MessageId>,
    pub visual_selection_style: VisualSelectionStyle,
    pub confirmation: Option<MutationConfirmation>,
    pub detach_confirmation: Option<DetachConfirmation>,
    pub composer: Option<Composer>,
    pub contacts: ContactBook,
    pub contacts_modal: Option<ContactsModal>,
    contacts_path: PathBuf,
    contacts_writable: bool,
    contacts_autocomplete: bool,
    contacts_learn_inbox: bool,
    contacts_learn_sent: bool,
    contacts_dirty: bool,
    contacts_save_due: Option<Instant>,
    pending_compose: Option<PendingCompose>,
    resumed_draft_account: Option<usize>,
}

impl App {
    pub fn new(config: &Config, workers: Vec<WorkerHandle>) -> Self {
        let app_search_mode = config.ui.search_mode;
        let accounts = config
            .accounts
            .iter()
            .zip(workers)
            .map(|(config, worker)| {
                let (snapshot, cache_writer) = cache::open(config);
                let uid_validities = snapshot
                    .folders
                    .iter()
                    .filter_map(|(name, folder)| {
                        folder.uid_validity.map(|validity| (name.clone(), validity))
                    })
                    .collect();
                let uid_nexts = snapshot
                    .folders
                    .iter()
                    .filter_map(|(name, folder)| {
                        folder.uid_next.map(|uid_next| (name.clone(), uid_next))
                    })
                    .collect();
                let unverified_mailboxes = snapshot.folders.keys().cloned().collect();
                let mut account = AccountState {
                    name: config.name.clone(),
                    default_mailbox: config.default_mailbox.clone(),
                    exclude_mailboxes: config.exclude_mailboxes.clone(),
                    from: crate::outgoing::sender_from_account(config),
                    outgoing_configured: config.outgoing.is_some(),
                    copy_to: config.copy_to.clone(),
                    capabilities: None,
                    mailboxes: snapshot
                        .mailboxes
                        .into_iter()
                        .filter(|mailbox| {
                            !crate::config::mailbox_is_excluded(
                                &mailbox.name,
                                &config.exclude_mailboxes,
                            )
                        })
                        .collect(),
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
                    page_request_id: None,
                    body_request_id: None,
                    last_prefetch_window: None,
                    last_prefetch_anchor: None,
                    mailbox_cache: snapshot.folders,
                    cache_writer,
                    uid_validities,
                    uid_nexts,
                    unverified_mailboxes,
                    search_query: String::new(),
                    search_editing: false,
                    search_mode: app_search_mode,
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
                };
                if let Some(index) = account
                    .mailboxes
                    .iter()
                    .position(|m| m.name == account.default_mailbox)
                {
                    account.mailbox_index = index;
                }
                if let Some(name) = account
                    .selected_mailbox()
                    .map(|mailbox| mailbox.name.clone())
                {
                    let _ = account.restore_cached_mailbox(&name);
                }
                account
            })
            .collect();
        let mut themes = load_themes();
        let border_type = match config.ui.border_style {
            UiBorderStyle::Square => BorderType::Plain,
            UiBorderStyle::Rounded => BorderType::Rounded,
        };
        for theme in &mut themes {
            theme.border_type = border_type;
        }
        let theme_index = theme_index(&themes, &config.ui.theme);
        let contacts_path = ContactBook::default_path();
        let loaded_contacts = ContactBook::load(&contacts_path);
        let contacts_writable = loaded_contacts.is_ok();
        let contacts_warning = loaded_contacts.as_ref().err().map(|error| {
            format!("Contacts file could not be loaded; preserving it unchanged: {error:#}")
        });
        let contacts = loaded_contacts.unwrap_or_default();
        Self {
            accounts,
            account_index: 0,
            focus: Focus::Messages,
            status: StatusLine::info("starting workers…"),
            startup_warnings: contacts_warning.into_iter().collect(),
            show_startup_warnings: false,
            should_quit: false,
            setup: config.accounts.is_empty().then(|| SetupWizard::new(true)),
            remove_account_confirmation: None,
            setup_config_path: config
                .config_path
                .clone()
                .unwrap_or_else(crate::config::default_config_path),
            setup_reload_requested: false,
            setup_sender: None,
            inbox_poll_interval: Duration::from_secs(config.ui.inbox_poll_seconds.clamp(10, 3600)),
            new_mail_bell_enabled: config.ui.new_mail_bell,
            new_mail_bell_pending: false,
            new_mail_notifications_enabled: config.ui.new_mail_notifications,
            notification_icon_style: config.ui.notification_icon_style,
            new_mail_since_launch: 0,
            new_mail_sound_name: config.ui.new_mail_sound_name.clone(),
            new_mail_sound_file: config.ui.new_mail_sound_file.clone(),
            new_mail_notices: VecDeque::new(),
            mailbox_width_percent: stored_mailbox_width()
                .unwrap_or(config.ui.mailbox_width_percent)
                .clamp(12, 50),
            mailbox_resizing: false,
            preview_width_percent: stored_preview_width()
                .unwrap_or(config.ui.preview_width_percent)
                .clamp(25, 75),
            preview_resizing: false,
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
            reader_line_count: 0,
            reader_view_height: 1,
            reader_scroll_follow_pending: false,
            image_picker: None,
            image_loader: None,
            reader_image: None,
            reader_image_pending: None,
            reader_image_size_hint: None,
            reader_image_target_width: 48,
            reader_image_target_height: 18,
            reader_images_enabled: config.ui.remote_images,
            show_header_details: false,
            theme_config_path: config.config_path.clone(),
            attachment_picker_visible: false,
            attachment_selection: 0,
            attachment_files: Arc::new(Vec::new()),
            attachment_source: None,
            attachment_cache: VecDeque::new(),
            attachment_marked: HashSet::new(),
            attachment_visual_anchor: None,
            attachment_save_directory: None,
            attachment_destination_picker: None,
            attachment_new_folder: None,
            attachment_pending_count: 0,
            attachment_pending_g: false,
            attachment_smooth_target: None,
            attachment_preview_due: None,
            attachment_preview_inflight: HashSet::new(),
            attachment_preview_results: Arc::new(Mutex::new(VecDeque::new())),
            attachment_preview_cache: VecDeque::new(),
            attachment_pending: None,
            attachment_preview: None,
            attachment_preview_identity: None,
            compose_file_preview_image: None,
            compose_file_preview_identity: None,
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
            message_marked: HashSet::new(),
            visual_selection_style: config.ui.visual_selection_style,
            confirmation: None,
            detach_confirmation: None,
            composer: None,
            contacts,
            contacts_modal: None,
            contacts_path,
            contacts_writable,
            contacts_autocomplete: config.contacts.autocomplete,
            contacts_learn_inbox: config.contacts.learn_from_inbox,
            contacts_learn_sent: config.contacts.learn_from_sent,
            contacts_dirty: false,
            contacts_save_due: None,
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
    pub fn inbox_poll_interval(&self) -> Duration {
        self.inbox_poll_interval
    }

    /// Poll only Inbox counts, not every message body or folder. IMAP servers
    /// without IDLE can still report new mail with one inexpensive STATUS.
    pub fn poll_inboxes(&mut self) {
        for account in &mut self.accounts {
            if account.capabilities.is_none()
                || account.mailboxes_loading
                || account.inbox_poll_inflight.is_some()
                || account.mailbox_status_inflight.is_some()
                || account.pending_mutations > 0
            {
                continue;
            }
            let Some(inbox) = account
                .mailboxes
                .iter()
                .find(|mailbox| mailbox.role == MailboxRole::Inbox)
                .map(|mailbox| mailbox.name.clone())
            else {
                continue;
            };
            if let Ok(request) = account.worker.post(ActionKind::MailboxStatus(inbox)) {
                account.inbox_poll_inflight = Some(request);
            }
        }
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
        for account in &mut self.accounts {
            if account.worker.post(ActionKind::Connect).is_ok() {
                account.connecting = true;
            }
        }
    }

    pub fn attach_setup_sender(&mut self, sender: SyncSender<RuntimeEvent>) {
        self.setup_sender = Some(sender);
    }

    /// Keep the already-negotiated terminal graphics protocol across an
    /// in-place account reload. Re-querying after the input thread starts
    /// would consume terminal escape sequences as keypresses.
    pub fn inherit_image_runtime(&mut self, previous: &mut Self) {
        self.image_picker = previous.image_picker.take();
        self.image_loader = previous.image_loader.take();
    }

    pub fn setup_discovered(
        &mut self,
        email: String,
        result: Result<crate::discovery::MailConfiguration, String>,
    ) {
        let Some(wizard) = self.setup.as_mut() else {
            return;
        };
        if !wizard.discovering
            || wizard.authenticating
            || wizard.saved
            || !wizard.fields[crate::setup::EMAIL]
                .trim()
                .eq_ignore_ascii_case(&email)
        {
            return;
        }
        match result {
            Ok(config) => wizard.apply_discovery(config),
            Err(error) => wizard.discovery_failed(&error),
        }
        // An alias doesn't establish which Exchange mailbox identity to use
        // for SMTP. Authenticate first, then offer the login UPN for review.
        if self
            .setup
            .as_ref()
            .is_some_and(|wizard| wizard.step == crate::setup::SetupStep::Authenticating)
        {
            self.start_setup_oauth();
        }
    }

    pub fn setup_login_finished(&mut self, result: Result<Option<String>, String>) {
        if let Some(wizard) = self.setup.as_mut() {
            match result {
                Ok(login) => wizard.suggest_login(login),
                Err(error) => {
                    wizard.authenticating = false;
                    wizard.step = crate::setup::SetupStep::Settings;
                    wizard.selected = crate::setup::PROVIDER;
                    wizard.login_failed = true;
                    wizard.error = true;
                    wizard.message =
                        format!("OAuth failed: {error}. Ctrl-S/F6 retry; F7 saves offline.");
                }
            }
        }
    }

    /// Start a browser login after validating generated server settings.
    /// Used both for a Microsoft alias (before settings review) and for the
    /// explicit Continue action in the settings step.
    fn start_setup_oauth(&mut self) {
        let Some(wizard) = self.setup.as_mut() else {
            return;
        };
        if wizard.authenticating || wizard.authenticated {
            return;
        }
        match wizard.preview_account() {
            Ok(account) => {
                // OAuth tokens are keyed by account name, so never let wizard
                // setup overwrite a configured account's credentials.
                let duplicate = self
                    .accounts
                    .iter()
                    .any(|existing| existing.name == account.name)
                    || crate::config::Config::load_or_empty(Some(&self.setup_config_path))
                        .is_ok_and(|config| {
                            config
                                .accounts
                                .iter()
                                .any(|existing| existing.name == account.name)
                        });
                if duplicate {
                    wizard.step = crate::setup::SetupStep::Settings;
                    wizard.error = true;
                    wizard.message =
                        "This account name is already in use. Choose another before signing in."
                            .into();
                    return;
                }
                let Some(sender) = self.setup_sender.clone() else {
                    wizard.step = crate::setup::SetupStep::Settings;
                    wizard.error = true;
                    wizard.message = "OAuth runtime unavailable".into();
                    return;
                };
                wizard.begin_authentication();
                std::thread::spawn(move || {
                    let name = account.name.clone();
                    let result =
                        crate::oauth::login_account_quiet(std::slice::from_ref(&account), &name)
                            .and_then(|()| {
                                let settings = crate::oauth::OAuthSettings::for_account(&account)?
                                    .ok_or_else(|| anyhow::anyhow!("OAuth is not configured"))?;
                                if settings.provider == "microsoft" {
                                    settings.suggested_login_username()
                                } else {
                                    Ok(None)
                                }
                            })
                            .map_err(|error| format!("{}", error.root_cause()));
                    let _ = sender.send(RuntimeEvent::SetupLoginFinished(result));
                });
            }
            Err(error) => {
                wizard.step = crate::setup::SetupStep::Settings;
                wizard.selected = crate::setup::PROVIDER;
                wizard.error = true;
                wizard.message =
                    format!("Check settings: {error:#}. F3 opens advanced OAuth settings.");
            }
        }
    }

    fn handle_setup_key(&mut self, key: KeyEvent) {
        let action = if let Some(wizard) = self.setup.as_mut() {
            if wizard.saved && !wizard.authenticating && key.code == KeyCode::Enter {
                self.setup_reload_requested = true;
                return;
            }
            wizard.key(key)
        } else {
            return;
        };
        match action {
            WizardAction::Nothing => {}
            WizardAction::Close => {
                if self.setup.as_ref().is_some_and(|wizard| !wizard.first_run) {
                    self.setup = None;
                } else {
                    self.should_quit = true;
                }
            }
            WizardAction::Discover => {
                let Some(sender) = self.setup_sender.clone() else {
                    return;
                };
                let Some(wizard) = self.setup.as_mut() else {
                    return;
                };
                if wizard.discovering {
                    return;
                }
                if let Err(error) = wizard.discovery_ready() {
                    wizard.error = true;
                    wizard.message = format!("Cannot discover settings: {error}");
                    return;
                }
                let email = wizard.fields[crate::setup::EMAIL].trim().to_owned();
                let provider = wizard.provider_hint().to_owned();
                wizard.discovering = true;
                wizard.error = false;
                wizard.spinner_frame = 0;
                wizard.message = "Finding account settings…".into();
                std::thread::spawn(move || {
                    let result = crate::discovery::discover_with_provider(&email, &provider)
                        .map_err(|error| error.to_string());
                    let _ = sender.send(RuntimeEvent::SetupDiscovered { email, result });
                });
            }
            WizardAction::SaveOffline => {
                let Some(wizard) = self.setup.as_mut() else {
                    return;
                };
                match wizard.save(&self.setup_config_path) {
                    Ok(_) => {
                        wizard.saved = true;
                        wizard.error = false;
                        // Confirmation already explains the offline account state.
                        wizard.message.clear();
                    }
                    Err(error) => {
                        wizard.error = true;
                        wizard.message = format!("Cannot save: {error:#}");
                    }
                }
            }
            WizardAction::Save => {
                if self.setup.as_ref().is_some_and(|wizard| wizard.saved) {
                    return;
                }
                if self
                    .setup
                    .as_ref()
                    .is_some_and(|wizard| !wizard.authenticated)
                {
                    self.start_setup_oauth();
                    return;
                }
                let Some(wizard) = self.setup.as_mut() else {
                    return;
                };
                match wizard.save(&self.setup_config_path) {
                    Ok(_) => {
                        wizard.saved = true;
                        wizard.error = false;
                        // The confirmation page and its centered action provide
                        // the success feedback without a duplicate status line.
                        wizard.message.clear();
                    }
                    Err(error) => {
                        wizard.error = true;
                        wizard.message = format!("Cannot save: {error:#}");
                    }
                }
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.setup.is_some() {
            self.handle_setup_key(key);
            return;
        }
        if key.modifiers.contains(KeyModifiers::ALT)
            && key.code == KeyCode::Char('c')
            && self.contacts_modal.is_none()
            && self.confirmation.is_none()
            && self.setup.is_none()
            && !self.attachment_picker_visible
            && !self.help_visible
            && !self.theme_picker_visible
            && !self.show_startup_warnings
            && self.remove_account_confirmation.is_none()
            && self.composer.as_ref().is_none_or(|c| {
                !c.sending
                    && !c.confirm_send
                    && !c.exit_prompt
                    && !c.help_visible
                    && !c.attachment_manager_open
                    && c.file_picker.is_none()
                    && !c.advanced_headers_open
            })
        {
            self.open_contacts();
            return;
        }
        if self.contacts_modal.is_some() {
            self.handle_contacts_key(key);
            return;
        }
        // Account actions share Alt shortcuts. Ctrl-a remains available for
        // the compose attachment picker, not the account setup wizard.
        if key.modifiers == KeyModifiers::ALT
            && key.code == KeyCode::Char('a')
            && self.composer.is_none()
            && self.confirmation.is_none()
            && !self.help_visible
            && !self.theme_picker_visible
            && !self.attachment_picker_visible
        {
            self.setup = Some(SetupWizard::new(false));
            return;
        }
        if self.show_startup_warnings {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('!')
            ) {
                self.show_startup_warnings = false;
            }
            return;
        }
        if !self.startup_warnings.is_empty()
            && key.code == KeyCode::Char('!')
            && self.composer.is_none()
            && !self.attachment_picker_visible
            && !self.help_visible
            && !self.theme_picker_visible
            && self.confirmation.is_none()
        {
            self.show_startup_warnings = true;
            return;
        }
        if self.attachment_picker_visible {
            self.handle_attachment_picker_key(key);
            return;
        }
        if self.composer.is_none()
            && !self.message_open
            && self.preview_split
            && key
                .modifiers
                .contains(KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            let adjustment = match key.code {
                KeyCode::Char('h') | KeyCode::Char('H') => Some(3_i32),
                KeyCode::Char('l') | KeyCode::Char('L') => Some(-3_i32),
                _ => None,
            };
            if let Some(adjustment) = adjustment {
                self.preview_width_percent =
                    (i32::from(self.preview_width_percent) + adjustment).clamp(25, 75) as u16;
                let _ = persist_preview_width(self.preview_width_percent);
                return;
            }
        }
        if self.composer.is_none()
            && !self.message_open
            && self.sidebar_visible
            && key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
        {
            let adjustment = match key.code {
                KeyCode::Char('h') => Some(-2_i32),
                KeyCode::Char('l') => Some(2_i32),
                _ => None,
            };
            if let Some(adjustment) = adjustment {
                self.mailbox_width_percent =
                    (i32::from(self.mailbox_width_percent) + adjustment).clamp(12, 50) as u16;
                let _ = persist_mailbox_width(self.mailbox_width_percent);
                return;
            }
        }
        if self.remove_account_confirmation.is_some() {
            self.handle_remove_account_confirmation_key(key);
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
        // Alt-d removes the currently selected *local account*, never a remote
        // IMAP folder. Only available from the mailbox browser, with a prompt.
        if key.modifiers.contains(KeyModifiers::ALT)
            && key.code == KeyCode::Char('d')
            && !self.message_open
            && self.sidebar_visible
        {
            if let Some(account) = self.account() {
                self.remove_account_confirmation = Some(account.name.clone());
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            self.restore_postponed_composer();
            return;
        }
        // Global account shortcuts work in the reader as well as the mailbox
        // list, but must not steal input from compose or modal dialogs.
        if key.modifiers.contains(KeyModifiers::ALT) {
            let command = command_for_key(key);
            if matches!(
                command,
                Command::NextAccount | Command::PreviousAccount | Command::SelectAccount(_)
            ) {
                self.handle_command(command);
                return;
            }
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

    pub fn handle_command(&mut self, command: Command) {
        if self.status.expired() {
            self.status = StatusLine::info("ready");
        }

        match command {
            Command::Quit => self.should_quit = true,
            Command::NextAccount => self.change_account(1),
            Command::PreviousAccount => self.change_account(-1),
            Command::SelectAccount(index) => self.select_account(index),
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
            Command::ToggleMark => {
                self.count_prefix = None;
                if self.message_open {
                    // Preserve the old reader Space navigation behavior.
                    self.move_selection(10);
                } else {
                    self.toggle_message_mark();
                }
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
            Command::ToggleSpam => {
                self.count_prefix = None;
                self.toggle_spam_selected();
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
                let preview_split_enabled = self.preview_split;
                if let Some(account) = self.account_mut() {
                    account.last_prefetch_window = None;
                    if preview_split_enabled {
                        account.request_selected_body();
                        account.prefetch_near_cursor(1);
                        Self::mark_cached_selected_seen(account);
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
            Command::ToggleNotifications => {
                self.count_prefix = None;
                self.new_mail_notifications_enabled = !self.new_mail_notifications_enabled;
                if !self.new_mail_notifications_enabled {
                    // Prevent previously queued messages from appearing after mute.
                    self.new_mail_notices.clear();
                    self.new_mail_bell_pending = false;
                    self.new_mail_since_launch = 0;
                }
                self.status = StatusLine::temporary(
                    if self.new_mail_notifications_enabled {
                        "new-mail notifications enabled"
                    } else {
                        "new-mail notifications muted"
                    },
                    Duration::from_secs(2),
                );
            }
            Command::ToggleBell => {
                self.count_prefix = None;
                self.new_mail_bell_enabled = !self.new_mail_bell_enabled;
                self.status = StatusLine::temporary(
                    if self.new_mail_bell_enabled {
                        "new-mail bell enabled"
                    } else {
                        "new-mail bell muted"
                    },
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
                let had_selection = self.reader_visual_anchor.take().is_some()
                    | self.visual_anchor.take().is_some()
                    | !self.message_marked.is_empty();
                self.message_marked.clear();
                self.reader_visual_linewise = false;
                if had_selection {
                    self.status =
                        StatusLine::temporary("selection cleared", Duration::from_secs(1));
                } else if self.message_open {
                    self.close_message();
                } else {
                    if let Some(account) = self.account_mut()
                        && !account.search_query.is_empty()
                    {
                        account.search_query.clear();
                        account.search_editing = false;
                        account.normalize_search_selection();
                    }
                    self.focus = Focus::Messages;
                }
            }
            Command::Digit(digit) => self.push_count_digit(digit),
            Command::None => {}
        }
    }

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
            || self.reader_smooth_target.is_some()
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
            if let Ok(request_id) = account.worker.post(ActionKind::MailboxStatus(name.clone())) {
                account.mailbox_status_inflight = Some(request_id);
            }
            break;
        }
    }

    fn change_account(&mut self, delta: isize) {
        if self.accounts.is_empty() {
            return;
        }
        let target = offset_index(self.account_index, self.accounts.len(), delta);
        self.select_account(target);
    }

    fn select_account(&mut self, index: usize) {
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
        if delta > 0 && remaining <= prefetch_distance {
            let _ = account.request_next_page();
        }
    }

    fn move_mailbox_list(&mut self, delta: isize) {
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

    fn activate_selected_mailbox(account: &mut AccountState, preview_split: bool) {
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

    fn export_selected_raw(&mut self) {
        let path = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("jerc")
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
        match account
            .worker
            .post(ActionKind::ExportRaw { mailbox, id, path })
        {
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
            .join("jerc")
            .join("browser");
        if let Err(error) = std::fs::create_dir_all(&directory) {
            self.status = StatusLine::error(format!(
                "could not create browser preview directory: {error}"
            ));
            return;
        }
        let path = directory.join("message.html");
        if let Err(error) = std::fs::write(&path, html.as_bytes()) {
            self.status = StatusLine::error(format!("could not write browser preview: {error}"));
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
                self.status =
                    StatusLine::error(format!("could not open original HTML email: {error}"));
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
                .is_some_and(|body| body.has_html_alternative() && body.has_plain_view())
            {
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

    fn toggle_header_details(&mut self) {
        self.show_header_details = !self.show_header_details;
        self.status = StatusLine::temporary(
            if self.show_header_details {
                "email addresses and Message-ID shown"
            } else {
                "email addresses and Message-ID hidden"
            },
            Duration::from_secs(2),
        );
    }

    /// Drag the mailbox divider. Mouse moves over the attachment list select
    /// the hovered file, and footer clicks activate Open/Save/Mark actions.
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
                    let _ = persist_preview_width(self.preview_width_percent);
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
                let _ = persist_mailbox_width(self.mailbox_width_percent);
            }
            _ => {}
        }
    }

    /// Terminal graphics for the compose picker. Cached by path and cell size;
    /// selection changes never decode images on each render frame.
    pub fn ensure_compose_file_preview(&mut self, width: u16, height: u16) {
        if self
            .composer
            .as_ref()
            .and_then(|composer| composer.file_picker.as_ref())
            .is_some_and(|picker| picker.smooth_target.is_some())
        {
            return;
        }
        let candidate = self
            .composer
            .as_ref()
            .and_then(|composer| composer.file_picker.as_ref())
            .and_then(|picker| {
                picker
                    .entries
                    .get(picker.selected)
                    .map(|entry| (entry.path.clone(), &picker.preview))
            });
        let Some((path, ComposeFilePreview::Image(bytes))) = candidate else {
            self.compose_file_preview_image = None;
            self.compose_file_preview_identity = None;
            return;
        };
        let identity = (path, width, height);
        if self.compose_file_preview_identity.as_ref() == Some(&identity) {
            return;
        }
        self.compose_file_preview_identity = Some(identity);
        self.compose_file_preview_image = None;
        let Some(picker) = &self.image_picker else {
            return;
        };
        if let Ok(image) = image::load_from_memory(bytes) {
            let target = reader_image_target_size(
                (image.width(), image.height()),
                (None, None),
                (picker.font_size().width, picker.font_size().height),
                (width.max(1), height.max(1)),
            );
            self.compose_file_preview_image = picker
                .new_protocol(
                    image,
                    target,
                    Resize::Fit(Some(image::imageops::FilterType::Lanczos3)),
                )
                .ok();
        }
    }

    #[must_use]
    pub fn compose_file_preview_image(&self) -> Option<&Protocol> {
        self.compose_file_preview_image.as_ref()
    }

    #[must_use]
    pub fn attachment_preview(&self) -> Option<&Protocol> {
        self.attachment_preview.as_ref()
    }

    fn open_attachment(&mut self) {
        let Some(file) = self.attachment_files.get(self.attachment_selection) else {
            return;
        };
        match export_attachment_for_open(file) {
            Ok(path) => match open_url(&path.to_string_lossy()) {
                Ok(()) => {
                    self.status = StatusLine::temporary(
                        format!("opened {} with system viewer", file.filename),
                        Duration::from_secs(3),
                    )
                }
                Err(error) => {
                    self.status = StatusLine::error(format!("could not open attachment: {error}"))
                }
            },
            Err(error) => {
                self.status = StatusLine::error(format!("could not export attachment: {error:#}"))
            }
        }
    }

    pub fn configure_mail_images(&mut self, picker: Picker, runtime_tx: SyncSender<RuntimeEvent>) {
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
                    (image.width(), image.height()),
                    (size_hint.0, size_hint.1),
                    (font_size.width, font_size.height),
                    (
                        self.reader_image_target_width,
                        self.reader_image_target_height,
                    ),
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
            account.page_request_id = None;
            account.restore_message_index = Some(account.message_index);
            let _ = account.request_mailbox_page(name, 0);
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

/// Shared MIME-input assembly for SMTP and IMAP Drafts. The draft builder
/// allows unfinished addresses; the Send path validates them separately.
fn outgoing_from_composer(composer: &Composer, from: &str) -> anyhow::Result<OutgoingMessage> {
    let mut attachments =
        load_compose_attachments(&composer.attachments, &composer.attachment_paths)?;
    let custom_headers = parse_compose_headers(&composer.headers)?;
    let mut html_body = None;
    if let Some(raw) = composer.forwarded_raw.as_deref() {
        let parts = inline_forward_parts(raw)?;
        attachments.extend(parts.attachments);
        if let Some(html) = parts.html {
            let personal_note = composer.body.split(FORWARD_SEPARATOR).next().unwrap_or("");
            let original_headers = composer
                .body
                .split(FORWARD_SEPARATOR)
                .nth(1)
                .and_then(|tail| tail.split("\n\n").next())
                .unwrap_or("");
            html_body = Some(forward_html(
                &html,
                &format!("{FORWARD_SEPARATOR}{original_headers}"),
                personal_note,
            ));
        }
    }
    Ok(OutgoingMessage {
        from: from.to_owned(),
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
        forwarded_raw: None,
    })
}

fn parse_compose_headers(raw: &str) -> anyhow::Result<Vec<(String, String)>> {
    raw.split(';')
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let (name, value) = part.split_once(':').ok_or_else(|| {
                anyhow::anyhow!("use Header-Name: value; separated by semicolons")
            })?;
            Ok((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

fn load_compose_attachments(
    legacy: &str,
    selected: &[PathBuf],
) -> anyhow::Result<Vec<OutgoingAttachment>> {
    let mut output = Vec::new();
    // Older postponed drafts used a semicolon-delimited string. New selections
    // are distinct paths, so filenames containing semicolons work correctly.
    for part in legacy
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        output.push(load_compose_file(Path::new(part))?);
    }
    for path in selected {
        output.push(load_compose_file(path)?);
    }
    Ok(output)
}

fn load_compose_file(path: &Path) -> anyhow::Result<OutgoingAttachment> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
        anyhow::bail!(
            "attachment must be a file of at most 16 MiB: {}",
            path.display()
        );
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("attachment has no valid UTF-8 filename"))?
        .to_owned();
    let content_type = if ["LICENSE", "COPYING", "Makefile", "Dockerfile", ".gitignore"]
        .iter()
        .any(|name| filename.eq_ignore_ascii_case(name))
    {
        "text/plain"
    } else {
        match path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "txt" | "patch" | "diff" | "log" | "md" | "rst" | "lock" | "rs" | "c" | "h" | "cpp"
            | "hpp" | "py" | "sh" | "bash" | "zsh" | "fish" | "ini" | "conf" | "cfg"
            | "gitignore" => "text/plain",
            "toml" => "application/toml",
            "json" => "application/json",
            "yaml" | "yml" => "application/yaml",
            "csv" => "text/csv",
            "xml" => "application/xml",
            "css" => "text/css",
            "js" | "mjs" => "text/javascript",
            "html" | "htm" => "text/html",
            "pdf" => "application/pdf",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "eml" => "message/rfc822",
            "zip" => "application/zip",
            _ => "application/octet-stream",
        }
    };
    Ok(OutgoingAttachment {
        filename,
        content_type: content_type.to_owned(),
        data: std::fs::read(path)?,
    })
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

/// Only the final component of an attachment filename may be written to disk.
/// A malicious MIME filename must never escape the chosen destination folder.
fn received_attachment_is_pdf(file: &OutgoingAttachment) -> bool {
    file.content_type.eq_ignore_ascii_case("application/pdf")
        || file
            .filename
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

fn received_attachment_is_image(file: &OutgoingAttachment) -> bool {
    file.content_type.starts_with("image/")
}

fn decode_received_attachment_preview(
    file: &OutgoingAttachment,
) -> Result<image::DynamicImage, String> {
    // No unbounded PDF subprocess inputs or huge speculative image decodes.
    if file.data.len() > 24 * 1024 * 1024 {
        return Err("preview over 24 MiB; press o to open the original".into());
    }
    let image_bytes: std::borrow::Cow<'_, [u8]> = if received_attachment_is_pdf(file) {
        let temp = tempfile::tempdir().map_err(|err| err.to_string())?;
        let input = temp.path().join("received.pdf");
        std::fs::write(&input, &file.data).map_err(|err| err.to_string())?;
        std::borrow::Cow::Owned(compose_pdf_thumbnail(&input).map_err(|err| err.to_string())?)
    } else {
        std::borrow::Cow::Borrowed(&file.data)
    };
    // Check declared dimensions before allocating decoded pixels. Huge
    // compressed images must not expand into gigabytes of cache data.
    let dimensions = image::ImageReader::new(std::io::Cursor::new(image_bytes.as_ref()))
        .with_guessed_format()
        .map_err(|err| err.to_string())?
        .into_dimensions()
        .map_err(|err| err.to_string())?;
    if u64::from(dimensions.0) * u64::from(dimensions.1) > 24_000_000 {
        return Err("image has too many pixels for automatic preview; press o to open".into());
    }
    let decoded = image::load_from_memory(image_bytes.as_ref())
        .map_err(|err| format!("attachment image decode: {err}"))?;
    // Cache a terminal-sized image, not an enormous decoded camera original.
    Ok(if decoded.width() > 1_200 || decoded.height() > 1_200 {
        decoded.thumbnail(1_200, 1_200)
    } else {
        decoded
    })
}

fn attachment_safe_name(name: &str, index: usize) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or("");
    let safe = basename
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '/' && *ch != '\\')
        .collect::<String>();
    let safe = safe.trim().trim_matches('.');
    if safe.is_empty() {
        format!("attachment-{}.bin", index + 1)
    } else {
        safe.to_owned()
    }
}

/// Create-only save with collision suffixes; existing files and symlinks are
/// never truncated. Duplicate MIME filenames are common in message threads.
fn save_attachment_without_overwrite(
    directory: &Path,
    file: &OutgoingAttachment,
    index: usize,
) -> anyhow::Result<PathBuf> {
    use std::fs::OpenOptions;
    let name = attachment_safe_name(&file.filename, index);
    let (stem, suffix) = name
        .rsplit_once('.')
        .filter(|(stem, suffix)| !stem.is_empty() && !suffix.is_empty())
        .map_or((name.as_str(), ""), |(stem, suffix)| (stem, suffix));
    for attempt in 0..10_000 {
        let candidate = if attempt == 0 {
            name.clone()
        } else if suffix.is_empty() {
            format!("{stem} ({attempt})")
        } else {
            format!("{stem} ({attempt}).{suffix}")
        };
        let path = directory.join(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut handle) => {
                use std::io::Write as _;
                if let Err(error) = handle.write_all(&file.data) {
                    drop(handle);
                    let _ = std::fs::remove_file(&path);
                    return Err(error.into());
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("too many files named {name:?} in {}", directory.display())
}

fn attachment_list_start(selected: usize, count: usize, visible: usize) -> usize {
    selected
        .saturating_sub(visible / 2)
        .min(count.saturating_sub(visible))
}

fn mouse_width_percent(column: u16, total: u16) -> u16 {
    if total == 0 {
        return 22;
    }
    (((u32::from(column) + 1) * 100 / u32::from(total)) as u16).clamp(12, 50)
}

fn preview_percent_at(column: u16, sidebar_width: u16, remaining: u16) -> u16 {
    if remaining == 0 {
        return 57;
    }
    let inside = column.saturating_sub(sidebar_width).min(remaining);
    (100 - (u32::from(inside) * 100 / u32::from(remaining)) as u16).clamp(25, 75)
}

fn preview_width_state_file() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join("jerc").join("preview-width"))
}

fn stored_preview_width() -> Option<u16> {
    std::fs::read_to_string(preview_width_state_file()?)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|value| (25..=75).contains(value))
}

fn persist_preview_width(width: u16) -> std::io::Result<()> {
    let Some(path) = preview_width_state_file() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", width.clamp(25, 75)))
}

fn mailbox_width_state_file() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join("jerc").join("mailbox-width"))
}

fn stored_mailbox_width() -> Option<u16> {
    std::fs::read_to_string(mailbox_width_state_file()?)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|value| (12..=50).contains(value))
}

fn persist_mailbox_width(width: u16) -> std::io::Result<()> {
    let Some(path) = mailbox_width_state_file() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", width.clamp(12, 50)))
}

/// Keep the temporary path alive so external viewers can open it after the
/// TUI immediately regains control. tempfile creates files with mode 0600.
fn export_attachment_for_open(file: &OutgoingAttachment) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write as _;
    if file.data.len() > 32 * 1024 * 1024 {
        anyhow::bail!("attachment is larger than the 32 MiB open limit");
    }
    let extension = file
        .filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 12
                && extension.bytes().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or("bin");
    let mut file_out = tempfile::Builder::new()
        .prefix("jerc-attachment-")
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
                return Some(ReaderPosition {
                    line,
                    column: index,
                });
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
                    return Some(ReaderPosition {
                        line,
                        column: index,
                    });
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
            .as_chunks::<2>()
            .0
            .iter()
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
    for (line_index, line) in lines
        .iter()
        .enumerate()
        .take(end_line + 1)
        .skip(selection.start.line)
    {
        if line_index > selection.start.line {
            out.push('\n');
        }
        let chars = line.chars().collect::<Vec<_>>();
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
        matches!(
            ch,
            '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '\'' | '"' | ',' | ';'
        )
    });
    let token = token.trim_end_matches(['.', ':', '!', '?']);
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

/// A range is marked as a group: if fully marked, Space unmarks all of it.
fn toggle_message_marks(marked: &mut HashSet<MessageId>, ids: &[MessageId]) {
    let all_marked = ids.iter().all(|id| marked.contains(id));
    for id in ids {
        if all_marked {
            marked.remove(id);
        } else {
            marked.insert(id.clone());
        }
    }
}

fn bounded_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    current.saturating_add_signed(delta).min(len - 1)
}

/// Scrolloff in *rendered terminal rows* (not logical message lines). A long
/// wrapped line may occupy many rows, which must count toward the viewport.
fn reader_scroll_for_visual_cursor(
    current: usize,
    cursor_row: usize,
    view_height: usize,
    max_scroll: usize,
) -> usize {
    let view_height = view_height.max(1);
    let current = current.min(max_scroll);
    let scrolloff = READER_SCROLL_OFF.min(view_height.saturating_sub(1) / 2);
    let upper_guard = current.saturating_add(scrolloff);
    let lower_guard = current.saturating_add(view_height.saturating_sub(scrolloff + 1));
    if cursor_row < upper_guard {
        cursor_row.saturating_sub(scrolloff).min(max_scroll)
    } else if cursor_row > lower_guard {
        cursor_row
            .saturating_add(scrolloff + 1)
            .saturating_sub(view_height)
            .min(max_scroll)
    } else {
        current
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
    use super::{
        ComposeField, ComposeMode, Composer, ReaderPosition, ReaderSelection, ReaderWordMotion,
        SMOOTH_SCROLL_ROWS, bounded_index, inner_text_object_range, message_view_offset,
        offset_index, preview_neighbor_positions, preview_travel_positions,
        reader_image_target_size, reader_scroll_for_visual_cursor, reader_selection_text,
        reader_word_target, reply_attribution, url_at_cursor,
    };
    use crate::model::{MessageFlags, MessageId, MessageSummary};
    use ratatui::layout::Size;
    use ratatui::widgets::BorderType;
    use std::path::PathBuf;

    #[test]
    fn rounded_borders_apply_to_every_theme() {
        let config: crate::config::Config =
            toml::from_str("[ui]\nborder_style = \"rounded\"").unwrap();
        let app = super::App::new(&config, vec![]);
        assert!(
            app.themes
                .iter()
                .all(|theme| theme.border_type == BorderType::Rounded)
        );
    }

    #[test]
    fn space_marking_a_mail_range_is_idempotent_and_toggleable() {
        let ids = [
            MessageId("1".into()),
            MessageId("2".into()),
            MessageId("3".into()),
        ];
        let mut marked = std::collections::HashSet::new();
        super::toggle_message_marks(&mut marked, &ids[0..1]);
        assert!(marked.contains(&ids[0]));
        super::toggle_message_marks(&mut marked, &ids[0..2]);
        assert_eq!(marked.len(), 2);
        super::toggle_message_marks(&mut marked, &ids[0..2]);
        assert!(marked.is_empty());
        super::toggle_message_marks(&mut marked, &ids);
        assert_eq!(marked.len(), 3);
    }

    #[test]
    fn escape_clears_mail_marks_and_visual_selection_before_closing_reader() {
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = super::App::new(&config, vec![]);
        app.message_open = true;
        app.message_marked.insert(MessageId("chosen".into()));
        app.visual_anchor = Some(0);
        app.reader_visual_anchor = Some(ReaderPosition { line: 0, column: 0 });
        app.handle_command(crate::app::Command::Escape);
        assert!(app.message_marked.is_empty());
        assert!(app.visual_anchor.is_none());
        assert!(app.reader_visual_anchor.is_none());
        assert!(app.message_open, "first Escape must keep the reader open");
    }

    #[test]
    fn compose_wrapped_lines_use_visual_jk_and_preserve_draft() {
        use crossterm::event::KeyCode;
        let mut composer = Composer::new();
        composer.switch_field(ComposeField::Body);
        composer.body = "abcdefghijklmnop\nshort\nabcdefghijklmnop".into();
        composer.body_view_width = 5;
        composer.cursor = 2;
        let original = composer.body.clone();
        assert_eq!(composer.body_rows().len(), 9);
        assert!(composer.apply_motion(KeyCode::Char('j'), 1));
        assert_eq!(composer.cursor, 7, "j moves one wrapped terminal row");
        assert!(composer.apply_motion(KeyCode::Char('j'), 1));
        assert_eq!(composer.cursor, 12);
        assert!(composer.apply_motion(KeyCode::Char('k'), 1));
        assert_eq!(composer.cursor, 7);
        assert_eq!(composer.body, original);
    }

    #[test]
    fn compose_neoscroll_advances_one_display_row_per_tick() {
        let mut composer = Composer::new();
        composer.switch_field(ComposeField::Body);
        composer.body = "x\n".repeat(70);
        composer.body_view_width = 20;
        composer.cursor = 0;
        composer.queue_body_smooth_scroll(SMOOTH_SCROLL_ROWS);
        assert_eq!(composer.body_smooth_target, Some(22));
        composer.advance_body_smooth_scroll();
        assert_eq!(composer.cursor, 2);
        assert!(composer.body_smooth_target.is_some());
        // Repeated Ctrl-j extends the destination from the existing target.
        composer.queue_body_smooth_scroll(SMOOTH_SCROLL_ROWS);
        assert_eq!(composer.body_smooth_target, Some(44));
        for _ in 0..43 {
            composer.advance_body_smooth_scroll();
        }
        assert!(composer.body_smooth_target.is_none());
        assert_eq!(composer.cursor, 88);
        composer.queue_body_smooth_scroll(-SMOOTH_SCROLL_ROWS);
        for _ in 0..22 {
            composer.advance_body_smooth_scroll();
        }
        assert_eq!(composer.cursor, 44);
    }

    #[test]
    fn compose_soft_wrap_neoscroll_does_not_skip_boundaries() {
        let mut composer = Composer::new();
        composer.switch_field(ComposeField::Body);
        composer.body = "abcdefghijklmnopqrstuvwxyz".into();
        composer.body_view_width = 5;
        composer.cursor = 0;
        composer.queue_body_smooth_scroll(SMOOTH_SCROLL_ROWS);
        for _ in 0..10 {
            composer.advance_body_smooth_scroll();
        }
        assert!(composer.body_smooth_target.is_none());
        assert_eq!(
            super::compose_body_cursor_row(&composer.body_rows(), composer.cursor),
            5
        );
    }

    #[test]
    fn compose_contact_suggestions_accept_control_jk() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = super::App::new(&config, vec![]);
        app.contacts_autocomplete = true;
        app.contacts
            .upsert("Alex One", "alex1@example.org", true)
            .unwrap();
        app.contacts
            .upsert("Alex Two", "alex2@example.org", true)
            .unwrap();
        let mut composer = Composer::new();
        composer.to = "Alex".into();
        composer.cursor = 4;
        app.composer = Some(composer);
        assert!(app.contact_suggestions().len() >= 2);
        let control = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        app.handle_composer_key(control('j'));
        assert_eq!(app.composer.as_ref().unwrap().contact_suggestion, 1);
        app.handle_composer_key(control('k'));
        assert_eq!(app.composer.as_ref().unwrap().contact_suggestion, 0);
    }

    #[test]
    fn escape_clears_contacts_marks_without_closing_manager() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = super::App::new(&config, vec![]);
        let mut modal = crate::contacts::ContactsModal {
            visual_anchor: Some(0),
            ..Default::default()
        };
        modal.marked.insert("person@example.com".into());
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.handle_contacts_modal_key(&mut modal, escape));
        assert!(modal.marked.is_empty());
        assert!(modal.visual_anchor.is_none());
    }

    #[test]
    fn contacts_help_is_searchable_and_navigable_without_closing_contacts() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = super::App::new(&config, vec![]);
        let mut modal = crate::contacts::ContactsModal::default();
        let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        app.handle_contacts_modal_key(&mut modal, key('?'));
        assert!(modal.help);
        for ch in "favorite".chars() {
            app.handle_contacts_modal_key(&mut modal, key(ch));
        }
        assert_eq!(modal.help_query, "favorite");
        let matches =
            crate::app::ranked_help_matches(crate::app::CONTACT_HELP_ENTRIES, &modal.help_query);
        assert!(!matches.is_empty());
        assert!(
            crate::app::CONTACT_HELP_ENTRIES[matches[0]]
                .description
                .contains("favorite")
        );
        app.handle_contacts_modal_key(&mut modal, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!modal.help);
        assert!(app.contacts_modal.is_none());
    }

    #[test]
    fn contacts_visual_and_space_marks_support_bulk_actions() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = super::App::new(&config, vec![]);
        app.contacts_writable = true;
        app.contacts.entries.clear();
        for index in 0..30 {
            app.contacts
                .upsert(
                    &format!("Name{index:02}"),
                    &format!("name{index:02}@example.org"),
                    true,
                )
                .unwrap();
        }
        let mut modal = crate::contacts::ContactsModal {
            matches: app.contacts.filtered(""),
            view_height: 10,
            ..Default::default()
        };
        let key = |ch, modifiers| KeyEvent::new(KeyCode::Char(ch), modifiers);
        app.handle_contacts_modal_key(&mut modal, key('V', KeyModifiers::SHIFT));
        app.handle_contacts_modal_key(&mut modal, key('3', KeyModifiers::NONE));
        app.handle_contacts_modal_key(&mut modal, key('j', KeyModifiers::NONE));
        assert_eq!(modal.visual_range(), Some((0, 3)));
        app.handle_contacts_modal_key(&mut modal, key(' ', KeyModifiers::NONE));
        assert!(modal.visual_range().is_none());
        assert_eq!(modal.marked.len(), 4);
        app.handle_contacts_modal_key(&mut modal, key('p', KeyModifiers::NONE));
        assert_eq!(
            app.contacts.entries.iter().filter(|c| c.favorite).count(),
            4
        );
        assert!(modal.marked.is_empty());
        app.handle_contacts_modal_key(&mut modal, key('v', KeyModifiers::NONE));
        app.handle_contacts_modal_key(&mut modal, key('2', KeyModifiers::NONE));
        app.handle_contacts_modal_key(&mut modal, key('j', KeyModifiers::NONE));
        assert_eq!(modal.action_ids(&app.contacts).len(), 3);
        app.handle_contacts_modal_key(&mut modal, key('d', KeyModifiers::NONE));
        assert!(modal.confirm_delete);
        app.handle_contacts_modal_key(&mut modal, key('y', KeyModifiers::NONE));
        assert_eq!(app.contacts.entries.len(), 27);
        assert!(modal.visual_anchor.is_none());
    }

    #[test]
    fn editing_recipient_resets_autocomplete_dismissal() {
        let mut composer = Composer::new();
        composer.dismissed_contact_query = Some("Ja".into());
        composer.insert_char('J');
        assert!(composer.dismissed_contact_query.is_none());
        composer.dismissed_contact_query = Some("J".into());
        composer.switch_field(ComposeField::Cc);
        assert!(composer.dismissed_contact_query.is_none());
        assert_eq!(composer.contact_suggestion, 0);
        composer.insert_char('J');
        composer.dismissed_contact_query = Some("J".into());
        composer.backspace();
        assert!(composer.dismissed_contact_query.is_none());
    }

    #[test]
    fn mail_search_matches_typos_as_subsequences_but_ranks_contiguous_first() {
        let query = "calinv";
        assert!(super::mail_fuzzy_score("Calendar invitation", query).is_some());
        assert!(super::mail_fuzzy_score("Invoice", query).is_none());
        assert!(
            super::mail_fuzzy_score("calendar", "cal")
                > super::mail_fuzzy_score("c a l service", "cal")
        );
        assert!(super::mail_fuzzy_score("Patch from Takashi", "pft").is_some());
    }

    #[test]
    fn reader_scroll_follows_wrapped_visual_rows_not_logical_lines() {
        // Forty logical lines can occupy 140 physical rows after wrapping.
        // Each j motion must keep the resulting terminal row on screen.
        let view_height = 15;
        let max_scroll = 160;
        let mut scroll = 0;
        for visual_cursor_row in 12..145 {
            scroll =
                reader_scroll_for_visual_cursor(scroll, visual_cursor_row, view_height, max_scroll);
            assert!(
                visual_cursor_row >= scroll,
                "cursor scrolled above viewport at row {visual_cursor_row}"
            );
            assert!(
                visual_cursor_row < scroll + view_height,
                "cursor disappeared below viewport at row {visual_cursor_row}"
            );
        }
        for visual_cursor_row in (12..145).rev() {
            scroll =
                reader_scroll_for_visual_cursor(scroll, visual_cursor_row, view_height, max_scroll);
            assert!(visual_cursor_row >= scroll && visual_cursor_row < scroll + view_height);
        }
    }

    #[test]
    fn reader_scroll_respects_short_viewports_and_end_of_document() {
        assert_eq!(reader_scroll_for_visual_cursor(0, 5, 1, 20), 5);
        assert_eq!(reader_scroll_for_visual_cursor(0, 19, 10, 10), 10);
        assert_eq!(reader_scroll_for_visual_cursor(8, 4, 10, 20), 1);
    }

    #[test]
    fn alt_a_opens_account_setup_without_stealing_compose_ctrl_a() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let config: crate::config::Config =
            toml::from_str("[[accounts]]\nname = \"Test\"\nsource = \"maildir:///tmp/mail\"")
                .unwrap();
        let mut app = super::App::new(&config, vec![]);
        assert!(app.setup.is_none());
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(app.setup.is_none());
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
        assert!(app.setup.is_some());
    }

    #[test]
    fn received_attachment_help_is_searchable_and_keeps_viewer_open() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let config: crate::config::Config =
            toml::from_str("[[accounts]]\nname = \"Test\"\nsource = \"maildir:///tmp/mail\"")
                .unwrap();
        let mut app = super::App::new(&config, vec![]);
        app.attachment_picker_visible = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));
        assert!(app.help_visible);
        assert!(app.attachment_picker_visible);
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.help_query, "s");
        assert!(app.help_matches().iter().any(|&index| {
            super::ATTACHMENT_HELP_ENTRIES[index]
                .description
                .contains("save")
        }));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.help_visible);
        assert!(app.attachment_picker_visible);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.attachment_picker_visible);
    }

    #[test]
    fn mute_toggle_drops_queued_alerts_and_restores_desktop_notifications() {
        let config: crate::config::Config =
            toml::from_str("[ui]\nnew_mail_notifications = true\nnew_mail_bell = true")
                .expect("valid config");
        let mut app = super::App::new(&config, vec![]);
        app.new_mail_notices.push_back(super::NewMailNotice {
            account: "University".into(),
            count: 1,
            sender: String::new(),
            subject: "test".into(),
            excerpt: String::new(),
        });
        app.new_mail_bell_pending = true;
        app.new_mail_since_launch = 3;
        app.handle_command(super::Command::ToggleNotifications);
        assert!(!app.notifications_enabled());
        assert!(app.take_new_mail_notice().is_none());
        assert!(!app.take_new_mail_bell());
        assert_eq!(app.new_mail_count(), 0);
        app.handle_command(super::Command::ToggleNotifications);
        assert!(app.notifications_enabled());
    }

    #[test]
    fn nearby_previews_favor_direction_with_bounded_normal_and_fast_windows() {
        assert_eq!(
            preview_neighbor_positions(5, 10, 1, false),
            vec![6, 7, 8, 9, 4, 3, 2, 1, 0]
        );
        assert_eq!(
            preview_neighbor_positions(5, 10, -1, false),
            vec![4, 3, 2, 1, 0, 6, 7, 8, 9]
        );
        let normal = preview_neighbor_positions(50, 200, 1, false);
        assert_eq!(&normal[..12], &(51..=62).collect::<Vec<_>>());
        assert_eq!(&normal[12..], &(44..=49).rev().collect::<Vec<_>>());
        assert_eq!(normal.len(), 18);
        let fast = preview_neighbor_positions(50, 200, -1, true);
        assert_eq!(&fast[..22], &(28..=49).rev().collect::<Vec<_>>());
        assert_eq!(&fast[22..], &(51..=58).collect::<Vec<_>>());
        assert_eq!(fast.len(), 30);
        assert_eq!(preview_neighbor_positions(0, 3, 1, false), vec![1, 2]);
        assert_eq!(preview_neighbor_positions(2, 3, -1, false), vec![1, 0]);
        assert!(preview_neighbor_positions(3, 3, 1, false).is_empty());
        assert!(preview_neighbor_positions(0, 0, -1, true).is_empty());
    }

    #[test]
    fn smooth_scroll_prefetches_immediate_rows_then_destination_without_duplicates() {
        let positions = preview_travel_positions(5, 27, 100);
        assert_eq!(&positions[..7], &[5, 6, 7, 8, 27, 26, 28]);
        assert_eq!(
            positions.len(),
            positions
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len()
        );
        let backwards = preview_travel_positions(30, 8, 100);
        assert_eq!(&backwards[..7], &[30, 29, 28, 27, 8, 9, 7]);
        assert!(preview_travel_positions(100, 0, 100).is_empty());
        assert!(preview_travel_positions(0, 100, 100).is_empty());
        assert_eq!(preview_travel_positions(0, 0, 1), vec![0]);
    }

    #[test]
    fn preview_size_budget_accounts_for_decoded_html_and_plain_text() {
        use super::preview_body_bytes;
        use crate::model::{HtmlLine, HtmlSpan, HtmlSpanStyle, MessageBody};
        let body = MessageBody {
            id: MessageId("test".into()),
            plain_text: Some("plain".into()),
            plain_fallback: None,
            html_source: Some("<p>html</p>".into()),
            html_text: Some("html".into()),
            html_lines: Some(vec![HtmlLine {
                spans: vec![HtmlSpan {
                    text: "rich".into(),
                    style: HtmlSpanStyle {
                        link: Some("https://example.org".into()),
                        ..Default::default()
                    },
                }],
            }]),
            html_renderer: None,
            html_render_width: 80,
            attachment_count: 0,
            attachment_names: Vec::new(),
        };
        assert_eq!(preview_body_bytes(&body), 5 + 11 + 4 + 4 + 19);
    }

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
    fn normal_header_jk_switch_fields_without_changing_body_motions() {
        let mut composer = Composer::new();
        composer.mode = ComposeMode::Normal;
        assert!(composer.move_header_field(true, 1));
        assert_eq!(composer.field, ComposeField::Cc);
        assert!(composer.move_header_field(true, 3));
        assert_eq!(composer.field, ComposeField::Body);
        assert!(!composer.move_header_field(false, 1));
        composer.switch_field(ComposeField::To);
        assert!(composer.move_header_field(false, 3));
        assert_eq!(composer.field, ComposeField::To);
        composer.mode = ComposeMode::Insert;
        assert!(!composer.move_header_field(true, 1));
        assert_eq!(composer.field, ComposeField::To);
    }

    #[test]
    fn delete_review_scroll_advances_one_row_per_frame() {
        let mut review = super::ReviewScroll::default();
        review.queue(80, super::SMOOTH_SCROLL_ROWS);
        assert!(review.active());
        assert_eq!(review.position, 0);
        review.advance();
        assert_eq!(review.position, 1);
        for _ in 1..22 {
            review.advance();
        }
        assert_eq!(review.position, 22);
        assert!(!review.active());
        review.queue(80, -super::SMOOTH_SCROLL_ROWS);
        for _ in 0..22 {
            review.advance();
        }
        assert_eq!(review.position, 0);
        assert!(!review.active());
    }

    #[test]
    fn detach_confirmation_can_restore_multiple_attachments() {
        use std::path::PathBuf;
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = super::App::new(&config, vec![]);
        let mut composer = super::Composer::new();
        composer.attachment_paths = ["first.png", "second.pdf", "third.txt"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        composer.attachment_manager_open = true;
        app.composer = Some(composer);
        app.detach_confirmation = Some(super::DetachConfirmation {
            paths: vec![PathBuf::from("first.png"), PathBuf::from("third.txt")],
            review: super::ReviewScroll::default(),
        });
        app.handle_detach_confirmation_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('y'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let composer = app.composer.as_mut().unwrap();
        assert_eq!(composer.attachment_paths, vec![PathBuf::from("second.pdf")]);
        composer.undo_managed_attachment();
        composer.undo_managed_attachment();
        assert_eq!(
            composer.attachment_paths,
            vec![
                PathBuf::from("first.png"),
                PathBuf::from("second.pdf"),
                PathBuf::from("third.txt")
            ]
        );
    }

    #[test]
    fn attachment_manager_detach_can_be_undone_in_original_order() {
        let mut composer = Composer::new();
        let a = PathBuf::from("/tmp/a.txt");
        let b = PathBuf::from("/other/place/b.txt");
        let c = PathBuf::from("/tmp/c.txt");
        composer.attachment_paths = vec![a.clone(), b.clone(), c.clone()];
        composer.attachment_manager_selected = 1;
        assert_eq!(composer.remove_managed_attachment(), Some(b.clone()));
        assert_eq!(composer.attachment_paths, vec![a.clone(), c.clone()]);
        assert_eq!(composer.undo_managed_attachment(), Some(b.clone()));
        assert_eq!(composer.attachment_paths, vec![a, b, c]);
        assert_eq!(composer.attachment_manager_selected, 1);
        assert_eq!(composer.undo_managed_attachment(), None);
    }

    #[test]
    fn legacy_attachment_manager_detach_can_be_undone() {
        let mut composer = Composer::new();
        composer.attachments = "/tmp/old-a.txt;/tmp/old-b.txt".to_owned();
        composer.attachment_manager_selected = 0;
        assert_eq!(
            composer.remove_managed_attachment(),
            Some(PathBuf::from("/tmp/old-a.txt"))
        );
        assert_eq!(composer.attachment_count(), 1);
        assert_eq!(
            composer.undo_managed_attachment(),
            Some(PathBuf::from("/tmp/old-a.txt"))
        );
        assert_eq!(composer.attachments, "/tmp/old-a.txt;/tmp/old-b.txt");
    }

    #[test]
    fn closing_filled_composer_requires_explicit_draft_choice() {
        let mut composer = Composer::new();
        assert!(!composer.has_unsent_content());
        composer.to = "someone@example.org".to_owned();
        assert!(composer.has_unsent_content());
        composer.to.clear();
        composer
            .attachment_paths
            .push("/tmp/some-attachment.pdf".into());
        assert!(composer.has_unsent_content());
        composer.exit_prompt = true;
        let restored: Composer =
            serde_json::from_slice(&serde_json::to_vec(&composer).unwrap()).unwrap();
        assert!(!restored.exit_prompt);
    }

    #[test]
    fn smooth_picker_scroll_only_refreshes_preview_at_destination() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..30 {
            std::fs::write(
                temp.path().join(format!("file-{index:02}.txt")),
                format!("file content {index}"),
            )
            .unwrap();
        }
        let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
        let first = match &picker.preview {
            super::ComposeFilePreview::Text { lines, .. } => lines.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        picker.queue_smooth_scroll(SMOOTH_SCROLL_ROWS);
        assert_eq!(picker.smooth_target, Some(SMOOTH_SCROLL_ROWS as usize));
        // Preview I/O is deferred through every intermediate animation frame.
        for _ in 0..(SMOOTH_SCROLL_ROWS as usize - 1) {
            picker.advance_smooth_scroll();
            assert!(picker.smooth_target.is_some());
            assert!(matches!(&picker.preview,
                super::ComposeFilePreview::Text { lines, .. } if lines == &first));
        }
        picker.advance_smooth_scroll();
        assert_eq!(picker.selected, SMOOTH_SCROLL_ROWS as usize);
        assert!(picker.smooth_target.is_none());
        assert!(matches!(&picker.preview,
            super::ComposeFilePreview::Text { lines, .. } if lines != &first));
    }

    #[test]
    fn picker_has_parent_and_file_preview_and_count_navigation() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("alpha")).unwrap();
        std::fs::write(temp.path().join("hello.txt"), b"hello from jerc").unwrap();
        let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
        let index = picker
            .entries
            .iter()
            .position(|entry| entry.name == "hello.txt")
            .unwrap();
        picker.move_selection(index as isize);
        assert!(matches!(
            picker.preview,
            super::ComposeFilePreview::Text { .. }
        ));
        assert!(!picker.parent_entries.is_empty());
        let mut composer = Composer::new();
        composer.file_picker = Some(picker);
        composer
            .file_picker_key(crossterm::event::KeyCode::Char('g'))
            .unwrap();
        composer
            .file_picker_key(crossterm::event::KeyCode::Char('g'))
            .unwrap();
        assert_eq!(composer.file_picker.as_ref().unwrap().selected, 0);
        composer
            .file_picker_key(crossterm::event::KeyCode::Char('G'))
            .unwrap();
        assert_eq!(composer.file_picker.as_ref().unwrap().selected, index);
    }

    #[test]
    fn send_review_can_prepare_rich_preview_without_an_html_alternative() {
        let mut composer = Composer::new();
        composer.to = "friend@example.com".to_owned();
        composer.body = "Hello!".to_owned();
        composer.prepare_send_preview().unwrap();
        assert!(composer.confirm_send);
        assert!(composer.preview_rich_lines.is_none());
        assert_eq!(composer.preview_scroll, 0);
    }

    #[test]
    fn compose_tabs_skip_file_paths_and_advanced_headers() {
        assert_eq!(ComposeField::Subject.next(), ComposeField::Body);
        assert_eq!(ComposeField::Body.previous(), ComposeField::Subject);
        let mut composer = Composer::new();
        composer.switch_field(ComposeField::Subject);
        composer.toggle_advanced_headers();
        assert_eq!(composer.field, ComposeField::Headers);
        composer.headers = "Reply-To: team@example.com".to_owned();
        composer.toggle_advanced_headers();
        assert_eq!(composer.field, ComposeField::Subject);
        assert_eq!(composer.headers, "Reply-To: team@example.com");
    }

    #[test]
    fn picker_handles_semicolon_names_and_restores_selected_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("report;v2.txt"), b"attachment test").unwrap();
        std::fs::create_dir(temp.path().join("folder")).unwrap();
        let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
        assert!(picker.entries[0].is_directory);
        let file_index = picker
            .entries
            .iter()
            .position(|entry| entry.name == "report;v2.txt")
            .unwrap();
        picker.selected = file_index;
        let mut composer = Composer::new();
        composer.file_picker = Some(picker);
        composer
            .file_picker_key(crossterm::event::KeyCode::Char(' '))
            .unwrap();
        assert_eq!(composer.attachment_count(), 1);
        let loaded = super::load_compose_attachments("", &composer.attachment_paths).unwrap();
        assert_eq!(loaded[0].filename, "report;v2.txt");
        assert_eq!(loaded[0].data.as_slice(), b"attachment test");
        // Space on the final file does not jump back to the previous item.
        composer.file_picker.as_mut().unwrap().selected = file_index;
        composer
            .file_picker_key(crossterm::event::KeyCode::Char(' '))
            .unwrap();
        assert_eq!(composer.attachment_count(), 0);
    }

    #[test]
    fn picker_visual_range_attaches_files_after_count_motion() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..7 {
            std::fs::write(temp.path().join(format!("{index}.txt")), b"hi").unwrap();
        }
        let mut composer = Composer::new();
        composer.file_picker =
            Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
        let key = |ch| crossterm::event::KeyCode::Char(ch);
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('5')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        assert_eq!(composer.file_picker.as_ref().unwrap().selected, 5);
        assert_eq!(
            composer.file_picker.as_ref().unwrap().visual_anchor,
            Some(0)
        );
        composer
            .file_picker_key(crossterm::event::KeyCode::Enter)
            .unwrap();
        assert_eq!(composer.attachment_count(), 6);
        assert!(
            composer
                .file_picker
                .as_ref()
                .unwrap()
                .visual_anchor
                .is_none()
        );
        // Re-selecting the same range does not add duplicates.
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('5')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer
            .file_picker_key(crossterm::event::KeyCode::Enter)
            .unwrap();
        assert_eq!(composer.attachment_count(), 6);
    }

    #[test]
    fn picker_space_advances_and_stays_at_end_and_search_help_is_local() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(temp.path().join(name), b"ok").unwrap();
        }
        let mut composer = Composer::new();
        composer.file_picker =
            Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
        let key = |ch| crossterm::event::KeyCode::Char(ch);
        composer.file_picker_key(key(' ')).unwrap();
        assert_eq!(composer.file_picker.as_ref().unwrap().selected, 1);
        composer.file_picker_key(key(' ')).unwrap();
        assert_eq!(composer.file_picker.as_ref().unwrap().selected, 2);
        composer.file_picker_key(key(' ')).unwrap();
        assert_eq!(composer.file_picker.as_ref().unwrap().selected, 2);
        assert_eq!(composer.attachment_count(), 3);
        composer.file_picker_key(key(' ')).unwrap();
        assert_eq!(composer.file_picker.as_ref().unwrap().selected, 2);
        assert_eq!(composer.attachment_count(), 2);
        composer.file_picker_key(key('?')).unwrap();
        composer.file_picker_key(key('v')).unwrap();
        assert!(composer.file_picker.as_ref().unwrap().help_visible);
        assert_eq!(composer.file_picker.as_ref().unwrap().help_query, "v");
        composer
            .file_picker_key(crossterm::event::KeyCode::Esc)
            .unwrap();
        assert!(!composer.file_picker.as_ref().unwrap().help_visible);
        composer.file_picker_key(key('v')).unwrap();
        composer
            .file_picker_key(crossterm::event::KeyCode::Esc)
            .unwrap();
        assert!(
            composer
                .file_picker
                .as_ref()
                .unwrap()
                .visual_anchor
                .is_none()
        );
    }

    #[test]
    fn picker_slash_search_accepts_current_match_and_tab_opens_folder() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["photos-2025", "photos-2026", "notes"] {
            std::fs::create_dir(temp.path().join(name)).unwrap();
        }
        std::fs::write(temp.path().join("photos.txt"), b"hello").unwrap();
        let mut composer = Composer::new();
        composer.file_picker =
            Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
        let key = |ch| crossterm::event::KeyCode::Char(ch);
        composer.file_picker_key(key('/')).unwrap();
        for ch in "photos".chars() {
            composer.file_picker_key(key(ch)).unwrap();
        }
        let picker = composer.file_picker.as_ref().unwrap();
        assert_eq!(picker.search_matches().len(), 2); // folder results take precedence
        assert_eq!(picker.entries[picker.selected].name, "photos-2025");
        composer
            .file_picker_key(crossterm::event::KeyCode::Down)
            .unwrap();
        let picker = composer.file_picker.as_ref().unwrap();
        assert_eq!(picker.entries[picker.selected].name, "photos-2026");
        composer
            .file_picker_key(crossterm::event::KeyCode::Enter)
            .unwrap();
        let picker = composer.file_picker.as_ref().unwrap();
        assert_eq!(picker.entries[picker.selected].name, "photos-2026");
        assert!(picker.search_query.is_none());
        // Enter should return to normal motion, so l opens the highlighted folder.
        composer.file_picker_key(key('l')).unwrap();
        let picker = composer.file_picker.as_ref().unwrap();
        assert!(picker.directory.ends_with("photos-2026"));
        composer.file_picker_key(key('/')).unwrap();
        composer
            .file_picker_key(crossterm::event::KeyCode::Esc)
            .unwrap();
        assert!(
            composer
                .file_picker
                .as_ref()
                .unwrap()
                .search_query
                .is_none()
        );
        assert!(composer.file_picker.is_some());
    }

    #[test]
    fn picker_visual_x_detaches_multiple_files_without_touching_others() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(temp.path().join(name), b"ok").unwrap();
        }
        let mut composer = Composer::new();
        composer.file_picker =
            Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
        let key = |ch| crossterm::event::KeyCode::Char(ch);
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('2')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer
            .file_picker_key(crossterm::event::KeyCode::Enter)
            .unwrap();
        assert_eq!(composer.attachment_count(), 3);
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer.file_picker_key(key('x')).unwrap();
        assert_eq!(composer.attachment_count(), 1);
        assert_eq!(composer.attachment_paths[0].file_name().unwrap(), "c.txt");
        assert!(
            composer
                .file_picker
                .as_ref()
                .unwrap()
                .visual_anchor
                .is_none()
        );
    }

    #[test]
    fn picker_visual_space_detaches_only_when_every_file_is_attached() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("0folder")).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(temp.path().join(name), b"hello").unwrap();
        }
        let mut composer = Composer::new();
        composer.file_picker =
            Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
        let key = |ch| crossterm::event::KeyCode::Char(ch);
        assert!(composer.file_picker.as_ref().unwrap().entries[0].is_directory);
        // Attach a and b while selecting a directory too (which must be ignored).
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('2')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer
            .file_picker_key(crossterm::event::KeyCode::Enter)
            .unwrap();
        assert_eq!(composer.attachment_count(), 2);
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('2')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer.file_picker_key(key(' ')).unwrap();
        assert_eq!(composer.attachment_count(), 0); // folder is ignored
        assert!(
            composer
                .file_picker
                .as_ref()
                .unwrap()
                .visual_anchor
                .is_none()
        );

        // A mixed range should attach the missing file without detaching an
        // already attached file.
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer.file_picker_key(key(' ')).unwrap();
        assert_eq!(composer.attachment_count(), 1); // a.txt only
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('2')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer.file_picker_key(key(' ')).unwrap();
        assert_eq!(composer.attachment_count(), 2); // a.txt and b.txt
        assert!(composer.attachment_paths.iter().all(|path| path.is_file()));
    }

    #[test]
    fn picker_pdf_previews_are_deferred_and_stale_results_are_cached_not_displayed() {
        let temp = tempfile::tempdir().unwrap();
        let pdf = temp.path().join("a.pdf");
        let text = temp.path().join("b.txt");
        std::fs::write(&pdf, b"%PDF-1.4\n").unwrap();
        std::fs::write(&text, b"text file").unwrap();
        let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
        assert!(matches!(
            picker.preview,
            super::ComposeFilePreview::Loading(_)
        ));
        assert!(picker.pdf_preview_due.is_some());
        // Navigate away before the debounce expires: no converter was started.
        picker.move_selection(1);
        assert!(picker.pdf_preview_due.is_none());
        assert!(picker.pdf_preview_inflight.is_empty());
        assert!(matches!(
            picker.preview,
            super::ComposeFilePreview::Text { .. }
        ));
        // Pretend an earlier conversion completed while the selection changed.
        picker.pdf_preview_inflight.insert(pdf.clone());
        picker
            .pdf_preview_results
            .lock()
            .unwrap()
            .push_back((pdf.clone(), Ok(vec![7, 8, 9])));
        picker.advance_pdf_preview();
        assert!(matches!(
            picker.preview,
            super::ComposeFilePreview::Text { .. }
        ));
        assert!(picker.pdf_preview_inflight.is_empty());
        picker.move_selection(-1);
        assert!(
            matches!(picker.preview, super::ComposeFilePreview::Image(ref bytes) if bytes == &[7, 8, 9])
        );
        assert!(picker.pdf_preview_due.is_none());
    }

    #[test]
    fn manage_attachments_removes_files_from_unrelated_directories() {
        let mut composer = Composer::new();
        let first = std::path::PathBuf::from("/tmp/one/first.txt");
        let second = std::path::PathBuf::from("/home/example/elsewhere/second.txt");
        composer.attachment_paths = vec![first, second.clone()];
        composer.attachment_manager_selected = 0;
        assert_eq!(
            composer
                .remove_managed_attachment()
                .unwrap()
                .file_name()
                .unwrap(),
            "first.txt"
        );
        assert_eq!(composer.attachment_paths, vec![second]);
        assert_eq!(composer.attachment_manager_selected, 0);
        composer.attachments = "/tmp/legacy/old.txt".to_owned();
        composer.attachment_manager_selected = 1;
        assert_eq!(
            composer
                .remove_managed_attachment()
                .unwrap()
                .file_name()
                .unwrap(),
            "old.txt"
        );
        assert_eq!(composer.attachment_count(), 1);
    }

    #[test]
    fn picker_recognizes_image_files_for_terminal_preview() {
        let temp = tempfile::tempdir().unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(temp.path().join("photo.png"), png.into_inner()).unwrap();
        let picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
        match &picker.preview {
            super::ComposeFilePreview::Image(bytes) => {
                assert!(image::load_from_memory(bytes).is_ok())
            }
            preview => panic!("expected a decodable image, got {preview:?}"),
        }
        assert!(super::compose_image_extension(&temp.path().join("a.WEBP")));
    }

    #[test]
    fn picker_detects_known_source_file_mime_types() {
        let temp = tempfile::tempdir().unwrap();
        for (name, expected) in [
            ("Cargo.toml", "application/toml"),
            ("Cargo.lock", "text/plain"),
            ("lib.rs", "text/plain"),
            ("sample.json", "application/json"),
            ("LICENSE", "text/plain"),
        ] {
            let path = temp.path().join(name);
            std::fs::write(&path, "content").unwrap();
            assert_eq!(
                super::load_compose_file(&path).unwrap().content_type,
                expected
            );
        }
    }

    #[test]
    fn picker_attached_files_are_encoded_as_mime_parts() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("hello.txt");
        std::fs::write(&file, b"hello attachment content").unwrap();
        let files = super::load_compose_attachments("", &[file]).unwrap();
        let message = crate::model::OutgoingMessage {
            from: "sender@example.com".to_owned(),
            to: "receiver@example.com".to_owned(),
            cc: String::new(),
            bcc: String::new(),
            subject: "attachment smoke test".to_owned(),
            body: "See the attachment".to_owned(),
            html_body: None,
            attachments: files,
            custom_headers: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            forwarded_raw: None,
        };
        let raw = crate::outgoing::build_outgoing(&message, "sender@example.com")
            .unwrap()
            .formatted();
        let wire = String::from_utf8_lossy(&raw);
        assert!(wire.contains("multipart/mixed"));
        assert!(wire.contains("hello.txt"));
        assert!(wire.contains("Content-Disposition: attachment"));
    }

    #[test]
    fn compose_selected_file_paths_survive_postponed_draft_encoding() {
        let mut composer = Composer::new();
        composer
            .attachment_paths
            .push(std::path::PathBuf::from("/tmp/a file.pdf"));
        let bytes = serde_json::to_vec(&composer).unwrap();
        let restored: Composer = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.attachment_paths, composer.attachment_paths);
        assert!(restored.file_picker.is_none());
        assert!(!restored.advanced_headers_open);
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
    fn compose_vim_word_motions_and_undo_redo_are_consistent() {
        let mut composer = Composer::new();
        composer.switch_field(ComposeField::Body);
        composer.body = "first second".to_owned();
        composer.cursor = 0;
        assert!(composer.apply_motion(crossterm::event::KeyCode::Char('w'), 1));
        assert_eq!(composer.cursor, 6);
        composer.insert_char('X');
        assert_eq!(composer.body, "first Xsecond");
        composer.undo_edit(false);
        assert_eq!(composer.body, "first second");
        composer.undo_edit(true);
        assert_eq!(composer.body, "first Xsecond");
    }

    #[test]
    fn compose_inner_text_objects_choose_the_inner_enclosing_delimiter() {
        let mut composer = Composer::new();
        composer.switch_field(ComposeField::Body);
        composer.body = "call(foo[bar])".to_owned();
        composer.cursor = 10;
        composer.select_inner_object('[');
        assert_eq!(composer.mode, ComposeMode::Visual);
        assert_eq!(composer.visual_range(), Some((9, 12)));
        composer.mode = ComposeMode::Normal;
        composer.cursor = 10;
        composer.select_inner_object('(');
        assert_eq!(composer.visual_range(), Some((5, 13)));
    }

    #[test]
    fn compose_find_char_never_crosses_into_the_next_line() {
        let mut composer = Composer::new();
        composer.switch_field(ComposeField::Body);
        composer.body = "first line\nsecond line x".to_owned();
        composer.cursor = 0;
        composer.find_char('x', true, 1);
        assert_eq!(composer.cursor, 0);
        composer.cursor = composer.body.chars().count();
        composer.find_char('s', false, 1);
        assert_eq!(composer.cursor, "first line\n".chars().count());
    }

    #[test]
    fn compose_visual_linewise_range_covers_the_newline() {
        let mut composer = Composer::new();
        composer.body = "one\ntwo\nthree".to_owned();
        composer.switch_field(ComposeField::Body);
        composer.cursor = 5;
        composer.visual_anchor = Some(5);
        composer.visual_linewise = true;
        composer.mode = ComposeMode::Visual;
        assert_eq!(composer.visual_range(), Some((4, 8)));
    }

    #[test]
    fn compose_help_search_uses_its_own_keymap() {
        let mut composer = Composer::new();
        composer.help_query = "attachment".to_owned();
        let matches = composer.compose_help_matches();
        assert!(!matches.is_empty());
        assert!(matches.iter().all(|&index| {
            super::super::input::COMPOSE_HELP_ENTRIES[index]
                .description
                .contains("attachment")
        }));
    }

    #[test]
    fn reader_inner_text_objects_support_quotes_and_brackets() {
        assert_eq!(
            inner_text_object_range(r#"say "hello" now"#, 6, '"'),
            Some((5, 9))
        );
        assert_eq!(
            inner_text_object_range("call(foo[bar])", 9, ']'),
            Some((9, 11))
        );
        assert_eq!(
            inner_text_object_range("call(foo[bar])", 9, ')'),
            Some((5, 12))
        );
        assert_eq!(inner_text_object_range("'{value}'", 4, '\''), Some((1, 7)));
    }

    #[test]
    fn reader_selection_yanks_exact_character_range() {
        let selection = ReaderSelection {
            start: ReaderPosition { line: 0, column: 2 },
            end: ReaderPosition { line: 1, column: 2 },
        };
        assert_eq!(
            reader_selection_text("abcdef\nghijkl", selection),
            "cdef\nghi"
        );
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
            reader_image_target_size((148, 48), (Some(74), Some(24)), (8, 16), (120, 40),),
            Size::new(19, 3)
        );

        // A genuine 1x asset is still capped at native size and never upscaled.
        assert_eq!(
            reader_image_target_size((74, 24), (Some(300), Some(100)), (8, 16), (120, 40),),
            Size::new(10, 2)
        );
    }

    #[test]
    fn reader_images_use_full_reader_bounds_instead_of_legacy_caps() {
        assert_eq!(
            reader_image_target_size((1200, 600), (None, None), (8, 16), (120, 40),),
            Size::new(120, 38)
        );
        assert_eq!(
            reader_image_target_size((2400, 1600), (None, None), (8, 16), (90, 30),),
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
    fn quoted_reply_attribution_matches_expected_shape() {
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
            attachment_names_hint: Vec::new(),
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
            MessageId("forward".into()),
            raw,
            raw.len() as u64,
            MessageFlags::empty(),
        )
        .unwrap();
        let composer = Composer::forward(&message, raw.to_vec()).unwrap();
        assert!(
            composer
                .body
                .contains("---------- Forwarded message ---------")
        );
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

#[cfg(test)]
mod received_attachment_browser_tests {
    use super::*;

    fn attachment(name: &str, bytes: &[u8]) -> OutgoingAttachment {
        OutgoingAttachment {
            filename: name.into(),
            content_type: "text/plain".into(),
            data: bytes.to_vec(),
        }
    }

    #[test]
    fn received_attachment_filenames_cannot_escape_destination() {
        assert_eq!(
            attachment_safe_name("../../important.txt", 0),
            "important.txt"
        );
        assert_eq!(attachment_safe_name("..\\..\\secret.pdf", 2), "secret.pdf");
        assert_eq!(attachment_safe_name("..", 1), "attachment-2.bin");
        assert_eq!(attachment_safe_name("\n", 0), "attachment-1.bin");
    }

    #[test]
    fn received_pdf_attachment_recognizes_mime_and_filename() {
        let mut file = attachment("document.bin", b"%PDF-1.4");
        assert!(!received_attachment_is_pdf(&file));
        file.content_type = "application/pdf".into();
        assert!(received_attachment_is_pdf(&file));
        file.content_type = "application/octet-stream".into();
        file.filename = "presentation.PDF".into();
        assert!(received_attachment_is_pdf(&file));
    }

    #[test]
    fn destination_picker_does_not_decode_or_launch_pdf_preview() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.pdf"), b"%PDF-1.4\n").unwrap();
        let picker = ComposeFilePicker::open_directory_only(temp.path().to_path_buf()).unwrap();
        assert!(picker.directory_only);
        assert!(matches!(&picker.preview, ComposeFilePreview::Binary(_)));
        assert!(picker.pdf_preview_due.is_none());
        assert!(picker.pdf_preview_inflight.is_empty());
    }

    #[test]
    fn attachment_session_cache_keys_include_mailbox_account_and_uidvalidity() {
        let uid = MessageId("42".into());
        let key = (0usize, "INBOX".to_owned(), Some(123u32), uid.clone());
        assert_ne!(key, (0, "INBOX".to_owned(), Some(124u32), uid.clone()));
        assert_ne!(key, (1, "INBOX".to_owned(), Some(123u32), uid.clone()));
        assert_ne!(key, (0, "Archive".to_owned(), Some(123u32), uid));
    }

    #[test]
    fn saving_multiple_same_named_attachments_never_overwrites() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("report.txt"), "existing").unwrap();
        let first = save_attachment_without_overwrite(
            directory.path(),
            &attachment("report.txt", b"first"),
            0,
        )
        .unwrap();
        let second = save_attachment_without_overwrite(
            directory.path(),
            &attachment("../../report.txt", b"second"),
            1,
        )
        .unwrap();
        assert_ne!(first, second);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("report.txt")).unwrap(),
            "existing"
        );
        assert_eq!(std::fs::read(first).unwrap(), b"first");
        assert_eq!(std::fs::read(second).unwrap(), b"second");
    }

    #[test]
    fn selection_start_and_drag_width_are_safely_bounded() {
        assert_eq!(attachment_list_start(15, 30, 10), 10);
        assert_eq!(attachment_list_start(1, 30, 10), 0);
        assert_eq!(mouse_width_percent(0, 120), 12);
        assert_eq!(mouse_width_percent(110, 120), 50);
        assert_eq!(mouse_width_percent(24, 100), 25);
    }
}

#[cfg(test)]
mod preview_width_tests {
    use super::preview_percent_at;
    #[test]
    fn preview_drag_uses_remaining_right_hand_region() {
        assert_eq!(preview_percent_at(60, 20, 100), 60);
        assert_eq!(preview_percent_at(5, 20, 100), 75);
        assert_eq!(preview_percent_at(200, 20, 100), 25);
    }
}
