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
        // Mailing-list clients vary between `>>`, `> >`, and `>   >`.
        // Consume spaces and tabs between quote markers and before the body.
        while matches!(bytes.get(offset).copied(), Some(b' ' | b'\t')) {
            offset += 1;
        }
    }
    if depth == 0 { (0, 0) } else { (depth, offset) }
}

/// Remove quotation markers while preserving the leading Git diff context
/// space. `quote_prefix` intentionally consumes all whitespace for styling;
/// diff parsing must only remove one separator after the final marker.
pub(super) fn quoted_patch_payload(mut line: &str, depth: usize) -> &str {
    line = line.trim_start_matches([' ', '\t']);
    for level in 0..depth {
        if let Some(after) = line.strip_prefix('>') {
            line = after;
        }
        if let Some(after) = line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            line = after;
        }
        if level + 1 < depth {
            line = line.trim_start_matches([' ', '\t']);
        }
    }
    line
}

/// A visible attribution separates the writer's reply from the quoted mail.
/// Keep the rule narrow so ordinary prose ending in a colon remains normal.
pub(super) fn is_reply_attribution(text: &str) -> bool {
    let (_, start) = quote_prefix(text);
    let body = if start > 0 { &text[start..] } else { text };
    let body = body.trim();
    body.starts_with("On ") && body.ends_with(" wrote:")
}

/// Quotation levels must not reuse the red/green/blue palette reserved for
/// Git diffs. Derive readable violet, amber, and blended rose tones from the
/// active theme instead of making the first level nearly invisible (muted).
/// Cycling the palette also makes older quoted conversations distinguishable.
pub(super) fn quoted_body_style(depth: usize, theme: &Theme) -> Style {
    let color = match depth.saturating_sub(1) % 3 {
        0 => theme.visual,
        1 => theme.warning,
        _ => match (theme.visual, theme.warning) {
            (ratatui::style::Color::Rgb(vr, vg, vb), ratatui::style::Color::Rgb(wr, wg, wb)) => {
                let average = |a: u8, b: u8| ((u16::from(a) + u16::from(b)) / 2) as u8;
                ratatui::style::Color::Rgb(average(vr, wr), average(vg, wg), average(vb, wb))
            }
            _ => theme.foreground,
        },
    };
    Style::default().fg(color)
}

pub(super) fn quoted_prefix_style(depth: usize, theme: &Theme) -> Style {
    quoted_body_style(depth, theme).add_modifier(Modifier::BOLD)
}

pub(super) fn attribution_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.visual)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::{
        is_reply_attribution, quote_prefix, quoted_body_style, quoted_patch_payload,
        quoted_prefix_style,
    };
    use crate::theme::load_themes;
    use ratatui::style::Modifier;

    #[test]
    fn nested_quotes_are_counted_without_losing_utf8_body_boundaries() {
        assert_eq!(quote_prefix("Hello > world"), (0, 0));
        assert_eq!(quote_prefix("> Original"), (1, 2));
        assert_eq!(quote_prefix("> > Another"), (2, 4));
        assert_eq!(quote_prefix("  >>> Héllo"), (3, 6));
        assert_eq!(quote_prefix(">> ✅"), (2, 3));
        assert_eq!(quote_prefix("   -EIO"), (0, 0));
        assert_eq!(quote_prefix(">   >\tOlder reply"), (2, 6));
        assert_eq!(quote_prefix("  >  >>  Older reply"), (3, 9));
    }

    #[test]
    fn quote_levels_are_readable_and_do_not_borrow_diff_colors() {
        for theme in load_themes() {
            let first = quoted_body_style(1, &theme);
            let second = quoted_body_style(2, &theme);
            let third = quoted_body_style(3, &theme);
            assert_eq!(first.fg, Some(theme.visual));
            assert_eq!(second.fg, Some(theme.warning));
            assert_ne!(first.fg, Some(theme.accent), "{}", theme.id);
            // The monochrome palette intentionally uses white for several
            // semantic colors, so it cannot promise distinct hues.
            if theme.id != "monochrome" {
                assert_ne!(second.fg, Some(theme.error), "{}", theme.id);
            }
            assert_ne!(first.fg, third.fg, "{}", theme.id);
            assert_eq!(quoted_prefix_style(2, &theme).fg, second.fg);
            assert!(
                quoted_prefix_style(2, &theme)
                    .add_modifier
                    .contains(Modifier::BOLD)
            );
        }
    }

    #[test]
    fn unquoted_payload_retains_context_line_space() {
        assert_eq!(quoted_patch_payload("> +added", 1), "+added");
        assert_eq!(quoted_patch_payload("> -removed", 1), "-removed");
        assert_eq!(quoted_patch_payload(">  context", 1), " context");
        assert_eq!(quoted_patch_payload("> > +added", 2), "+added");
        assert_eq!(quoted_patch_payload(">>  context", 2), " context");
    }

    #[test]
    fn attribution_is_distinguished_from_plain_prose() {
        assert!(is_reply_attribution("On Mon, Nov 2, Alice wrote:"));
        assert!(is_reply_attribution("> On Mon, Nov 2, Alice wrote:"));
        assert!(!is_reply_attribution("The writer wrote:"));
        assert!(!is_reply_attribution("This is my reply"));
    }
}
