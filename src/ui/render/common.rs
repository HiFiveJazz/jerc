//! Common rendering.

use super::*;

/// Shared overlay shell. Clear before borders and keep theme-specific border
/// styling with the caller while ensuring consistent popup geometry.
pub(super) fn popup_inner(
    frame: &mut Frame<'_>,
    theme: &Theme,
    area: Rect,
    title: &str,
    border_style: Style,
) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

pub(super) fn centered_rect(area: Rect, max_width: u16, percent_height: u16) -> Rect {
    let width = area.width.min(max_width).max(1);
    let height = ((u32::from(area.height) * u32::from(percent_height) / 100) as u16)
        .max(3)
        .min(area.height.max(1));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// A slice of filtered indices with its current selection kept visible.
/// Both the help palette and theme selector use this instead of rebuilding a
/// temporary vector of rows every frame.
pub(super) fn visible_filtered(
    matches: &[usize], selected: usize, scroll: usize, height: usize,
) -> &[usize] {
    if matches.is_empty() || height == 0 {
        return &[];
    }
    let selected_position = matches.iter().position(|&index| index == selected).unwrap_or(0);
    let max_start = matches.len().saturating_sub(height);
    let start = scroll
        .max(selected_position.saturating_sub(height.saturating_sub(1)))
        .min(selected_position)
        .min(max_start);
    &matches[start..(start + height).min(matches.len())]
}

/// Window of list rows with the cursor kept near the center. Used by
/// file browsing and attachment lists; only the visible rows are rendered.
pub(super) fn centered_list_window(
    selected: usize, len: usize, height: usize,
) -> std::ops::Range<usize> {
    if len == 0 || height == 0 {
        return 0..0;
    }
    let selected = selected.min(len - 1);
    let start = selected.saturating_sub(height / 2).min(len.saturating_sub(height));
    start..start.saturating_add(height).min(len)
}

pub(super) fn mailbox_row_text(name: &str, count: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let count_width = count.chars().count();
    if count.is_empty() {
        let name = truncate_chars(name, width);
        return format!("{name:<width$}");
    }
    if count_width >= width {
        return truncate_chars(count, width);
    }

    let name_width = width.saturating_sub(count_width + 1);
    let name = truncate_chars(name, name_width);
    format!("{name:<name_width$} {count:>count_width$}")
}

pub(super) fn truncate_display_width(text: &str, width: usize) -> String {
    truncate_display_width_cow(text, width).into_owned()
}

/// Keep borrowed mail-list text when it fits rather than allocating every row.
pub(super) fn truncate_display_width_cow(text: &str, width: usize) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if width == 0 {
        return Cow::Borrowed("");
    }
    if UnicodeWidthStr::width(text) <= width {
        return Cow::Borrowed(text);
    }
    if width == 1 {
        return Cow::Borrowed("…");
    }

    let budget = width.saturating_sub(1);
    let mut used = 0usize;
    let mut out = String::new();
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used.saturating_add(grapheme_width) > budget {
            break;
        }
        out.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    out.push('…');
    Cow::Owned(out)
}

pub(super) fn truncate_chars(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut out = text.chars().take(width - 1).collect::<String>();
    out.push('…');
    out
}

#[cfg(test)]
pub(super) fn subject_for_list(subject: &str) -> String {
    subject_for_list_cow(subject).into_owned()
}

pub(super) fn subject_for_list_cow(subject: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    // Preserve the original bytes unless a text-capable emoji needs FE0E.
    // This avoids allocating for the overwhelming majority of mail subjects.
    let mut output: Option<String> = None;
    let mut copied_to = 0usize;
    for (offset, cluster) in UnicodeSegmentation::grapheme_indices(subject, true) {
        let mut chars = cluster.chars();
        if let Some(base) = chars.next() {
            let simple = chars.all(|ch| ch == '\u{fe0e}' || ch == '\u{fe0f}');
            if simple && supports_safe_text_presentation(base) {
                let out = output.get_or_insert_with(|| String::with_capacity(subject.len()));
                out.push_str(&subject[copied_to..offset]);
                out.push(base);
                out.push('\u{fe0e}');
                copied_to = offset + cluster.len();
            }
        }
    }
    match output {
        Some(mut out) => {
            out.push_str(&subject[copied_to..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(subject),
    }
}

/// Unicode characters with established text/emoji variants whose *text*
/// presentation can fit a normal terminal cell. Emoji-only pictographs stay
/// untouched because not all terminals/font stacks provide a text glyph.
pub(super) fn supports_safe_text_presentation(ch: char) -> bool {
    matches!(
        ch,
        '☀' | '☁'
            | '☂'
            | '☃'
            | '☎'
            | '☘'
            | '☺'
            | '☹'
            | '✂'
            | '✈'
            | '✉'
            | '✌'
            | '❤'
            | '❄'
            | '✔'
            | '✖'
    )
}

pub(super) fn relative_number(index: usize, current: usize) -> String {
    if index == current {
        (index + 1).to_string()
    } else {
        index.abs_diff(current).to_string()
    }
}

pub(super) fn relative_number_width(len: usize) -> u16 {
    let digits = len.max(1).ilog10() + 1;
    u16::try_from(digits + 1).unwrap_or(6).max(3)
}

#[cfg(test)]
mod navigation_tests {
    use super::{centered_list_window, visible_filtered};

    #[test]
    fn centered_window_tracks_selection_without_overflow() {
        assert_eq!(centered_list_window(0, 0, 20), 0..0);
        assert_eq!(centered_list_window(100, 200, 0), 0..0);
        assert_eq!(centered_list_window(0, 3, 10), 0..3);
        assert_eq!(centered_list_window(50, 100, 10), 45..55);
        assert_eq!(centered_list_window(99, 100, 10), 90..100);
    }

    #[test]
    fn visible_filtered_is_borrowed_and_keeps_selection_visible() {
        let indices = [1, 4, 9, 15, 20, 30];
        assert_eq!(visible_filtered(&indices, 20, 0, 3), &[9, 15, 20]);
        assert_eq!(visible_filtered(&indices, 4, 4, 3), &[4, 9, 15]);
        assert_eq!(visible_filtered(&indices, 999, 0, 3), &[1, 4, 9]);
        assert!(visible_filtered(&indices, 1, 0, 0).is_empty());
    }
}
