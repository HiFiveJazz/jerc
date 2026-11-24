//! Dialogs rendering.

use super::*;

/// Shared compact two-button confirmation. The caller owns the action and
/// this renderer only draws the same themed layout in each context.
pub(super) fn draw_short_confirmation(
    frame: &mut Frame<'_>,
    theme: &Theme,
    width: u16,
    title: &str,
    message: &str,
    border: Style,
    destructive: bool,
) {
    let viewport = frame.area();
    let width = viewport.width.min(width);
    let height = viewport.height.min(7);
    let area = Rect::new(
        viewport.x + viewport.width.saturating_sub(width) / 2,
        viewport.y + viewport.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let inner = popup_inner(frame, theme, area, title, border);
    if inner.width < 10 || inner.height < 3 {
        return;
    }
    frame.render_widget(
        Paragraph::new(truncate_display_width(message, usize::from(inner.width)))
            .alignment(Alignment::Center)
            .style(theme.text()),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
    );
    let (yes_style, no_style) = if destructive {
        // Reuse the same button palette as destructive mail confirmations.
        (destructive_button_style(theme), theme.current_line())
    } else {
        (setup_button_style(theme), destructive_button_style(theme))
    };
    draw_confirmation_buttons(
        frame,
        theme,
        Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 2),
        yes_style,
        no_style,
    );
}

/// Yes saves the draft; No discards it. Esc returns to editing.
pub(super) fn draw_compose_exit_prompt(frame: &mut Frame<'_>, theme: &Theme) {
    draw_short_confirmation(
        frame,
        theme,
        62,
        " Save unsent email? ",
        "Save this email to Drafts before closing?",
        theme.border(),
        false,
    );
}

/// The saved draft stays visible behind the confirmation popup.
pub(super) fn draw_draft_resume_confirmation(frame: &mut Frame<'_>, theme: &Theme) {
    draw_short_confirmation(
        frame,
        theme,
        68,
        " Continue saved draft? ",
        "Continue composing this saved email?",
        theme.border(),
        false,
    );
}

pub(super) fn draw_remove_account_confirmation(frame: &mut Frame<'_>, app: &App) {
    let Some(name) = app.remove_account_confirmation.as_ref() else {
        return;
    };
    draw_delete_account_confirmation(frame, app.theme(), name);
}

fn draw_delete_account_confirmation(frame: &mut Frame<'_>, theme: &Theme, name: &str) {
    draw_short_confirmation(
        frame,
        theme,
        68,
        " Delete Email Account ",
        &format!("Delete email account {name:?} from jerc?"),
        theme.error(),
        true,
    );
}

/// Shared Yazi-style review: simple, aligned item numbers without a visual
/// selection gutter. The list is for reviewing, not selecting, its contents.
pub(super) fn review_item_line(
    number: usize,
    digits: usize,
    item: &str,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let prefix = format!(" {number:>digits$}  ");
    let available = width.saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
    Line::from(vec![
        Span::styled(prefix, theme.muted()),
        Span::styled(truncate_display_width(item, available), theme.text()),
    ])
}

/// Shared horizontal rule and centered Yes/No buttons for confirmation popups.
/// The calling dialog decides which action is destructive.
pub(super) fn draw_confirmation_buttons(
    frame: &mut Frame<'_>,
    theme: &Theme,
    area: Rect,
    yes_style: Style,
    no_style: Style,
) {
    draw_action_buttons(
        frame,
        theme,
        area,
        yes_style,
        no_style,
        (" [Y]es ", " [N]o "),
    );
}

