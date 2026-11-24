//! Reader-specific link parsing and viewport helpers.
//! Keep these separate from application orchestration and the rendering layer.

use super::READER_SCROLL_OFF;

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
        current,
        cursor_row,
        view_height,
        max_scroll,
        READER_SCROLL_OFF,
    )
}
