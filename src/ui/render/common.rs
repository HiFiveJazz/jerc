//! Common rendering.

use super::*;

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