pub(super) fn draw_action_buttons(
    frame: &mut Frame<'_>,
    theme: &Theme,
    area: Rect,
    yes_style: Style,
    no_style: Style,
    labels: (&str, &str),
) {
    if area.height < 2 || area.width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width))).style(theme.border()),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let gap = 3u16.min(area.width.saturating_sub(2));
    let button_width = (UnicodeWidthStr::width(labels.0).max(UnicodeWidthStr::width(labels.1))
        as u16)
        .max(11)
        .min(area.width.saturating_sub(gap) / 2);
    if button_width == 0 {
        return;
    }
    let total = button_width * 2 + gap;
    let x = area.x + area.width.saturating_sub(total) / 2;
    let y = area.y.saturating_add(1);
    frame.render_widget(
        Paragraph::new(labels.0)
            .alignment(Alignment::Center)
            .style(yes_style),
        Rect::new(x, y, button_width, 1),
    );
    frame.render_widget(
        Paragraph::new(labels.1)
            .alignment(Alignment::Center)
            .style(no_style),
        Rect::new(x + button_width + gap, y, button_width, 1),
    );
}

/// Shared Yazi-style review: visible item list, scroll position, and the same
/// filled action treatment as the setup wizard. No action is taken by drawing.
pub(super) fn draw_review_dialog(
    frame: &mut Frame<'_>,
    theme: &Theme,
    title: &str,
    explanation: &str,
    items: &[String],
    scroll: usize,
) {
    let area = centered_rect(frame.area(), 86, 72);
    let inner = popup_inner(frame, theme, area, &format!(" {title} "), theme.error());
    if inner.width < 10 || inner.height < 5 {
        return;
    }
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(truncate_display_width(
            explanation,
            usize::from(chunks[0].width),
        ))
        .style(theme.text()),
        chunks[0],
    );
    let capacity = usize::from(chunks[1].height);
    let start = scroll.min(items.len().saturating_sub(capacity));
    let digits = items.len().max(1).to_string().len();
    for (offset, item) in items.iter().skip(start).take(capacity).enumerate() {
        let row = Rect::new(chunks[1].x, chunks[1].y + offset as u16, chunks[1].width, 1);
        frame.render_widget(
            Paragraph::new(review_item_line(
                start + offset + 1,
                digits,
                item,
                usize::from(row.width),
                theme,
            )),
            row,
        );
    }
    frame.render_widget(
        Paragraph::new(format!(
            " {} items  ·  {}–{}",
            items.len(),
            if items.is_empty() { 0 } else { start + 1 },
            (start + capacity).min(items.len()),
        ))
        .style(theme.muted()),
        chunks[2],
    );
    draw_confirmation_buttons(
        frame,
        theme,
        chunks[3],
        destructive_button_style(theme),
        theme.current_line(),
    );
}

/// Give subjects and sender addresses their own columns. Both fields are
/// truncated independently, so a long subject never hides the address.
fn mail_review_column_widths(width: usize, digits: usize) -> (usize, usize) {
    let prefix = UnicodeWidthStr::width(format!(" {:>digits$}  ", 1).as_str());
    let content = width.saturating_sub(prefix + 2);
    let sender_width = (content * 40 / 100).max(14).min(content.saturating_sub(8));
    (content.saturating_sub(sender_width), sender_width)
}

fn mail_review_line(
    number: Option<usize>,
    digits: usize,
    subject: &str,
    address: &str,
    width: usize,
    theme: &Theme,
    active: bool,
) -> Line<'static> {
    let (subject_width, sender_width) = mail_review_column_widths(width, digits);
    let prefix = number.map_or_else(
        || " ".repeat(digits + 3),
        |number| format!(" {number:>digits$}  "),
    );
    let subject = truncate_display_width(subject, subject_width);
    let subject_padding =
        " ".repeat(subject_width.saturating_sub(UnicodeWidthStr::width(subject.as_str())));
    let address = truncate_display_width(address, sender_width);
    let (dim, regular) = if active {
        (theme.current_line(), theme.current_line())
    } else {
        (theme.muted(), theme.text())
    };
    Line::from(vec![
        Span::styled(prefix, dim),
        Span::styled(format!("{subject}{subject_padding}"), regular),
        Span::raw("  "),
        Span::styled(address, dim),
    ])
}

/// Action-specific title and colors; archive warnings use the configured
/// yellow and deletion keeps the destructive palette.
fn mail_review_presentation(theme: &Theme, archive: bool) -> (&'static str, Style, Style) {
    if archive {
        (
            " Archive Emails ",
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(theme.popup_bg)
                .bg(theme.warning)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            " Delete Emails ",
            theme.error(),
            destructive_button_style(theme),
        )
    }
}

