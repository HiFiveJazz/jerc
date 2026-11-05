use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind, MouseButton};
use ratatui::layout::Size;
use ratatui_image::{Resize, picker::Picker, protocol::Protocol};

use crate::app::{ATTACHMENT_HELP_ENTRIES, Command, Focus, HELP_ENTRIES, command_for_key};
use crate::setup::{SetupWizard, WizardAction};
use crate::cache::{self, CachedMailbox};
use crate::backend::MailboxSyncHint;
use crate::config::Config;
use crate::mail::{parse_body, render_html_for_terminal};
use crate::outgoing::{ReplyAudience, reply_recipients, reply_subject, forward_subject, inline_forward_parts, forward_html, forward_attribution, FORWARD_SEPARATOR};
use crate::mail_image::{MailImageEvent, MailImageLoader};
use crate::model::{
    Capabilities, HtmlLine, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId, MessageSummary,
    OutgoingAttachment, OutgoingMessage,
};
use crate::theme::{Theme, load_themes, theme_index};
use crate::worker::{ActionKind, RuntimeEvent, WorkerEvent, WorkerEventKind, WorkerHandle};

const SMOOTH_SCROLL_ROWS: isize = 22;
const SMOOTH_MAILBOX_ROWS: isize = 5;
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
        body.plain_text.as_ref(), body.plain_fallback.as_ref(),
        body.html_source.as_ref(), body.html_text.as_ref(),
    ].into_iter().flatten().map(String::len).sum::<usize>();
    let rich = body.html_lines.as_ref().map_or(0, |lines| lines.iter().flat_map(|line| &line.spans)
        .map(|span| span.text.len()
            + span.style.link.as_ref().map_or(0, String::len)
            + span.style.image.as_ref().map_or(0, String::len))
        .sum::<usize>());
    text.saturating_add(rich)
}

fn preview_neighbor_positions(position: usize, count: usize, direction: isize, fast: bool) -> Vec<usize> {
    if position >= count { return Vec::new(); }
    let (forward, backward) = if fast {
        (BODY_PREFETCH_FAST_FORWARD, BODY_PREFETCH_FAST_BACKWARD)
    } else {
        (BODY_PREFETCH_FORWARD, BODY_PREFETCH_BACKWARD)
    };
    let (ahead, behind) = if direction < 0 { (backward, forward) } else { (forward, backward) };
    let after = (1..=ahead).filter_map(|distance| position.checked_add(distance))
        .filter(|index| *index < count);
    let before = (1..=behind).filter_map(|distance| position.checked_sub(distance));
    if direction < 0 { before.chain(after).collect() }
    else { after.chain(before).collect() }
}

/// Put imminent rows first, but also schedule the destination early enough
/// that a long neoscroll has a chance to arrive on a warm body. Never preload
/// unrelated rows between a far-away destination and the current viewport.
fn preview_travel_positions(position: usize, destination: usize, count: usize) -> Vec<usize> {
    if position >= count || destination >= count { return Vec::new(); }
    let direction = if destination < position { -1 } else { 1 };
    let mut positions = vec![position];
    for distance in 1..=3 {
        let next = if direction < 0 { position.checked_sub(distance) }
            else { position.checked_add(distance).filter(|p| *p < count) };
        if let Some(next) = next { positions.push(next); }
    }
    positions.push(destination);
    let distance = position.abs_diff(destination);
    if distance > 4 {
        // Also warm the row just before arrival, and the next row if the
        // animation continues after another Ctrl-j/k press.
        let before = if direction < 0 { destination.checked_add(1) }
            else { destination.checked_sub(1) };
        let after = if direction < 0 { destination.checked_sub(1) }
            else { destination.checked_add(1) };
        positions.extend(before.into_iter().chain(after).filter(|p| *p < count));
    }
    positions.extend(preview_neighbor_positions(destination, count, direction, true));
    let mut seen = HashSet::new();
    positions.into_iter().filter(|p| seen.insert(*p)).collect()
}

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
    pdf_preview_results: Arc<Mutex<VecDeque<(PathBuf, Result<Vec<u8>, String>)>>>,
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
        if !show_hidden && name.starts_with('.') { continue; }
        let path = item.path();
        let is_directory = path.is_dir();
        if is_directory || path.is_file() {
            entries.push(ComposeFileEntry { path, name, is_directory });
        }
    }
    entries.sort_by(|a, b| {
        b.is_directory.cmp(&a.is_directory)
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
        .args(["-f", "1", "-l", "1", "-singlefile", "-scale-to", "850", "-jpeg",
            "-jpegopt", "quality=70"])
        .arg(path).arg(&output)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        .map_err(|error| anyhow::anyhow!("PDF preview requires Poppler's pdftoppm: {error}"))?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() { anyhow::bail!("PDF preview could not render page one"); }
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
    if metadata.len() > 16 * 1024 * 1024 { anyhow::bail!("PDF thumbnail too large"); }
    Ok(std::fs::read(rendered)?)
}

fn compose_image_extension(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str())
        .is_some_and(|extension| ["png", "jpg", "jpeg", "gif", "webp"]
            .iter().any(|supported| extension.eq_ignore_ascii_case(supported)))
}

fn validate_picker_attachment(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
        anyhow::bail!("attachment must be a file of at most 16 MiB: {}", path.display());
    }
    Ok(())
}

impl ComposeFilePicker {
    fn open(directory: PathBuf) -> anyhow::Result<Self> {
        Self::open_with_mode(directory, false)
    }

    fn open_directory_only(directory: PathBuf) -> anyhow::Result<Self> {
        Self::open_with_mode(directory, true)
    }

    fn open_with_mode(directory: PathBuf, directory_only: bool) -> anyhow::Result<Self> {
        let mut picker = Self {
            directory,
            entries: Vec::new(),
            selected: 0,
            show_hidden: false,
            directory_only,
            parent_entries: Vec::new(),
            parent_selected: 0,
            preview: ComposeFilePreview::Empty,
            pending_count: 0,
            pending_g: false,
            smooth_target: None,
            visual_anchor: None,
            help_visible: false,
            help_query: String::new(),
            help_selected: 0,
            help_scroll: 0,
            search_query: None,
            pdf_preview_due: None,
            pdf_preview_inflight: HashSet::new(),
            pdf_preview_results: Arc::new(Mutex::new(VecDeque::new())),
            pdf_preview_cache: VecDeque::new(),
        };
        picker.refresh()?;
        Ok(picker)
    }

    fn refresh(&mut self) -> anyhow::Result<()> {
        self.smooth_target = None;
        self.visual_anchor = None;
        self.entries = compose_read_dir(&self.directory, self.show_hidden)?;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.parent_entries.clear();
        self.parent_selected = 0;
        if let Some(parent) = self.directory.parent() {
            self.parent_entries = compose_read_dir(parent, self.show_hidden).unwrap_or_default();
            if let Some(index) = self.parent_entries.iter()
                .position(|entry| entry.path == self.directory) {
                self.parent_selected = index;
            }
        }
        self.refresh_preview();
        Ok(())
    }

