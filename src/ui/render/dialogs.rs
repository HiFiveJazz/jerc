//! Dialogs rendering.

use super::*;

/// The exit prompt uses the same two-button layout as deletion reviews. Yes
/// saves the draft; No discards it. Esc still returns to editing, without an
/// extra on-screen instruction competing with the actions.
pub(super) fn draw_compose_exit_prompt(frame: &mut Frame<'_>, theme: &Theme) {
    let viewport = frame.area();
    let width = viewport.width.min(62);
    let height = viewport.height.min(7);
    let area = Rect::new(
        viewport.x + viewport.width.saturating_sub(width) / 2,
        viewport.y + viewport.height.saturating_sub(height) / 2,
        width, height,
    );
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Save unsent email? ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.border());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 10 || inner.height < 3 { return; }
    let prompt = "Save this email to Drafts before closing?";
    frame.render_widget(Paragraph::new(truncate_display_width(prompt, usize::from(inner.width)))
        .alignment(Alignment::Center).style(theme.text()),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1));
    let buttons = Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 2);
    draw_confirmation_buttons(frame, theme, buttons,
        setup_button_style(theme), destructive_button_style(theme));
}

pub(super) fn draw_remove_account_confirmation(frame: &mut Frame<'_>, app: &App) {
    let Some(name) = app.remove_account_confirmation.as_ref() else { return; };
    let theme = app.theme();
    let area = centered_rect(frame.area(), 72, 27);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Remove local email account ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.error());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(format!(
        "Remove account {:?} from aerc-rs?\n\nThis edits only config.toml. \
         No messages or folders on the server are deleted. Cached OAuth tokens\nare not revoked.\n\n[y] Remove   [n/Esc] Cancel", name))
        .style(theme.text()).wrap(Wrap { trim: false }), inner);
}

/// Shared Yazi-style review: simple, aligned item numbers without a visual
/// selection gutter. The list is for reviewing, not selecting, its contents.
pub(super) fn review_item_line(number: usize, digits: usize, item: &str, width: usize,
    theme: &Theme) -> Line<'static> {
    let prefix = format!(" {number:>digits$}  ");
    let available = width.saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
    Line::from(vec![
        Span::styled(prefix, theme.muted()),
        Span::styled(truncate_display_width(item, available), theme.text()),
    ])
}

/// Shared horizontal rule and centered Yes/No buttons for confirmation popups.
/// The calling dialog decides which action is destructive.
pub(super) fn draw_confirmation_buttons(frame: &mut Frame<'_>, theme: &Theme, area: Rect,
    yes_style: Style, no_style: Style) {
    if area.height < 2 || area.width == 0 { return; }
    frame.render_widget(Paragraph::new("─".repeat(usize::from(area.width)))
        .style(theme.border()), Rect::new(area.x, area.y, area.width, 1));
    let gap = 3u16;
    let yes_width = 11u16;
    let no_width = 11u16;
    let total = yes_width + no_width + gap;
    let x = area.x + area.width.saturating_sub(total) / 2;
    let y = area.y.saturating_add(1);
    frame.render_widget(Paragraph::new(" [Y]es ").alignment(Alignment::Center)
        .style(yes_style), Rect::new(x, y, yes_width.min(area.width), 1));
    if area.width > yes_width + gap {
        frame.render_widget(Paragraph::new(" [N]o ").alignment(Alignment::Center)
            .style(no_style), Rect::new(x + yes_width + gap, y,
                no_width.min(area.right().saturating_sub(x + yes_width + gap)), 1));
    }
}

/// Shared Yazi-style review: visible item list, scroll position, and the same
/// filled action treatment as the setup wizard. No action is taken by drawing.
pub(super) fn draw_review_dialog(frame: &mut Frame<'_>, theme: &Theme, title: &str,
    explanation: &str, items: &[String], scroll: usize) {
    let area = centered_rect(frame.area(), 86, 72);
    frame.render_widget(Clear, area);
    let block = Block::default().title(format!(" {title} "))
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.error());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 10 || inner.height < 5 { return; }
    let chunks = Layout::vertical([
        Constraint::Length(2), Constraint::Min(1),
        Constraint::Length(1), Constraint::Length(2),
    ]).split(inner);
    frame.render_widget(Paragraph::new(truncate_display_width(explanation, usize::from(chunks[0].width)))
        .style(theme.text()), chunks[0]);
    let capacity = usize::from(chunks[1].height);
    let start = scroll.min(items.len().saturating_sub(capacity));
    let digits = items.len().max(1).to_string().len();
    for (offset, item) in items.iter().skip(start).take(capacity).enumerate() {
        let row = Rect::new(chunks[1].x, chunks[1].y + offset as u16, chunks[1].width, 1);
        frame.render_widget(Paragraph::new(review_item_line(start + offset + 1, digits,
            item, usize::from(row.width), theme)), row);
    }
    frame.render_widget(Paragraph::new(format!(
        " {} items  ·  {}–{}",
        items.len(), if items.is_empty() { 0 } else { start + 1 }, (start + capacity).min(items.len()),
    )).style(theme.muted()), chunks[2]);
    draw_confirmation_buttons(frame, theme, chunks[3],
        destructive_button_style(theme), theme.current_line());
}

