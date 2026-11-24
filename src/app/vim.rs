//! Shared cursor motions and identity-stable bulk selection.
//!
//! Compose and Contacts share pending Vim commands and bounded history while
//! keeping independent field validation and editing operations.

use std::collections::{HashSet, VecDeque};

/// The Vim key sequences shared by text editors. Editing actions remain with
/// the owning surface (single-line Contacts vs. multiline Compose).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VimPrefix {
    Goto,
    Delete,
    Yank,
    Inner,
    FindForward,
    FindBackward,
}

/// Normal/Visual-mode input prefix state. A single pending command is possible
/// at a time; storing it as one enum prevents impossible combinations (e.g.
/// pending Delete and Yank simultaneously). Counts are bounded at input time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct VimSequence {
    command: Option<VimPrefix>,
    count: Option<usize>,
}

impl VimSequence {
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn start(&mut self, command: VimPrefix) {
        self.command = Some(command);
    }

    pub(crate) fn take_command(&mut self) -> Option<VimPrefix> {
        self.command.take()
    }

    pub(crate) fn has_operator(&self) -> bool {
        matches!(self.command, Some(VimPrefix::Delete | VimPrefix::Yank))
    }

    /// `0` without an existing count remains a normal Vim motion.
    pub(crate) fn push_digit(&mut self, digit: char, max: usize) -> bool {
        let Some(value) = digit.to_digit(10) else {
            return false;
        };
        if value == 0 && self.count.is_none() {
            return false;
        }
        self.count = Some(
            self.count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(value as usize)
                .min(max),
        );
        true
    }

    pub(crate) fn set_count(&mut self, count: usize) {
        self.count = Some(count);
    }

    pub(crate) fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }

    pub(crate) fn discard_count(&mut self) {
        self.count = None;
    }
}

/// Read-only motion vocabulary shared by the message reader and outgoing review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadMotion {
    Pending,
    Vertical(isize),
    Horizontal(isize),
    Smooth(isize),
    Jump(usize),
    FirstColumn,
    FirstNonBlank,
    LastColumn,
    Word { backward: bool, count: usize },
    Paragraph { forward: bool, count: usize },
}

/// Keep both readers on the same key/count/prefix semantics. The caller owns
/// layout and content, and handles non-motion actions separately.
pub(crate) fn read_motion(
    key: crossterm::event::KeyEvent,
    count: &mut Option<usize>,
    pending_g: &mut bool,
    rows: usize,
    height: usize,
    smooth_rows: isize,
    max_count: usize,
) -> Option<ReadMotion> {
    use ReadMotion::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    let take = |count: &mut Option<usize>| count.take().unwrap_or(1).max(1);
    let signed = |n: usize| isize::try_from(n).unwrap_or(isize::MAX);
    if key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::SHIFT)
    {
        *pending_g = false;
        return match key.code {
            KeyCode::Char('j' | 'k') => {
                *count = None;
                Some(Smooth(if key.code == KeyCode::Char('j') {
                    smooth_rows
                } else {
                    -smooth_rows
                }))
            }
            KeyCode::Char('d' | 'u') => {
                let n = signed((height / 2).max(1).saturating_mul(take(count)));
                Some(Vertical(if key.code == KeyCode::Char('d') {
                    n
                } else {
                    -n
                }))
            }
            _ => None,
        };
    }
    if std::mem::take(pending_g) && key.code == KeyCode::Char('g') {
        return Some(Jump(take(count).saturating_sub(1)));
    }
    Some(match key.code {
        KeyCode::Char('0') if count.is_none() => FirstColumn,
        KeyCode::Char(ch) if ch.is_ascii_digit() => {
            *count = Some(
                count
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(ch.to_digit(10).unwrap_or_default() as usize)
                    .min(max_count),
            );
            Pending
        }
        KeyCode::Char('g') => {
            *pending_g = true;
            Pending
        }
        KeyCode::Char('G') => Jump(
            count
                .take()
                .map_or(rows.saturating_sub(1), |n| n.saturating_sub(1)),
        ),
        KeyCode::Char('j') | KeyCode::Down => Vertical(signed(take(count))),
        KeyCode::Char('k') | KeyCode::Up => Vertical(-signed(take(count))),
        KeyCode::Char('h') | KeyCode::Left => Horizontal(-signed(take(count))),
        KeyCode::Char('l') | KeyCode::Right => Horizontal(signed(take(count))),
        KeyCode::Char('w' | 'b') => Word {
            backward: key.code == KeyCode::Char('b'),
            count: take(count),
        },
        KeyCode::Char('{' | '}') => Paragraph {
            forward: key.code == KeyCode::Char('}'),
            count: take(count),
        },
        KeyCode::Char('^') => FirstNonBlank,
        KeyCode::Char('H') | KeyCode::Home => FirstColumn,
        KeyCode::Char('$' | 'L') | KeyCode::End => LastColumn,
        KeyCode::PageDown => Smooth(signed(height.max(1).saturating_mul(take(count)))),
        KeyCode::PageUp => Smooth(-signed(height.max(1).saturating_mul(take(count)))),
        _ => return None,
    })
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
pub(crate) enum ReaderWordMotion {
    NextStart,
    PreviousStart,
}

