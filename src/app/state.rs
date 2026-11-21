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

pub use self::reader_text::{ReaderPosition, ReaderSelection};
use self::reader_text::{
    ReaderWordMotion, reader_scroll_for_visual_cursor, reader_selection_text, reader_word_target,
    url_at_cursor,
};
use crate::app::navigation::{bounded_index, reveal_row, wrapped_index as offset_index};
use crate::app::vim::{YankRegister, inner_text_object_range};

use crate::app::{
    ATTACHMENT_HELP_ENTRIES, CONTACT_HELP_ENTRIES, Command, DESTINATION_HELP_ENTRIES, Focus,
    HELP_ENTRIES, command_for_key, ranked_help_matches,
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
    CacheIdentity, Capabilities, HtmlLine, Mailbox, MailboxRole, MessageBody, MessageFlags,
    MessageId, MessageSummary, OutgoingAttachment, OutgoingMessage,
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
// Account and application methods are grouped by responsibility. State fields
// and the serialized representation remain defined in this parent module.
mod account_state;
mod app_animation;
mod app_controls;
mod app_core;
mod app_navigation;
mod app_notices;
mod app_reader;
mod app_recovery;
mod app_setup;
// Shared helpers are separate from state ownership and event handling.
mod attachment_util;
mod clipboard_util;
mod compose_util;
mod view_util;
pub(crate) use self::attachment_util::attachment_safe_name;
use self::attachment_util::*;
use self::clipboard_util::*;
use self::compose_util::*;
use self::view_util::*;
mod account_cache;
mod account_sync;
use self::account_sync::{MailboxStatusQueue, StatusReason};
pub(crate) use self::manual_review::{ReviewItem, ReviewOperation};
use self::notice_ack::{NoticeAcknowledgments, key_digest};
pub(crate) use self::notices::{NoticeBrowser, NoticeRow};
mod attachment_actions;
mod compose_actions;
mod composer;
mod contacts;
mod file_picker;
mod initialization;
mod input_dispatch;
mod mail_actions;
mod manual_review;
mod mouse;
mod notice_ack;
mod notices;
mod reader_actions;
mod reader_text;
mod search_help_actions;
mod worker_events;

// Only a foreground network operation can hold the sync footer. If its
// worker never responds, abandon read-only progress instead of spinning forever.
const FOREGROUND_SYNC_TIMEOUT: Duration = Duration::from_secs(120);
// A blocked read-only IMAP request should never make a healthy network wait
// for the socket's 90-second timeout before a fresh connection can start.
const FOREGROUND_RECOVERY_TIMEOUT: Duration = Duration::from_secs(15);
// A large header FETCH may be legitimately slower than LIST/CONNECT.
const FOREGROUND_PAGE_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Conservative width for authored replies to Linux kernel mailing lists.
pub(crate) const REPLY_WRAP_COLUMNS: usize = 72;

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
const MAX_WARM_MAILBOXES: usize = 16;
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
    .map(|text| text.len())
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

#[derive(Debug, Clone)]
pub struct StatusLine {
    pub text: String,
    pub is_error: bool,
    expires_at: Option<Instant>,
}

impl StatusLine {
    pub(crate) fn info(text: impl Into<String>) -> Self {
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

    pub(crate) fn temporary_error(text: impl Into<String>, duration: Duration) -> Self {
        Self {
            text: text.into(),
            is_error: true,
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
pub struct DraftSource {
    pub mailbox: String,
    pub id: MessageId,
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
    /// Hard-wrap newly written reply prose, leaving quoted source lines untouched.
    #[serde(default)]
    pub reply_wrap: bool,
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
    /// Original server draft, used to preserve attached MIME parts on resubmit.
    #[serde(default)]
    pub draft_raw: Option<Vec<u8>>,
    /// Names of attachments already embedded in a server draft. Read-only in
    /// the attachment manager; retained when editing the text or re-saving.
    #[serde(default)]
    pub draft_attachment_names: Vec<String>,
    /// Only retire an earlier server draft AFTER SMTP accepts or a replacement
    /// draft is successfully appended. Keep in local recovery JSON as well.
    #[serde(default)]
    pub draft_source: Option<DraftSource>,
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
    /// Transient Vim operator/count state; never serialized into drafts.
    #[serde(skip)]
    vim_sequence: crate::app::vim::VimSequence,
    /// Bounded undo/redo of one field's value/cursor at a time.
    #[serde(skip)]
    edit_history: crate::app::vim::VimHistory<(ComposeField, String, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeIntent {
    Reply {
        quote: bool,
        audience: ReplyAudience,
    },
    Forward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCompose {
    account: usize,
    request_id: u64,
    message_id: MessageId,
    intent: ComposeIntent,
}

/// A pending read-only Drafts preview. Enter opens the normal reader first;
/// the Yes/No dialog is shown only once the original MIME is ready.
struct PendingDraft {
    account: usize,
    mailbox: String,
    message_id: MessageId,
    composer: Option<Composer>,
}

/// Match a query as an ordered, case-insensitive subsequence. Consecutive
/// characters, word boundaries and exact substrings rank ahead of scattered
/// matches; no external search dependency or indexing is required for a page.
fn mail_fuzzy_score(field: &str, query_lower: &str) -> Option<i64> {
    let lower = field.to_lowercase();
    if query_lower.is_empty() {
        return Some(0);
    }
    let mut wanted = query_lower.chars();
    let mut next = wanted.next();
    let mut previous_match = None;
    let mut previous_char: Option<char> = None;
    let mut score = 0i64;
    let mut length = 0usize;
    for (position, ch) in lower.chars().enumerate() {
        length += 1;
        if next == Some(ch) {
            score += 10;
            if previous_char.is_none_or(|c| !c.is_alphanumeric()) {
                score += 9;
            }
            if let Some(prev) = previous_match {
                if position == prev + 1 {
                    score += 12;
                } else {
                    score -= i64::try_from(position - prev - 1).unwrap_or(100).min(12);
                }
            }
            previous_match = Some(position);
            next = wanted.next();
        }
        previous_char = Some(ch);
    }
    if next.is_some() {
        return None;
    }
    if lower.contains(query_lower) {
        score += 80;
    }
    score -= i64::try_from(length / 20).unwrap_or(10).min(10);
    Some(score)
}

/// Cache only row indices and weak header identities, never a duplicate copy
/// of message strings. A changed header, order, query or matching mode rebuilds it.
#[derive(Default)]
struct LocalSearchIndex {
    query: String,
    mode: crate::config::MailSearchMode,
    headers: Vec<std::sync::Weak<crate::model::MessageHeaders>>,
    rows: Vec<usize>,
}

impl LocalSearchIndex {
    fn matches(
        &mut self,
        messages: &[MessageSummary],
        query: &str,
        mode: crate::config::MailSearchMode,
    ) -> Vec<usize> {
        if self.query == query
            && self.mode == mode
            && self.headers.len() == messages.len()
            && self
                .headers
                .iter()
                .zip(messages)
                .all(|(old, message)| old.as_ptr() == Arc::as_ptr(&message.headers))
        {
            return self.rows.clone();
        }
        self.query = query.to_owned();
        self.mode = mode;
        self.headers = messages
            .iter()
            .map(|m| Arc::downgrade(&m.headers))
            .collect();
        let mut matches: Vec<_> = messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                let fields = [
                    message.subject.as_str(),
                    message.from_name.as_str(),
                    message.from_address.as_str(),
                    message.date_label.as_ref(),
                    message.to.as_str(),
                    message.cc.as_str(),
                    message.bcc.as_str(),
                ];
                let mut fields = fields
                    .into_iter()
                    .chain(message.attachment_names_hint.iter().map(String::as_str));
                let score = match mode {
                    crate::config::MailSearchMode::Strict => fields
                        .any(|field| field.to_lowercase().contains(query))
                        .then_some(0),
                    crate::config::MailSearchMode::Fuzzy => fields
                        .filter_map(|field| mail_fuzzy_score(field, query))
                        .max(),
                };
                score.map(|score| (index, score))
            })
            .collect();
        if mode == crate::config::MailSearchMode::Fuzzy {
            matches.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        }
        self.rows = matches.into_iter().map(|(index, _)| index).collect();
        self.rows.clone()
    }
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
    warm_folder_order: VecDeque<String>,
    /// Read-only speculative folder loads; only one outstanding per account.
    mailbox_warm_queue: VecDeque<String>,
    mailbox_warm_request: Option<(u64, String)>,
    cache_writer: Option<cache::Writer>,
    body_store: Option<cache::BodyStore>,
    raw_store: Option<cache::RawStore>,
    cache_identities: HashMap<String, CacheIdentity>,
    uid_nexts: HashMap<String, u32>,
    unverified_mailboxes: HashSet<String>,
    pub search_query: String,
    pub search_editing: bool,
    pub native_search_editing: bool,
    pub native_view: Option<crate::backend::MailboxView>,
    pub(crate) local_thread_view: Option<crate::threading::LocalThreadView>,
    /// Ordinary mailbox grouping preference, independent of temporary views.
    pub(crate) threading_enabled: bool,
    local_thread_index: std::cell::RefCell<crate::threading::LocalThreadIndex>,
    // Only active-mailbox state. Extras stay after the canonical page window.
    expanded_threads: HashSet<MessageId>,
    inline_thread_extra: HashSet<MessageId>,
    inline_thread_requests: HashMap<u64, MessageId>,
    pub search_mode: crate::config::MailSearchMode,
    local_search_index: std::cell::RefCell<LocalSearchIndex>,
    pub bodies: HashMap<MessageId, MessageBody>,
    pub pending_bodies: HashSet<MessageId>,
    pub pending_seen: HashMap<MessageId, u64>,
    deferred_seen: Vec<cache::DeferredSeen>,
    // Crash-durable destructive operations. NeedsReview entries must never
    // be automatically retried after an ambiguous IMAP response.
    deferred_mutations: Vec<cache::DeferredMutation>,
    inflight_deferred_mutations: HashMap<u64, cache::DeferredMutation>,
    // Only offline/restored operations should produce queue notices. The
    // normal online queue -> in-flight transition is NOT an alert.
    offline_notice_mutations: HashSet<String>,
    outbox_store: Option<crate::outgoing::outbox::Store>,
    outbox: Vec<crate::outgoing::outbox::QueuedSend>,
    offline_notice_sends: HashSet<String>,
    // A failure is a notice only after an actual connection problem, never
    // merely because a newly opened account is still connecting.
    offline_incident_active: bool,
    offline_incident_number: u64,
    inflight_outbox: HashMap<u64, String>,
    // Correlates Seen ACKs with the original mailbox, including navigation.
    inflight_deferred_seen: HashMap<u64, cache::DeferredSeen>,
    status_requests: MailboxStatusQueue,
    pending_inbox_refresh: bool,
    jmap: bool,
    pending_account_refresh: bool,
    push_list_inflight: bool,
    pending_mutation_labels: HashMap<u64, &'static str>,
    pending_destination_refresh: HashMap<u64, String>,
    pub connecting: bool,
    connect_request_id: Option<u64>,
    // A list response/error must close only the matching list request.
    mailbox_list_request_id: Option<u64>,
    // Bounded tombstones for LIST requests abandoned by refresh/disconnect.
    // Their late errors must not tear down a newly connected session.
    abandoned_list_requests: VecDeque<u64>,
    sync_progress_at: Option<Instant>,
    pub mailboxes_loading: bool,
    pub messages_loading: bool,
    pub last_scan_micros: Option<u128>,
    pub restore_message_index: Option<usize>,
    pub worker: WorkerHandle,
    foreground_generation: u64,
}

// Ignore journal state: Queued -> NeedsReview is a normal in-flight
// transition, so the notice must retain its identity until the server ACK.
fn mutation_notice_key(entry: &cache::DeferredMutation) -> String {
    format!(
        "{}:{}:{:?}:{:?}",
        entry.mailbox, entry.validity, entry.kind, entry.ids
    )
}

fn notice_count(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

// A single displayed notice can represent several queued actions. Track
// acknowledgment per action, not per aggregate count: completing one of three
// already-acknowledged emails must not resurrect the banner for the other two.
struct WarningNotice {
    identities: Vec<String>,
    message: String,
}

#[derive(Debug, Clone)]
enum MutationKind {
    Delete,
    Move { destination: String },
    Archive { destination: String },
    Unarchive { destination: String },
}

/// One-row-per-frame smooth cursor movement shared by review dialogs.
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

/// Separate fields so mail review columns stay aligned regardless of subject length.
#[derive(Debug, Clone)]
pub struct MailReviewItem {
    pub subject: String,
    pub from_address: String,
}

#[derive(Debug, Clone)]
pub struct MutationConfirmation {
    pub prompt: String,
    pub items: Vec<MailReviewItem>,
    pub review: ReviewScroll,
    source: String,
    ids: Vec<MessageId>,
    kind: MutationKind,
}

impl MutationConfirmation {
    pub fn is_archive(&self) -> bool {
        matches!(self.kind, MutationKind::Archive { .. })
    }
}

/// An attachment is only detached after the user accepts the review.
#[derive(Debug, Clone)]
pub struct DetachConfirmation {
    pub paths: Vec<PathBuf>,
    pub review: ReviewScroll,
}

type PdfPreviewResult = (PathBuf, Result<Vec<u8>, String>);
type AttachmentCacheEntry = (
    (usize, String, Option<CacheIdentity>, MessageId),
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
    sync_spinner_frame: usize,
    pub startup_warnings: Vec<String>,
    pub show_startup_warnings: bool,
    // UI acknowledgment only; never alters durable Outbox/mutation state.
    pub acknowledged_warning_keys: HashSet<String>,
    notice_ack_store: Option<NoticeAcknowledgments>,
    notice_first_seen: HashMap<String, u64>,
    pub(crate) notices: NoticeBrowser,
    pub manual_review_visible: bool,
    pub manual_review_selected: usize,
    manual_review_pending: Option<ReviewItem>,
    pub should_quit: bool,
    /// In-app first-run or add-account setup, modal over normal mail UI.
    pub setup: Option<SetupWizard>,
    pub remove_account_confirmation: Option<String>,
    /// Purge only after reload has dropped the removed account's cache writer.
    pub pending_account_cleanup: Vec<crate::config::AccountConfig>,
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
    /// Show Git patch colors even inside quoted mailing-list replies.
    pub quoted_diff_highlighting: bool,
    pub show_header_details: bool,
    theme_config_path: Option<PathBuf>,
    pub attachment_picker_visible: bool,
    pub attachment_selection: usize,
    pub attachment_files: Arc<Vec<OutgoingAttachment>>,
    /// TempPath deletes the opened attachment on eviction or App shutdown.
    /// Bound both the number and total size of these exports.
    opened_attachment_files: VecDeque<Arc<tempfile::TempPath>>,
    pub remote_attachment_refs: Vec<crate::backend::RemoteAttachment>,
    remote_preview_request: Option<(u64, usize)>,
    remote_preview_file: Option<(usize, Arc<OutgoingAttachment>)>,
    remote_transfer_request: Option<u64>,
    // A small per-session MIME cache makes reopening a received attachment
    // browser instantaneous instead of refetching the entire raw email.
    // Include UIDVALIDITY to avoid reusing a recycled IMAP UID.
    attachment_source: Option<(usize, String, Option<CacheIdentity>, MessageId)>,
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
    /// Correlates an asynchronous MIME attachment check with its worker reply.
    attachment_pending_request_id: Option<u64>,
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
    /// Session-only shared register: reader selections can be pasted into Compose.
    yank_register: Option<YankRegister>,
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
    pending_draft: Option<PendingDraft>,
    /// Draft replacement cleanup is tied to the exact APPEND response.
    pending_draft_replacements: HashMap<(usize, u64), DraftSource>,
    /// Only the corresponding SMTP worker response may close the composer.
    pending_send: Option<(usize, u64)>,
    resumed_draft_account: Option<usize>,
}

#[cfg(test)]
mod inbox_poll_tests;
#[cfg(test)]
mod preview_width_tests;
#[cfg(test)]
mod received_attachment_browser_tests;
#[cfg(test)]
mod tests;

// Simulate arbitrary worker completion order, disconnections, and reconnections.
// Keep these tests separate from reader/editor behavior tests.
#[cfg(test)]
#[path = "state/sync_regression_tests.rs"]
mod sync_regression_tests;
