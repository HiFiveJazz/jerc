//! Lightweight RFC-style email quote highlighting, independent of Tree-sitter.
//! Mail quotes are not a programming language: only explicit `>` prefixes
//! count as quote depth. This avoids miscoloring arbitrary text and diffs.

use ratatui::style::{Modifier, Style};

use crate::theme::Theme;

/// (nesting depth, byte offset after the `>` prefix and its separators).
/// Works with `> foo`, `>> foo`, `> > foo` and indented variants.
pub(super) fn quote_prefix(line: &str) -> (usize, usize) {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let mut offset = line.len() - trimmed.len();
    let bytes = line.as_bytes();
    let mut depth = 0;
    while bytes.get(offset) == Some(&b'>') {
        depth += 1;
        offset += 1;
        if bytes.get(offset) == Some(&b' ') {
            offset += 1;
        }
    }
    if depth == 0 {
        (0, 0)
    } else {
        (depth, offset)
    }
}

/// A visible attribution separates the writer's reply from the quoted mail.
/// Keep the rule narrow so ordinary prose ending in a colon remains normal.
pub(super) fn is_reply_attribution(text: &str) -> bool {
    let (_, start) = quote_prefix(text);
    let body = if start > 0 { &text[start..] } else { text };
    let body = body.trim();
    body.starts_with("On ") && body.ends_with(" wrote:")
}

/// The writer's own text uses theme.text(); quotes use progressively different
/// theme-native tones. No hardcoded colors, so light/dark custom themes work.
pub(super) fn quoted_body_style(depth: usize, theme: &Theme) -> Style {
    let color = match depth.saturating_sub(1) % 3 {
        0 => theme.muted,
        1 => theme.accent,
        _ => theme.unread,
    };
    Style::default().fg(color)
}

pub(super) fn quoted_prefix_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn attribution_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::{is_reply_attribution, quote_prefix};

    #[test]
    fn nested_quotes_are_counted_without_losing_utf8_body_boundaries() {
        assert_eq!(quote_prefix("Hello > world"), (0, 0));
        assert_eq!(quote_prefix("> Original"), (1, 2));
        assert_eq!(quote_prefix("> > Another"), (2, 4));
        assert_eq!(quote_prefix("  >>> Héllo"), (3, 6));
        assert_eq!(quote_prefix(">> ✅"), (2, 3));
        assert_eq!(quote_prefix("   -EIO"), (0, 0));
    }

    #[test]
    fn attribution_is_distinguished_from_plain_prose() {
        assert!(is_reply_attribution("On Mon, Nov 2, Alice wrote:"));
        assert!(is_reply_attribution("> On Mon, Nov 2, Alice wrote:"));
        assert!(!is_reply_attribution("The writer wrote:"));
        assert!(!is_reply_attribution("This is my reply"));
    }
}