impl ReaderSelection {
    #[must_use]
    pub(crate) fn from_anchor(
        anchor: ReaderPosition,
        cursor: ReaderPosition,
        linewise: bool,
    ) -> Self {
        if linewise {
            return Self {
                start: ReaderPosition {
                    line: anchor.line.min(cursor.line),
                    column: 0,
                },
                end: ReaderPosition {
                    line: anchor.line.max(cursor.line),
                    column: usize::MAX,
                },
            };
        }
        Self {
            start: anchor.min(cursor),
            end: anchor.max(cursor),
        }
    }

    #[must_use]
    pub fn columns_for_line(self, line: usize, line_len: usize) -> Option<(usize, usize)> {
        if line < self.start.line || line > self.end.line {
            return None;
        }
        let start = if line == self.start.line {
            self.start.column
        } else {
            0
        };
        let end = if line == self.end.line {
            self.end.column
        } else {
            usize::MAX
        };
        // A linewise selection uses the same actual-character range as a
        // characterwise selection, never the renderer's terminal padding.
        visual_chars(start, end, line_len).map(|(start, end)| (start, end - 1))
    }
}

/// Reuse the same Unicode character-based word motion as composing and
/// contact editing. Reader motions can cross line boundaries without building
/// a `Vec<char>` or `Vec<&str>` for every keystroke.
pub(crate) fn reader_word_target(
    text: &str,
    line: usize,
    column: usize,
    motion: ReaderWordMotion,
) -> Option<ReaderPosition> {
    let current = text.lines().nth(line)?;
    let length = current.chars().count();
    let at = column.min(length);
    match motion {
        ReaderWordMotion::NextStart => {
            let next = word_start(current, at, false, false);
            if next < length && current.chars().nth(next).is_some_and(is_word_char) {
                return Some(ReaderPosition { line, column: next });
            }
            text.lines()
                .enumerate()
                .skip(line + 1)
                .find_map(|(line, text)| {
                    text.chars()
                        .position(is_word_char)
                        .map(|column| ReaderPosition { line, column })
                })
        }
        ReaderWordMotion::PreviousStart => {
            if column > 0 {
                let previous = word_start(current, at, true, false);
                if previous < length && current.chars().nth(previous).is_some_and(is_word_char) {
                    return Some(ReaderPosition {
                        line,
                        column: previous,
                    });
                }
            }
            text.lines()
                .enumerate()
                .take(line)
                .filter_map(|(line, text)| {
                    let length = text.chars().count();
                    let column = word_start(text, length, true, false);
                    (column < length && text.chars().nth(column).is_some_and(is_word_char))
                        .then_some(ReaderPosition { line, column })
                })
                .last()
        }
    }
}

/// Paragraph navigation shared by incoming and outgoing readers.
pub(crate) fn reader_paragraph_target(text: &str, current: usize, forward: bool) -> usize {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return 0;
    }
    let current = current.min(lines.len().saturating_sub(1));
    if forward {
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
                (0..blank)
                    .rev()
                    .find(|index| !lines[*index].trim().is_empty())
            })
            .unwrap_or(0)
    }
}

/// Extract a visual selection using shared UTF-8-safe Vim ranges, without
/// allocating a temporary vector of chars for each selected line.
pub(crate) fn reader_selection_text(text: &str, selection: ReaderSelection) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() || selection.start.line >= lines.len() {
        return String::new();
    }
    let end_line = selection.end.line.min(lines.len() - 1);
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
        if let Some((start, end)) = selection.columns_for_line(line_index, line.chars().count())
            && let Some(slice) = text_range(line, start, end + 1)
        {
            out.push_str(slice);
        }
    }
    out
}