/// Mail reviews use the same current-line highlight as the inbox. The modal
/// remains read-only: moving its cursor does not change which messages act.
fn draw_mail_review_dialog(
    frame: &mut Frame<'_>,
    theme: &Theme,
    explanation: &str,
    items: &[MailReviewItem],
    selected: usize,
    archive: bool,
) {
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(118),
        76,
    );
    let (title, border, yes_style) = mail_review_presentation(theme, archive);
    let inner = popup_inner(frame, theme, area, title, border);
    if inner.width < 10 || inner.height < 6 {
        return;
    }
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(truncate_display_width(
            explanation,
            usize::from(chunks[0].width),
        ))
        .style(theme.text()),
        chunks[0],
    );
    let digits = items.len().max(1).to_string().len();
    let header = mail_review_line(
        None,
        digits,
        "Subject",
        "From",
        usize::from(chunks[1].width),
        theme,
        false,
    );
    frame.render_widget(Paragraph::new(header).style(theme.muted()), chunks[1]);

    let capacity = usize::from(chunks[2].height);
    let selected = selected.min(items.len().saturating_sub(1));
    let window = centered_list_window(selected, items.len(), capacity);
    let end = window.end;
    let start = window.start;
    for (row_index, item_index) in (start..end).enumerate() {
        let item = &items[item_index];
        let active = item_index == selected;
        let row = Rect::new(
            chunks[2].x,
            chunks[2].y + row_index as u16,
            chunks[2].width,
            1,
        );
        frame.render_widget(
            Paragraph::new(mail_review_line(
                Some(item_index + 1),
                digits,
                &item.subject,
                &item.from_address,
                usize::from(row.width),
                theme,
                active,
            ))
            .style(if active {
                theme.current_line()
            } else {
                theme.text()
            }),
            row,
        );
    }
    frame.render_widget(
        Paragraph::new(format!(
            " {} email{}  ·  {}–{} of {}",
            items.len(),
            if items.len() == 1 { "" } else { "s" },
            if items.is_empty() { 0 } else { start + 1 },
            end,
            items.len(),
        ))
        .style(theme.muted()),
        chunks[3],
    );
    draw_confirmation_buttons(frame, theme, chunks[4], yes_style, theme.current_line());
}

pub(super) fn draw_confirmation(frame: &mut Frame<'_>, app: &App) {
    let Some(confirmation) = app.confirmation.as_ref() else {
        return;
    };
    draw_mail_review_dialog(
        frame,
        app.theme(),
        &confirmation.prompt,
        &confirmation.items,
        confirmation.review.position,
        confirmation.is_archive(),
    );
}