pub(super) fn draw_confirmation(frame: &mut Frame<'_>, app: &App) {
    let Some(confirmation) = app.confirmation.as_ref() else { return; };
    draw_review_dialog(frame, app.theme(), "Review mail action", &confirmation.prompt,
        &confirmation.items, confirmation.review.position);
}

pub(super) fn draw_detach_confirmation(frame: &mut Frame<'_>, app: &App) {
    let Some(confirmation) = app.detach_confirmation.as_ref() else { return; };
    let names = confirmation.paths.iter().map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    draw_review_dialog(frame, app.theme(), "Detach attachments",
        "Remove these attachments from the outgoing message?", &names,
        confirmation.review.position);
}

// A full outgoing-message preview, using the same colored headers and rich
// HTML renderer as the message reader. Enter sends; Esc returns to the editor.
pub(super) fn draw_send_confirmation(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme();
    let Some(composer) = app.composer.as_ref().filter(|composer| composer.confirm_send) else { return; };
    let area = centered_rect(frame.area(), frame.area().width.saturating_sub(4).min(100), 88);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Review outgoing email ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 8 { return; }
    let footer_y = inner.bottom().saturating_sub(1);
    let headers = [
        ("From", app.account().map(|account| account.from.as_str()).unwrap_or("")),
        ("To", composer.to.as_str()),
        ("Cc", composer.cc.as_str()),
        ("Bcc", composer.bcc.as_str()),
        ("Subject", composer.subject.as_str()),
    ];
    let mut y = inner.y;
    for (label, value) in headers {
        if y >= footer_y.saturating_sub(3) { break; }
        let value_style = if value.trim().is_empty() { theme.muted() } else { theme.accent() };
        let line = Line::from(vec![
            Span::styled(format!(" {label:<8} "), theme.muted()),
            Span::styled(if value.trim().is_empty() { "—" } else { value }, value_style),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
        y += 1;
    }
    let mut filenames = compose_attachment_names(composer);
    filenames.extend(composer.preview_forwarded_attachments.iter().cloned());
    let files = if filenames.is_empty() { "none".to_owned() } else { filenames.join(" · ") };
    let attachment_text = format!(" Attachments ({})  {files}", filenames.len());
    let lines = clipped_wrap_text(&attachment_text, usize::from(inner.width), 2);
    let attachment_height = (lines.len() as u16).min(2);
    if y.saturating_add(attachment_height) < footer_y.saturating_sub(1) {
        frame.render_widget(Paragraph::new(lines.into_iter().map(|line| Line::styled(line,
            if !filenames.is_empty() { theme.accent() } else { theme.muted() })).collect::<Vec<_>>()),
            Rect::new(inner.x, y, inner.width, attachment_height));
        y += attachment_height;
    }
    if y >= footer_y { return; }
    frame.render_widget(Paragraph::new(Line::from("─".repeat(usize::from(inner.width))))
        .style(theme.border()), Rect::new(inner.x, y, inner.width, 1));
    y += 1;
    let content_height = footer_y.saturating_sub(y);
    if content_height > 0 {
        let body = if composer.preview_html {
            composer.preview_rich_lines.as_ref().map(|lines| Text::from(lines.iter().map(|line| {
                Line::from(line.spans.iter().map(|span| {
                    Span::styled(span.text.clone(), rich_html_style(&span.style, &span.text, theme))
                }).collect::<Vec<_>>())
            }).collect::<Vec<_>>()))
        } else { None };
        let body = body.unwrap_or_else(|| Text::from(composer.body.clone()));
        let max_scroll = body.lines.len().saturating_sub(usize::from(content_height));
        let scroll = composer.preview_scroll.min(max_scroll);
        frame.render_widget(Paragraph::new(body).style(theme.text())
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
            Rect::new(inner.x.saturating_add(1), y, inner.width.saturating_sub(2), content_height));
    }
    let mode = if composer.preview_html && composer.preview_rich_lines.is_some() { "HTML" } else { "Plain" };
    let footer = Line::from(vec![
        Span::styled(" Enter ", Style::default().bg(theme.accent).fg(Color::Black).add_modifier(Modifier::BOLD)),
        Span::styled(" Send   ", theme.text()),
        Span::styled("Esc", theme.accent()), Span::styled(" Edit   ", theme.muted()),
        Span::styled("t", theme.accent()), Span::styled(format!(" {mode}   "), theme.muted()),
        Span::styled("O", theme.accent()), Span::styled(" Browser   ", theme.muted()),
        Span::styled("j/k", theme.accent()), Span::styled(" Scroll", theme.muted()),
    ]);
    frame.render_widget(Paragraph::new(footer), Rect::new(inner.x, footer_y, inner.width, 1));
}