/// Shared read-only Vim state. The owning surface supplies the text and row
/// count, and decides how a logical cursor maps to its rendered viewport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReaderNavigation {
    pub(crate) row: usize,
    pub(crate) column: usize,
    pub(crate) preferred_column: Option<usize>,
    pub(crate) smooth_target: Option<usize>,
    pub(crate) visual_anchor: Option<ReaderPosition>,
    pub(crate) visual_linewise: bool,
    pub(crate) pending_g: bool,
}

impl ReaderNavigation {
    fn line(text: &str, row: usize) -> &str {
        text.split('\n')
            .nth(row)
            .unwrap_or_default()
            .trim_end_matches('\r')
    }

    fn line_len(text: &str, row: usize) -> usize {
        Self::line(text, row).chars().count()
    }

    pub(crate) fn clamp_column(&mut self, text: &str) {
        self.column = self
            .column
            .min(Self::line_len(text, self.row).saturating_sub(1));
    }

    pub(crate) fn clamp(&mut self, rows: usize) {
        self.row = self.row.min(rows.saturating_sub(1));
        self.smooth_target = self
            .smooth_target
            .map(|target| target.min(rows.saturating_sub(1)))
            .filter(|target| *target != self.row);
    }

    pub(crate) fn clamp_document(&mut self, text: &str, rows: usize) {
        self.clamp(rows);
        if rows == 0 {
            self.column = 0;
            self.preferred_column = None;
            self.clear_visual();
            return;
        }
        self.clamp_column(text);
        if let Some(anchor) = self.visual_anchor.as_mut() {
            anchor.line = anchor.line.min(rows - 1);
            anchor.column = anchor
                .column
                .min(Self::line_len(text, anchor.line).saturating_sub(1));
        }
    }

    pub(crate) fn jump(&mut self, row: usize, rows: usize) {
        self.row = row.min(rows.saturating_sub(1));
        self.smooth_target = None;
    }

    pub(crate) fn active(&self) -> bool {
        self.smooth_target.is_some()
    }

    pub(crate) fn queue(&mut self, rows: usize, delta: isize) {
        self.clamp(rows);
        let origin = self.smooth_target.unwrap_or(self.row);
        let target = crate::app::navigation::bounded_index(origin, rows, delta);
        self.smooth_target = (target != self.row).then_some(target);
        self.preferred_column = Some(self.preferred_column.unwrap_or(self.column));
    }

    /// Advance exactly one logical row; callers follow the cursor using their
    /// own layout (incoming logical lines vs. outgoing wrapped rows).
    pub(crate) fn advance(&mut self, text: &str, rows: usize) -> bool {
        self.clamp(rows);
        let Some(target) = self.smooth_target else {
            return false;
        };
        self.row = self
            .row
            .saturating_add_signed(if target > self.row { 1 } else { -1 });
        self.column = self.preferred_column.unwrap_or(self.column);
        self.clamp_column(text);
        if target == self.row {
            self.smooth_target = None;
        }
        true
    }

    pub(crate) fn clear_visual(&mut self) {
        self.visual_anchor = None;
        self.visual_linewise = false;
    }

    pub(crate) fn toggle_visual(&mut self, linewise: bool, rows: usize) {
        if rows == 0 {
            return;
        }
        if self.visual_anchor.is_some() && self.visual_linewise == linewise {
            self.clear_visual();
        } else {
            self.visual_anchor.get_or_insert(ReaderPosition {
                line: self.row,
                column: self.column,
            });
            self.visual_linewise = linewise;
        }
        self.pending_g = false;
    }

    pub(crate) fn visual_range(&self) -> Option<ReaderSelection> {
        Some(ReaderSelection::from_anchor(
            self.visual_anchor?,
            ReaderPosition {
                line: self.row,
                column: self.column,
            },
            self.visual_linewise,
        ))
    }