pub(super) fn draw_detach_confirmation(frame: &mut Frame<'_>, app: &App) {
    let Some(confirmation) = app.detach_confirmation.as_ref() else {
        return;
    };
    let names = confirmation
        .paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    draw_review_dialog(
        frame,
        app.theme(),
        "Detach attachments",
        "Remove these attachments from the outgoing message?",
        &names,
        confirmation.review.position,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::load_themes;

    #[test]
    fn delete_account_confirmation_reuses_yes_no_buttons_without_cleanup_details() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw_delete_account_confirmation(frame, &theme, "Personal"))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0u16..24)
            .map(|y| {
                (0u16..80)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<Vec<_>>()
                    .concat()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Delete Email Account"), "{rendered}");
        assert!(
            rendered.contains("Delete email account \"Personal\" from jerc?"),
            "{rendered}"
        );
        assert!(rendered.contains("[Y]es"), "{rendered}");
        assert!(rendered.contains("[N]o"), "{rendered}");
        assert!(!rendered.contains("Remove local email account"));
        assert!(!rendered.contains("OAuth"));
        assert!(!rendered.contains("cached mail"));
        assert!(!rendered.contains("postponed drafts"));
        assert!(!rendered.contains("[n/Esc]"));
    }

    #[test]
    fn mail_review_uses_action_specific_titles_and_archive_warning_color() {
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let (archive_title, archive_border, archive_yes) = mail_review_presentation(&theme, true);
        let (delete_title, delete_border, delete_yes) = mail_review_presentation(&theme, false);
        assert_eq!(archive_title.trim(), "Archive Emails");
        assert_eq!(delete_title.trim(), "Delete Emails");
        assert_eq!(archive_border.fg, Some(theme.warning));
        assert_eq!(archive_yes.bg, Some(theme.warning));
        assert_eq!(delete_border.fg, Some(theme.error));
        assert_eq!(delete_yes.bg, Some(theme.error));
    }

    #[test]
    fn mail_review_columns_align_and_truncate_both_fields_independently() {
        let theme = load_themes().into_iter().next().unwrap();
        let width = 58;
        let one = mail_review_line(
            Some(1),
            2,
            "Short",
            "alice@example.com",
            width,
            &theme,
            false,
        );
        let long = mail_review_line(
            Some(12),
            2,
            "A very long title with kanji 漢字 and more words that must not eat the sender column",
            "really.really.long.email.address@example-larger-than-normal-domain.test",
            width,
            &theme,
            true,
        );
        let header = mail_review_line(None, 2, "Subject", "From", width, &theme, false);
        let subject_start = UnicodeWidthStr::width(one.spans[0].content.as_ref());
        assert_eq!(
            subject_start,
            UnicodeWidthStr::width(long.spans[0].content.as_ref())
        );
        assert_eq!(
            subject_start,
            UnicodeWidthStr::width(header.spans[0].content.as_ref())
        );
        let address_offset = |line: &Line<'_>| {
            line.spans[..3]
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
        };
        assert_eq!(address_offset(&one), address_offset(&long));
        assert_eq!(address_offset(&one), address_offset(&header));
        assert_eq!(one.spans[3].content.as_ref(), "alice@example.com");
        assert!(long.spans[1].content.contains('…'));
        assert!(long.spans[3].content.contains('…'));
        assert!(long.spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(long.spans[1].style.bg, Some(theme.popup_bg));
        for row in [&one, &long, &header] {
            assert!(
                row.spans
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum::<usize>()
                    <= width,
                "review row exceeded {width} terminal columns: {row:?}"
            );
        }
    }

    #[test]
    fn mail_review_highlight_follows_scroll_and_keeps_selected_email_visible() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = load_themes().into_iter().next().unwrap();
        let rows = (0..50)
            .map(|i| MailReviewItem {
                subject: format!("Subject-{i:02}"),
                from_address: format!("sender-{i:02}@example.com"),
            })
            .collect::<Vec<_>>();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw_mail_review_dialog(frame, &theme, "Archive 50 emails?", &rows, 34, true)
            })
            .unwrap();
        let rendered = (0u16..20)
            .map(|y| {
                (0u16..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Archive Emails"), "{rendered}");
        assert!(rendered.contains("Subject"), "{rendered}");
        assert!(rendered.contains("From"), "{rendered}");
        assert!(rendered.contains("Subject-34"), "{rendered}");
        assert!(rendered.contains("sender-34@example.com"), "{rendered}");
        assert!(!rendered.contains("Subject-00"), "{rendered}");
        assert!(rendered.contains("[Y]es"), "{rendered}");
        assert!(rendered.contains("[N]o"), "{rendered}");
        assert!(!rendered.contains("IMAP expunge"));
    }

    #[test]
    fn mail_review_fits_small_terminal_without_adding_a_second_row() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = load_themes().into_iter().next().unwrap();
        let rows = vec![MailReviewItem {
            subject: "A very very very long email subject".to_owned(),
            from_address: "someone.with.a.long.address@example.com".to_owned(),
        }];
        for (width, height) in [(40u16, 15u16), (60u16, 16u16)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    draw_mail_review_dialog(
                        frame,
                        &theme,
                        "Move 1 email to Trash?",
                        &rows,
                        0,
                        false,
                    );
                })
                .unwrap();
            let rendered = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(rendered.contains("Delete Emails"), "{rendered}");
            assert!(rendered.contains("[Y]es"), "{rendered}");
        }
    }
}