    fn refresh_preview(&mut self) {
        // Only the *current* PDF may be scheduled. An in-flight conversion from
        // a previous row can finish in the background but cannot replace this preview.
        self.pdf_preview_due = None;
        if self.directory_only {
            // Unlike the compose picker, saving only needs directory contents.
            // Never block navigation on decoding images or reading file bodies.
            self.preview = match self.entries.get(self.selected) {
                None => ComposeFilePreview::Empty,
                Some(entry) if entry.is_directory => match compose_read_dir(&entry.path, self.show_hidden) {
                    Ok(entries) => ComposeFilePreview::Directory(entries),
                    Err(error) => ComposeFilePreview::Error(error.to_string()),
                },
                Some(entry) => match std::fs::metadata(&entry.path) {
                    Ok(metadata) => ComposeFilePreview::Binary(metadata.len()),
                    Err(error) => ComposeFilePreview::Error(error.to_string()),
                },
            };
            return;
        }
        self.preview = match self.entries.get(self.selected) {
            None => ComposeFilePreview::Empty,
            Some(entry) if entry.is_directory => match compose_read_dir(&entry.path, self.show_hidden) {
                Ok(entries) => ComposeFilePreview::Directory(entries),
                Err(error) => ComposeFilePreview::Error(error.to_string()),
            },
            Some(entry) if entry.path.extension().and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf")) => {
                if let Some((_, cached)) = self.pdf_preview_cache.iter()
                    .find(|(path, _)| path == &entry.path) {
                    match cached {
                        Ok(bytes) => ComposeFilePreview::Image(bytes.clone()),
                        Err(error) => ComposeFilePreview::Error(error.clone()),
                    }
                } else {
                    if !self.pdf_preview_inflight.contains(&entry.path) {
                        self.pdf_preview_due = Some((entry.path.clone(), Instant::now() + PDF_PREVIEW_DEBOUNCE));
                    }
                    ComposeFilePreview::Loading("Rendering PDF preview…")
                }
            }
            Some(entry) if compose_image_extension(&entry.path) => {
                match std::fs::metadata(&entry.path) {
                    Ok(metadata) if metadata.len() <= 16 * 1024 * 1024 => {
                        match std::fs::read(&entry.path) {
                            Ok(bytes) => ComposeFilePreview::Image(bytes),
                            Err(error) => ComposeFilePreview::Error(error.to_string()),
                        }
                    }
                    Ok(metadata) => ComposeFilePreview::Binary(metadata.len()),
                    Err(error) => ComposeFilePreview::Error(error.to_string()),
                }
            }
            Some(entry) => match std::fs::File::open(&entry.path) {
                Err(error) => ComposeFilePreview::Error(error.to_string()),
                Ok(mut file) => match file.metadata() {
                    Err(error) => ComposeFilePreview::Error(error.to_string()),
                    Ok(metadata) => {
                        // No unbounded reads while traversing large binary attachments.
                        let mut bytes = Vec::new();
                        match std::io::Read::by_ref(&mut file).take(24 * 1024).read_to_end(&mut bytes) {
                            Err(error) => ComposeFilePreview::Error(error.to_string()),
                            Ok(_) => match String::from_utf8(bytes) {
                                Ok(text) if !text.contains('\0') => {
                                    let truncated = metadata.len() > 24 * 1024 || text.lines().count() > 180;
                                    if entry.path.extension().and_then(|ext| ext.to_str())
                                        .is_some_and(|ext| ext.eq_ignore_ascii_case("patch") || ext.eq_ignore_ascii_case("diff")) {
                                        ComposeFilePreview::Patch(crate::patch_preview::highlight_patch(&text, truncated))
                                    } else {
                                        ComposeFilePreview::Text { lines: text.lines().take(180).map(str::to_owned).collect(), truncated }
                                    }
                                }
                                _ => ComposeFilePreview::Binary(metadata.len()),
                            },
                        }
                    }
                },
            },
        };
    }

    /// Poll completed previews without waiting, and launch only the most
    /// recently selected PDF once the selection has been stable for 140 ms.
    /// Yazi similarly preloads previews off the UI thread, avoiding navigation
    /// stalls on external converters.
    fn advance_pdf_preview(&mut self) {
        // Drain a completed result batch under one brief lock. The mutex never
        // covers the expensive Poppler process or image decoding.
        let completed = if let Ok(mut results) = self.pdf_preview_results.lock() {
            results.drain(..).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut refresh = false;
        for (path, result) in completed {
            self.pdf_preview_inflight.remove(&path);
            self.pdf_preview_cache.retain(|(cached, _)| cached != &path);
            self.pdf_preview_cache.push_back((path.clone(), result));
            while self.pdf_preview_cache.len() > PDF_PREVIEW_CACHE_SIZE {
                self.pdf_preview_cache.pop_front();
            }
            refresh |= self.smooth_target.is_none()
                && self.entries.get(self.selected).is_some_and(|entry| entry.path == path);
        }
        if refresh { self.refresh_preview(); }

        let ready = self.pdf_preview_due.as_ref()
            .is_some_and(|(_, due)| *due <= Instant::now());
        if !ready || self.pdf_preview_inflight.len() >= PDF_PREVIEW_CONCURRENCY { return; }
        let Some((path, _)) = self.pdf_preview_due.take() else { return; };
        if self.pdf_preview_inflight.contains(&path) { return; }
        let results = Arc::clone(&self.pdf_preview_results);
        let worker_path = path.clone();
        match std::thread::Builder::new().name("aerc-pdf-preview".into()).spawn(move || {
            let result = compose_pdf_thumbnail(&worker_path).map_err(|error| error.to_string());
            if let Ok(mut queue) = results.lock() {
                queue.push_back((worker_path, result));
            }
        }) {
            Ok(_handle) => { self.pdf_preview_inflight.insert(path); }
            Err(error) => { self.preview = ComposeFilePreview::Error(format!("PDF preview worker: {error}")); }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        self.smooth_target = None;
        self.selected = self.selected.saturating_add_signed(delta)
            .min(self.entries.len().saturating_sub(1));
        self.refresh_preview();
    }

    fn queue_smooth_scroll(&mut self, delta: isize) {
        // Queue relative to the *visible* cursor so opposite-direction keys
        // reverse immediately instead of inheriting an unfinished target.
        let target = self.selected.saturating_add_signed(delta)
            .min(self.entries.len().saturating_sub(1));
        self.smooth_target = (target != self.selected).then_some(target);
        // A new scroll invalidates a pending preview debounce; schedule only
        // the final destination once the animation has settled.
        if self.smooth_target.is_some() { self.pdf_preview_due = None; }
    }

    fn advance_smooth_scroll(&mut self) {
        let Some(target) = self.smooth_target else { return; };
        if target == self.selected { self.smooth_target = None; return; }
        self.selected = self.selected.saturating_add_signed(if target > self.selected { 1 } else { -1 });
        // Rebuilding a preview for every *intermediate* animation row makes
        // Ctrl-j/k do filesystem I/O and image rasterization for files the user
        // never stopped on. Keep the old preview while moving and refresh once.
        if target == self.selected {
            self.smooth_target = None;
            self.refresh_preview();
        }
    }

    pub fn help_matches(&self) -> Vec<usize> {
        let query = self.help_query.trim().to_lowercase();
        crate::app::PICKER_HELP_ENTRIES.iter().enumerate()
            .filter(|(_, entry)| query.is_empty() || format!("{} {} {}", entry.category, entry.key, entry.description)
                .to_lowercase().contains(&query))
            .map(|(index, _)| index).collect()
    }

    /// Immutable match indices for the renderer's current-search counter.
    pub fn search_matches_for_display(&self) -> Vec<usize> { self.search_matches() }

    fn search_matches(&self) -> Vec<usize> {
        let query = self.search_query.as_deref().unwrap_or("").to_lowercase();
        let matches: Vec<usize> = self.entries.iter().enumerate()
            .filter(|(_, entry)| entry.name.to_lowercase().contains(&query))
            .map(|(index, _)| index).collect();
        // A directory jump is the primary use case. Fall back to files when
        // no folder matches, so slash remains useful for finding attachments.
        let directories: Vec<usize> = matches.iter().copied()
            .filter(|&index| self.entries[index].is_directory).collect();
        if directories.is_empty() { matches } else { directories }
    }

    fn update_search_selection(&mut self) {
        self.smooth_target = None;
        if let Some(index) = self.search_matches().first().copied() {
            self.selected = index;
            self.refresh_preview();
        }
    }

    fn next_search_match(&mut self, backwards: bool) {
        self.smooth_target = None;
        let matches = self.search_matches();
        if matches.is_empty() { return; }
        let next = matches.iter().position(|&index| index == self.selected)
            .map_or(0, |position| if backwards {
                (position + matches.len() - 1) % matches.len()
            } else { (position + 1) % matches.len() });
        self.selected = matches[next];
        self.refresh_preview();
    }

    fn enter_directory(&mut self, directory: PathBuf) -> anyhow::Result<()> {
        let previous = self.directory.clone();
        self.search_query = None;
        self.directory = directory;
        self.selected = 0;
        if let Err(error) = self.refresh() {
            self.directory = previous;
            self.refresh()?;
            return Err(error);
        }
        if self.directory == previous.parent().unwrap_or(Path::new("")) {
            if let Some(index) = self.entries.iter().position(|entry| entry.path == previous) {
                self.selected = index;
                self.refresh_preview();
            }
        }
        Ok(())
    }

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
    pub help_query: String,
    #[serde(skip)]
    pub help_selected: usize,
    #[serde(skip)]
    pub help_scroll: usize,
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

impl Composer {
    pub fn new() -> Self {
        Self {
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            attachments: String::new(),
            attachment_paths: Vec::new(),
            headers: String::new(),
            file_picker: None,
            attachment_manager_open: false,
            attachment_manager_selected: 0,
            removed_attachments: Vec::new(),
            advanced_headers_open: false,
            advanced_previous: None,
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
            confirm_send: false, exit_prompt: false,
            preview_html: true, preview_scroll: 0, preview_rich_lines: None,
            preview_forwarded_attachments: Vec::new(),
            help_visible: false, help_query: String::new(), help_selected: 0, help_scroll: 0,
            visual_linewise: false, pending_g: false, pending_delete: false,
            pending_inner: false, pending_find: None, count: None,
            undo_history: Vec::new(), redo_history: Vec::new(),
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
            attachment_paths: Vec::new(),
            headers: String::new(),
            file_picker: None,
            attachment_manager_open: false,
            attachment_manager_selected: 0,
            removed_attachments: Vec::new(),
            advanced_headers_open: false,
            advanced_previous: None,
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
            confirm_send: false, exit_prompt: false,
            preview_html: true, preview_scroll: 0, preview_rich_lines: None,
            preview_forwarded_attachments: Vec::new(),
            help_visible: false, help_query: String::new(), help_selected: 0, help_scroll: 0,
            visual_linewise: false, pending_g: false, pending_delete: false,
            pending_inner: false, pending_find: None, count: None,
            undo_history: Vec::new(), redo_history: Vec::new(),
        }
    }

    fn forward(message: &MessageSummary, raw: Vec<u8>) -> anyhow::Result<Self> {
        let parts = inline_forward_parts(&raw)?;
        let subject = forward_subject(&message.subject);
        let attribution = forward_attribution(&raw)?;
        let body = format!("\n\n{attribution}\n{}\n", parts.plain);
        Ok(Self {
            to: String::new(), cc: String::new(), bcc: String::new(),
            attachments: String::new(), attachment_paths: Vec::new(), headers: String::new(),
            file_picker: None, attachment_manager_open: false, attachment_manager_selected: 0,
            removed_attachments: Vec::new(),
            advanced_headers_open: false, advanced_previous: None,
            subject, body,
            field: ComposeField::To, mode: ComposeMode::Insert, cursor: 0,
            visual_anchor: None, in_reply_to: None, references: Vec::new(),
            forwarded_raw: Some(raw), sending: false, confirm_send: false, exit_prompt: false,
            preview_html: true, preview_scroll: 0, preview_rich_lines: None,
            preview_forwarded_attachments: Vec::new(),
            help_visible: false, help_query: String::new(), help_selected: 0, help_scroll: 0,
            visual_linewise: false, pending_g: false, pending_delete: false,
            pending_inner: false, pending_find: None, count: None,
            undo_history: Vec::new(), redo_history: Vec::new(),
        })
    }

    fn prepare_send_preview(&mut self) -> anyhow::Result<()> {
        crate::outgoing::validate_recipients(&self.to, &self.cc, &self.bcc)?;
        // Only parse/reflow rich mail and attachment metadata upon opening the
        // preview, never while the terminal draws successive frames.
        self.preview_forwarded_attachments = self.forwarded_raw.as_deref()
            .and_then(|raw| crate::mail::extract_attachments(raw).ok())
            .map(|parts| parts.into_iter().map(|part| part.filename).collect())
            .unwrap_or_default();
        self.preview_rich_lines = self.outgoing_html_preview()?
            .map(|html| render_html_for_terminal(&html, 90).1);
        self.preview_html = true;
        self.preview_scroll = 0;
        self.confirm_send = true;
        Ok(())
    }

    /// The HTML actually used for inline forwards, if present. Normal plain
    /// messages have no HTML alternative, so T correctly shows the same text.
    pub fn outgoing_html_preview(&self) -> anyhow::Result<Option<String>> {
        let Some(raw) = self.forwarded_raw.as_deref() else { return Ok(None); };
        let parts = inline_forward_parts(raw)?;
        Ok(parts.html.map(|html| {
            let personal_note = self.body.split(FORWARD_SEPARATOR).next().unwrap_or("");
            let original_headers = self.body.split(FORWARD_SEPARATOR).nth(1)
                .and_then(|tail| tail.split("\n\n").next()).unwrap_or("");
            let attribution = format!("{FORWARD_SEPARATOR}{original_headers}");
            forward_html(&html, &attribution, personal_note)
        }))
    }

    /// Unsaved mail must never disappear on a stray `q` or `Esc`.
    /// A reply or forward with original material is also worth protecting.
    pub fn has_unsent_content(&self) -> bool {
        [&self.to, &self.cc, &self.bcc, &self.subject, &self.body, &self.headers,
         &self.attachments].iter().any(|value| !value.trim().is_empty())
            || !self.attachment_paths.is_empty()
            || self.forwarded_raw.is_some()
            || self.in_reply_to.is_some()
    }

    pub fn attachment_count(&self) -> usize {
        self.attachment_paths.len()
            + self.attachments.split(';').filter(|part| !part.trim().is_empty()).count()
    }

    fn remove_managed_attachment(&mut self) -> Option<PathBuf> {
        let selected = self.attachment_manager_selected;
        let (removed, index, legacy) = if selected < self.attachment_paths.len() {
            (self.attachment_paths.remove(selected), selected, false)
        } else {
            let index = selected - self.attachment_paths.len();
            let mut paths: Vec<String> = self.attachments.split(';').map(str::trim)
                .filter(|path| !path.is_empty()).map(str::to_owned).collect();
            if index >= paths.len() { return None; }
            let removed = PathBuf::from(paths.remove(index));
            self.attachments = paths.join(";");
            (removed, index, true)
        };
        self.removed_attachments.push(RemovedAttachment {
            index, path: removed.clone(), legacy,
        });
        self.attachment_manager_selected = selected.min(self.attachment_count().saturating_sub(1));
        Some(removed)
    }

    fn undo_managed_attachment(&mut self) -> Option<PathBuf> {
        let entry = self.removed_attachments.pop()?;
        if entry.legacy {
            let mut paths: Vec<String> = self.attachments.split(';').map(str::trim)
                .filter(|path| !path.is_empty()).map(str::to_owned).collect();
            let index = entry.index.min(paths.len());
            paths.insert(index, entry.path.to_string_lossy().into_owned());
            self.attachments = paths.join(";");
            self.attachment_manager_selected = self.attachment_paths.len() + index;
        } else {
            let index = entry.index.min(self.attachment_paths.len());
            self.attachment_paths.insert(index, entry.path.clone());
            self.attachment_manager_selected = index;
        }
        Some(entry.path)
    }

    fn toggle_advanced_headers(&mut self) {
        if let Some((field, mode, cursor)) = self.advanced_previous.take() {
            self.advanced_headers_open = false;
            self.field = field;
            self.mode = mode;
            self.cursor = cursor;
            self.clear_pending();
        } else {
            let prior_mode = if self.mode == ComposeMode::Visual { ComposeMode::Normal } else { self.mode };
            self.advanced_previous = Some((self.field, prior_mode, self.cursor));
            self.advanced_headers_open = true;
            self.switch_field(ComposeField::Headers);
            self.mode = ComposeMode::Insert;
        }
    }

    fn open_file_picker(&mut self) -> anyhow::Result<()> {
        let directory = std::env::current_dir().or_else(|_| {
            dirs::home_dir().ok_or_else(|| std::io::Error::other("no home directory"))
        })?;
        self.file_picker = Some(ComposeFilePicker::open(directory)?);
        Ok(())
    }

    /// Return a short message to display in the status bar, if applicable.
    fn file_picker_key(&mut self, key: KeyCode) -> anyhow::Result<Option<String>> {
        let Some(picker) = self.file_picker.as_mut() else { return Ok(None); };
        if picker.help_visible {
            match key {
                KeyCode::Esc | KeyCode::Char('?') if picker.help_query.is_empty() => picker.help_visible = false,
                KeyCode::Esc => picker.help_visible = false,
                KeyCode::Backspace => { picker.help_query.pop(); picker.help_selected = *picker.help_matches().first().unwrap_or(&0); picker.help_scroll = 0; }
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') if picker.help_query.is_empty() => {
                    let matches = picker.help_matches();
                    if let Some(index) = matches.iter().position(|&i| i == picker.help_selected) {
                        let index = if matches!(key, KeyCode::Up | KeyCode::Char('k')) { index.saturating_sub(1) } else { (index + 1).min(matches.len().saturating_sub(1)) };
                        picker.help_selected = matches[index];
                        picker.help_scroll = index.saturating_sub(5);
                    }
                }
                KeyCode::Char(ch) => { picker.help_query.push(ch); picker.help_selected = *picker.help_matches().first().unwrap_or(&0); picker.help_scroll = 0; }
                _ => {}
            }
            return Ok(None);
        }
        if picker.search_query.is_some() {
            match key {
                KeyCode::Esc => picker.search_query = None,
                // Accept the highlighted match and return to normal navigation.
                // Down/Up still cycle suggestions while the search is active.
                KeyCode::Enter => picker.search_query = None,
                KeyCode::Down => picker.next_search_match(false),
                KeyCode::Up => picker.next_search_match(true),
                KeyCode::Backspace => {
                    if let Some(query) = picker.search_query.as_mut() { query.pop(); }
                    picker.update_search_selection();
                }
                KeyCode::Tab | KeyCode::Right => {
                    if let Some(entry) = picker.entries.get(picker.selected) {
                        if entry.is_directory { picker.enter_directory(entry.path.clone())?; }
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(query) = picker.search_query.as_mut() { query.push(ch); }
                    picker.update_search_selection();
                }
                _ => {}
            }
            return Ok(None);
        }
        if key == KeyCode::Char('/') {
            picker.smooth_target = None;
            picker.search_query = Some(String::new());
            return Ok(None);
        }
        if key == KeyCode::Char('?') {
            picker.help_visible = true;
            picker.help_query.clear();
            picker.help_selected = 0;
            picker.help_scroll = 0;
            return Ok(None);
        }
        if matches!(key, KeyCode::Esc | KeyCode::Char('q')) {
            if picker.visual_anchor.take().is_none() { self.file_picker = None; }
            return Ok(None);
        }
        if matches!(key, KeyCode::Char('v' | 'V')) {
            if picker.visual_anchor.is_some() { picker.visual_anchor = None; }
            else { picker.visual_anchor = Some(picker.selected); }
            picker.pending_count = 0;
            return Ok(None);
        }
        if picker.pending_g {
            picker.pending_g = false;
            if key == KeyCode::Char('g') {
                picker.selected = 0;
                picker.refresh_preview();
                picker.pending_count = 0;
                return Ok(None);
            }
        }
        match key {
            KeyCode::Char(digit @ '1'..='9') => {
                picker.pending_count = picker.pending_count.saturating_mul(10)
                    .saturating_add(digit.to_digit(10).unwrap_or(1) as usize).min(99_999);
                return Ok(None);
            }
            KeyCode::Char('0') if picker.pending_count > 0 => {
                picker.pending_count = picker.pending_count.saturating_mul(10).min(99_999);
                return Ok(None);
            }
            KeyCode::Char('g') => { picker.pending_g = true; return Ok(None); }
            _ => {}
        }
        let count = picker.pending_count.max(1);
        picker.pending_count = 0;
        if key == KeyCode::Char('x') {
            let (start, end) = picker.visual_anchor
                .map(|anchor| (anchor.min(picker.selected), anchor.max(picker.selected)))
                .unwrap_or((picker.selected, picker.selected));
            let paths: Vec<PathBuf> = picker.entries.iter().skip(start)
                .take(end - start + 1).filter(|entry| !entry.is_directory)
                .map(|entry| entry.path.clone()).collect();
            let before = self.attachment_paths.len();
            self.attachment_paths.retain(|path| !paths.contains(path));
            let removed = before - self.attachment_paths.len();
            picker.visual_anchor = None;
            return Ok(Some(format!("removed {removed} attached file(s)")));
        }
        match key {
            KeyCode::Up | KeyCode::Char('k') => picker.move_selection(-(count as isize)),
            KeyCode::Down | KeyCode::Char('j') => picker.move_selection(count as isize),
            KeyCode::PageUp => picker.move_selection(-10),
            KeyCode::PageDown => picker.move_selection(10),
            KeyCode::Home => { picker.selected = 0; picker.refresh_preview(); }
            KeyCode::End | KeyCode::Char('G') => {
                picker.selected = picker.entries.len().saturating_sub(1);
                picker.refresh_preview();
            }
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                if let Some(parent) = picker.directory.parent() {
                    picker.enter_directory(parent.to_path_buf())?;
                }
            }
            KeyCode::Char('~') => {
                if let Some(home) = dirs::home_dir() { picker.enter_directory(home)?; }
            }
            KeyCode::Char('d') => {
                if let Some(home) = dirs::home_dir() {
                    let downloads = home.join("Downloads");
                    if downloads.is_dir() { picker.enter_directory(downloads)?; }
                }
            }
            KeyCode::Char('.') => {
                picker.show_hidden = !picker.show_hidden;
                picker.refresh()?;
            }
            KeyCode::Char('l') | KeyCode::Right if picker.visual_anchor.is_none() => {
                if let Some(entry) = picker.entries.get(picker.selected) {
                    if entry.is_directory { picker.enter_directory(entry.path.clone())?; }
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(anchor) = picker.visual_anchor {
                    let start = anchor.min(picker.selected);
                    let end = anchor.max(picker.selected);
                    // Directories are navigation-only, never attachment targets.
                    let paths: Vec<_> = picker.entries.iter().skip(start).take(end - start + 1)
                        .filter(|entry| !entry.is_directory).map(|entry| entry.path.clone()).collect();
                    if paths.is_empty() {
                        picker.visual_anchor = None;
                        return Ok(Some("no files in visual selection".to_owned()));
                    }
                    // Space over an all-attached range detaches it. A mixed
                    // range attaches only missing files, without disturbing
                    // the files that are already attached.
                    if key == KeyCode::Char(' ') && paths.iter().all(|path| self.attachment_paths.contains(path)) {
                        self.attachment_paths.retain(|path| !paths.contains(path));
                        picker.visual_anchor = None;
                        return Ok(Some(format!("detached {} files from visual selection", paths.len())));
                    }
                    // Validate *before* any mutation so one invalid file
                    // cannot leave a partially attached selection.
                    for path in &paths {
                        if !self.attachment_paths.contains(path) { validate_picker_attachment(path)?; }
                    }
                    let mut attached = 0;
                    for path in paths {
                        if !self.attachment_paths.contains(&path) {
                            self.attachment_paths.push(path);
                            attached += 1;
                        }
                    }
                    picker.visual_anchor = None;
                    return Ok(Some(format!("attached {attached} files from visual selection")));
                }
                if let Some(entry) = picker.entries.get(picker.selected).cloned() {
                    if entry.is_directory {
                        if key == KeyCode::Enter {
                            picker.enter_directory(entry.path)?;
                        } else {
                            let last = picker.entries.len().saturating_sub(1);
                            if picker.selected < last { picker.move_selection(1); }
                        }
                    } else {
                        validate_picker_attachment(&entry.path)?;
                        if let Some(index) = self.attachment_paths.iter().position(|existing| existing == &entry.path) {
                            self.attachment_paths.remove(index);
                        } else {
                            self.attachment_paths.push(entry.path);
                        }
                        // Space is rapid multi-selection, Enter only toggles.
                        if key == KeyCode::Char(' ') {
                            let last = picker.entries.len().saturating_sub(1);
                            if picker.selected < last { picker.move_selection(1); }
                        }
                        return Ok(Some(format!("updated attachments: {}", self.attachment_paths.len())));
                    }
                }
            }
            _ => {}
        }
        Ok(None)
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
        self.visual_linewise = false;
        self.clear_pending();
    }

    /// Normal-mode j/k on headers navigate *fields*, not characters/lines.
    /// Never leave the body using j/k: once there, motions are Vim motions.
    fn move_header_field(&mut self, down: bool, count: usize) -> bool {
        if self.field == ComposeField::Body || self.mode == ComposeMode::Insert { return false; }
        for _ in 0..count.min(5) {
            let next = if down { self.field.next() } else { self.field.previous() };
            if !down && next == ComposeField::Body { break; }
            self.switch_field(next);
            if self.field == ComposeField::Body { break; }
        }
        true
    }

    fn clear_pending(&mut self) {
        self.pending_g = false;
        self.pending_delete = false;
        self.pending_inner = false;
        self.pending_find = None;
        self.count = None;
    }

    fn remember_edit(&mut self) {
        if self.undo_history.len() >= 200 { self.undo_history.remove(0); }
        self.undo_history.push((self.field, self.field_value().to_owned(), self.cursor));
        self.redo_history.clear();
    }

    fn undo_edit(&mut self, redo: bool) {
        let previous = if redo { self.redo_history.pop() } else { self.undo_history.pop() };
        let Some((field, text, cursor)) = previous else { return; };
        let current = (self.field, self.field_value().to_owned(), self.cursor);
        if redo { self.undo_history.push(current); } else { self.redo_history.push(current); }
        self.field = field;
        *self.field_mut() = text;
        self.cursor = cursor.min(self.char_count());
        self.visual_anchor = None;
        self.visual_linewise = false;
    }

    fn byte_index(text: &str, char_index: usize) -> usize {
        text.char_indices()
            .nth(char_index)
            .map_or(text.len(), |(index, _)| index)
    }

    fn insert_char(&mut self, ch: char) {
        let cursor = self.cursor.min(self.char_count());
        let byte = Self::byte_index(self.field_value(), cursor);
        self.remember_edit();
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
        self.remember_edit();
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
        self.remember_edit();
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
        self.remember_edit();
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

    fn move_word_kind(&mut self, backwards: bool, whole: bool) {
        let chars: Vec<char> = self.field_value().chars().collect();
        let belongs = |ch: char| if whole { !ch.is_whitespace() } else { ch.is_alphanumeric() || ch == '_' };
        if backwards {
            let mut i = self.cursor.min(chars.len());
            while i > 0 && !belongs(chars[i - 1]) { i -= 1; }
            while i > 0 && belongs(chars[i - 1]) { i -= 1; }
            self.cursor = i;
        } else {
            let mut i = self.cursor.min(chars.len());
            while i < chars.len() && belongs(chars[i]) { i += 1; }
            while i < chars.len() && !belongs(chars[i]) { i += 1; }
            self.cursor = i;
        }
    }

    fn move_word_end(&mut self, whole: bool) {
        let chars: Vec<char> = self.field_value().chars().collect();
        if chars.is_empty() { self.cursor = 0; return; }
        let belongs = |ch: char| if whole { !ch.is_whitespace() } else { ch.is_alphanumeric() || ch == '_' };
        let mut i = self.cursor.min(chars.len().saturating_sub(1));
        if belongs(chars[i]) { i = (i + 1).min(chars.len().saturating_sub(1)); }
        while i < chars.len().saturating_sub(1) && !belongs(chars[i]) { i += 1; }
        while i < chars.len().saturating_sub(1) && belongs(chars[i + 1]) { i += 1; }
        self.cursor = i;
    }

    fn first_nonblank(&self) -> usize {
        let chars: Vec<char> = self.field_value().chars().collect();
        let mut i = self.line_start();
        while i < chars.len() && chars[i] != '\n' && matches!(chars[i], ' ' | '\t') { i += 1; }
        i
    }

    fn move_paragraph(&mut self, forward: bool) {
        if self.field != ComposeField::Body { return; }
        let chars: Vec<char> = self.body.chars().collect();
        let mut i = self.cursor.min(chars.len());
        if forward {
            while i < chars.len() {
                if chars[i] == '\n' && chars.get(i + 1) == Some(&'\n') {
                    self.cursor = (i + 2).min(chars.len());
                    return;
                }
                i += 1;
            }
            self.cursor = chars.len();
        } else {
            i = i.saturating_sub(1);
            while i > 0 {
                if chars[i - 1] == '\n' && chars[i] == '\n' {
                    self.cursor = i + 1;
                    return;
                }
                i -= 1;
            }
            self.cursor = 0;
        }
    }

    fn delete_current_lines(&mut self, count: usize) {
        if self.field != ComposeField::Body {
            self.delete_range(0, self.char_count());
            return;
        }
        let start = self.line_start();
        let chars: Vec<char> = self.body.chars().collect();
        let mut end = start;
        for _ in 0..count {
            while end < chars.len() && chars[end] != '\n' { end += 1; }
            if end < chars.len() { end += 1; }
        }
        // Last line without newline still counts as a line.
        if start == end && start > 0 { self.delete_range(start - 1, end); }
        else { self.delete_range(start, end); }
        self.cursor = start.min(self.char_count());
    }

    fn delete_words(&mut self, count: usize) {
        let start = self.cursor;
        for _ in 0..count { self.move_word_kind(false, false); }
        self.delete_range(start, self.cursor);
        self.cursor = start.min(self.char_count());
    }

    fn find_char(&mut self, ch: char, forward: bool, count: usize) {
        let chars: Vec<char> = self.field_value().chars().collect();
        let mut position = self.cursor.min(chars.len());
        let start = self.line_start();
        let end = self.line_end();
        for _ in 0..count {
            let match_index = if forward {
                (position.saturating_add(1)..end).find(|&index| chars[index] == ch)
            } else {
                (start..position).rev().find(|&index| chars[index] == ch)
            };
            let Some(index) = match_index else { break };
            position = index;
        }
        self.cursor = position;
    }

    fn select_inner_object(&mut self, delimiter: char) {
        // Reuse the reader's tested matching algorithm: it understands nested
        // delimiters and paired quotes, unlike a first-open/first-close scan.
        if let Some((start, end)) = inner_text_object_range(
            self.field_value(), self.cursor, delimiter,
        ) {
            self.visual_anchor = Some(start);
            self.cursor = end;
            self.visual_linewise = false;
            self.mode = ComposeMode::Visual;
        }
    }

    fn apply_motion(&mut self, code: KeyCode, count: usize) -> bool {
        match code {
            KeyCode::Char('h') | KeyCode::Left => self.move_horizontal(-(count as isize)),
            KeyCode::Char('l') | KeyCode::Right => self.move_horizontal(count as isize),
            KeyCode::Char('j') | KeyCode::Down => for _ in 0..count { self.move_line(1); },
            KeyCode::Char('k') | KeyCode::Up => for _ in 0..count { self.move_line(-1); },
            KeyCode::Char('w') => for _ in 0..count { self.move_word_kind(false, false); },
            KeyCode::Char('W') => for _ in 0..count { self.move_word_kind(false, true); },
            KeyCode::Char('b') => for _ in 0..count { self.move_word_kind(true, false); },
            KeyCode::Char('B') => for _ in 0..count { self.move_word_kind(true, true); },
            KeyCode::Char('e') => for _ in 0..count { self.move_word_end(false); },
            KeyCode::Char('E') => for _ in 0..count { self.move_word_end(true); },
            KeyCode::Char('0') | KeyCode::Home | KeyCode::Char('H') => self.cursor = self.line_start(),
            KeyCode::Char('^') => self.cursor = self.first_nonblank(),
            KeyCode::Char('$') | KeyCode::End | KeyCode::Char('L') => self.cursor = self.line_end(),
            KeyCode::Char('G') => self.cursor = self.char_count(),
            KeyCode::Char('{') => for _ in 0..count { self.move_paragraph(false); },
            KeyCode::Char('}') => for _ in 0..count { self.move_paragraph(true); },
            _ => return false,
        }
        true
    }

    pub fn compose_help_matches(&self) -> Vec<usize> {
        let query = self.help_query.trim().to_lowercase();
        super::input::COMPOSE_HELP_ENTRIES.iter().enumerate()
            .filter_map(|(index, entry)| {
                (query.is_empty() || format!("{} {} {}", entry.category, entry.key, entry.description)
                    .to_lowercase().contains(&query)).then_some(index)
            })
            .collect()
    }

    pub fn visual_range(&self) -> Option<(usize, usize)> {
        let anchor = self.visual_anchor?;
        let cursor = self.cursor.min(self.char_count());
        if self.visual_linewise && self.field == ComposeField::Body {
            let chars: Vec<char> = self.body.chars().collect();
            let mut start = anchor.min(cursor);
            while start > 0 && chars[start - 1] != '\n' { start -= 1; }
            let mut end = anchor.max(cursor);
            while end < chars.len() && chars[end] != '\n' { end += 1; }
            if end < chars.len() { end += 1; }
            return Some((start, end));
        }
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


/// Match a query as an ordered, case-insensitive subsequence. Consecutive
/// characters, word boundaries and exact substrings rank ahead of scattered
/// matches; no external search dependency or indexing is required for a page.
fn mail_fuzzy_score(field: &str, query_lower: &str) -> Option<i64> {
    let lower = field.to_lowercase();
    let hay = lower.chars().collect::<Vec<_>>();
    let needle = query_lower.chars().collect::<Vec<_>>();
    if needle.is_empty() { return Some(0); }
    let mut at = 0usize;
    let mut previous = None;
    let mut score = 0i64;
    for wanted in needle {
        let position = (at..hay.len()).find(|&index| hay[index] == wanted)?;
        score += 10;
        if position == 0 || !hay[position - 1].is_alphanumeric() { score += 9; }
        if let Some(prev) = previous {
            if position == prev + 1 { score += 12; }
            else { score -= i64::try_from(position - prev - 1).unwrap_or(100).min(12); }
        }
        previous = Some(position);
        at = position + 1;
    }
    if lower.contains(query_lower) { score += 80; }
    score -= i64::try_from(hay.len() / 20).unwrap_or(10).min(10);
    Some(score)
}

pub struct AccountState {
    pub name: String,
    pub default_mailbox: String,
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
        let mut matches = self.messages.iter().enumerate().filter_map(|(index, message)| {
            let fields = [
                message.subject.as_str(), message.from_name.as_str(),
                message.from_address.as_str(), message.date_label.as_str(),
            ];
            match self.search_mode {
                crate::config::MailSearchMode::Strict => {
                    fields.iter().any(|field| field.to_lowercase().contains(&query))
                        .then_some((index, 0i64))
                }
                crate::config::MailSearchMode::Fuzzy => fields.iter()
                    .filter_map(|field| mail_fuzzy_score(field, &query))
                    .max().map(|score| (index, score)),
            }
        }).collect::<Vec<_>>();
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
        self.save_current_cache();
        let Some(name) = self.current_mailbox.as_ref() else { return; };
        if let (Some(writer), Some(cached)) = (&self.cache_writer, self.mailbox_cache.get(name)) {
            if cached.uid_validity.is_some() && !self.unverified_mailboxes.contains(name) {
                writer.send(cache::Change::Folder(name.clone(), cached.clone()));
            }
        }
    }

    fn remove_persistent_folder(&self, name: &str) {
        if let Some(writer) = &self.cache_writer {
            writer.send(cache::Change::RemoveFolder(name.to_owned()));
        }
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
        // Give the worker only cached headers (never MIME bodies). It must
        // validate UIDVALIDITY with the server before reusing even one row.
        let hint = (offset == 0).then(|| self.mailbox_cache.get(&name))
            .flatten()
            .and_then(|folder| folder.uid_validity.map(|uid_validity| MailboxSyncHint {
                uid_validity,
                uid_next: folder.uid_next,
                messages: folder.messages.clone(),
            }));
        if let Ok(request_id) = self.worker.post(ActionKind::OpenMailbox {
            name,
            offset,
            limit: self.page_size,
            hint,
        }) {
            self.page_inflight = Some((pending_name, offset));
            self.page_request_id = Some(request_id);
            self.messages_loading = offset == 0;
            crate::perf::record_request("ui_mailbox_request", request_id, None, Duration::ZERO, true);
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
        let Some(position) = visible.iter().position(|index| *index == self.message_index) else {
            return;
        };
        self.prefetch_around_position(position, direction);
    }

    fn prefetch_around_position(&mut self, position: usize, direction: isize) {
        self.prefetch_window(position, direction, None);
    }

    fn prefetch_for_scroll(&mut self, position: usize, destination: usize) {
        if position == destination { return; }
        self.prefetch_window(position, if destination < position { -1 } else { 1 }, Some(destination));
    }

    fn prefetch_window(&mut self, position: usize, direction: isize, destination: Option<usize>) {
        let Some(mailbox) = self.current_mailbox.clone() else { return; };
        if self.unverified_mailboxes.contains(&mailbox) { return; }
        let visible = self.visible_message_indices();
        if position >= visible.len() { return; }
        let direction = if direction < 0 { -1 } else { 1 };
        let destination = destination.filter(|target| *target < visible.len());
        let anchor = destination.unwrap_or(position);
        // Keep the existing window while it covers the moving cursor; a new
        // window every animation frame would cancel almost every useful fetch.
        if self.last_prefetch_window.is_some()
            && self.last_prefetch_anchor.as_ref().is_some_and(|(name, last, dir)| {
                name == &mailbox && *dir == direction
                    && last.abs_diff(anchor) < PREFETCH_WINDOW_REARM_ROWS
            }) {
            return;
        }
        let positions = if let Some(target) = destination {
            preview_travel_positions(position, target, visible.len())
        } else {
            std::iter::once(position)
                .chain(preview_neighbor_positions(position, visible.len(), direction, false))
                .collect::<Vec<_>>()
        };
        let mut raw_bytes = 0u64;
        let ids = positions.into_iter()
            .filter_map(|position| self.messages.get(visible[position]))
            .filter(|message| message.size > 0 && message.size <= MAX_PREFETCH_MESSAGE_BYTES)
            .filter(|message| !self.bodies.contains_key(&message.id)
                && !self.pending_bodies.contains(&message.id))
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
        if self.last_prefetch_window.as_ref() == Some(&window) { return; }
        if self.worker.post(ActionKind::PrefetchBodies { mailbox: mailbox.clone(), ids }).is_ok() {
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
        let mut ranked = self.messages.iter().enumerate()
            .filter(|(_, message)| self.bodies.contains_key(&message.id))
            .map(|(position, message)| (position.abs_diff(self.message_index), message.id.clone()))
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(distance, _)| *distance);
        let mut used = 0usize;
        let mut keep = HashSet::new();
        for (_, id) in ranked {
            let Some(body) = self.bodies.get(&id) else { continue; };
            let weight = preview_body_bytes(body);
            if keep.len() >= MAX_CACHED_PREVIEW_BODIES
                || (!keep.is_empty() && used.saturating_add(weight) > MAX_CACHED_PREVIEW_BYTES)
            { continue; }
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
    attachment_cache: VecDeque<((usize, String, Option<u32>, MessageId), Arc<Vec<OutgoingAttachment>>)>,
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
    attachment_preview_results: Arc<Mutex<VecDeque<(usize, Result<image::DynamicImage, String>)>>>,
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
    pub confirmation: Option<MutationConfirmation>,
    pub composer: Option<Composer>,
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
                let uid_validities = snapshot.folders.iter().filter_map(|(name, folder)| {
                    folder.uid_validity.map(|validity| (name.clone(), validity))
                }).collect();
                let uid_nexts = snapshot.folders.iter().filter_map(|(name, folder)| {
                    folder.uid_next.map(|uid_next| (name.clone(), uid_next))
                }).collect();
                let unverified_mailboxes = snapshot.folders.keys().cloned().collect();
                let mut account = AccountState {
                name: config.name.clone(),
                default_mailbox: config.default_mailbox.clone(),
                from: crate::outgoing::sender_from_account(config),
                outgoing_configured: config.outgoing.is_some(),
                copy_to: config.copy_to.clone(),
                capabilities: None,
                mailboxes: snapshot.mailboxes,
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
                if let Some(index) = account.mailboxes.iter().position(|m| m.name == account.default_mailbox) {
                    account.mailbox_index = index;
                }
                if let Some(name) = account.selected_mailbox().map(|mailbox| mailbox.name.clone()) {
                    let _ = account.restore_cached_mailbox(&name);
                }
                account
            })
            .collect();
        let themes = load_themes();
        let theme_index = theme_index(&themes, &config.ui.theme);
        Self {
            accounts,
            account_index: 0,
            focus: Focus::Messages,
            status: StatusLine::info("starting workers…"),
            startup_warnings: Vec::new(),
            show_startup_warnings: false,
            should_quit: false,
            setup: config.accounts.is_empty().then(|| SetupWizard::new(true)),
            remove_account_confirmation: None,
            setup_config_path: config.config_path.clone()
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
            mailbox_width_percent: stored_mailbox_width().unwrap_or(config.ui.mailbox_width_percent).clamp(12, 50),
            mailbox_resizing: false,
            preview_width_percent: stored_preview_width().unwrap_or(config.ui.preview_width_percent).clamp(25, 75),
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

    /// Only known count increases trigger notifications; never notify on initial sync.
    pub fn take_new_mail_notice(&mut self) -> Option<NewMailNotice> {
        self.new_mail_notices.pop_front()
    }

    /// Desktop banners and their associated sound; independent of the terminal BEL setting.
    pub fn notifications_enabled(&self) -> bool { self.new_mail_notifications_enabled }

    pub fn notification_icon_style(&self) -> crate::config::NotificationIconStyle {
        self.notification_icon_style
    }

    /// Count only arrivals detected during this session; older unread mail is
    /// not incorrectly presented as a new notification.
    pub fn new_mail_count(&self) -> usize { self.new_mail_since_launch }

    pub fn new_mail_sound_name(&self) -> &str { &self.new_mail_sound_name }
    pub fn new_mail_sound_file(&self) -> Option<&str> { self.new_mail_sound_file.as_deref() }

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

    pub fn setup_discovered(&mut self, email: String, result: Result<crate::discovery::MailConfiguration, String>) {
        let Some(wizard) = self.setup.as_mut() else { return; };
        if !wizard.discovering || wizard.authenticating || wizard.saved ||
            !wizard.fields[crate::setup::EMAIL].trim().eq_ignore_ascii_case(&email) { return; }
        match result {
            Ok(config) => wizard.apply_discovery(config),
            Err(error) => wizard.discovery_failed(&error),
        }
        // An alias doesn't establish which Exchange mailbox identity to use
        // for SMTP. Authenticate first, then offer the login UPN for review.
        if self.setup.as_ref().is_some_and(|wizard|
            wizard.step == crate::setup::SetupStep::Authenticating)
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
                    wizard.message = format!(
                        "OAuth failed: {error}. Ctrl-S/F6 retry; F7 saves offline."
                    );
                }
            }
        }
    }

    /// Start a browser login after validating generated server settings.
    /// Used both for a Microsoft alias (before settings review) and for the
    /// explicit Continue action in the settings step.
    fn start_setup_oauth(&mut self) {
        let Some(wizard) = self.setup.as_mut() else { return; };
        if wizard.authenticating || wizard.authenticated { return; }
        match wizard.preview_account() {
            Ok(account) => {
                // OAuth tokens are keyed by account name, so never let wizard
                // setup overwrite a configured account's credentials.
                let duplicate = self.accounts.iter().any(|existing| existing.name == account.name)
                    || crate::config::Config::load_or_empty(Some(&self.setup_config_path))
                        .is_ok_and(|config| config.accounts.iter().any(|existing| existing.name == account.name));
                if duplicate {
                    wizard.step = crate::setup::SetupStep::Settings;
                    wizard.error = true;
                    wizard.message = "This account name is already in use. Choose another before signing in.".into();
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
                    let result = crate::oauth::login_account_quiet(&[account.clone()], &name)
                        .and_then(|()| {
                            let settings = crate::oauth::OAuthSettings::for_account(&account)?
                                .ok_or_else(|| anyhow::anyhow!("OAuth is not configured"))?;
                            if settings.provider == "microsoft" {
                                settings.suggested_login_username()
                            } else { Ok(None) }
                        })
                        .map_err(|error| format!("{}", error.root_cause()));
                    let _ = sender.send(RuntimeEvent::SetupLoginFinished(result));
                });
            }
            Err(error) => {
                wizard.step = crate::setup::SetupStep::Settings;
                wizard.selected = crate::setup::PROVIDER;
                wizard.error = true;
                wizard.message = format!("Check settings: {error:#}. F3 opens advanced OAuth settings.");
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
        } else { return; };
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
                let Some(sender) = self.setup_sender.clone() else { return; };
                let Some(wizard) = self.setup.as_mut() else { return; };
                if wizard.discovering { return; }
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
                let Some(wizard) = self.setup.as_mut() else { return; };
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
                if self.setup.as_ref().is_some_and(|wizard| wizard.saved) { return; }
                if self.setup.as_ref().is_some_and(|wizard| !wizard.authenticated) {
                    self.start_setup_oauth();
                    return;
                }
                let Some(wizard) = self.setup.as_mut() else { return; };
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
        // Account actions share Alt shortcuts. Ctrl-a remains available for
        // the compose attachment picker, not the account setup wizard.
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('a')
            && self.composer.is_none() && self.confirmation.is_none()
            && !self.help_visible && !self.theme_picker_visible
            && !self.attachment_picker_visible
        {
            self.setup = Some(SetupWizard::new(false));
            return;
        }
        if self.show_startup_warnings {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('!')) {
                self.show_startup_warnings = false;
            }
            return;
        }
        if !self.startup_warnings.is_empty() && key.code == KeyCode::Char('!')
            && self.composer.is_none() && !self.attachment_picker_visible
            && !self.help_visible && !self.theme_picker_visible && self.confirmation.is_none() {
            self.show_startup_warnings = true;
            return;
        }
        if self.attachment_picker_visible {
            self.handle_attachment_picker_key(key);
            return;
        }
        if self.composer.is_none() && !self.message_open && self.preview_split
            && key.modifiers.contains(KeyModifiers::ALT | KeyModifiers::SHIFT) {
            let adjustment = match key.code {
                KeyCode::Char('h') | KeyCode::Char('H') => Some(3_i32),
                KeyCode::Char('l') | KeyCode::Char('L') => Some(-3_i32),
                _ => None,
            };
            if let Some(adjustment) = adjustment {
                self.preview_width_percent = (i32::from(self.preview_width_percent) + adjustment)
                    .clamp(25, 75) as u16;
                let _ = persist_preview_width(self.preview_width_percent);
                return;
            }
        }
        if self.composer.is_none() && !self.message_open && self.sidebar_visible
            && key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::SHIFT) {
            let adjustment = match key.code {
                KeyCode::Char('h') => Some(-2_i32),
                KeyCode::Char('l') => Some(2_i32),
                _ => None,
            };
            if let Some(adjustment) = adjustment {
                self.mailbox_width_percent = (i32::from(self.mailbox_width_percent) + adjustment)
                    .clamp(12, 50) as u16;
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
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('d')
            && !self.message_open && self.sidebar_visible
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
            if matches!(command, Command::NextAccount | Command::PreviousAccount | Command::SelectAccount(_)) {
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
                self.toggle_header_details();
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
            KeyCode::Char('t') => {
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
                account.persist_mailboxes();
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
                    if account.current_mailbox.as_deref() != Some(name.as_str())
                        && !account.restore_cached_mailbox(&name)
                    {
                        account.current_mailbox = Some(name.clone());
                        account.messages.clear();
                        account.bodies.clear();
                        account.message_index = 0;
                        account.message_view_offset = 0;
                        account.next_page_offset = 0;
                        account.has_more = false;
                    }
                    // Cached rows remain visible while the first-page refresh
                    // runs on the interactive IMAP worker.
                    if account.page_inflight.is_none() {
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
                    if self.new_mail_notifications_enabled {
                        self.new_mail_since_launch = self.new_mail_since_launch.saturating_add(new_count);
                    }
                    // Keep the UID-keyed header cache on new arrivals. The
                    // next SELECT validates UIDVALIDITY and obtains a current
                    // UID+FLAGS page before we reuse any old summary. Dropping
                    // the cache here would force full header downloads on
                    // precisely the refreshes that benefit from delta fetches.
                    self.status = StatusLine::temporary(format!(
                        "{} new message(s) in {}", new_count, account.name,
                    ), Duration::from_secs(6));
                    if self.new_mail_bell_enabled && self.new_mail_notifications_enabled {
                        self.new_mail_bell_pending = true;
                    }
                    if self.new_mail_notifications_enabled {
                        // Use the existing background IMAP session; do not block
                        // rendering, fetch on the foreground session, or change \Seen.
                        if account.worker.post(ActionKind::NewMailPreview {
                            mailbox: name.clone(), count: new_count,
                        }).is_err() {
                            self.new_mail_notices.push_back(NewMailNotice {
                                account: account.name.clone(), count: new_count,
                                sender: String::new(), subject: String::new(), excerpt: String::new(),
                            });
                        }
                    }
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
                account.persist_mailboxes();
                Self::pump_mailbox_status(account);
            }
            WorkerEventKind::NewMailPreview { count, sender, subject, excerpt } => {
                if self.new_mail_notifications_enabled {
                    self.new_mail_notices.push_back(NewMailNotice {
                        account: account.name.clone(), count, sender, subject, excerpt,
                    });
                }
            }
            WorkerEventKind::MailboxOpened {
                name,
                page,
                uid_validity,
                uid_next,
                elapsed_micros,
            } => {
                // A newer folder/page request supersedes this result even if
                // both requests happen to name the same mailbox.
                if account.page_request_id != Some(event.request_id) {
                    crate::perf::record_request("ui_mailbox_stale", event.request_id, None, Duration::ZERO, true);
                    return;
                }
                account.page_request_id = None;
                let first_page = page.offset == 0;
                let previous_validity = account.uid_validities.get(&name).copied();
                let generation_changed = first_page && previous_validity.is_some()
                    && previous_validity != uid_validity;
                if generation_changed {
                    // Cached UIDs may point to entirely different emails after
                    // a server UIDVALIDITY reset: drop all old derived data.
                    account.mailbox_cache.remove(&name);
                    account.remove_persistent_folder(&name);
                    account.bodies.clear();
                }
                if let Some(validity) = uid_validity {
                    account.uid_validities.insert(name.clone(), validity);
                } else {
                    account.uid_validities.remove(&name);
                }
                if let Some(next) = uid_next {
                    account.uid_nexts.insert(name.clone(), next);
                } else {
                    account.uid_nexts.remove(&name);
                }
                account.unverified_mailboxes.remove(&name);
                let selected_name = account.selected_mailbox().map(|mailbox| mailbox.name.clone());
                if selected_name.as_deref() != Some(name.as_str()) {
                    account.page_inflight = None;
                    account.messages_loading = false;
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
                                uid_validity,
                                uid_next,
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
                crate::perf::record_request("ui_mailbox_applied", event.request_id, None, Duration::ZERO, true);
                account.current_mailbox = Some(name.clone());
                if page.offset == 0 {
                    let selected_id = (!generation_changed)
                        .then(|| account.selected_message().map(|message| message.id.clone()))
                        .flatten();
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
                    // A verified same-folder refresh can finish while the
                    // priority worker fetches the selected body. Keep the
                    // request if its UID survived this fresh page; otherwise
                    // invalidate it (especially after UIDVALIDITY changes).
                    let still_selected = !generation_changed
                        && account.body_request_id.is_some()
                        && account.pending_bodies.len() == 1
                        && account.selected_message().is_some_and(|message|
                            account.pending_bodies.contains(&message.id));
                    if !still_selected {
                        account.pending_bodies.clear();
                        account.body_request_id = None;
                    }
                    account.pending_seen.clear();
                }
                account.last_scan_micros = Some(elapsed_micros);
                account.persist_current_cache();
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
                account.last_prefetch_window = None;
                if event.account == self.account_index { account.prefetch_near_cursor(1); }
                if self.preview_split && event.account == self.account_index {
                    account.request_selected_body();
                    Self::mark_cached_selected_seen(account);
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
            WorkerEventKind::MailboxOpenFailed { name, error } => {
                if account.page_request_id != Some(event.request_id) {
                    crate::perf::record_request("ui_mailbox_stale", event.request_id, None, Duration::ZERO, false);
                    return;
                }
                account.page_request_id = None;
                account.page_inflight = None;
                account.messages_loading = false;
                self.status = StatusLine::error(format!("{}: could not open {name}: {error}", account.name));
            }
            WorkerEventKind::BodyLoadFailed { error } => {
                if account.body_request_id != Some(event.request_id) { return; }
                account.body_request_id = None;
                account.pending_bodies.clear();
                self.status = StatusLine::error(format!("{}: could not load email: {error}", account.name));
            }
            WorkerEventKind::BodyLoaded(body) => {
                if account.body_request_id != Some(event.request_id) {
                    return;
                }
                account.body_request_id = None;
                let id = body.id.clone();
                account.pending_bodies.remove(&id);
                if account.bodies.contains_key(&id) {
                    crate::perf::record("ui_selected_body_duplicate", None, Duration::ZERO, true);
                }
                account.bodies.insert(id.clone(), body);
                account.trim_preview_bodies();
                account.persist_current_cache();

                if (self.message_open || self.preview_split)
                    && account
                        .selected_message()
                        .is_some_and(|message| message.id == id && message.unread())
                {
                    account.request_seen(id, true);
                }
                Self::pump_mailbox_status(account);

            }
            WorkerEventKind::BodyPrefetched { mailbox, body } => {
                // The background connection can finish after a folder change,
                // UIDVALIDITY reset, expunge, or account switch. Never apply a
                // body unless it still belongs to the verified visible folder.
                let id = body.id.clone();
                if account.current_mailbox.as_deref() != Some(mailbox.as_str())
                    || account.unverified_mailboxes.contains(&mailbox)
                    || !account.messages.iter().any(|message| message.id == id)
                {
                    crate::perf::record("ui_prefetch_discarded", None, Duration::ZERO, true);
                    return;
                }
                if account.bodies.contains_key(&id) {
                    crate::perf::record("ui_prefetch_duplicate", None, Duration::ZERO, true);
                    return;
                }
                crate::perf::record("ui_prefetch_applied", None, Duration::ZERO, true);
                account.bodies.insert(id.clone(), body);
                account.trim_preview_bodies();
                if event.account == self.account_index && (self.message_open || self.preview_split)
                    && account.selected_message().is_some_and(|message| message.id == id && message.unread())
                {
                    account.request_seen(id, true);
                }
                // Do not serialize the entire mailbox for every opportunistic
                // body. The regular cache checkpoint saves it when needed.
            }
            WorkerEventKind::AttachmentsLoaded { mailbox, id, attachments } => {
                let expected = self.attachment_source.as_ref().is_some_and(|(account, folder, _, wanted)|
                    *account == event.account && folder == &mailbox && wanted == &id);
                if expected && self.attachment_pending.as_ref() == Some(&id) && self.attachment_picker_visible {
                    self.attachment_files = Arc::new(attachments);
                    if let Some(source) = self.attachment_source.as_ref()
                        .filter(|(account, _, _, message)| *account == event.account && message == &id).cloned() {
                        let total_bytes = self.attachment_files.iter().map(|file| file.data.len()).sum::<usize>();
                        if total_bytes <= 12 * 1024 * 1024 {
                            self.attachment_cache.retain(|(key, _)| key != &source);
                            self.attachment_cache.push_back((source, Arc::clone(&self.attachment_files)));
                            while self.attachment_cache.len() > 6 || self.attachment_cache.iter()
                                .flat_map(|(_, files)| files.iter()).map(|file| file.data.len()).sum::<usize>()
                                > 24 * 1024 * 1024 {
                                self.attachment_cache.pop_front();
                            }
                        }
                    }
                    self.attachment_selection = 0;
                    self.attachment_marked.clear();
                    self.attachment_visual_anchor = None;
                    self.attachment_preview = None;
                    self.attachment_preview_identity = None;
                    self.attachment_pending = None;
                    self.queue_attachment_preview();
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
                account.persist_current_cache();
                account.persist_mailboxes();
            }
            WorkerEventKind::MutationComplete => {
                account.pending_mutations = account.pending_mutations.saturating_sub(1);
                let message = account.pending_mutation_labels.remove(&event.request_id)
                    .unwrap_or("mail updated");
                self.status = StatusLine::temporary(message, Duration::from_secs(2));
            }
            WorkerEventKind::DraftSaveFailed { mailbox, error } => {
                self.status = StatusLine::error(format!(
                    "saved locally only: uploading to {mailbox} failed ({error}); Ctrl+n resumes"));
            }
            WorkerEventKind::DraftSaved { mailbox } => {
                // Refresh cached Drafts on the next visit, or immediately if
                // the user is already browsing that folder.
                account.mailbox_cache.remove(&mailbox);
                account.remove_persistent_folder(&mailbox);
                if let Some(folder) = account.mailboxes.iter_mut()
                    .find(|folder| folder.name == mailbox) {
                    folder.counts_known = false;
                }
                if account.current_mailbox.as_deref() == Some(mailbox.as_str()) {
                    let _ = account.request_mailbox_page(mailbox.clone(), 0);
                }
                self.status = StatusLine::info(format!(
                    "draft saved in {mailbox}; Ctrl+n resumes editing local recovery copy"));
            }
            WorkerEventKind::MessageSent {
                smtp_status,
                sent_mailbox,
                sent_copy_saved,
                sent_copy_warning,
                sent_copy_disabled,
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
                    account.remove_persistent_folder(name);
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
                } else if sent_copy_disabled {
                    StatusLine::temporary(format!("Message accepted ({smtp_status}); Sent copy disabled"), Duration::from_secs(5))
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
                if account.mailbox_status_inflight == Some(event.request_id) {
                    account.mailbox_status_inflight = None;
                    // Background count lookup failure is nonfatal: keep the
                    // current mailbox, reader, and interactive loading alive.
                    return;
                }
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
                // A failure in an unrelated mutation/compose request must not
                // cancel a currently loading mailbox or email. Navigation has
                // dedicated error variants above and is correlated by ID.
                if account.page_request_id.is_none() {
                    account.messages_loading = false;
                    account.page_inflight = None;
                }
                if account.mailbox_status_inflight == Some(event.request_id) {
                    account.mailbox_status_inflight = None;
                }
                if account.body_request_id.is_none() {
                    account.pending_bodies.clear();
                }
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
                    if self.new_mail_notifications_enabled { "new-mail notifications enabled" }
                    else { "new-mail notifications muted" },
                    Duration::from_secs(2),
                );
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

    /// Keep smooth scrolling at the normal animation rate, but check idle PDF
    /// conversions less often to avoid redrawing the whole TUI 100 times/sec.
    #[must_use]
    pub fn animation_frame_interval(&self) -> Duration {
        if self.attachment_picker_visible && self.attachment_smooth_target.is_none()
            && self.attachment_destination_picker.as_ref().is_none_or(|picker| picker.smooth_target.is_none())
            && (self.attachment_preview_due.is_some() || !self.attachment_preview_inflight.is_empty()) {
            return Duration::from_millis(40);
        }
        if self.composer.as_ref().and_then(|composer| composer.file_picker.as_ref())
            .is_some_and(|picker| picker.smooth_target.is_none()
                && (picker.pdf_preview_due.is_some() || !picker.pdf_preview_inflight.is_empty())) {
            Duration::from_millis(60)
        } else {
            Duration::from_millis(10) // 100 FPS for smooth cursor animation.
        }
    }

    #[must_use]
    pub fn animation_active(&self) -> bool {
        (self.attachment_picker_visible && (self.attachment_smooth_target.is_some()
            || self.attachment_preview_due.is_some() || !self.attachment_preview_inflight.is_empty()
            || self.attachment_destination_picker.as_ref().is_some_and(|picker| picker.smooth_target.is_some())))
            || self.composer.as_ref().and_then(|composer| composer.file_picker.as_ref())
            .is_some_and(|picker| picker.smooth_target.is_some()
                || picker.pdf_preview_due.is_some() || !picker.pdf_preview_inflight.is_empty())
            || self.reader_smooth_target.is_some() || self.account().is_some_and(|account| {
            account.smooth_message_target.is_some()
                || account.smooth_mailbox_target.is_some()
                || account.smooth_preview_target.is_some()
        })
    }

    /// Advance one row of a pending smooth-scroll animation. Message targets
    /// are positions inside the filtered list; mailbox targets deliberately do
    /// not open intermediate folders, so Ctrl-Shift-j/k stays responsive.
    pub fn advance_animation(&mut self) {
        if self.attachment_picker_visible {
            if let Some(picker) = &mut self.attachment_destination_picker {
                if picker.smooth_target.is_some() { picker.advance_smooth_scroll(); }
            } else if let Some(target) = self.attachment_smooth_target {
                let next = self.attachment_selection.saturating_add_signed(
                    if target > self.attachment_selection { 1 } else { -1 });
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
        if let Some(picker) = self.composer.as_mut().and_then(|composer| composer.file_picker.as_mut()) {
            // Advance one row and check PDF completions on each scheduled frame.
            // Neither operation waits for the PDF renderer. If the picker is
            // idle, let animations beneath the popup settle normally.
            if picker.smooth_target.is_some() || picker.pdf_preview_due.is_some()
                || !picker.pdf_preview_inflight.is_empty() {
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
                        account.prefetch_near_cursor(if target_position >= current { 1 } else { -1 });
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
        if self.accounts.is_empty() { return; }
        let target = offset_index(self.account_index, self.accounts.len(), delta);
        self.select_account(target);
    }

    fn select_account(&mut self, index: usize) {
        if index >= self.accounts.len() || index == self.account_index { return; }
        if let Some(account) = self.account_mut() {
            account.persist_current_cache();
        }
        self.count_prefix = None;
        self.message_open = false;
        self.reader_smooth_target = None;
        self.reader_visual_linewise = false;
        self.pending_compose = None;
        self.visual_anchor = None;
        self.confirmation = None;
        self.focus = Focus::Messages;
        self.account_index = index;
        if let Some(account) = self.account_mut() {
            account.last_prefetch_window = None;
        }
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
        self.visual_anchor = None;
        let preview_split = self.preview_split;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.persist_current_cache();
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

    /// True only after a Vim cursor motion. Measuring the wrapped cursor row
    /// on every idle redraw would be expensive for large HTML emails.
    pub fn reader_needs_scroll_follow(&self) -> bool {
        self.reader_scroll_follow_pending
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
        line_count: usize,
        view_height: usize,
        max_scroll: u16,
        cursor_visual_row: Option<usize>,
    ) {
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

        let follow_cursor = self.reader_scroll_follow_pending && line_count > 0;
        self.reader_scroll_follow_pending = false;
        let cursor_at_start = self.reader_cursor == 0;
        let Some(account) = self.account_mut() else {
            return;
        };
        account.preview_max_scroll = max_scroll;
        account.preview_scroll = account.preview_scroll.min(max_scroll);
        account.smooth_preview_target = account
            .smooth_preview_target
            .map(|target| target.min(max_scroll))
            .filter(|target| *target != account.preview_scroll);
        if follow_cursor {
            if cursor_at_start {
                // Keep the real header and hero image visible at the top.
                account.preview_scroll = 0;
            } else if let Some(row) = cursor_visual_row {
                account.preview_scroll = u16::try_from(reader_scroll_for_visual_cursor(
                    usize::from(account.preview_scroll), row, view_height.max(1),
                    usize::from(max_scroll),
                )).unwrap_or(u16::MAX);
            }
        }
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
        // The renderer knows the actual wrapped row; logical body-line numbers
        // do not. Defer scrolloff until it has measured the current paragraph.
        self.reader_scroll_follow_pending = true;
        if let Some(account) = self.account_mut() {
            account.smooth_preview_target = None;
        }
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
                let target = bounded_index(base, visible.len(), delta);
                account.smooth_message_target = Some(target);
                if target != current {
                    // Warm the entire travel corridor instead of repeatedly
                    // canceling batches aimed at the moving destination.
                    account.prefetch_for_scroll(current, target);
                }
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
        account.prefetch_near_cursor(if bottom { -1 } else { 1 });
        if preview_split {
            account.request_selected_body();
            Self::mark_cached_selected_seen(account);
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
                self.reader_line_count = 0;
                self.reader_image = None;
                self.reader_image_pending = None;
                self.reader_image_size_hint = None;
                self.message_open = true;
                if let Some(account) = self.account_mut() {
                    account.preview_scroll = 0;
                    account.preview_max_scroll = 0;
                    account.request_selected_body();
                    account.prefetch_near_cursor(1);
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

    /// Toggle spam by moving the selected message(s) to Junk, or back to Inbox.
    /// Use discovered mailbox roles rather than guessing Gmail or Outlook paths.
    fn toggle_spam_selected(&mut self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() { return; }
        let Some(account) = self.account() else { return; };
        let Some(source) = account.current_mailbox.clone() else { return; };
        let in_junk = account.mailboxes.iter().any(|mailbox|
            mailbox.name == source && mailbox.role == MailboxRole::Junk);
        let target_role = if in_junk { MailboxRole::Inbox } else { MailboxRole::Junk };
        let destination = account.mailboxes.iter()
            .find(|mailbox| mailbox.role == target_role && mailbox.name != source)
            .map(|mailbox| mailbox.name.clone());
        let Some(destination) = destination else {
            self.status = StatusLine::error(if in_junk {
                "cannot mark as not spam: no Inbox mailbox found"
            } else { "cannot mark as spam: no Junk/Spam mailbox found" });
            return;
        };
        // Move is the existing IMAP worker operation, which already updates
        // the source list and invalidates the destination cache.
        self.start_mutation(source, ids, MutationKind::Move { destination });
        if !self.status.is_error {
            self.status = StatusLine::temporary(if in_junk {
                "moved out of Spam to Inbox"
            } else { "marked as Spam" }, Duration::from_secs(2));
        }
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

    fn handle_remove_account_confirmation_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let Some(name) = self.remove_account_confirmation.take() else { return; };
                match crate::setup::remove_account(&self.setup_config_path, &name) {
                    Ok(()) => {
                        self.status = StatusLine::info(format!("Removed {name:?} from local config; server mail untouched"));
                        self.setup_reload_requested = true;
                    }
                    Err(error) => {
                        self.status = StatusLine::error(format!("Cannot remove account: {error:#}"));
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.remove_account_confirmation = None;
                self.status = StatusLine::temporary("account removal cancelled", Duration::from_secs(2));
            }
            _ => {}
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
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('d') => {
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
            account.remove_persistent_folder(destination);
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
            account.remove_persistent_folder(destination);
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
        account.persist_current_cache();
        account.persist_mailboxes();
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
        let entries = if self.attachment_picker_visible { ATTACHMENT_HELP_ENTRIES }
            else { HELP_ENTRIES };
        entries.iter()
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
                // Kitty graphics can be erased by a modal's Clear even when the
                // attachment selection and geometry have not changed.
                self.invalidate_attachment_preview_layout();
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
        if let Some(path) = self.theme_config_path.as_deref() {
            if let Err(error) = crate::config::save_ui_theme(path, &id) {
                self.status = StatusLine::error(format!("could not save [ui] theme: {error:#}"));
            } else {
                self.status = StatusLine::temporary("theme saved in config.toml", Duration::from_secs(2));
            }
        } else {
            self.status = StatusLine::temporary(
                "theme changed for this session (set [ui] theme in config.toml to persist)",
                Duration::from_secs(4),
            );
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

    fn toggle_header_details(&mut self) {
        self.show_header_details = !self.show_header_details;
        self.status = StatusLine::temporary(
            if self.show_header_details { "email addresses and Message-ID shown" } else { "email addresses and Message-ID hidden" },
            Duration::from_secs(2),
        );
    }

    fn reset_received_attachment_previews(&mut self) {
        self.attachment_preview = None;
        self.attachment_preview_identity = None;
        self.attachment_preview_due = None;
        self.attachment_preview_inflight.clear();
        self.attachment_preview_cache.clear();
        // Detach any outstanding worker of an earlier email: its Arc is no
        // longer visible to this popup, even if the new email uses index 0.
        self.attachment_preview_results = Arc::new(Mutex::new(VecDeque::new()));
    }

    fn open_attachment_picker(&mut self) {
        let Some(account) = self.account() else { return; };
        let Some(message) = account.selected_message() else { return; };
        let Some(mailbox) = account.current_mailbox.clone() else { return; };
        let id = message.id.clone();
        let validity = account.uid_validities.get(&mailbox).copied();
        let source = (self.account_index, mailbox.clone(), validity, id.clone());
        let cached = if account.unverified_mailboxes.contains(&mailbox) { None } else {
            self.attachment_cache.iter().find(|(key, _)| key == &source)
                .map(|(_, files)| Arc::clone(files))
        };
        crate::perf::record(if cached.is_some() { "ui_attachment_cache_hit" } else { "ui_attachment_cache_miss" },
            None, Duration::ZERO, true);
        if cached.is_none()
            && account.worker.post(ActionKind::FetchAttachments { mailbox, id: id.clone() }).is_err() {
            self.status = StatusLine::error("mail worker busy; cannot load attachments");
            return;
        }
        self.attachment_picker_visible = true;
        self.attachment_selection = 0;
        self.attachment_files = cached.clone().unwrap_or_else(|| Arc::new(Vec::new()));
        self.attachment_marked.clear();
        self.attachment_visual_anchor = None;
        self.attachment_save_directory = None;
        self.attachment_destination_picker = None;
        self.attachment_new_folder = None;
        self.attachment_pending_count = 0;
        self.attachment_pending_g = false;
        self.attachment_smooth_target = None;
        self.attachment_source = Some(source);
        self.reset_received_attachment_previews();
        self.attachment_pending = if cached.is_some() { None } else { Some(id) };
        if cached.is_some() {
            self.queue_attachment_preview();
            self.status = StatusLine::info("attachments restored from memory");
        } else {
            self.status = StatusLine::info("loading attachments…");
        }
    }

    fn dismiss_attachment_picker(&mut self) {
        self.help_visible = false;
        self.attachment_picker_visible = false;
        self.attachment_pending = None;
        self.attachment_files = Arc::new(Vec::new());
        self.attachment_source = None;
        self.attachment_marked.clear();
        self.attachment_visual_anchor = None;
        self.attachment_save_directory = None;
        self.attachment_destination_picker = None;
        self.attachment_new_folder = None;
        self.attachment_smooth_target = None;
        self.reset_received_attachment_previews();
    }

    fn queue_attachment_preview(&mut self) {
        self.attachment_preview_due = None;
        if self.attachment_smooth_target.is_some() || self.attachment_destination_picker.is_some() { return; }
        let index = self.attachment_selection;
        let Some(file) = self.attachment_files.get(index) else { return; };
        if !received_attachment_is_image(file) && !received_attachment_is_pdf(file) { return; }
        if self.attachment_preview_cache.iter().any(|(cached, _)| *cached == index)
            || self.attachment_preview_inflight.contains(&index) { return; }
        // A short debounce avoids launching Poppler for every intermediate
        // attachment while navigating rapidly. Images use a shorter delay.
        let delay = if received_attachment_is_pdf(file) { 95 } else { 25 };
        self.attachment_preview_due = Some((index, Instant::now() + Duration::from_millis(delay)));
    }

    fn advance_attachment_preview(&mut self) {
        let completed = if let Ok(mut results) = self.attachment_preview_results.lock() {
            results.drain(..).collect::<Vec<_>>()
        } else { Vec::new() };
        let mut selected_completed = false;
        for (index, result) in completed {
            self.attachment_preview_inflight.remove(&index);
            self.attachment_preview_cache.retain(|(cached, _)| *cached != index);
            self.attachment_preview_cache.push_back((index, result));
            while self.attachment_preview_cache.len() > 8 { self.attachment_preview_cache.pop_front(); }
            selected_completed |= index == self.attachment_selection;
        }
        if selected_completed { self.attachment_preview_identity = None; }
        if self.attachment_smooth_target.is_some() || self.attachment_destination_picker.is_some() { return; }
        let Some((index, due)) = self.attachment_preview_due else { return; };
        if Instant::now() < due || !self.attachment_preview_inflight.is_empty() { return; }
        self.attachment_preview_due = None;
        if self.attachment_files.get(index).is_none() { return; }
        // The MIME buffers are shared with the worker, not copied per image.
        let files = Arc::clone(&self.attachment_files);
        let queue = Arc::clone(&self.attachment_preview_results);
        match std::thread::Builder::new().name("aerc-attachment-preview".into()).spawn(move || {
            let start = Instant::now();
            let result = decode_received_attachment_preview(&files[index]);
            crate::perf::record("attachment_preview_decode", None, start.elapsed(), result.is_ok());
            if let Ok(mut pending) = queue.lock() { pending.push_back((index, result)); }
        }) {
            Ok(_handle) => { self.attachment_preview_inflight.insert(index); }
            Err(error) => {
                self.attachment_preview_cache.push_back((index, Err(format!("preview worker: {error}"))));
                self.attachment_preview_identity = None;
            }
        }
    }

    fn move_attachment_selection(&mut self, new_selection: usize) {
        if self.attachment_files.is_empty() { return; }
        let next = new_selection.min(self.attachment_files.len() - 1);
        if next != self.attachment_selection {
            self.attachment_selection = next;
            self.attachment_preview = None;
            self.attachment_preview_identity = None;
            self.queue_attachment_preview();
        }
    }

    fn handle_attachment_destination_key(&mut self, key: KeyEvent) {
        if self.attachment_new_folder.is_some() {
            match key.code {
                KeyCode::Esc => self.attachment_new_folder = None,
                KeyCode::Backspace => { if let Some(name) = &mut self.attachment_new_folder { name.pop(); } }
                KeyCode::Enter => {
                    let name = self.attachment_new_folder.take().unwrap_or_default();
                    if name.is_empty() || name == "." || name == ".."
                        || name.contains('/') || name.contains('\\') || name.chars().any(char::is_control) {
                        self.status = StatusLine::error("enter a single safe folder name");
                    } else if let Some(picker) = &mut self.attachment_destination_picker {
                        let path = picker.directory.join(name);
                        let result = (|| -> anyhow::Result<()> {
                            std::fs::create_dir(&path)?;
                            // Keep navigation consistent and refresh the current directory.
                            picker.enter_directory(path)
                        })();
                        if let Err(error) = result { self.status = StatusLine::error(format!("could not create folder: {error}")); }
                    }
                }
                KeyCode::Char(ch) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    if let Some(name) = &mut self.attachment_new_folder { name.push(ch); }
                },
                _ => {}
            }
            return;
        }
        // Modal-wide actions must run before borrowing the picker mutably.
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.attachment_destination_picker = None;
                self.queue_attachment_preview();
                return;
            }
            KeyCode::Char('s') => { self.save_selected_attachments(); return; }
            KeyCode::Char('n') => { self.attachment_new_folder = Some(String::new()); return; }
            _ => {}
        }
        let Some(picker) = self.attachment_destination_picker.as_mut() else { return; };
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('j' | 'k')) {
            let delta = if key.code == KeyCode::Char('j') { 12 } else { -12 };
            picker.queue_smooth_scroll(delta);
            return;
        }
        if picker.pending_g {
            picker.pending_g = false;
            if key.code == KeyCode::Char('g') {
                picker.selected = 0;
                picker.pending_count = 0;
                picker.refresh_preview();
                return;
            }
        }
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                picker.pending_count = picker.pending_count.saturating_mul(10)
                    .saturating_add(d.to_digit(10).unwrap_or(1) as usize).min(99_999);
                return;
            }
            KeyCode::Char('0') if picker.pending_count > 0 => {
                picker.pending_count = picker.pending_count.saturating_mul(10).min(99_999);
                return;
            }
            KeyCode::Char('g') => { picker.pending_g = true; return; }
            _ => {}
        }
        let count = picker.pending_count.max(1);
        let explicit = picker.pending_count > 0;
        picker.pending_count = 0;
        let result = match key.code {
            KeyCode::Char('j') | KeyCode::Down => { picker.move_selection(count as isize); Ok(()) }
            KeyCode::Char('k') | KeyCode::Up => { picker.move_selection(-(count as isize)); Ok(()) }
            KeyCode::PageDown => { picker.move_selection(12); Ok(()) }
            KeyCode::PageUp => { picker.move_selection(-12); Ok(()) }
            KeyCode::Char('G') | KeyCode::End => {
                picker.selected = if explicit { count.saturating_sub(1).min(picker.entries.len().saturating_sub(1)) }
                    else { picker.entries.len().saturating_sub(1) };
                picker.refresh_preview(); Ok(())
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                if let Some(parent) = picker.directory.parent().map(Path::to_path_buf) {
                    picker.enter_directory(parent)
                } else { Ok(()) }
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                if let Some(entry) = picker.entries.get(picker.selected) {
                    if entry.is_directory { picker.enter_directory(entry.path.clone()) } else { Ok(()) }
                } else { Ok(()) }
            }
            KeyCode::Char('~') => {
                if let Some(home) = dirs::home_dir() { picker.enter_directory(home) }
                else { Ok(()) }
            }
            KeyCode::Char('.') => { picker.show_hidden = !picker.show_hidden; picker.refresh() }
            _ => Ok(()),
        };
        if let Err(error) = result { self.status = StatusLine::error(format!("file browser: {error}")); }
    }

    fn handle_attachment_picker_key(&mut self, key: KeyEvent) {
        if self.help_visible {
            self.handle_help_key(key);
            return;
        }
        if self.attachment_destination_picker.is_none()
            && key.code == KeyCode::Char('?')
            && !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) {
            self.open_help();
            return;
        }
        if self.attachment_destination_picker.is_some() {
            self.handle_attachment_destination_key(key);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('j' | 'k')) {
            let delta = if key.code == KeyCode::Char('j') { 12isize } else { -12isize };
            let target = self.attachment_selection.saturating_add_signed(delta)
                .min(self.attachment_files.len().saturating_sub(1));
            self.attachment_smooth_target = (target != self.attachment_selection).then_some(target);
            self.attachment_preview_due = None;
            return;
        }
        if self.attachment_pending_g {
            self.attachment_pending_g = false;
            if key.code == KeyCode::Char('g') {
                self.attachment_smooth_target = None;
                self.move_attachment_selection(0);
                self.attachment_pending_count = 0;
                return;
            }
        }
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                self.attachment_pending_count = self.attachment_pending_count.saturating_mul(10)
                    .saturating_add(d.to_digit(10).unwrap_or(1) as usize).min(99_999);
                return;
            }
            KeyCode::Char('0') if self.attachment_pending_count > 0 => {
                self.attachment_pending_count = self.attachment_pending_count.saturating_mul(10).min(99_999);
                return;
            }
            KeyCode::Char('g') => { self.attachment_pending_g = true; return; }
            _ => {}
        }
        let count = self.attachment_pending_count.max(1);
        let explicit = self.attachment_pending_count > 0;
        self.attachment_pending_count = 0;
        self.attachment_smooth_target = None;
        match key.code {
            KeyCode::Esc if self.attachment_visual_anchor.is_some() => { self.attachment_visual_anchor = None; }
            KeyCode::Esc | KeyCode::Char('q') => self.dismiss_attachment_picker(),
            KeyCode::Char('j') | KeyCode::Down => self.move_attachment_selection(self.attachment_selection.saturating_add(count)),
            KeyCode::Char('k') | KeyCode::Up => self.move_attachment_selection(self.attachment_selection.saturating_sub(count)),
            KeyCode::Char('G') => self.move_attachment_selection(if explicit { count.saturating_sub(1) }
                else { self.attachment_files.len().saturating_sub(1) }),
            KeyCode::PageDown => self.move_attachment_selection(self.attachment_selection.saturating_add(12)),
            KeyCode::PageUp => self.move_attachment_selection(self.attachment_selection.saturating_sub(12)),
            KeyCode::Char('v') | KeyCode::Char('V') => {
                if self.attachment_visual_anchor.is_some() { self.attachment_visual_anchor = None; }
                else if !self.attachment_files.is_empty() { self.attachment_visual_anchor = Some(self.attachment_selection); }
            }
            KeyCode::Char(' ') => {
                if let Some(anchor) = self.attachment_visual_anchor.take() {
                    let (start, end) = (anchor.min(self.attachment_selection), anchor.max(self.attachment_selection));
                    for index in start..=end { self.attachment_marked.insert(index); }
                } else if !self.attachment_files.is_empty() {
                    let index = self.attachment_selection;
                    if !self.attachment_marked.insert(index) { self.attachment_marked.remove(&index); }
                    self.move_attachment_selection(index + 1);
                }
            }
            KeyCode::Char('a') => {
                if self.attachment_marked.len() == self.attachment_files.len() { self.attachment_marked.clear(); }
                else { self.attachment_marked = (0..self.attachment_files.len()).collect(); }
            }
            KeyCode::Char('p') => self.attachment_preview_identity = None,
            KeyCode::Enter | KeyCode::Char('o') => self.open_attachment(),
            KeyCode::Char('s') => self.prompt_attachment_save(),
            _ => {}
        }
    }

    /// Invalidate the terminal-side protocol, not the decoded thumbnail cache.
    /// Resize events and closing `?` can clear Kitty graphics independently
    /// of their selection/size identity. The next draw re-uploads the image.
    pub fn invalidate_attachment_preview_layout(&mut self) {
        if self.attachment_picker_visible {
            self.attachment_preview_identity = None;
            self.attachment_preview = None;
        }
    }

    /// Create terminal graphics only from decoded, cached thumbnails. Decoding
    /// images and spawning pdftoppm happen on a worker, never on the 100 FPS
    /// event/render thread.
    pub fn ensure_attachment_preview(&mut self, width: u16, height: u16) {
        if !self.attachment_picker_visible || width == 0 || height == 0 { return; }
        let identity = (self.attachment_selection, width, height);
        if self.attachment_preview_identity == Some(identity) { return; }
        self.attachment_preview_identity = Some(identity);
        self.attachment_preview = None;
        let Some((_, Ok(image))) = self.attachment_preview_cache.iter()
            .find(|(cached, _)| *cached == self.attachment_selection) else { return; };
        let Some(picker) = &self.image_picker else { return; };
        let target = reader_image_target_size(image.width(), image.height(), None, None,
            picker.font_size().width, picker.font_size().height, width, height);
        self.attachment_preview = picker.new_protocol(image.clone(), target,
            Resize::Fit(Some(image::imageops::FilterType::Lanczos3))).ok();
    }

    pub fn attachment_preview_decoded(&self) -> bool {
        self.attachment_preview_cache.iter().any(|(index, result)|
            *index == self.attachment_selection && result.is_ok())
    }

    pub fn attachment_preview_error(&self) -> Option<&str> {
        self.attachment_preview_cache.iter().find(|(index, _)| *index == self.attachment_selection)
            .and_then(|(_, result)| result.as_ref().err().map(String::as_str))
    }

    fn prompt_attachment_save(&mut self) {
        if self.attachment_files.is_empty() { return; }
        let directory = dirs::download_dir().or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        match ComposeFilePicker::open_directory_only(directory) {
            Ok(picker) => {
                self.attachment_preview_due = None;
                self.attachment_destination_picker = Some(picker);
                self.attachment_new_folder = None;
            }
            Err(error) => self.status = StatusLine::error(format!("cannot open destination picker: {error}")),
        }
    }

    fn save_selected_attachments(&mut self) {
        let Some(picker) = &self.attachment_destination_picker else { return; };
        let directory = picker.directory.clone();
        if !directory.is_dir() {
            self.status = StatusLine::error(format!("not an existing directory: {}", directory.display()));
            return;
        }
        let mut indices = if self.attachment_marked.is_empty() {
            if self.attachment_files.is_empty() { Vec::new() } else { vec![self.attachment_selection] }
        } else { self.attachment_marked.iter().copied().collect::<Vec<_>>() };
        indices.sort_unstable();
        let mut saved = 0;
        let mut first_error = None;
        for index in indices {
            if let Some(file) = self.attachment_files.get(index) {
                match save_attachment_without_overwrite(&directory, file, index) {
                    Ok(_) => saved += 1,
                    Err(error) => { if first_error.is_none() { first_error = Some(error.to_string()); } }
                }
            }
        }
        self.attachment_destination_picker = None;
        if let Some(error) = first_error {
            self.status = StatusLine::error(format!("saved {saved} file(s), error: {error}"));
        } else {
            self.status = StatusLine::temporary(format!("saved {saved} attachment(s) to {}", directory.display()),
                Duration::from_secs(5));
            self.attachment_marked.clear();
        }
        self.queue_attachment_preview();
    }

    pub fn attachment_pending_loading(&self) -> bool { self.attachment_pending.is_some() }

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
                let header_height = if wizard.step == crate::setup::SetupStep::Identity { 1 } else { 3 };
                let list_y = popup_y.saturating_add(1 + crate::setup::PROGRESS_ROWS + header_height);
                let list_height = usize::from(popup_height.saturating_sub(5 + crate::setup::PROGRESS_ROWS + header_height));
                let rows = wizard.display_rows();
                let selected_pos = rows.iter().position(|row|
                    matches!(row, crate::setup::SetupDisplayRow::Field(index) if *index == wizard.selected))
                    .unwrap_or(0);
                let first = selected_pos.saturating_sub(list_height.saturating_sub(1))
                    .min(rows.len().saturating_sub(list_height));
                if mouse.column > popup_x && mouse.column < popup_x.saturating_add(popup_width) {
                    if mouse.row >= list_y && usize::from(mouse.row - list_y) < list_height
                        && !wizard.authenticating && !wizard.discovering {
                        if let Some(crate::setup::SetupDisplayRow::Field(field)) = rows.get(first + usize::from(mouse.row - list_y)) {
                            wizard.selected = *field;
                        }
                    }
                    // The last interior row presents the primary Continue action.
                    let button_y = if wizard.saved {
                        // Confirmation button occupies the penultimate inner row.
                        popup_y.saturating_add(popup_height).saturating_sub(3)
                    } else {
                        // Primary button occupies the last inner row.
                        popup_y.saturating_add(popup_height).saturating_sub(2)
                    };
                    let label = if wizard.saved { "Open account" }
                        else if wizard.step == crate::setup::SetupStep::Identity { "Continue" }
                        else if wizard.authenticated { "Save and open account" }
                        else if wizard.login_failed { "Retry sign-in" }
                        else { "Continue to Browser Sign-In" };
                    let button_width = (if wizard.saved { 26 }
                        else { (label.chars().count() as u16).saturating_add(6) })
                        .min(popup_width.saturating_sub(2));
                    let button_x = popup_x.saturating_add(1)
                        .saturating_add(popup_width.saturating_sub(2).saturating_sub(button_width) / 2);
                    if mouse.row == button_y && mouse.column >= button_x
                        && mouse.column < button_x.saturating_add(button_width)
                        && !wizard.authenticating && !wizard.discovering {
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
        if self.show_startup_warnings || self.remove_account_confirmation.is_some() { return; }
        // The rightmost footer bell is an action as well as a status indicator.
        // Never steal clicks from compose, confirmation, or other open dialogs.
        if height > 0 && width >= crate::ui::NOTIFICATION_INDICATOR_WIDTH
            && mouse.row == height - 1 && mouse.column < width
            && mouse.column >= width - crate::ui::NOTIFICATION_INDICATOR_WIDTH
            && self.composer.is_none() && !self.attachment_picker_visible
            && !self.help_visible && !self.theme_picker_visible && self.confirmation.is_none()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.handle_command(Command::ToggleNotifications);
            return;
        }
        if mouse.row == 0 && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.composer.is_none() && !self.attachment_picker_visible
            && !self.help_visible && !self.theme_picker_visible && self.confirmation.is_none() {
            let mut left = 0usize;
            for index in 0..self.accounts.len() {
                let cells = unicode_width::UnicodeWidthStr::width(self.accounts[index].name.as_str()) + 2;
                if usize::from(mouse.column) >= left && usize::from(mouse.column) < left + cells {
                    self.select_account(index);
                    return;
                }
                left += cells;
                if index + 1 < self.accounts.len() { left += 2; }
            }
            if !self.startup_warnings.is_empty() {
                let label = format!("  ! {} config issue(s) [! to inspect]", self.startup_warnings.len());
                let len = unicode_width::UnicodeWidthStr::width(label.as_str());
                if (left..left + len).contains(&usize::from(mouse.column)) {
                    self.show_startup_warnings = true;
                    return;
                }
            }
        }
        if self.attachment_picker_visible {
            if self.help_visible { return; }
            if self.attachment_destination_picker.is_some() {
                let (_, _, files, _, footer) = crate::ui::attachment_destination_regions(
                    ratatui::layout::Rect::new(0, 0, width, height));
                // Only the [s] Save here button is actionable; clicking
                // the Cancel instructions must never unexpectedly write files.
                if mouse.row == footer.y && mouse.column >= footer.x
                    && mouse.column < footer.x.saturating_add(15)
                    && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.save_selected_attachments();
                    return;
                }
                let Some(picker) = self.attachment_destination_picker.as_mut() else { return; };
                match mouse.kind {
                    MouseEventKind::ScrollDown => picker.move_selection(3),
                    MouseEventKind::ScrollUp => picker.move_selection(-3),
                    MouseEventKind::Moved | MouseEventKind::Down(MouseButton::Left) => {
                        if mouse.column >= files.x && mouse.column < files.right()
                            && mouse.row >= files.y && mouse.row < files.bottom() {
                            let visible = usize::from(files.height);
                            let start = picker.selected.saturating_sub(visible / 2)
                                .min(picker.entries.len().saturating_sub(visible));
                            let next = start.saturating_add(usize::from(mouse.row - files.y))
                                .min(picker.entries.len().saturating_sub(1));
                            let already_selected = next == picker.selected;
                            if !already_selected {
                                picker.selected = next;
                                picker.refresh_preview();
                            } else if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                                // Hover selects; clicking a selected folder enters it.
                                if let Some(entry) = picker.entries.get(next) {
                                    if entry.is_directory {
                                        if let Err(error) = picker.enter_directory(entry.path.clone()) {
                                            self.status = StatusLine::error(format!("file browser: {error}"));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                return;
            }
            let (_, list, _, _) = crate::ui::attachment_regions(ratatui::layout::Rect::new(0, 0, width, height));
            let inside_list = mouse.column >= list.x && mouse.column < list.x.saturating_add(list.width)
                && mouse.row > list.y && mouse.row < list.y.saturating_add(list.height);
            if inside_list && matches!(mouse.kind,
                MouseEventKind::Moved | MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)) {
                let rows = usize::from(list.height.saturating_sub(1));
                let start = attachment_list_start(self.attachment_selection, self.attachment_files.len(), rows);
                self.move_attachment_selection(start.saturating_add(usize::from(mouse.row - list.y).saturating_sub(1)));
            }
            match mouse.kind {
                MouseEventKind::ScrollDown => self.move_attachment_selection(self.attachment_selection.saturating_add(3)),
                MouseEventKind::ScrollUp => self.move_attachment_selection(self.attachment_selection.saturating_sub(3)),
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
            if let Some(code) = key { self.handle_key(KeyEvent::new(code, KeyModifiers::NONE)); }
            self.mailbox_resizing = false;
            return;
        }
        if self.composer.is_some() || self.help_visible || self.theme_picker_visible
            || self.confirmation.is_some() {
            self.mailbox_resizing = false;
            self.preview_resizing = false;
            return;
        }
        let sidebar_width = if self.sidebar_visible {
            crate::ui::mailbox_column_width(width, self.mailbox_width_percent)
        } else { 0 };
        let separator = sidebar_width.saturating_sub(1);
        let right_width = width.saturating_sub(sidebar_width);
        let preview_separator = sidebar_width.saturating_add(right_width.saturating_sub(
            crate::ui::preview_column_width(right_width, self.preview_width_percent)
        ));
        if self.preview_split {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) if mouse.row > 0
                    && mouse.row < height.saturating_sub(1)
                    && mouse.column.abs_diff(preview_separator) <= 1 => {
                        self.preview_resizing = true;
                        self.mailbox_resizing = false;
                        return;
                    }
                MouseEventKind::Drag(MouseButton::Left) if self.preview_resizing => {
                    self.preview_width_percent = preview_percent_at(mouse.column, sidebar_width, right_width);
                    return;
                }
                MouseEventKind::Up(MouseButton::Left) if self.preview_resizing => {
                    self.preview_resizing = false;
                    self.preview_width_percent = preview_percent_at(mouse.column, sidebar_width, right_width);
                    let _ = persist_preview_width(self.preview_width_percent);
                    return;
                }
                _ => {}
            }
        }
        if !self.sidebar_visible { self.mailbox_resizing = false; return; }
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
            MouseEventKind::Down(MouseButton::Left) if mouse.row > 0 && mouse.row < height.saturating_sub(1)
                && mouse.column.abs_diff(separator) <= 1 => self.mailbox_resizing = true,
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
        if self.composer.as_ref().and_then(|composer| composer.file_picker.as_ref())
            .is_some_and(|picker| picker.smooth_target.is_some()) {
            return;
        }
        let candidate = self.composer.as_ref()
            .and_then(|composer| composer.file_picker.as_ref())
            .and_then(|picker| picker.entries.get(picker.selected)
                .map(|entry| (entry.path.clone(), &picker.preview)));
        let Some((path, ComposeFilePreview::Image(bytes))) = candidate else {
            self.compose_file_preview_image = None;
            self.compose_file_preview_identity = None;
            return;
        };
        let identity = (path, width, height);
        if self.compose_file_preview_identity.as_ref() == Some(&identity) { return; }
        self.compose_file_preview_identity = Some(identity);
        self.compose_file_preview_image = None;
        let Some(picker) = &self.image_picker else { return; };
        if let Ok(image) = image::load_from_memory(bytes) {
            let target = reader_image_target_size(image.width(), image.height(), None, None,
                picker.font_size().width, picker.font_size().height, width.max(1), height.max(1));
            self.compose_file_preview_image = picker.new_protocol(image, target,
                Resize::Fit(Some(image::imageops::FilterType::Lanczos3))).ok();
        }
    }

    #[must_use]
    pub fn compose_file_preview_image(&self) -> Option<&Protocol> {
        self.compose_file_preview_image.as_ref()
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
            account.page_request_id = None;
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
        let Some(composer) = self.composer.as_ref() else { return; };
        if composer.sending {
            if key.code == KeyCode::Esc {
                self.status = StatusLine::temporary("send is already in progress", Duration::from_secs(2));
            }
            return;
        }
        if composer.exit_prompt {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    if let Some(composer) = self.composer.as_mut() { composer.exit_prompt = false; }
                    self.postpone_composer();
                }
                KeyCode::Char('d') | KeyCode::Char('n') => {
                    self.composer = None;
                    self.resumed_draft_account = None;
                    self.status = StatusLine::temporary("unsent email discarded", Duration::from_secs(2));
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    if let Some(composer) = self.composer.as_mut() { composer.exit_prompt = false; }
                }
                _ => {}
            }
            return;
        }
        if composer.confirm_send {
            // The user reviews headers and the body before the final Send key.
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(composer) = self.composer.as_mut() { composer.confirm_send = false; }
                    self.send_composer();
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    if let Some(composer) = self.composer.as_mut() { composer.confirm_send = false; }
                }
                KeyCode::Char('t') => {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.preview_html = !composer.preview_html;
                        composer.preview_scroll = 0;
                    }
                }
                KeyCode::Char('O') => self.open_compose_browser_preview(),
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.preview_scroll = composer.preview_scroll.saturating_add(1);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.preview_scroll = composer.preview_scroll.saturating_sub(1);
                    }
                }
                KeyCode::Char('G') | KeyCode::End => {
                    if let Some(composer) = self.composer.as_mut() { composer.preview_scroll = usize::MAX; }
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    if let Some(composer) = self.composer.as_mut() { composer.preview_scroll = 0; }
                }
                KeyCode::PageDown => {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.preview_scroll = composer.preview_scroll.saturating_add(15);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(composer) = self.composer.as_mut() {
                        composer.preview_scroll = composer.preview_scroll.saturating_sub(15);
                    }
                }
                _ => {}
            }
            return;
        }
        if composer.help_visible {
            let composer = self.composer.as_mut().expect("composer exists");
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') if composer.help_query.is_empty() => {
                    composer.help_visible = false;
                }
                KeyCode::Esc => composer.help_visible = false,
                KeyCode::Backspace => {
                    composer.help_query.pop();
                    composer.help_selected = *composer.compose_help_matches().first().unwrap_or(&0);
                    composer.help_scroll = 0;
                }
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k')
                    if !key.modifiers.contains(KeyModifiers::CONTROL) && composer.help_query.is_empty() => {
                    let matches = composer.compose_help_matches();
                    if let Some(position) = matches.iter().position(|&i| i == composer.help_selected) {
                        let next = if matches!(key.code, KeyCode::Up | KeyCode::Char('k')) {
                            position.saturating_sub(1)
                        } else { (position + 1).min(matches.len().saturating_sub(1)) };
                        composer.help_selected = matches[next];
                        composer.help_scroll = next.saturating_sub(5);
                    }
                }
                KeyCode::Char(ch) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    composer.help_query.push(ch);
                    composer.help_selected = *composer.compose_help_matches().first().unwrap_or(&0);
                    composer.help_scroll = 0;
                }
                _ => {}
            }
            return;
        }
        if composer.attachment_manager_open {
            let composer = self.composer.as_mut().expect("composer exists");
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => composer.attachment_manager_open = false,
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    composer.attachment_manager_open = false;
                }
                KeyCode::Char('j') | KeyCode::Down => composer.attachment_manager_selected =
                    composer.attachment_manager_selected.saturating_add(1)
                        .min(composer.attachment_count().saturating_sub(1)),
                KeyCode::Char('k') | KeyCode::Up => composer.attachment_manager_selected =
                    composer.attachment_manager_selected.saturating_sub(1),
                KeyCode::Char('?') => {
                    composer.help_visible = true;
                    composer.help_query.clear();
                    composer.help_selected = 0;
                    composer.help_scroll = 0;
                }
                KeyCode::Char('x' | 'd') | KeyCode::Delete | KeyCode::Backspace => {
                    if let Some(path) = composer.remove_managed_attachment() {
                        self.status = StatusLine::temporary(
                            format!("detached {}", path.display()), Duration::from_secs(2));
                    }
                }
                KeyCode::Char('u') => {
                    if let Some(path) = composer.undo_managed_attachment() {
                        self.status = StatusLine::temporary(
                            format!("reattached {}", path.display()), Duration::from_secs(2));
                    }
                }
                _ => {}
            }
            return;
        }
        if composer.file_picker.is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
                self.composer.as_mut().expect("composer exists").file_picker = None;
                return;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let picker = self.composer.as_mut().and_then(|composer| composer.file_picker.as_mut());
                if let Some(picker) = picker {
                    match key.code {
                        KeyCode::Char('j') => picker.queue_smooth_scroll(8),
                        KeyCode::Char('k') => picker.queue_smooth_scroll(-8),
                        _ => {}
                    }
                }
                return;
            }
            match self.composer.as_mut().expect("composer exists").file_picker_key(key.code) {
                Ok(Some(message)) => self.status = StatusLine::temporary(message, Duration::from_secs(2)),
                Ok(None) => {},
                Err(error) => self.status = StatusLine::error(format!("file picker: {error:#}")),
            }
            return;
        }
        if composer.advanced_headers_open {
            let composer = self.composer.as_mut().expect("composer exists");
            match key.code {
                KeyCode::Esc | KeyCode::Enter => composer.toggle_advanced_headers(),
                KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => composer.toggle_advanced_headers(),
                KeyCode::Backspace => composer.backspace(),
                KeyCode::Delete => composer.delete_at_cursor(),
                KeyCode::Left => composer.move_horizontal(-1),
                KeyCode::Right => composer.move_horizontal(1),
                KeyCode::Home => composer.cursor = 0,
                KeyCode::End => composer.cursor = composer.char_count(),
                KeyCode::Char(ch) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => composer.insert_char(ch),
                _ => {},
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            let composer = self.composer.as_mut().expect("composer exists");
            composer.attachment_manager_open = true;
            composer.attachment_manager_selected = 0;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            if let Err(error) = self.composer.as_mut().expect("composer exists").open_file_picker() {
                self.status = StatusLine::error(format!("file picker: {error:#}"));
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.composer.as_mut().expect("composer exists").toggle_advanced_headers();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.postpone_composer();
            return;
        }
        let composer = self.composer.as_mut().expect("composer exists");
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('r') => composer.undo_edit(true),
                KeyCode::Char('j') if composer.field == ComposeField::Body => {
                    for _ in 0..5 { composer.move_line(1); }
                }
                KeyCode::Char('k') if composer.field == ComposeField::Body => {
                    for _ in 0..5 { composer.move_line(-1); }
                }
                _ => {}
            }
            return;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let next = if key.code == KeyCode::Tab {
                composer.field.next()
            } else { composer.field.previous() };
            composer.switch_field(next);
            return;
        }
        if composer.mode == ComposeMode::Insert {
            match key.code {
                KeyCode::Esc => {
                    composer.mode = ComposeMode::Normal;
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                }
                KeyCode::Enter if composer.field == ComposeField::Body => composer.insert_char('\n'),
                KeyCode::Enter => composer.switch_field(composer.field.next()),
                KeyCode::Backspace => composer.backspace(),
                KeyCode::Delete => composer.delete_at_cursor(),
                KeyCode::Left => composer.move_horizontal(-1),
                KeyCode::Right => composer.move_horizontal(1),
                KeyCode::Up if composer.field != ComposeField::Body => {
                    let previous = composer.field.previous();
                    if previous != ComposeField::Body { composer.switch_field(previous); }
                }
                KeyCode::Down if composer.field != ComposeField::Body => composer.switch_field(composer.field.next()),
                KeyCode::Up => composer.move_line(-1),
                KeyCode::Down => composer.move_line(1),
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::ALT) => composer.insert_char(ch),
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('?') {
            composer.help_visible = true;
            composer.help_query.clear();
            composer.help_selected = 0;
            composer.help_scroll = 0;
            composer.clear_pending();
            return;
        }

        // Multi-key Vim operations take precedence over simple motions.
        if let Some(forward) = composer.pending_find.take() {
            if let KeyCode::Char(ch) = key.code {
                let count = composer.count.take().unwrap_or(1);
                composer.find_char(ch, forward, count);
            }
            return;
        }
        if composer.pending_inner {
            composer.pending_inner = false;
            if let KeyCode::Char(delim) = key.code { composer.select_inner_object(delim); }
            return;
        }
        if composer.pending_g {
            composer.pending_g = false;
            if key.code == KeyCode::Char('g') { composer.cursor = 0; composer.count = None; return; }
        }
        if composer.pending_delete {
            composer.pending_delete = false;
            let count = composer.count.take().unwrap_or(1);
            match key.code {
                KeyCode::Char('d') => composer.delete_current_lines(count),
                KeyCode::Char('w') => composer.delete_words(count),
                KeyCode::Char('$') => {
                    let end = composer.line_end();
                    composer.delete_range(composer.cursor, end);
                }
                _ => {}
            }
            return;
        }
        if let KeyCode::Char(digit @ '1'..='9') = key.code {
            composer.count = Some(composer.count.unwrap_or(0)
                .saturating_mul(10).saturating_add(digit.to_digit(10).unwrap_or(0) as usize)
                .min(MAX_COUNT_PREFIX));
            return;
        }
        if key.code == KeyCode::Char('0') && composer.count.is_some() {
            composer.count = Some(composer.count.unwrap_or(0).saturating_mul(10).min(MAX_COUNT_PREFIX));
            return;
        }
        if matches!(key.code, KeyCode::Char('g')) { composer.pending_g = true; return; }
        if matches!(key.code, KeyCode::Char('f' | 'F')) {
            composer.pending_find = Some(key.code == KeyCode::Char('f'));
            return;
        }
        let count = composer.count.take().unwrap_or(1);
        // Header navigation is separate from body Vim motions.
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if composer.move_header_field(true, count) => return,
            KeyCode::Char('k') | KeyCode::Up if composer.move_header_field(false, count) => return,
            _ => {}
        }
        if composer.apply_motion(key.code, count) { return; }
        match composer.mode {
            ComposeMode::Normal => match key.code {
                KeyCode::Enter => {
                    if let Err(error) = composer.prepare_send_preview() {
                        self.status = StatusLine::error(format!("cannot preview outgoing email: {error:#}"));
                    }
                },
                KeyCode::Esc | KeyCode::Char('q') => {
                    if composer.has_unsent_content() {
                        composer.exit_prompt = true;
                    } else {
                        self.composer = None;
                    }
                }
                KeyCode::Char('i') => composer.mode = ComposeMode::Insert,
                KeyCode::Char('a') => { composer.move_horizontal(1); composer.mode = ComposeMode::Insert; }
                KeyCode::Char('I') => { composer.cursor = composer.first_nonblank(); composer.mode = ComposeMode::Insert; }
                KeyCode::Char('A') => { composer.cursor = composer.line_end(); composer.mode = ComposeMode::Insert; }
                KeyCode::Char('v') => {
                    composer.visual_anchor = Some(composer.cursor.min(composer.char_count()));
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Visual;
                }
                KeyCode::Char('V') => {
                    composer.visual_anchor = Some(composer.cursor.min(composer.char_count()));
                    composer.visual_linewise = true;
                    composer.mode = ComposeMode::Visual;
                }
                KeyCode::Char('u') => composer.undo_edit(false),
                KeyCode::Char('x') | KeyCode::Delete => for _ in 0..count { composer.delete_at_cursor(); },
                KeyCode::Char('D') => {
                    let end = composer.line_end();
                    composer.delete_range(composer.cursor, end);
                }
                KeyCode::Char('d') => { composer.pending_delete = true; composer.count = Some(count); }
                KeyCode::Char('o') if composer.field == ComposeField::Body => {
                    composer.open_line_below(); composer.mode = ComposeMode::Insert;
                }
                KeyCode::Char('O') if composer.field == ComposeField::Body => {
                    composer.open_line_above(); composer.mode = ComposeMode::Insert;
                }
                _ => {}
            },
            ComposeMode::Visual => match key.code {
                KeyCode::Esc | KeyCode::Char('v') => {
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Normal;
                }
                KeyCode::Char('V') => composer.visual_linewise = !composer.visual_linewise,
                KeyCode::Char('i') => composer.pending_inner = true,
                KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => {
                    if let Some((start, end)) = composer.visual_range() { composer.delete_range(start, end); }
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Normal;
                }
                KeyCode::Char('c') => {
                    if let Some((start, end)) = composer.visual_range() { composer.delete_range(start, end); }
                    composer.visual_anchor = None;
                    composer.visual_linewise = false;
                    composer.mode = ComposeMode::Insert;
                }
                _ => {}
            },
            ComposeMode::Insert => {}
        }
    }

    fn open_compose_browser_preview(&mut self) {
        let Some(composer) = self.composer.as_ref() else { return; };
        let html = match composer.outgoing_html_preview() {
            Ok(Some(html)) => html,
            Ok(None) => {
                // Plain-text-only emails still have a safe HTML browser projection.
                let escaped = composer.body.replace('&', "&amp;")
                    .replace('<', "&lt;").replace('>', "&gt;");
                format!("<html><meta charset=\"utf-8\"><body><pre style=\"white-space:pre-wrap\">{escaped}</pre></body></html>")
            }
            Err(error) => {
                self.status = StatusLine::error(format!("cannot preview HTML: {error:#}"));
                return;
            }
        };
        let result = (|| -> anyhow::Result<()> {
            let directory = dirs::cache_dir().unwrap_or_else(std::env::temp_dir)
                .join("aerc-rs").join("browser");
            std::fs::create_dir_all(&directory)?;
            let path = directory.join("compose-preview.html");
            std::fs::write(&path, html.as_bytes())?;
            open_url(path.to_string_lossy().as_ref())?;
            Ok(())
        })();
        match result {
            Ok(()) => self.status = StatusLine::temporary("opened outgoing email preview in browser", Duration::from_secs(2)),
            Err(error) => self.status = StatusLine::error(format!("browser preview: {error:#}")),
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
        // Always write a local recovery copy first. If IMAP is offline or the
        // Drafts APPEND fails, Ctrl+N can still recover the unsent message.
        let result = Self::postponed_drafts_root().and_then(|root| {
            crate::outgoing::drafts::save_draft(&root, &account.name, composer)
        });
        if let Err(error) = result {
            self.status = StatusLine::error(format!("could not save draft: {error:#}"));
            return;
        }
        let draft_mailbox = account.mailboxes.iter()
            .find(|mailbox| mailbox.role == MailboxRole::Drafts)
            .map(|mailbox| mailbox.name.clone());
        let draft_queued = if let Some(mailbox) = draft_mailbox {
            let message = match outgoing_from_composer(composer, &account.from) {
                Ok(message) => message,
                Err(error) => {
                    self.status = StatusLine::error(format!(
                        "saved local recovery copy, but could not prepare IMAP draft: {error:#}"));
                    return;
                }
            };
            if self.account().is_some_and(|account|
                account.worker.post(ActionKind::SaveDraft { message, mailbox }).is_ok()) {
                true
            } else {
                self.status = StatusLine::error(
                    "saved local recovery copy; Drafts worker is busy (Ctrl+n resumes)");
                return;
            }
        } else {
            false
        };
        self.composer = None;
        self.resumed_draft_account = None;
        self.status = if draft_queued {
            StatusLine::info("saved local recovery copy; saving to Drafts folder…")
        } else {
            StatusLine::error("saved locally only: no Drafts mailbox found; Ctrl+n resumes")
        };
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
        if let Err(error) = crate::outgoing::validate_recipients(&composer.to, &composer.cc, &composer.bcc) {
            self.status = StatusLine::error(format!("invalid recipient: {error:#}"));
            return;
        }
        let Some(account) = self.account() else { return; };
        let message = match outgoing_from_composer(composer, &account.from) {
            Ok(message) => message,
            Err(error) => {
                self.status = StatusLine::error(format!("cannot prepare email: {error:#}"));
                return;
            }
        };
        let copy_to = crate::outgoing::sent_copy_targets(account.copy_to.as_deref());
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
                copy_to,
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



/// Shared MIME-input assembly for SMTP and IMAP Drafts. The draft builder
/// allows unfinished addresses; the Send path validates them separately.
fn outgoing_from_composer(composer: &Composer, from: &str) -> anyhow::Result<OutgoingMessage> {
    let mut attachments = load_compose_attachments(&composer.attachments, &composer.attachment_paths)?;
    let custom_headers = parse_compose_headers(&composer.headers)?;
    let mut html_body = None;
    if let Some(raw) = composer.forwarded_raw.as_deref() {
        let parts = inline_forward_parts(raw)?;
        attachments.extend(parts.attachments);
        if let Some(html) = parts.html {
            let personal_note = composer.body.split(FORWARD_SEPARATOR).next().unwrap_or("");
            let original_headers = composer.body.split(FORWARD_SEPARATOR)
                .nth(1).and_then(|tail| tail.split("\n\n").next()).unwrap_or("");
            html_body = Some(forward_html(&html,
                &format!("{FORWARD_SEPARATOR}{original_headers}"), personal_note));
        }
    }
    Ok(OutgoingMessage {
        from: from.to_owned(), to: composer.to.clone(), cc: composer.cc.clone(),
        bcc: composer.bcc.clone(), subject: composer.subject.clone(), body: composer.body.clone(),
        html_body, attachments, custom_headers,
        in_reply_to: composer.in_reply_to.clone(), references: composer.references.clone(),
        forwarded_raw: None,
    })
}

fn parse_compose_headers(raw: &str) -> anyhow::Result<Vec<(String, String)>> {
    raw.split(';').filter(|part| !part.trim().is_empty()).map(|part| {
        let (name, value) = part.split_once(':')
            .ok_or_else(|| anyhow::anyhow!("use Header-Name: value; separated by semicolons"))?;
        Ok((name.trim().to_owned(), value.trim().to_owned()))
    }).collect()
}

fn load_compose_attachments(legacy: &str, selected: &[PathBuf]) -> anyhow::Result<Vec<OutgoingAttachment>> {
    let mut output = Vec::new();
    // Older postponed drafts used a semicolon-delimited string. New selections
    // are distinct paths, so filenames containing semicolons work correctly.
    for part in legacy.split(';').map(str::trim).filter(|part| !part.is_empty()) {
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
        anyhow::bail!("attachment must be a file of at most 16 MiB: {}", path.display());
    }
    let filename = path.file_name().and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("attachment has no valid UTF-8 filename"))?
        .to_owned();
    let content_type = if ["LICENSE", "COPYING", "Makefile", "Dockerfile", ".gitignore"]
        .iter().any(|name| filename.eq_ignore_ascii_case(name)) {
        "text/plain"
    } else { match path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "txt" | "patch" | "diff" | "log" | "md" | "rst" | "lock" | "rs"
        | "c" | "h" | "cpp" | "hpp" | "py" | "sh" | "bash" | "zsh" | "fish"
        | "ini" | "conf" | "cfg" | "gitignore" => "text/plain",
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
    }};
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
        || file.filename.rsplit('.').next().is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

fn received_attachment_is_image(file: &OutgoingAttachment) -> bool {
    file.content_type.starts_with("image/")
}

fn decode_received_attachment_preview(file: &OutgoingAttachment) -> Result<image::DynamicImage, String> {
    // No unbounded PDF subprocess inputs or huge speculative image decodes.
    if file.data.len() > 24 * 1024 * 1024 {
        return Err("preview over 24 MiB; press o to open the original".into());
    }
    let image_bytes: std::borrow::Cow<'_, [u8]> = if received_attachment_is_pdf(file) {
        let temp = tempfile::tempdir().map_err(|err| err.to_string())?;
        let input = temp.path().join("received.pdf");
        std::fs::write(&input, &file.data).map_err(|err| err.to_string())?;
        std::borrow::Cow::Owned(compose_pdf_thumbnail(&input).map_err(|err| err.to_string())?)
    } else { std::borrow::Cow::Borrowed(&file.data) };
    // Check declared dimensions before allocating decoded pixels. Huge
    // compressed images must not expand into gigabytes of cache data.
    let dimensions = image::ImageReader::new(std::io::Cursor::new(image_bytes.as_ref()))
        .with_guessed_format().map_err(|err| err.to_string())?
        .into_dimensions().map_err(|err| err.to_string())?;
    if u64::from(dimensions.0) * u64::from(dimensions.1) > 24_000_000 {
        return Err("image has too many pixels for automatic preview; press o to open".into());
    }
    let decoded = image::load_from_memory(image_bytes.as_ref())
        .map_err(|err| format!("attachment image decode: {err}"))?;
    // Cache a terminal-sized image, not an enormous decoded camera original.
    Ok(if decoded.width() > 1_200 || decoded.height() > 1_200 {
        decoded.thumbnail(1_200, 1_200)
    } else { decoded })
}

fn attachment_safe_name(name: &str, index: usize) -> String {
    let basename = name.rsplit(|ch| ch == '/' || ch == '\\').next().unwrap_or("");
    let safe = basename.chars().filter(|ch| !ch.is_control() && *ch != '/' && *ch != '\\')
        .collect::<String>();
    let safe = safe.trim().trim_matches('.');
    if safe.is_empty() { format!("attachment-{}.bin", index + 1) } else { safe.to_owned() }
}

fn expand_attachment_directory(directory: &str) -> PathBuf {
    let directory = directory.trim();
    if directory == "~" { return dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")); }
    if let Some(suffix) = directory.strip_prefix("~/") {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(suffix);
    }
    PathBuf::from(directory)
}

/// Create-only save with collision suffixes; existing files and symlinks are
/// never truncated. Duplicate MIME filenames are common in message threads.
fn save_attachment_without_overwrite(
    directory: &Path, file: &OutgoingAttachment, index: usize,
) -> anyhow::Result<PathBuf> {
    use std::fs::OpenOptions;
    let name = attachment_safe_name(&file.filename, index);
    let (stem, suffix) = name.rsplit_once('.')
        .filter(|(stem, suffix)| !stem.is_empty() && !suffix.is_empty())
        .map_or((name.as_str(), ""), |(stem, suffix)| (stem, suffix));
    for attempt in 0..10_000 {
        let candidate = if attempt == 0 { name.clone() }
            else if suffix.is_empty() { format!("{stem} ({attempt})") }
            else { format!("{stem} ({attempt}).{suffix}") };
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
    selected.saturating_sub(visible / 2).min(count.saturating_sub(visible))
}

fn mouse_width_percent(column: u16, total: u16) -> u16 {
    if total == 0 { return 22; }
    (((u32::from(column) + 1) * 100 / u32::from(total)) as u16).clamp(12, 50)
}

fn preview_percent_at(column: u16, sidebar_width: u16, remaining: u16) -> u16 {
    if remaining == 0 { return 57; }
    let inside = column.saturating_sub(sidebar_width).min(remaining);
    (100 - (u32::from(inside) * 100 / u32::from(remaining)) as u16).clamp(25, 75)
}

fn preview_width_state_file() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join("aerc-rs").join("preview-width"))
}

fn stored_preview_width() -> Option<u16> {
    std::fs::read_to_string(preview_width_state_file()?).ok()?
        .trim().parse::<u16>().ok().filter(|value| (25..=75).contains(value))
}

fn persist_preview_width(width: u16) -> std::io::Result<()> {
    let Some(path) = preview_width_state_file() else { return Ok(()); };
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::write(path, format!("{}\n", width.clamp(25, 75)))
}

fn mailbox_width_state_file() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join("aerc-rs").join("mailbox-width"))
}

fn stored_mailbox_width() -> Option<u16> {
    std::fs::read_to_string(mailbox_width_state_file()?).ok()?
        .trim().parse::<u16>().ok().filter(|value| (12..=50).contains(value))
}

fn persist_mailbox_width(width: u16) -> std::io::Result<()> {
    let Some(path) = mailbox_width_state_file() else { return Ok(()); };
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::write(path, format!("{}\n", width.clamp(12, 50)))
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
        cursor_row.saturating_add(scrolloff + 1)
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
        bounded_index, inner_text_object_range, message_view_offset, offset_index,
        reader_scroll_for_visual_cursor,
        preview_neighbor_positions, preview_travel_positions, reader_image_target_size, reader_selection_text,
        reader_word_target, reply_attribution, url_at_cursor,
    };
    use crate::model::{MessageFlags, MessageId, MessageSummary};
    use ratatui::layout::Size;
    use std::path::PathBuf;

    #[test]
    fn mail_search_matches_typos_as_subsequences_but_ranks_contiguous_first() {
        let query = "calinv";
        assert!(super::mail_fuzzy_score("Calendar invitation", query).is_some());
        assert!(super::mail_fuzzy_score("Invoice", query).is_none());
        assert!(super::mail_fuzzy_score("calendar", "cal")
            > super::mail_fuzzy_score("c a l service", "cal"));
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
            scroll = reader_scroll_for_visual_cursor(
                scroll, visual_cursor_row, view_height, max_scroll,
            );
            assert!(visual_cursor_row >= scroll,
                "cursor scrolled above viewport at row {visual_cursor_row}");
            assert!(visual_cursor_row < scroll + view_height,
                "cursor disappeared below viewport at row {visual_cursor_row}");
        }
        for visual_cursor_row in (12..145).rev() {
            scroll = reader_scroll_for_visual_cursor(
                scroll, visual_cursor_row, view_height, max_scroll,
            );
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
        let config: crate::config::Config = toml::from_str(
            "[[accounts]]\nname = \"Test\"\nsource = \"maildir:///tmp/mail\""
        ).unwrap();
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
        let config: crate::config::Config = toml::from_str(
            "[[accounts]]\nname = \"Test\"\nsource = \"maildir:///tmp/mail\""
        ).unwrap();
        let mut app = super::App::new(&config, vec![]);
        app.attachment_picker_visible = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));
        assert!(app.help_visible);
        assert!(app.attachment_picker_visible);
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.help_query, "s");
        assert!(app.help_matches().iter().any(|&index|
            super::ATTACHMENT_HELP_ENTRIES[index].description.contains("save")));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.help_visible);
        assert!(app.attachment_picker_visible);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.attachment_picker_visible);
    }

    #[test]
    fn mute_toggle_drops_queued_alerts_and_restores_desktop_notifications() {
        let config: crate::config::Config = toml::from_str(
            "[ui]\nnew_mail_notifications = true\nnew_mail_bell = true"
        ).expect("valid config");
        let mut app = super::App::new(&config, vec![]);
        app.new_mail_notices.push_back(super::NewMailNotice {
            account: "University".into(), count: 1, sender: String::new(),
            subject: "test".into(), excerpt: String::new(),
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
        assert_eq!(preview_neighbor_positions(5, 10, 1, false), vec![6, 7, 8, 9, 4, 3, 2, 1, 0]);
        assert_eq!(preview_neighbor_positions(5, 10, -1, false), vec![4, 3, 2, 1, 0, 6, 7, 8, 9]);
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
        assert_eq!(positions.len(), positions.iter().copied().collect::<std::collections::HashSet<_>>().len());
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
            plain_text: Some("plain".into()), plain_fallback: None,
            html_source: Some("<p>html</p>".into()), html_text: Some("html".into()),
            html_lines: Some(vec![HtmlLine { spans: vec![HtmlSpan {
                text: "rich".into(),
                style: HtmlSpanStyle { link: Some("https://example.org".into()), ..Default::default() },
            }] }]),
            html_renderer: None, html_render_width: 80,
            attachment_count: 0, attachment_names: Vec::new(),
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
        assert_eq!(composer.remove_managed_attachment(), Some(PathBuf::from("/tmp/old-a.txt")));
        assert_eq!(composer.attachment_count(), 1);
        assert_eq!(composer.undo_managed_attachment(), Some(PathBuf::from("/tmp/old-a.txt")));
        assert_eq!(composer.attachments, "/tmp/old-a.txt;/tmp/old-b.txt");
    }

    #[test]
    fn closing_filled_composer_requires_explicit_draft_choice() {
        let mut composer = Composer::new();
        assert!(!composer.has_unsent_content());
        composer.to = "someone@example.org".to_owned();
        assert!(composer.has_unsent_content());
        composer.to.clear();
        composer.attachment_paths.push("/tmp/some-attachment.pdf".into());
        assert!(composer.has_unsent_content());
        composer.exit_prompt = true;
        let restored: Composer = serde_json::from_slice(&serde_json::to_vec(&composer).unwrap()).unwrap();
        assert!(!restored.exit_prompt);
    }

    #[test]
    fn smooth_picker_scroll_only_refreshes_preview_at_destination() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..12 {
            std::fs::write(temp.path().join(format!("file-{index:02}.txt")),
                format!("file content {index}")).unwrap();
        }
        let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
        let first = match &picker.preview {
            super::ComposeFilePreview::Text { lines, .. } => lines.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        picker.queue_smooth_scroll(8);
        for _ in 0..7 { picker.advance_smooth_scroll(); }
        assert!(picker.smooth_target.is_some());
        assert!(matches!(&picker.preview,
            super::ComposeFilePreview::Text { lines, .. } if lines == &first));
        picker.advance_smooth_scroll();
        assert_eq!(picker.selected, 8);
        assert!(picker.smooth_target.is_none());
        assert!(matches!(&picker.preview,
            super::ComposeFilePreview::Text { lines, .. } if lines != &first));
    }

    #[test]
    fn picker_has_parent_and_file_preview_and_count_navigation() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("alpha")).unwrap();
        std::fs::write(temp.path().join("hello.txt"), b"hello from aerc").unwrap();
        let mut picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
        let index = picker.entries.iter().position(|entry| entry.name == "hello.txt").unwrap();
        picker.move_selection(index as isize);
        assert!(matches!(picker.preview, super::ComposeFilePreview::Text { .. }));
        assert!(!picker.parent_entries.is_empty());
        let mut composer = Composer::new();
        composer.file_picker = Some(picker);
        composer.file_picker_key(crossterm::event::KeyCode::Char('g')).unwrap();
        composer.file_picker_key(crossterm::event::KeyCode::Char('g')).unwrap();
        assert_eq!(composer.file_picker.as_ref().unwrap().selected, 0);
        composer.file_picker_key(crossterm::event::KeyCode::Char('G')).unwrap();
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
        let file_index = picker.entries.iter().position(|entry| entry.name == "report;v2.txt").unwrap();
        picker.selected = file_index;
        let mut composer = Composer::new();
        composer.file_picker = Some(picker);
        composer.file_picker_key(crossterm::event::KeyCode::Char(' ')).unwrap();
        assert_eq!(composer.attachment_count(), 1);
        let loaded = super::load_compose_attachments("", &composer.attachment_paths).unwrap();
        assert_eq!(loaded[0].filename, "report;v2.txt");
        assert_eq!(loaded[0].data.as_slice(), b"attachment test");
        // Space on the final file does not jump back to the previous item.
        composer.file_picker.as_mut().unwrap().selected = file_index;
        composer.file_picker_key(crossterm::event::KeyCode::Char(' ')).unwrap();
        assert_eq!(composer.attachment_count(), 0);
    }

    #[test]
    fn picker_visual_range_attaches_files_after_count_motion() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..7 { std::fs::write(temp.path().join(format!("{index}.txt")), b"hi").unwrap(); }
        let mut composer = Composer::new();
        composer.file_picker = Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
        let key = |ch| crossterm::event::KeyCode::Char(ch);
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('5')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        assert_eq!(composer.file_picker.as_ref().unwrap().selected, 5);
        assert_eq!(composer.file_picker.as_ref().unwrap().visual_anchor, Some(0));
        composer.file_picker_key(crossterm::event::KeyCode::Enter).unwrap();
        assert_eq!(composer.attachment_count(), 6);
        assert!(composer.file_picker.as_ref().unwrap().visual_anchor.is_none());
        // Re-selecting the same range does not add duplicates.
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('5')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer.file_picker_key(crossterm::event::KeyCode::Enter).unwrap();
        assert_eq!(composer.attachment_count(), 6);
    }

    #[test]
    fn picker_space_advances_and_stays_at_end_and_search_help_is_local() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] { std::fs::write(temp.path().join(name), b"ok").unwrap(); }
        let mut composer = Composer::new();
        composer.file_picker = Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
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
        composer.file_picker_key(crossterm::event::KeyCode::Esc).unwrap();
        assert!(!composer.file_picker.as_ref().unwrap().help_visible);
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(crossterm::event::KeyCode::Esc).unwrap();
        assert!(composer.file_picker.as_ref().unwrap().visual_anchor.is_none());
    }

    #[test]
    fn picker_slash_search_accepts_current_match_and_tab_opens_folder() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["photos-2025", "photos-2026", "notes"] {
            std::fs::create_dir(temp.path().join(name)).unwrap();
        }
        std::fs::write(temp.path().join("photos.txt"), b"hello").unwrap();
        let mut composer = Composer::new();
        composer.file_picker = Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
        let key = |ch| crossterm::event::KeyCode::Char(ch);
        composer.file_picker_key(key('/')).unwrap();
        for ch in "photos".chars() { composer.file_picker_key(key(ch)).unwrap(); }
        let picker = composer.file_picker.as_ref().unwrap();
        assert_eq!(picker.search_matches().len(), 2); // folder results take precedence
        assert_eq!(picker.entries[picker.selected].name, "photos-2025");
        composer.file_picker_key(crossterm::event::KeyCode::Down).unwrap();
        let picker = composer.file_picker.as_ref().unwrap();
        assert_eq!(picker.entries[picker.selected].name, "photos-2026");
        composer.file_picker_key(crossterm::event::KeyCode::Enter).unwrap();
        let picker = composer.file_picker.as_ref().unwrap();
        assert_eq!(picker.entries[picker.selected].name, "photos-2026");
        assert!(picker.search_query.is_none());
        // Enter should return to normal motion, so l opens the highlighted folder.
        composer.file_picker_key(key('l')).unwrap();
        let picker = composer.file_picker.as_ref().unwrap();
        assert!(picker.directory.ends_with("photos-2026"));
        composer.file_picker_key(key('/')).unwrap();
        composer.file_picker_key(crossterm::event::KeyCode::Esc).unwrap();
        assert!(composer.file_picker.as_ref().unwrap().search_query.is_none());
        assert!(composer.file_picker.is_some());
    }

    #[test]
    fn picker_visual_x_detaches_multiple_files_without_touching_others() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(temp.path().join(name), b"ok").unwrap();
        }
        let mut composer = Composer::new();
        composer.file_picker = Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
        let key = |ch| crossterm::event::KeyCode::Char(ch);
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('2')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer.file_picker_key(crossterm::event::KeyCode::Enter).unwrap();
        assert_eq!(composer.attachment_count(), 3);
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer.file_picker_key(key('x')).unwrap();
        assert_eq!(composer.attachment_count(), 1);
        assert_eq!(composer.attachment_paths[0].file_name().unwrap(), "c.txt");
        assert!(composer.file_picker.as_ref().unwrap().visual_anchor.is_none());
    }

    #[test]
    fn picker_visual_space_detaches_only_when_every_file_is_attached() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("0folder")).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(temp.path().join(name), b"hello").unwrap();
        }
        let mut composer = Composer::new();
        composer.file_picker = Some(super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap());
        let key = |ch| crossterm::event::KeyCode::Char(ch);
        assert!(composer.file_picker.as_ref().unwrap().entries[0].is_directory);
        // Attach a and b while selecting a directory too (which must be ignored).
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('2')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer.file_picker_key(crossterm::event::KeyCode::Enter).unwrap();
        assert_eq!(composer.attachment_count(), 2);
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('g')).unwrap();
        composer.file_picker_key(key('v')).unwrap();
        composer.file_picker_key(key('2')).unwrap();
        composer.file_picker_key(key('j')).unwrap();
        composer.file_picker_key(key(' ')).unwrap();
        assert_eq!(composer.attachment_count(), 0); // folder is ignored
        assert!(composer.file_picker.as_ref().unwrap().visual_anchor.is_none());

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
        assert!(matches!(picker.preview, super::ComposeFilePreview::Loading(_)));
        assert!(picker.pdf_preview_due.is_some());
        // Navigate away before the debounce expires: no converter was started.
        picker.move_selection(1);
        assert!(picker.pdf_preview_due.is_none());
        assert!(picker.pdf_preview_inflight.is_empty());
        assert!(matches!(picker.preview, super::ComposeFilePreview::Text { .. }));
        // Pretend an earlier conversion completed while the selection changed.
        picker.pdf_preview_inflight.insert(pdf.clone());
        picker.pdf_preview_results.lock().unwrap().push_back((pdf.clone(), Ok(vec![7, 8, 9])));
        picker.advance_pdf_preview();
        assert!(matches!(picker.preview, super::ComposeFilePreview::Text { .. }));
        assert!(picker.pdf_preview_inflight.is_empty());
        picker.move_selection(-1);
        assert!(matches!(picker.preview, super::ComposeFilePreview::Image(ref bytes) if bytes == &[7, 8, 9]));
        assert!(picker.pdf_preview_due.is_none());
    }

    #[test]
    fn manage_attachments_removes_files_from_unrelated_directories() {
        let mut composer = Composer::new();
        let first = std::path::PathBuf::from("/tmp/one/first.txt");
        let second = std::path::PathBuf::from("/home/example/elsewhere/second.txt");
        composer.attachment_paths = vec![first, second.clone()];
        composer.attachment_manager_selected = 0;
        assert_eq!(composer.remove_managed_attachment().unwrap().file_name().unwrap(), "first.txt");
        assert_eq!(composer.attachment_paths, vec![second]);
        assert_eq!(composer.attachment_manager_selected, 0);
        composer.attachments = "/tmp/legacy/old.txt".to_owned();
        composer.attachment_manager_selected = 1;
        assert_eq!(composer.remove_managed_attachment().unwrap().file_name().unwrap(), "old.txt");
        assert_eq!(composer.attachment_count(), 1);
    }

    #[test]
    fn picker_recognizes_image_files_for_terminal_preview() {
        let temp = tempfile::tempdir().unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1).write_to(&mut png, image::ImageFormat::Png).unwrap();
        std::fs::write(temp.path().join("photo.png"), png.into_inner()).unwrap();
        let picker = super::ComposeFilePicker::open(temp.path().to_path_buf()).unwrap();
        match &picker.preview {
            super::ComposeFilePreview::Image(bytes) => assert!(image::load_from_memory(bytes).is_ok()),
            preview => panic!("expected a decodable image, got {preview:?}"),
        }
        assert!(super::compose_image_extension(&temp.path().join("a.WEBP")));
    }

    #[test]
    fn picker_detects_known_source_file_mime_types() {
        let temp = tempfile::tempdir().unwrap();
        for (name, expected) in [("Cargo.toml", "application/toml"),
            ("Cargo.lock", "text/plain"), ("lib.rs", "text/plain"),
            ("sample.json", "application/json"), ("LICENSE", "text/plain")] {
            let path = temp.path().join(name);
            std::fs::write(&path, "content").unwrap();
            assert_eq!(super::load_compose_file(&path).unwrap().content_type, expected);
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
            cc: String::new(), bcc: String::new(),
            subject: "attachment smoke test".to_owned(),
            body: "See the attachment".to_owned(),
            html_body: None, attachments: files,
            custom_headers: Vec::new(), in_reply_to: None,
            references: Vec::new(), forwarded_raw: None,
        };
        let raw = crate::outgoing::build_outgoing(&message, "sender@example.com")
            .unwrap().formatted();
        let wire = String::from_utf8_lossy(&raw);
        assert!(wire.contains("multipart/mixed"));
        assert!(wire.contains("hello.txt"));
        assert!(wire.contains("Content-Disposition: attachment"));
    }

    #[test]
    fn compose_selected_file_paths_survive_postponed_draft_encoding() {
        let mut composer = Composer::new();
        composer.attachment_paths.push(std::path::PathBuf::from("/tmp/a file.pdf"));
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
            super::super::input::COMPOSE_HELP_ENTRIES[index].description.contains("attachment")
        }));
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

#[cfg(test)]
mod received_attachment_browser_tests {
    use super::*;

    fn attachment(name: &str, bytes: &[u8]) -> OutgoingAttachment {
        OutgoingAttachment { filename: name.into(), content_type: "text/plain".into(), data: bytes.to_vec() }
    }

    #[test]
    fn received_attachment_filenames_cannot_escape_destination() {
        assert_eq!(attachment_safe_name("../../important.txt", 0), "important.txt");
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
        let first = save_attachment_without_overwrite(directory.path(), &attachment("report.txt", b"first"), 0).unwrap();
        let second = save_attachment_without_overwrite(directory.path(), &attachment("../../report.txt", b"second"), 1).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(directory.path().join("report.txt")).unwrap(), "existing");
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