    /// Execute the same motions in both readers. Return whether the surface
    /// should follow the logical cursor (word motions may move within a wrap).
    pub(crate) fn execute(&mut self, motion: ReadMotion, text: &str, rows: usize) -> bool {
        use crate::app::navigation::bounded_index;
        use ReadMotion::*;
        if let Smooth(delta) = motion {
            self.queue(rows, delta);
            return false;
        }
        self.smooth_target = None;
        if rows == 0 || motion == Pending {
            return false;
        }
        self.clamp(rows);
        let follow = match motion {
            Vertical(delta) => {
                let preferred = self.preferred_column.unwrap_or(self.column);
                self.row = bounded_index(self.row, rows, delta);
                self.column = preferred;
                self.preferred_column = Some(preferred);
                true
            }
            Horizontal(delta) => {
                self.column = bounded_index(self.column, Self::line_len(text, self.row), delta);
                self.preferred_column = None;
                false
            }
            Jump(row) => {
                self.jump(row, rows);
                self.preferred_column = None;
                true
            }
            FirstColumn | FirstNonBlank | LastColumn => {
                self.column = match motion {
                    FirstColumn => 0,
                    FirstNonBlank => Self::line(text, self.row)
                        .chars()
                        .position(|ch| !ch.is_whitespace())
                        .unwrap_or(0),
                    _ => Self::line_len(text, self.row).saturating_sub(1),
                };
                self.preferred_column = None;
                false
            }
            Word { backward, count } => {
                let direction = if backward {
                    ReaderWordMotion::PreviousStart
                } else {
                    ReaderWordMotion::NextStart
                };
                for _ in 0..count {
                    let Some(target) = reader_word_target(text, self.row, self.column, direction)
                    else {
                        break;
                    };
                    self.row = target.line.min(rows.saturating_sub(1));
                    self.column = target.column;
                }
                self.preferred_column = None;
                true
            }
            Paragraph { forward, count } => {
                for _ in 0..count.min(rows) {
                    let target = reader_paragraph_target(text, self.row, forward)
                        .min(rows.saturating_sub(1));
                    if target == self.row {
                        break;
                    }
                    self.row = target;
                }
                self.column = Self::line(text, self.row)
                    .chars()
                    .position(|ch| !ch.is_whitespace())
                    .unwrap_or(0);
                self.preferred_column = None;
                true
            }
            Pending | Smooth(_) => false,
        };
        self.clamp_column(text);
        follow
    }
}

/// Bounded, reusable undo/redo state shared by Compose and Contacts.
/// Snapshots belong to each editor: the history doesn't know about fields,
/// selections, or how text should be restored.
#[derive(Debug, Clone)]
pub(crate) struct VimHistory<T> {
    undo: VecDeque<T>,
    redo: VecDeque<T>,
    limit: usize,
}

impl<T> Default for VimHistory<T> {
    fn default() -> Self {
        Self::new(200)
    }
}

impl<T> VimHistory<T> {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    pub(crate) fn record(&mut self, snapshot: T) {
        if self.undo.len() == self.limit {
            self.undo.pop_front();
        }
        self.undo.push_back(snapshot);
        self.redo.clear();
    }

    pub(crate) fn undo(&mut self, current: T) -> Option<T> {
        let previous = self.undo.pop_back()?;
        if self.redo.len() == self.limit {
            self.redo.pop_front();
        }
        self.redo.push_back(current);
        Some(previous)
    }

    pub(crate) fn redo(&mut self, current: T) -> Option<T> {
        let next = self.redo.pop_back()?;
        if self.undo.len() == self.limit {
            self.undo.pop_front();
        }
        self.undo.push_back(current);
        Some(next)
    }
}
use std::hash::Hash;

/// One unnamed Vim register is shared by Compose, Contacts, and the reader.
/// It is never serialized with drafts or synchronized to mail providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YankRegister {
    pub(crate) text: String,
    pub(crate) linewise: bool,
}

pub(crate) fn byte_offset(text: &str, cursor: usize) -> usize {
    text.char_indices()
        .nth(cursor)
        .map_or(text.len(), |(at, _)| at)
}

/// Return a character-indexed selection without splitting UTF-8 codepoints.
/// A missing or empty selection must not overwrite the yank register.
pub(crate) fn text_range(text: &str, start: usize, end: usize) -> Option<&str> {
    let len = text.chars().count();
    if start >= end || start >= len {
        return None;
    }
    Some(&text[byte_offset(text, start)..byte_offset(text, end.min(len))])
}

pub(crate) fn horizontal(cursor: usize, len: usize, delta: isize) -> usize {
    cursor.saturating_add_signed(delta).min(len)
}

