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
mod account_cache;
mod account_sync;
use self::account_sync::{MailboxStatusQueue, StatusReason};
mod attachment_actions;
mod compose_actions;
mod composer;
mod contacts;
mod file_picker;
mod initialization;
mod input_dispatch;
mod mail_actions;
mod mouse;
mod reader_actions;
mod reader_text;
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
const MAX_WARM_MAILBOXES: usize = 6;
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
    warm_folder_order: VecDeque<String>,
    cache_writer: Option<cache::Writer>,
    body_store: Option<cache::BodyStore>,
    raw_store: Option<cache::RawStore>,
    uid_validities: HashMap<String, u32>,
    uid_nexts: HashMap<String, u32>,
    unverified_mailboxes: HashSet<String>,
    pub search_query: String,
    pub search_editing: bool,
    pub search_mode: crate::config::MailSearchMode,
    pub bodies: HashMap<MessageId, MessageBody>,
    pub pending_bodies: HashSet<MessageId>,
    pub pending_seen: HashMap<MessageId, u64>,
    deferred_seen: Vec<cache::DeferredSeen>,
    // Correlates ACKs with the original mailbox, including after navigation.
    inflight_deferred_seen: HashMap<u64, cache::DeferredSeen>,
    status_requests: MailboxStatusQueue,
    pending_inbox_refresh: bool,
    pending_mutation_labels: HashMap<u64, &'static str>,
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
    pub fn cached_mailbox_unverified(&self) -> bool {
        self.current_mailbox
            .as_ref()
            .is_some_and(|name| self.unverified_mailboxes.contains(name))
    }

    /// Only foreground mailbox synchronization is shown as a progress spinner.
    /// Background STATUS polling and IDLE do not keep the spinner running.
    #[must_use]
    pub fn syncing_with_server(&self) -> bool {
        self.connecting
            || self.mailboxes_loading
            || self.messages_loading
            || self.page_inflight.is_some()
    }

    #[must_use]
    pub fn selected_message(&self) -> Option<&MessageSummary> {
        // Unverified cached rows are safe to READ. Only UID-bearing server
        // requests must wait for SELECT to verify UIDVALIDITY.
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
        // Cached rows stay navigable while IMAP reconciliation runs. They are
        // provisional, so all server-facing UID actions check verification.
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
        // Reading offline is safe before SELECT; only UID mutations need a
        // verified server generation. Do not start a network fetch on a hit.
        if let (Some(store), Some(validity)) = (
            self.body_store.as_ref(),
            self.uid_validities.get(&mailbox).copied(),
        ) && let Some(body) = store.read(&mailbox, validity, &id)
        {
            self.bodies.insert(id, body);
            self.trim_preview_bodies();
            crate::perf::record("ui_preview_disk_hit", None, Duration::ZERO, true);
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
                // Disk-cached messages are loaded when selected. Preloading
                // them again over IMAP wastes bandwidth and server latency.
                !self.body_store.as_ref().is_some_and(|store| {
                    self.uid_validities.get(&mailbox).is_some_and(|validity| {
                        store.has_candidate(&mailbox, *validity, &message.id)
                    })
                })
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

    /// Record a read/unread preference immediately; the server is reconciled
    /// later, but only after SELECT confirms UIDVALIDITY and the UID is still
    /// present. The on-disk journal is coalesced and bounded.
    fn request_seen(&mut self, id: MessageId, seen: bool) -> bool {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return false;
        };
        let verified = !self.unverified_mailboxes.contains(&mailbox);
        let validity = self.uid_validities.get(&mailbox).copied();
        if !self.messages.iter().any(|message| message.id == id) {
            return false;
        }
        // Maildir and IMAP accounts without persistent caching retain the
        // original immediate-worker behavior. Never send an unverified UID.
        if validity.is_none() && verified {
            if self.pending_seen.contains_key(&id) {
                return false;
            }
            if let Ok(request_id) = self.worker.post(ActionKind::SetSeen {
                mailbox,
                id: id.clone(),
                seen,
            }) {
                self.pending_seen.insert(id.clone(), request_id);
                self.apply_seen_flag(&id, seen);
                return true;
            }
            return false;
        }
        let Some(validity) = validity else {
            return false;
        };
        let entry = cache::DeferredSeen {
            mailbox,
            validity,
            id: id.clone(),
            seen,
        };
        if !entry.has_valid_identity() {
            return false;
        }
        if let Some(previous) = self
            .deferred_seen
            .iter_mut()
            .find(|previous| previous.mailbox == entry.mailbox && previous.id == entry.id)
        {
            previous.seen = seen;
        } else {
            if self.deferred_seen.len() >= cache::MAX_DEFERRED_SEEN {
                return false;
            }
            self.deferred_seen.push(entry);
        }
        self.apply_seen_flag(&id, seen);
        self.persist_deferred_seen();
        // Offline flag changes are stored in the journal, but the cached
        // mailbox count stays at its last server-derived value. Persisting an
        // optimistic count would double-apply this change on next startup.
        self.persist_current_cache();
        if verified {
            self.persist_mailboxes();
        }
        self.pump_deferred_seen();
        true
    }

    fn persist_deferred_seen(&self) {
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::DeferredSeen(self.deferred_seen.clone()));
        }
    }

    /// Reapply local preferences after a fresh server FLAGS snapshot (or
    /// restoring a cached mailbox). This does not issue network operations.
    fn overlay_deferred_seen(&mut self) {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return;
        };
        let Some(validity) = self.uid_validities.get(&mailbox).copied() else {
            return;
        };
        let updates = self
            .deferred_seen
            .iter()
            .filter(|entry| entry.mailbox == mailbox && entry.validity == validity)
            .map(|entry| (entry.id.clone(), entry.seen))
            .collect::<Vec<_>>();
        for (id, seen) in updates {
            self.apply_seen_flag(&id, seen);
        }
    }

    /// Replay only for a confirmed generation and UIDs present in the loaded
    /// server page. Missing UIDs remain pending for later pages; changed
    /// generations are discarded rather than risking a write to another mail.
    fn pump_deferred_seen(&mut self) {
        let Some(mailbox) = self.current_mailbox.clone() else {
            return;
        };
        if self.unverified_mailboxes.contains(&mailbox) {
            return;
        }
        let Some(validity) = self.uid_validities.get(&mailbox).copied() else {
            return;
        };
        let ready =
            self.deferred_seen
                .iter()
                .filter(|entry| entry.mailbox == mailbox && entry.validity == validity)
                .filter(|entry| self.messages.iter().any(|message| message.id == entry.id))
                .filter(|entry| !self.pending_seen.contains_key(&entry.id))
                .filter(|entry| {
                    !self.inflight_deferred_seen.values().any(|inflight| {
                        inflight.mailbox == entry.mailbox && inflight.id == entry.id
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
        for entry in ready {
            if let Ok(request_id) = self.worker.post(ActionKind::SetSeen {
                mailbox: entry.mailbox.clone(),
                id: entry.id.clone(),
                seen: entry.seen,
            }) {
                self.pending_seen.insert(entry.id.clone(), request_id);
                self.inflight_deferred_seen.insert(request_id, entry);
            } else {
                break;
            }
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
    sync_spinner_frame: usize,
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
    /// Show Git patch colors even inside quoted mailing-list replies.
    pub quoted_diff_highlighting: bool,
    pub show_header_details: bool,
    theme_config_path: Option<PathBuf>,
    pub attachment_picker_visible: bool,
    pub attachment_selection: usize,
    pub attachment_files: Arc<Vec<OutgoingAttachment>>,
    /// TempPath deletes the opened attachment on eviction or App shutdown.
    /// Bound both the number and total size of these exports.
    opened_attachment_files: VecDeque<tempfile::TempPath>,
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
        if account.cached_mailbox_unverified() {
            self.status = StatusLine::info("raw MIME export requires a verified server connection");
            return;
        }
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
    /// Persist a divider adjustment to the active config.toml, including custom
    /// --config paths. When Jerc has no config file, the width remains session-only.
    fn save_panel_width(&mut self, panel: crate::config::PanelWidth) {
        let Some(path) = self.theme_config_path.as_deref() else {
            return;
        };
        let value = match panel {
            crate::config::PanelWidth::Mailbox => self.mailbox_width_percent,
            crate::config::PanelWidth::Preview => self.preview_width_percent,
        };
        if let Err(error) = crate::config::save_ui_panel_width(path, panel, value) {
            self.status = StatusLine::error(format!("could not save panel width: {error:#}"));
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
        let filename = file.filename.clone();
        match export_attachment_for_open(file) {
            Ok(path) => {
                match open_url(&path.to_string_lossy()) {
                    Ok(()) => {
                        // External viewers may open asynchronously, so retain
                        // the file while Jerc runs, not just for this call.
                        self.opened_attachment_files.push_back(path);
                        while self.opened_attachment_files.len() > 4 {
                            self.opened_attachment_files.pop_front();
                        }
                        self.status = StatusLine::temporary(
                            format!("opened {filename} with system viewer"),
                            Duration::from_secs(3),
                        );
                    }
                    Err(error) => {
                        self.status =
                            StatusLine::error(format!("could not open attachment: {error}"));
                    }
                }
            }
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
    if let Some(raw) = composer.draft_raw.as_deref() {
        attachments.extend(crate::mail::extract_attachments(raw)?);
    }
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

/// Explicit Vim cuts (`x`) share one clipboard/register policy across
/// Compose and Contacts. Plain yanks stay in Jerc's in-memory register.
fn record_cut(
    register: &mut Option<YankRegister>,
    text: String,
    linewise: bool,
) -> Option<StatusLine> {
    record_cut_with(register, text, linewise, copy_text_to_clipboard)
}

/// Keep the clipboard attempt injectable so success, failure, and an empty cut
/// can be tested without depending on wl-copy, pbcopy, or a running desktop.
fn record_cut_with(
    register: &mut Option<YankRegister>,
    text: String,
    linewise: bool,
    copy: impl FnOnce(&str) -> std::io::Result<()>,
) -> Option<StatusLine> {
    if text.is_empty() {
        return None;
    }
    *register = Some(YankRegister {
        text: text.clone(),
        linewise,
    });
    Some(match copy(&text) {
        Ok(()) => StatusLine::temporary("cut copied to clipboard", Duration::from_secs(2)),
        Err(_) => StatusLine::temporary(
            "cut to internal register (system clipboard unavailable)",
            Duration::from_secs(3),
        ),
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
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
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

/// Normal exit cleans up TempPath values automatically. After a crash, only
/// old, private, regular files with our prefix are eligible for cleanup.
#[cfg(unix)]
fn cleanup_stale_attachment_files() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::time::SystemTime;

    let Ok(identity) = tempfile::NamedTempFile::new() else {
        return;
    };
    let Ok(owner) = identity.as_file().metadata() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("jerc-attachment-") {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.file_type().is_file()
            && meta.uid() == owner.uid()
            && meta.permissions().mode() & 0o777 == 0o600
            && meta
                .modified()
                .ok()
                .and_then(|time| SystemTime::now().duration_since(time).ok())
                .is_some_and(|age| age >= Duration::from_secs(7 * 24 * 60 * 60))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(not(unix))]
fn cleanup_stale_attachment_files() {}

/// Keep the temporary path alive so external viewers can open it after the
/// TUI immediately regains control. tempfile creates files with mode 0600.
fn export_attachment_for_open(file: &OutgoingAttachment) -> anyhow::Result<tempfile::TempPath> {
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
    Ok(file_out.into_temp_path())
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
    reveal_row(
        current_offset,
        index.min(len - 1),
        height,
        len.saturating_sub(height),
        MESSAGE_SCROLL_OFF,
    )
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
    crate::app::vim::toggle_group(marked, ids);
}

#[cfg(test)]
mod inbox_poll_tests;
#[cfg(test)]
mod preview_width_tests;
#[cfg(test)]
mod received_attachment_browser_tests;
#[cfg(test)]
mod tests;
