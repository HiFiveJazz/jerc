//! Reader-specific cursor positions, motions, selections and link parsing.
//! Keep these separate from application orchestration and the rendering layer.

use super::READER_SCROLL_OFF;
use crate::app::vim::is_word_char;

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
pub(super) enum ReaderWordMotion {
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

/// Reuse the same Unicode character-based word motion as composing and
/// contact editing. Reader motions can cross line boundaries without building
/// a `Vec<char>` or `Vec<&str>` for every keystroke.
pub(super) fn reader_word_target(
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
            let next = crate::app::vim::word_start(current, at, false, false);
            if next < length && current.chars().nth(next).is_some_and(is_word_char) {
                return Some(ReaderPosition { line, column: next });
            }
            text.lines().enumerate().skip(line + 1).find_map(|(line, text)| {
                text.chars().position(is_word_char).map(|column| ReaderPosition { line, column })
            })
        }
        ReaderWordMotion::PreviousStart => {
            if column > 0 {
                let previous = crate::app::vim::word_start(current, at, true, false);
                if previous < length && current.chars().nth(previous).is_some_and(is_word_char) {
                    return Some(ReaderPosition { line, column: previous });
                }
            }
            text.lines().enumerate().take(line).filter_map(|(line, text)| {
                let length = text.chars().count();
                let column = crate::app::vim::word_start(text, length, true, false);
                (column < length && text.chars().nth(column).is_some_and(is_word_char))
                    .then_some(ReaderPosition { line, column })
            }).last()
        }
    }
}

/// Extract a visual selection using shared UTF-8-safe Vim ranges, without
/// allocating a temporary vector of chars for each selected line.
pub(super) fn reader_selection_text(text: &str, selection: ReaderSelection) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() || selection.start.line >= lines.len() {
        return String::new();
    }
    let end_line = selection.end.line.min(lines.len() - 1);
    let mut out = String::new();
    for (line_index, line) in lines.iter().enumerate()
        .take(end_line + 1).skip(selection.start.line)
    {
        if line_index > selection.start.line {
            out.push('\n');
        }
        let count = line.chars().count();
        if count == 0 { continue; }
        let start = if line_index == selection.start.line {
            selection.start.column.min(count - 1)
        } else { 0 };
        let end = if line_index == end_line {
            selection.end.column.min(count - 1)
        } else { count - 1 };
        if let Some(slice) = crate::app::vim::text_range(line, start, end.saturating_add(1)) {
            out.push_str(slice);
        }
    }
    out
}

pub(super) fn url_at_cursor(line: &str, column: usize) -> Option<String> {
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

/// Scrolloff counts terminal display rows, including wrapped lines.
pub(super) fn reader_scroll_for_visual_cursor(
    current: usize,
    cursor_row: usize,
    view_height: usize,
    max_scroll: usize,
) -> usize {
    crate::app::navigation::reveal_row(
        current, cursor_row, view_height, max_scroll, READER_SCROLL_OFF,
    )
}