/// Vim word motion without allocating a temporary Vec<char> per keystroke.
pub(crate) fn word_start(text: &str, cursor: usize, backward: bool, whole: bool) -> usize {
    let belongs = |ch: char| is_word_part(ch, whole);
    let len = text.chars().count();
    let mut at = cursor.min(len);
    if backward {
        let mut previous = text.chars().rev().skip(len - at).peekable();
        while previous.peek().is_some_and(|ch| !belongs(*ch)) {
            at -= 1;
            previous.next();
        }
        while previous.peek().is_some_and(|ch| belongs(*ch)) {
            at -= 1;
            previous.next();
        }
    } else {
        let mut upcoming = text.chars().skip(at).peekable();
        while upcoming.peek().is_some_and(|ch| belongs(*ch)) {
            at += 1;
            upcoming.next();
        }
        while upcoming.peek().is_some_and(|ch| !belongs(*ch)) {
            at += 1;
            upcoming.next();
        }
    }
    at
}

fn is_word_part(ch: char, whole: bool) -> bool {
    if whole {
        !ch.is_whitespace()
    } else {
        is_word_char(ch)
    }
}

/// Move to the final character of the next Vim word, without allocating a
/// character vector on each keystroke. This preserves the composer's existing
/// behavior when the cursor starts on punctuation or at the end of a field.
pub(crate) fn word_end(text: &str, cursor: usize, whole: bool) -> usize {
    let len = text.chars().count();
    if len == 0 {
        return 0;
    }
    let mut index = cursor.min(len - 1);
    let mut chars = text.chars().skip(index).peekable();
    if chars.peek().is_some_and(|ch| is_word_part(*ch, whole)) && index < len - 1 {
        chars.next();
        index += 1;
    }
    while index < len - 1 && chars.peek().is_some_and(|ch| !is_word_part(*ch, whole)) {
        chars.next();
        index += 1;
    }
    chars.next(); // the current character; peek now examines the next one
    while index < len - 1 && chars.peek().is_some_and(|ch| is_word_part(*ch, whole)) {
        chars.next();
        index += 1;
    }
    index
}

/// Word characters have the same meaning in the editor, reader motions,
/// and reader highlighting. Keep this predicate in one place.
pub(crate) fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
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

pub(crate) fn inner_text_object_range(
    text: &str,
    column: usize,
    delimiter: char,
) -> Option<(usize, usize)> {
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

/// Inclusive Vim character selection, expressed as an exclusive range of
/// actual text. An editor caret may be at EOF, which is not a character.
pub(crate) fn visual_text_range(anchor: usize, cursor: usize, len: usize) -> (usize, usize) {
    let anchor = anchor.min(len);
    let cursor = cursor.min(len);
    (
        anchor.min(cursor),
        anchor.max(cursor).saturating_add(1).min(len),
    )
}

/// Read-only text cursors stop on the last character, never on synthetic
/// padding. Empty rows have no selectable characters.
pub(crate) fn visual_chars(anchor: usize, cursor: usize, len: usize) -> Option<(usize, usize)> {
    (len > 0).then(|| visual_text_range(anchor.min(len - 1), cursor.min(len - 1), len))
}

/// Visible range plus persistent marks; hidden marked identities survive filters.
/// Falling back to the current row happens only when neither selection is active.
pub(crate) fn selected<T: Eq + Hash + Clone>(
    visible: &[T],
    all: &[T],
    cursor: usize,
    anchor: Option<usize>,
    marked: &HashSet<T>,
) -> Vec<T> {
    let mut chosen = Vec::new();
    let mut seen = HashSet::new();
    if let Some(anchor) = anchor.filter(|_| !visible.is_empty()) {
        let end = visible.len() - 1;
        let from = anchor.min(end).min(cursor.min(end));
        let to = anchor.min(end).max(cursor.min(end));
        for item in &visible[from..=to] {
            if seen.insert(item.clone()) {
                chosen.push(item.clone());
            }
        }
    }
    for item in all {
        if marked.contains(item) && seen.insert(item.clone()) {
            chosen.push(item.clone());
        }
    }
    if chosen.is_empty()
        && let Some(item) = visible.get(cursor)
    {
        chosen.push(item.clone());
    }
    chosen
}

/// One semantic toggle for visual ranges in mail, contacts, and attachments.
pub(crate) fn toggle_group<T: Eq + Hash + Clone>(marked: &mut HashSet<T>, items: &[T]) {
    if items.is_empty() {
        return;
    }
    let all_marked = items.iter().all(|item| marked.contains(item));
    for item in items {
        if all_marked {
            marked.remove(item);
        } else {
            marked.insert(item.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_navigation_preserves_preferred_column_through_short_and_empty_rows() {
        let text = "abcdef\nx\n\nuvwxyz";
        let mut nav = ReaderNavigation {
            column: 4,
            ..ReaderNavigation::default()
        };
        assert!(nav.execute(ReadMotion::Vertical(1), text, 4));
        assert_eq!((nav.row, nav.column), (1, 0));
        assert_eq!(nav.preferred_column, Some(4));
        assert!(!nav.execute(ReadMotion::Smooth(20), text, 4));
        assert!(nav.advance(text, 4));
        assert_eq!((nav.row, nav.column), (2, 0));
        assert!(nav.advance(text, 4));
        assert_eq!((nav.row, nav.column), (3, 4));
        assert!(!nav.active());
        nav.execute(ReadMotion::Horizontal(-2), text, 4);
        assert_eq!(nav.preferred_column, None);
        nav.execute(ReadMotion::Vertical(-3), text, 4);
        assert_eq!((nav.row, nav.column), (0, 2));
    }

    #[test]
    fn reader_navigation_bounds_smooth_scroll_and_visual_range_after_resize() {
        let mut nav = ReaderNavigation::default();
        nav.toggle_visual(false, 4);
        nav.execute(ReadMotion::Smooth(100), "abc\nx\ny\nz", 4);
        nav.clamp_document("abc\nx", 2);
        assert!(nav.advance("abc\nx", 2));
        assert_eq!(nav.row, 1);
        assert!(!nav.active());
        assert_eq!(
            reader_selection_text("abc\nx", nav.visual_range().unwrap()),
            "abc\nx"
        );
        nav.toggle_visual(true, 2);
        assert!(nav.visual_linewise);
        nav.toggle_visual(false, 2);
        assert!(!nav.visual_linewise);
        assert!(nav.visual_range().is_some());
        nav.toggle_visual(false, 2);
        assert!(nav.visual_range().is_none());
        nav.execute(ReadMotion::Jump(usize::MAX), "", 0);
        nav.clamp_document("", 0);
        assert_eq!((nav.row, nav.column), (0, 0));
        assert!(!nav.active());
    }

    #[test]
    fn reader_navigation_word_paragraph_and_column_motions_use_one_executor() {
        let text = "  café words\n\nlast line";
        let mut nav = ReaderNavigation::default();
        nav.execute(ReadMotion::FirstNonBlank, text, 3);
        assert_eq!(nav.column, 2);
        nav.execute(
            ReadMotion::Word {
                backward: false,
                count: 1,
            },
            text,
            3,
        );
        assert_eq!((nav.row, nav.column), (0, 7));
        nav.execute(
            ReadMotion::Paragraph {
                forward: true,
                count: 1,
            },
            text,
            3,
        );
        assert_eq!((nav.row, nav.column), (2, 0));
        nav.execute(ReadMotion::LastColumn, text, 3);
        assert_eq!(nav.column, 8);
        nav.execute(ReadMotion::Jump(0), text, 3);
        nav.execute(ReadMotion::FirstColumn, text, 3);
        assert_eq!((nav.row, nav.column), (0, 0));
    }

    #[test]
    fn read_motion_shares_counted_jumps_and_scroll_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut count = None;
        let mut pending_g = false;
        let mut decode = |code, modifiers| {
            read_motion(
                KeyEvent::new(code, modifiers),
                &mut count,
                &mut pending_g,
                100,
                20,
                22,
                99_999,
            )
        };
        let plain = KeyModifiers::NONE;
        assert_eq!(
            decode(KeyCode::Char('0'), plain),
            Some(ReadMotion::FirstColumn)
        );
        assert_eq!(decode(KeyCode::Char('2'), plain), Some(ReadMotion::Pending));
        assert_eq!(decode(KeyCode::Char('0'), plain), Some(ReadMotion::Pending));
        assert_eq!(
            decode(KeyCode::Char('G'), plain),
            Some(ReadMotion::Jump(19))
        );
        assert_eq!(decode(KeyCode::Char('g'), plain), Some(ReadMotion::Pending));
        assert_eq!(decode(KeyCode::Char('g'), plain), Some(ReadMotion::Jump(0)));
        assert_eq!(
            decode(KeyCode::Char('G'), plain),
            Some(ReadMotion::Jump(99))
        );
        assert_eq!(
            decode(KeyCode::Char('k'), KeyModifiers::CONTROL),
            Some(ReadMotion::Smooth(-22))
        );
        assert_eq!(
            decode(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(ReadMotion::Vertical(10))
        );
    }

    #[test]
    fn unicode_motions_and_visual_ranges_use_character_offsets() {
        assert_eq!(byte_offset("a😊é", 2), 5);
        assert_eq!(text_range("a😊é", 1, 3), Some("😊é"));
        assert_eq!(text_range("a😊é", 1, 99), Some("😊é"));
        assert_eq!(text_range("a😊é", 3, 4), None);
        assert_eq!(text_range("a😊é", 1, 1), None);
        assert_eq!(horizontal(0, 3, -1), 0);
        assert_eq!(horizontal(1, 3, 100), 3);
        assert_eq!(visual_chars(3, 1, 4), Some((1, 4)));
        assert_eq!(visual_chars(0, 0, 0), None);
        assert_eq!(word_start("first second", 0, false, true), 6);
        assert_eq!(word_start("first second", 8, true, true), 6);
        assert_eq!(word_end("first second", 0, false), 4);
        assert_eq!(word_end("first second", 4, false), 11); // advances to next word
        assert_eq!(word_end("first second", 5, false), 11);
        assert_eq!(word_end("éé 😊abc", 0, false), 1);
        assert_eq!(word_end("", 0, true), 0);
    }

    #[test]
    fn nested_text_objects_work_in_reader_and_composer() {
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
        assert_eq!(inner_text_object_range("{empty}", 0, '('), None);
        assert!(is_word_char('é'));
        assert!(!is_word_char('-'));
    }

    #[test]
    fn marked_and_visual_ranges_combine_without_duplicates_or_losing_hidden_marks() {
        let visible = [1, 3, 5];
        let all = [1, 2, 3, 4, 5];
        let marks = HashSet::from([2, 3]);
        assert_eq!(selected(&visible, &all, 2, Some(1), &marks), vec![3, 5, 2]);
        assert_eq!(selected(&visible, &all, 0, None, &HashSet::new()), vec![1]);
        assert_eq!(
            selected(&visible, &all, 9, None, &HashSet::new()),
            Vec::<i32>::new()
        );
    }

    #[test]
    fn marking_a_whole_range_twice_toggles_it_as_a_group() {
        let mut marks = HashSet::from([2]);
        toggle_group(&mut marks, &[2, 3]);
        assert_eq!(marks, HashSet::from([2, 3]));
        toggle_group(&mut marks, &[2, 3]);
        assert!(marks.is_empty());
    }
}

#[cfg(test)]
mod editing_tests {
    use super::*;

    #[test]
    fn pending_sequences_do_not_overlap_and_zero_without_count_is_a_motion() {
        let mut sequence = VimSequence::default();
        assert!(!sequence.push_digit('0', 99));
        assert!(sequence.push_digit('2', 99));
        assert!(sequence.push_digit('0', 99));
        assert_eq!(sequence.take_count(), 20);
        assert!(sequence.push_digit('9', 99));
        assert!(sequence.push_digit('9', 99));
        assert_eq!(sequence.take_count(), 99);
        sequence.start(VimPrefix::Delete);
        assert!(sequence.has_operator());
        sequence.start(VimPrefix::Yank);
        assert_eq!(sequence.take_command(), Some(VimPrefix::Yank));
        assert_eq!(sequence.take_command(), None);
        assert!(!sequence.has_operator());
        sequence.start(VimPrefix::FindForward);
        sequence.clear();
        assert_eq!(sequence.take_command(), None);
        assert_eq!(sequence.take_count(), 1);
    }

    #[test]
    fn bounded_history_supports_undo_redo_and_new_edits_invalidate_redo() {
        let mut history = VimHistory::new(2);
        history.record("initial");
        history.record("second");
        history.record("third");
        assert_eq!(history.undo("current"), Some("third"));
        assert_eq!(history.undo("third"), Some("second"));
        assert_eq!(history.undo("second"), None); // oldest entry evicted
        assert_eq!(history.redo("second"), Some("third"));
        history.record("fourth");
        assert_eq!(history.redo("next"), None);
        assert_eq!(history.undo("next"), Some("fourth"));
    }
}
