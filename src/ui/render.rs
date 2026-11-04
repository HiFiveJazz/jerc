use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui_image::Image as TerminalImage;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{
    App, ComposeField, ComposeMode, COMPOSE_HELP_ENTRIES, PICKER_HELP_ENTRIES, HELP_ENTRIES, ReaderSelection,
};
use crate::model::{HtmlLine, HtmlSpanStyle, MessageFlags};
use crate::theme::Theme;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    draw_tabs(frame, app, outer[0]);
    draw_main(frame, app, outer[1]);
    draw_status(frame, app, outer[2]);

    if app.help_visible {
        draw_help(frame, app);
    }
    if app.show_startup_warnings {
        draw_startup_warnings(frame, app);
    }
    if app.attachment_picker_visible {
        draw_attachments(frame, app);
        if app.attachment_destination_picker.is_some() {
            draw_attachment_destination_picker(frame, app);
        }
    }
    if app.theme_picker_visible {
        draw_theme_picker(frame, app);
    }
    if app.confirmation.is_some() {
        draw_confirmation(frame, app);
    }
    if app.composer.is_some() {
        draw_composer(frame, app);
        if app.composer.as_ref().is_some_and(|composer| composer.exit_prompt) {
            draw_compose_exit_prompt(frame, app);
        } else if app.composer.as_ref().is_some_and(|composer| composer.confirm_send) {
            draw_send_confirmation(frame, app);
        } else if app.composer.as_ref().is_some_and(|composer| composer.help_visible) {
            draw_compose_help(frame, app);
        } else if app.composer.as_ref().is_some_and(|composer| composer.attachment_manager_open) {
            draw_compose_attachment_manager(frame, app);
        } else if app.composer.as_ref().is_some_and(|composer| composer.file_picker.is_some()) {
            draw_compose_file_picker(frame, app);
        } else if app.composer.as_ref().is_some_and(|composer| composer.advanced_headers_open) {
            draw_compose_advanced_options(frame, app);
        }
    }
}

fn draw_tabs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = app.theme().clone();
    let mut spans = Vec::new();
    for (index, account) in app.accounts.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if index == app.account_index {
            theme.selected()
        } else {
            theme.muted()
        };
        spans.push(Span::styled(format!(" {} ", account.name), style));
    }
    if !app.startup_warnings.is_empty() {
        spans.push(Span::styled(
            format!("  ! {} config issue(s) [! to inspect]", app.startup_warnings.len()),
            theme.error(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.text()), area);
}

fn draw_startup_warnings(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let area = centered_rect(frame.area(), 85, 55);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Account configuration warnings ")
        .borders(Borders::ALL).border_style(theme.error());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let details = app.startup_warnings.iter().enumerate()
        .map(|(i, issue)| format!("{}. {}\n", i + 1, issue))
        .collect::<Vec<_>>().join("\n");
    frame.render_widget(Paragraph::new(format!(
        "Other usable accounts remain active. Fix config.toml and restart to re-enable affected accounts or sending.\n\n{details}\nEsc / q / ! to close"
    )).style(theme.text()).wrap(Wrap { trim: false }), inner);
}

fn draw_main(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(_account) = app.account() else {
        frame.render_widget(
            Paragraph::new("No accounts configured").style(app.theme().text()),
            area,
        );
        return;
    };

    if app.message_open {
        draw_message(frame, app, area);
        return;
    }

    match (app.sidebar_visible, app.preview_split) {
        (true, false) => {
            let mailbox_width = super::mailbox_column_width(area.width, app.mailbox_width_percent);
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(mailbox_width), Constraint::Min(1)])
                .split(area);
            draw_mailboxes(frame, app, columns[0]);
            draw_messages(frame, app, columns[1]);
        }
        (false, false) => draw_messages(frame, app, area),
        (true, true) => {
            let mailbox_width = super::mailbox_column_width(area.width, app.mailbox_width_percent);
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(mailbox_width), Constraint::Min(1)])
                .split(area);
            let right = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
                .split(columns[1]);
            draw_mailboxes(frame, app, columns[0]);
            draw_messages(frame, app, right[0]);
            draw_message(frame, app, right[1]);
        }
        (false, true) => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
                .split(area);
            draw_messages(frame, app, columns[0]);
            draw_message(frame, app, columns[1]);
        }
    }
}

fn draw_mailboxes(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.set_mailbox_view_height(usize::from(area.height.max(1)));
    let theme = app.theme().clone();
    let account = app.account().expect("account exists");
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(if app.mailbox_resizing { theme.accent() } else { theme.border() });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if account.mailboxes.is_empty() {
        let text = if account.connecting || account.mailboxes_loading {
            "Loading mailboxes…"
        } else {
            "No mailboxes"
        };
        frame.render_widget(Paragraph::new(text).style(theme.muted()), inner);
        return;
    }

    let height = usize::from(inner.height.max(1));
    let start = account
        .mailbox_view_offset
        .min(account.mailboxes.len().saturating_sub(1));
    let end = start.saturating_add(height).min(account.mailboxes.len());
    let width = usize::from(inner.width);
    let lines = account.mailboxes[start..end]
        .iter()
        .enumerate()
        .map(|(visible_index, mailbox)| {
            let absolute_index = start + visible_index;
            let count = if !mailbox.counts_known {
                "…".to_owned()
            } else if mailbox.unread > 0 {
                mailbox.unread.to_string()
            } else {
                String::new()
            };
            let style = if absolute_index == account.mailbox_index {
                theme.current_line()
            } else {
                theme.text()
            };
            Line::styled(mailbox_row_text(&mailbox.name, &count, width), style)
        })
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(Text::from(lines)).style(theme.text()), inner);
}

fn draw_messages(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let theme = app.theme().clone();
    frame.render_widget(Clear, area);

    let search_editing = app.account().is_some_and(|account| account.search_editing);
    let search_query = app
        .account()
        .map(|account| account.search_query.clone())
        .unwrap_or_default();
    let search_split = if search_editing && area.height > 1 {
        Some(
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(area),
        )
    } else {
        None
    };
    let list_area = search_split.as_ref().map_or(area, |chunks| chunks[1]);
    if let Some(chunks) = &search_split {
        draw_search_header(frame, chunks[0], &search_query, app);
    }

    let (visible, current_position, offset, loading, current_mailbox) = {
        let account = app.account().expect("account exists");
        let visible = account.visible_message_indices();
        let current_position = visible
            .iter()
            .position(|index| *index == account.message_index);
        (
            visible,
            current_position,
            account.message_view_offset,
            account.messages_loading,
            account
                .current_mailbox
                .clone()
                .unwrap_or_else(|| "mailbox".to_owned()),
        )
    };

    app.set_message_view_height(usize::from(list_area.height.max(1)));

    if visible.is_empty() {
        let message = if loading {
            " Loading messages…".to_owned()
        } else if !search_query.is_empty() {
            format!(" No loaded messages match /{search_query}")
        } else {
            format!(" No messages in {current_mailbox}.")
        };
        frame.render_widget(Paragraph::new(message).style(theme.muted()), list_area);
        return;
    }

    let height = usize::from(list_area.height.max(1));
    let current_position = current_position.unwrap_or(0);
    // A message with files has two display lines, like Gmail's filename chips.
    // Compute the viewport in terminal rows rather than assuming every email
    // consumes one line (which clipped selected rows at the bottom).
    let row_heights = {
        let account = app.account().expect("account exists");
        visible.iter().map(|&index| {
            let message = &account.messages[index];
            if message.has_attachment(account.bodies.get(&message.id)) { 2usize } else { 1 }
        }).collect::<Vec<_>>()
    };
    let mut start = offset.min(visible.len().saturating_sub(1));
    if current_position < start { start = current_position; }
    let mut cursor_window_rows = row_heights[start..=current_position].iter().sum::<usize>();
    while start < current_position && cursor_window_rows > height {
        cursor_window_rows -= row_heights[start];
        start += 1;
    }
    let mut end = start;
    let mut used = 0usize;
    while end < visible.len() && used + row_heights[end] <= height {
        used += row_heights[end];
        end += 1;
    }
    if end == start { end = (start + 1).min(visible.len()); }
    if let Some(account) = app.account_mut() { account.message_view_offset = start; }
    let shown = &visible[start..end];
    let number_width = relative_number_width(visible.len());
    let message_columns = [
        Constraint::Length(number_width),
        Constraint::Length(2),
        Constraint::Percentage(28),
        Constraint::Min(12),
        Constraint::Length(6),
    ];
    let column_areas = Layout::horizontal(message_columns.clone())
        .spacing(1)
        .split(list_area);
    // Use the *actual* subject column width. The old two-cell safety gutter
    // and forced U+FE0E presentation selectors clipped Apple Color Emoji even
    // when the remaining cells were empty.
    let subject_width = column_areas.get(3).map_or(0, |area| usize::from(area.width));
    let rows = shown.iter().enumerate().map(|(row_position, index)| {
        let account = app.account().expect("account exists");
        let message = &account.messages[*index];
        let absolute_position = start + row_position;
        let number = relative_number(absolute_position, current_position);
        let visual_selected = app.message_is_visual_selected(*index);
        let current = absolute_position == current_position;
        let unread_mark = if visual_selected { "✓" } else if message.flags.contains(MessageFlags::SEEN) { " " } else { "●" };
        let attachment_mark = message.has_attachment(account.bodies.get(&message.id));
        // A separate box glyph in the mark column, never embedded in the
        // subject where it could be mistaken for part of the title.
        let mark = Line::from(vec![
            Span::raw(unread_mark),
            if attachment_mark {
                Span::styled("⧉", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))
            } else {
                Span::raw(" ")
            },
        ]);
        let sender = if message.from_name.is_empty() {
            &message.from_address
        } else {
            &message.from_name
        };
        let style = if visual_selected {
            theme.selected()
        } else if current && message.unread() {
            theme.unread().bg(theme.popup_bg)
        } else if current {
            theme.current_line()
        } else if message.unread() {
            theme.unread()
        } else {
            theme.text()
        };
        let mut subject_lines = vec![Line::raw(
            truncate_display_width(&subject_for_list(&message.subject), subject_width)
        )];
        if attachment_mark {
            let names: &[String] = account.bodies.get(&message.id)
                .map_or(message.attachment_names_hint.as_slice(), |body| body.attachment_names.as_slice());
            let subtitle = attachment_subtitle(names);
            subject_lines.push(Line::styled(truncate_display_width(&subtitle, subject_width), theme.muted()));
        }
        Row::new(vec![
            Cell::from(number),
            Cell::from(mark),
            Cell::from(sender.clone()),
            Cell::from(Text::from(subject_lines)),
            Cell::from(message.date_label.clone()),
        ])
        .height(if attachment_mark { 2 } else { 1 })
        .style(style)
    });

    let table = Table::new(rows, message_columns)
    .style(theme.text())
    .column_spacing(1);
    frame.render_widget(table, list_area);

}

/// Attachment chips share the same left edge as the first subject character.
fn attachment_subtitle(names: &[String]) -> String {
    if names.is_empty() { "Attachment".to_owned() }
    else { names.iter().take(2).map(String::as_str).collect::<Vec<_>>().join(" · ") }
}

fn draw_message(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let theme = app.theme().clone();
    let content_area = if !app.message_open {
        let block = Block::default()
            .borders(Borders::LEFT)
            .border_style(theme.border());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };

    if app.account().and_then(|account| account.selected_message()).is_none() {
        frame.render_widget(
            Paragraph::new("No message selected").style(theme.muted()),
            content_area,
        );
        app.set_reader_layout(0, 0, usize::from(content_area.height.max(1)), 0);
        return;
    }

    // html2text performs real table/centering decisions while it renders. Reflow
    // the original HTML to the actual reader width rather than freezing every
    // message at the worker's fallback width and wrapping that output later.
    let reader_active = app.message_open;
    let gutter_width = if reader_active { 5usize } else { 0 };
    let body_width = usize::from(content_area.width)
        .saturating_sub(gutter_width)
        .max(20);
    app.ensure_reader_html_layout(body_width);
    app.ensure_reader_image(
        content_area
            .width
            .saturating_sub(u16::try_from(gutter_width).unwrap_or(0)),
        // Leave the header/divider visible but otherwise let images use the real
        // reader viewport. The old fixed 12-row cap made banners/photos much
        // smaller than the same message in a browser.
        content_area.height.saturating_sub(6).max(1),
    );
    let reader_image_size = app.reader_image_protocol().map(|image| image.size());

    let reader_cursor = app.reader_cursor;
    let reader_column = app.reader_column;
    let reader_visual = app.reader_visual_range();
    let reader_illuminate = (reader_active && reader_visual.is_none())
        .then(|| {
            let account = app.account()?;
            let body = account.selected_body()?;
            let showing_html = body.has_html_alternative() && account.prefer_html;
            reader_word_at(body.display_text(showing_html), reader_cursor, reader_column)
        })
        .flatten();

    let (lines, body_start, body_line_count, image_start) = {
        let account = app.account().expect("account exists");
        let message = account.selected_message().expect("message checked above");

        let from = if message.from_name.is_empty() {
            message.from_address.clone()
        } else if message.from_address.is_empty() {
            message.from_name.clone()
        } else {
            format!("{} <{}>", message.from_name, message.from_address)
        };
        let mut lines = vec![
            Line::from(vec![Span::styled("From: ", theme.accent()), Span::raw(from)]),
            Line::from(vec![Span::styled("Date: ", theme.accent()), Span::raw(
                if message.timestamp > 0 {
                    chrono::DateTime::<chrono::Utc>::from_timestamp(message.timestamp, 0)
                        .map(|date| date.with_timezone(&chrono::Local)
                            .format("%a, %b %-d, %Y at %-I:%M %p %Z").to_string())
                        .unwrap_or_else(|| "unknown".to_owned())
                } else { "unknown".to_owned() }
            )]),
            Line::from(vec![Span::styled("To: ", theme.accent()), Span::raw(
                if message.to.is_empty() { "(not supplied)".to_owned() } else { message.to.clone() }
            )]),
            Line::from(vec![
                Span::styled("Subject: ", theme.accent()),
                Span::raw(message.subject.clone()),
            ]),
        ];
        if !message.cc.is_empty() {
            lines.push(Line::from(vec![Span::styled("Cc: ", theme.accent()), Span::raw(message.cc.clone())]));
        }
        if !message.bcc.is_empty() {
            lines.push(Line::from(vec![Span::styled("Bcc: ", theme.accent()), Span::raw(message.bcc.clone())]));
        }
        if app.show_message_ids {
            if let Some(message_id) = &message.message_id {
                lines.push(Line::from(vec![
                    Span::styled("Message-ID: ", theme.accent()),
                    Span::raw(format!("<{message_id}>")),
                ]));
            }
        }
        if let Some(body) = account.selected_body() {
            if body.attachment_count > 0 {
                let preview = body.attachment_names.iter().take(2).cloned().collect::<Vec<_>>().join("  ·  ");
                let overflow = body.attachment_names.len().saturating_sub(2);
                let label = if preview.is_empty() {
                    format!("{} attachment(s)", body.attachment_count)
                } else if overflow > 0 {
                    format!("{preview}  +{overflow} more")
                } else { preview };
                lines.push(Line::from(vec![
                    Span::styled("⧉  ", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
                    Span::styled(label, theme.text()),
                    Span::styled("   z: view", theme.muted()),
                ]));
            }
        } else if message.has_attachment_hint {
            let names = message.attachment_names_hint.iter().take(2).cloned().collect::<Vec<_>>().join("  ·  ");
            lines.push(Line::from(vec![Span::styled("⧉  ", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
                Span::styled(if names.is_empty() { "Attachments (z: browse)".to_owned() } else { names }, theme.text())]));
        }
        lines.push(Line::styled(
            "─".repeat(usize::from(content_area.width)),
            theme.border(),
        ));
        lines.push(Line::raw(""));

        let image_start = reader_image_size.map(|_| lines.len());
        if let Some(size) = reader_image_size {
            lines.extend((0..usize::from(size.height.saturating_add(1))).map(|_| Line::raw("")));
        }
        let body_start = lines.len();
        let mut body_line_count = 0usize;

        if let Some(body) = account.selected_body() {
            let showing_html = body.has_html_alternative() && account.prefer_html;
            if showing_html {
                if let Some(html_lines) = &body.html_lines {
                    body_line_count = html_lines.len();
                    lines.extend(html_lines.iter().enumerate().map(|(line_index, line)| {
                        rich_html_line(
                            line,
                            line_index,
                            body_line_count,
                            body_width,
                            reader_cursor,
                            reader_column,
                            reader_visual,
                            reader_active,
                            &theme,
                            reader_illuminate.as_deref(),
                        )
                    }));
                } else {
                    let rendered = body.display_text(true).lines().collect::<Vec<_>>();
                    body_line_count = rendered.len();
                    lines.extend(rendered.into_iter().enumerate().map(|(line_index, line)| {
                        plain_reader_line(
                            line,
                            line_index,
                            body_line_count,
                            body_width,
                            reader_cursor,
                            reader_column,
                            reader_visual,
                            reader_active,
                            &theme,
                            reader_illuminate.as_deref(),
                        )
                    }));
                }
            } else {
                let rendered = body.display_text(false).lines().collect::<Vec<_>>();
                body_line_count = rendered.len();
                lines.extend(rendered.into_iter().enumerate().map(|(line_index, line)| {
                    plain_reader_line(
                        line,
                        line_index,
                        body_line_count,
                        body_width,
                        reader_cursor,
                        reader_column,
                        reader_visual,
                        reader_active,
                        &theme,
                        reader_illuminate.as_deref(),
                    )
                }));
            }

        } else if account.is_loading_body() {
            lines.push(Line::styled("Loading message…", theme.muted()));
        } else {
            lines.push(Line::styled("Message body not loaded", theme.muted()));
        }

        (lines, body_start, body_line_count, image_start)
    };

    let image_visual_start = image_start.map(|start| {
        Paragraph::new(Text::from(lines[..start].to_vec()))
            .wrap(Wrap { trim: false })
            .line_count(content_area.width.max(1))
    });
    let paragraph = Paragraph::new(Text::from(lines))
        .style(theme.text())
        .wrap(Wrap { trim: false });
    let visual_lines = paragraph.line_count(content_area.width.max(1));
    let viewport_height = usize::from(content_area.height.max(1));
    let max_scroll =
        u16::try_from(visual_lines.saturating_sub(viewport_height)).unwrap_or(u16::MAX);
    app.set_reader_layout(body_start, body_line_count, viewport_height, max_scroll);
    let scroll = app.account().map_or(0, |account| account.preview_scroll);
    frame.render_widget(paragraph.scroll((scroll, 0)), content_area);

    if let (Some(protocol), Some(start)) = (app.reader_image_protocol(), image_visual_start)
        && start >= usize::from(scroll)
    {
        let size = protocol.size();
        let gutter = u16::try_from(gutter_width).unwrap_or(0);
        let available = content_area.width.saturating_sub(gutter);
        let x = content_area
            .x
            .saturating_add(gutter)
            .saturating_add(available.saturating_sub(size.width) / 2);
        let visible_start = start.saturating_sub(usize::from(scroll));
        let y = content_area
            .y
            .saturating_add(u16::try_from(visible_start).unwrap_or(u16::MAX));
        if y.saturating_add(size.height) <= content_area.bottom() {
            frame.render_widget(
                TerminalImage::new(protocol),
                Rect::new(x, y, size.width.min(available), size.height),
            );
        }
    }
}

/// Git unified diff styling. Email quoting (`> `) is ignored while
/// classifying the patch, so reviewing a mailed patch resembles lazygitrs.
fn diff_line_style(line: &str, theme: &Theme) -> Option<Style> {
    let mut text = line;
    while let Some(after) = text.strip_prefix('>') {
        text = after.strip_prefix(' ').unwrap_or(after);
    }
    if text.starts_with("diff --git ") || text.starts_with("index ")
        || text.starts_with("--- ") || text.starts_with("+++ ")
        || text.starts_with("new file mode ") || text.starts_with("deleted file mode ")
    {
        Some(theme.accent())
    } else if text.starts_with("@@") || text.starts_with("\\ No newline at end of file") {
        Some(Style::default().fg(Color::Rgb(122, 162, 247)).add_modifier(Modifier::BOLD))
    } else if text.starts_with('+') {
        Some(Style::default().fg(Color::Rgb(158, 206, 106)))
    } else if text.starts_with('-') {
        Some(Style::default().fg(Color::Rgb(247, 118, 142)))
    } else { None }
}

fn plain_reader_line(
    text: &str,
    line_index: usize,
    line_count: usize,
    content_width: usize,
    reader_cursor: usize,
    reader_column: usize,
    reader_visual: Option<ReaderSelection>,
    reader_active: bool,
    theme: &crate::theme::Theme,
    illuminate_word: Option<&str>,
) -> Line<'static> {
    let base = diff_line_style(text, theme).unwrap_or_else(|| {
        if is_quoted_line(text) { Style::default().fg(theme.muted) }
        else { theme.text() }
    });
    let illuminate_ranges = illuminate_word
        .map(|word| word_match_ranges(text, word))
        .unwrap_or_default();
    let mut spans = reader_gutter(
        line_index,
        line_count,
        reader_cursor,
        reader_active,
        theme,
    );
    spans.extend(reader_fragments(
        text,
        base,
        0,
        text.chars().count(),
        line_index,
        reader_cursor,
        reader_column,
        reader_visual,
        reader_active,
        theme,
        &illuminate_ranges,
    ));
    pad_reader_line(
        &mut spans,
        text.chars().count(),
        content_width,
        line_index,
        reader_cursor,
        reader_column,
        reader_visual,
        reader_active,
        theme,
    );
    Line::from(spans)
}

fn rich_html_line(
    line: &HtmlLine,
    line_index: usize,
    line_count: usize,
    content_width: usize,
    reader_cursor: usize,
    reader_column: usize,
    reader_visual: Option<ReaderSelection>,
    reader_active: bool,
    theme: &crate::theme::Theme,
    illuminate_word: Option<&str>,
) -> Line<'static> {
    let mut spans = reader_gutter(
        line_index,
        line_count,
        reader_cursor,
        reader_active,
        theme,
    );
    let line_len = line
        .spans
        .iter()
        .map(|span| span.text.chars().count())
        .sum::<usize>();
    let line_text = line
        .spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>();
    let illuminate_ranges = illuminate_word
        .map(|word| word_match_ranges(&line_text, word))
        .unwrap_or_default();
    let quoted = is_quoted_line(&line_text);
    let diff = diff_line_style(&line_text, theme);
    let mut column = 0usize;
    for span in &line.spans {
        let mut style = rich_html_style(&span.style, &span.text, theme);
        if let Some(diff) = diff { style = style.fg(diff.fg.unwrap_or(theme.foreground)); }
        else if quoted { style = style.fg(theme.muted); }
        spans.extend(reader_fragments(
            &span.text,
            style,
            column,
            line_len,
            line_index,
            reader_cursor,
            reader_column,
            reader_visual,
            reader_active,
            theme,
            &illuminate_ranges,
        ));
        column = column.saturating_add(span.text.chars().count());
    }
    pad_reader_line(
        &mut spans,
        column,
        content_width,
        line_index,
        reader_cursor,
        reader_column,
        reader_visual,
        reader_active,
        theme,
    );
    Line::from(spans)
}

fn reader_gutter(
    line_index: usize,
    line_count: usize,
    reader_cursor: usize,
    reader_active: bool,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    if !reader_active {
        return Vec::new();
    }
    let digits = line_count.max(1).to_string().len().max(4);
    let number = if line_index == reader_cursor {
        line_index.saturating_add(1)
    } else {
        line_index.abs_diff(reader_cursor)
    };
    let style = if line_index == reader_cursor {
        theme.accent()
    } else {
        theme.muted()
    };
    vec![Span::styled(format!("{number:>digits$} "), style)]
}

fn reader_fragments(
    text: &str,
    style: Style,
    global_start: usize,
    line_len: usize,
    line_index: usize,
    reader_cursor: usize,
    reader_column: usize,
    reader_visual: Option<ReaderSelection>,
    reader_active: bool,
    theme: &crate::theme::Theme,
    illuminate_ranges: &[(usize, usize)],
) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let len = text.chars().count();
    let global_end = global_start.saturating_add(len);
    let mut boundaries = vec![global_start, global_end];

    let selected_columns = reader_visual
        .and_then(|selection| selection.columns_for_line(line_index, line_len));
    if let Some((start, end)) = selected_columns {
        boundaries.push(start.max(global_start).min(global_end));
        boundaries.push(end.saturating_add(1).max(global_start).min(global_end));
    }
    if reader_active && line_index == reader_cursor {
        boundaries.push(reader_column.max(global_start).min(global_end));
        boundaries.push(
            reader_column
                .saturating_add(1)
                .max(global_start)
                .min(global_end),
        );
    }
    for &(start, end) in illuminate_ranges {
        if start < global_end && end > global_start {
            boundaries.push(start.max(global_start).min(global_end));
            boundaries.push(end.max(global_start).min(global_end));
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut out = Vec::new();
    for pair in boundaries.windows(2) {
        let start = pair[0];
        let end = pair[1];
        if start >= end || start < global_start || end > global_end {
            continue;
        }
        let mut fragment_style = style;
        if reader_active && line_index == reader_cursor {
            fragment_style = fragment_style
                .bg(theme.popup_bg)
                .add_modifier(Modifier::BOLD);
        }
        if illuminate_ranges
            .iter()
            .any(|&(match_start, match_end)| start >= match_start && start < match_end)
        {
            // vim-illuminate delegates the actual reference appearance to the
            // active Neovim colorscheme. Mirror that here by deriving a cool,
            // low-contrast reference background from the mail theme's surface
            // and border colours instead of using accent/muted colours. This
            // matches the #3b4261-style reference highlight used by TokyoNight
            // without adding an underline that the user's Neovim view does not.
            fragment_style = fragment_style.bg(reader_illuminate_bg(theme));
        }
        if selected_columns.is_some_and(|(selected_start, selected_end)| {
            start >= selected_start && start <= selected_end
        }) {
            fragment_style = fragment_style
                .fg(theme.selected_fg)
                .bg(theme.selected_bg)
                .add_modifier(Modifier::BOLD);
        }
        // Keep the Vim block cursor visually independent from theme selection
        // colours. A white block mirrors the reader/editor cursor users expect
        // and remains obvious even when the current line or Visual selection
        // uses a blue/purple background.
        if reader_active && line_index == reader_cursor && start == reader_column {
            fragment_style = reader_cursor_style();
        }
        out.push(Span::styled(
            char_slice(text, start - global_start, end - global_start),
            fragment_style,
        ));
    }
    out
}

fn reader_word_at(text: &str, line_index: usize, column: usize) -> Option<String> {
    let line = text.lines().nth(line_index)?;
    let chars = line.chars().collect::<Vec<_>>();
    let current = *chars.get(column)?;
    if !is_reader_word_char(current) {
        return None;
    }

    let mut start = column;
    while start > 0 && is_reader_word_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = column + 1;
    while end < chars.len() && is_reader_word_char(chars[end]) {
        end += 1;
    }
    Some(chars[start..end].iter().collect())
}

fn word_match_ranges(text: &str, word: &str) -> Vec<(usize, usize)> {
    let chars = text.chars().collect::<Vec<_>>();
    let needle = word.chars().collect::<Vec<_>>();
    if needle.is_empty() || chars.len() < needle.len() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for start in 0..=chars.len() - needle.len() {
        let end = start + needle.len();
        if chars[start..end] != needle[..]
            || start > 0 && is_reader_word_char(chars[start - 1])
            || end < chars.len() && is_reader_word_char(chars[end])
        {
            continue;
        }
        matches.push((start, end));
    }
    matches
}

fn is_reader_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn reader_illuminate_bg(theme: &Theme) -> Color {
    match (theme.popup_bg, theme.border) {
        (Color::Rgb(sr, sg, sb), Color::Rgb(br, bg, bb)) => Color::Rgb(
            mix_channel(sr, br, 82),
            mix_channel(sg, bg, 82),
            mix_channel(sb, bb, 82),
        ),
        _ => Color::Rgb(59, 66, 97),
    }
}

fn mix_channel(from: u8, to: u8, toward_to_percent: u16) -> u8 {
    let toward_to_percent = toward_to_percent.min(100);
    let toward_from_percent = 100 - toward_to_percent;
    ((u16::from(from) * toward_from_percent
        + u16::from(to) * toward_to_percent
        + 50)
        / 100) as u8
}

fn reader_cursor_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn pad_reader_line(
    spans: &mut Vec<Span<'static>>,
    content_len: usize,
    content_width: usize,
    line_index: usize,
    reader_cursor: usize,
    reader_column: usize,
    reader_visual: Option<ReaderSelection>,
    reader_active: bool,
    theme: &crate::theme::Theme,
) {
    if !reader_active {
        return;
    }
    let visual_line_selected = reader_visual.is_some_and(|selection| {
        selection.start.line <= line_index && line_index <= selection.end.line
    });
    let linewise_visual = reader_visual.is_some_and(|selection| {
        selection.start.column == 0 && selection.end.column == usize::MAX
    });
    if line_index != reader_cursor && !visual_line_selected {
        return;
    }
    let mut style = theme.text().add_modifier(Modifier::BOLD);
    if line_index == reader_cursor {
        style = style.bg(theme.popup_bg);
    }
    if visual_line_selected && (linewise_visual || content_len == 0) {
        style = theme.selected();
    }
    let padding = content_width
        .saturating_sub(content_len)
        .max(usize::from(content_len == 0));
    if padding == 0 {
        return;
    }

    // Empty lines (and any future cursor position in trailing padding) have no
    // text fragment in which to draw the block cursor. Split the padding around
    // the cursor cell so the cursor remains visible instead of disappearing.
    let cursor_in_padding = reader_active
        && line_index == reader_cursor
        && reader_column >= content_len
        && reader_column < content_len.saturating_add(padding);
    if !cursor_in_padding {
        spans.push(Span::styled(" ".repeat(padding), style));
        return;
    }

    let before = reader_column.saturating_sub(content_len);
    if before > 0 {
        spans.push(Span::styled(" ".repeat(before), style));
    }
    spans.push(Span::styled(" ", reader_cursor_style()));
    let after = padding.saturating_sub(before.saturating_add(1));
    if after > 0 {
        spans.push(Span::styled(" ".repeat(after), style));
    }
}

fn char_slice(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn rich_html_style(
    style: &HtmlSpanStyle,
    text: &str,
    theme: &crate::theme::Theme,
) -> Style {
    // Sender CSS is designed for a graphical page whose light/dark surface we
    // do not control. Importing literal foreground colours into a themed TUI
    // causes exactly the failure seen in dark-mode mail (white table text on a
    // light selected/current-line background). Keep the sender's *semantics*
    // -- strong, emphasis, links, code, images -- but let the terminal theme
    // own foreground/background colours.
    let mut rendered = if is_box_drawing_span(text) {
        theme.border()
    } else {
        theme.text()
    };
    if style.link.is_some() {
        rendered = rendered
            .fg(theme.accent)
            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD);
    } else if style.bold {
        rendered = rendered
            .fg(theme.unread)
            .add_modifier(Modifier::BOLD);
    }
    if style.image.is_some() {
        rendered = if style.link.is_some() {
            theme
                .accent()
                .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
        } else {
            theme.muted().add_modifier(Modifier::ITALIC)
        };
    }
    if style.italic {
        rendered = rendered.add_modifier(Modifier::ITALIC);
    }
    if style.strikeout {
        rendered = theme.error().add_modifier(Modifier::CROSSED_OUT);
    }
    if style.code {
        rendered = rendered.bg(theme.popup_bg);
    }
    rendered
}

fn is_box_drawing_span(text: &str) -> bool {
    let mut saw_box = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if matches!(
            ch,
            '─' | '│' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼'
                | '╭' | '╮' | '╰' | '╯' | '═' | '║' | '╔' | '╗' | '╚' | '╝'
                | '╠' | '╣' | '╦' | '╩' | '╬'
        ) {
            saw_box = true;
        } else {
            return false;
        }
    }
    saw_box
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = app.theme().clone();
    let mut spans = Vec::new();
    if let Some(account) = app.account() {
        spans.push(Span::styled(format!("[{}] ", account.name), theme.accent()));
        if app.message_open {
            let (label, style) = if app.reader_visual_anchor.is_some() {
                (" VISUAL ", theme.selected())
            } else {
                (" NORMAL ", theme.accent())
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::raw(" "));
        } else if app.visual_anchor.is_some() {
            spans.push(Span::styled(" VISUAL ", theme.selected()));
            spans.push(Span::raw(" "));
        }
        if account.search_editing {
            let matches = account.visible_message_indices().len();
            spans.push(Span::styled(" SEARCH ", theme.accent()));
            spans.push(Span::styled(
                format!("{matches} match{}  ", if matches == 1 { "" } else { "es" }),
                theme.muted(),
            ));
        } else if !account.search_query.is_empty() {
            spans.push(Span::styled(format!("filter:/{}  ", account.search_query), theme.accent()));
        }
        if account.search_editing {
            // The editable query is already rendered in the message-list search
            // header. Avoid echoing the status text below it a second time.
        } else if account.connecting {
            spans.push(Span::raw("Connecting…"));
        } else if account.mailboxes_loading {
            spans.push(Span::raw("Loading mailboxes…"));
        } else if account.messages_loading {
            spans.push(Span::raw("Loading messages…"));
        } else if account.is_loading_body() {
            spans.push(Span::raw("Loading message…"));
        } else {
            let style = if app.status.is_error { theme.error() } else { theme.text() };
            spans.push(Span::styled(app.status.text.as_str(), style));
        }
        if app.preview_split {
            spans.push(Span::styled("    SPLIT", theme.muted()));
        }
    } else {
        spans.push(Span::raw(app.status.text.as_str()));
    }
    if let Some(count) = app.count_prefix {
        spans.push(Span::styled(format!("    {count}"), theme.accent()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.status()), area);
}

fn draw_search_header(frame: &mut Frame<'_>, area: Rect, query: &str, app: &App) {
    let theme = app.theme().clone();
    let prompt = if query.is_empty() { "type to search…" } else { query };
    let style = if query.is_empty() { theme.muted() } else { theme.text() };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" / ", theme.accent()),
            Span::styled(prompt, style),
        ])),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let area = centered_rect(frame.area(), 86, 70);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Keybindings ")
        .borders(Borders::ALL)
        .border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    draw_search_header(frame, chunks[0], &app.help_query, app);

    let matches = app.help_matches();
    let visible = usize::from(chunks[1].height.max(1));
    let max_start = matches.len().saturating_sub(visible);
    let start = app.help_scroll.min(max_start);
    let shown: Vec<usize> = matches.iter().copied().skip(start).take(visible).collect();
    if shown.is_empty() {
        frame.render_widget(Paragraph::new(" No matching keybindings").style(theme.muted()), chunks[1]);
    } else {
        let rows = shown.iter().map(|index| {
            let entry = HELP_ENTRIES[*index];
            Row::new(vec![
                Cell::from(entry.category),
                Cell::from(entry.key),
                Cell::from(entry.description),
            ])
        });
        let table = Table::new(
            rows,
            [Constraint::Length(10), Constraint::Length(22), Constraint::Min(20)],
        )
        .style(theme.text())
        .column_spacing(1)
        .row_highlight_style(theme.selected());
        let selected = shown.iter().position(|index| *index == app.help_selected);
        let mut state = TableState::default().with_selected(selected);
        frame.render_stateful_widget(table, chunks[1], &mut state);
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ↑↓", theme.accent()),
            Span::styled(" navigate   ", theme.muted()),
            Span::styled("type", theme.accent()),
            Span::styled(" filter   ", theme.muted()),
            Span::styled("Backspace", theme.accent()),
            Span::styled(" edit   ", theme.muted()),
            Span::styled("Esc/?", theme.accent()),
            Span::styled(" close", theme.muted()),
        ])),
        chunks[2],
    );
}

fn draw_theme_picker(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let area = centered_rect(frame.area(), 52, 46);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Themes ")
        .borders(Borders::ALL)
        .border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    draw_search_header(frame, chunks[0], &app.theme_query, app);

    let matches = app.theme_matches();
    let visible = usize::from(chunks[1].height.max(1));
    let max_start = matches.len().saturating_sub(visible);
    let start = app.theme_scroll.min(max_start);
    let shown: Vec<usize> = matches.iter().copied().skip(start).take(visible).collect();
    if shown.is_empty() {
        frame.render_widget(Paragraph::new(" No matching themes").style(theme.muted()), chunks[1]);
    } else {
        let rows = shown.iter().map(|index| {
            let candidate = &app.themes[*index];
            let marker = if *index == app.theme_selected { "●" } else { " " };
            Row::new(vec![Cell::from(marker), Cell::from(candidate.name.clone())])
        });
        let table = Table::new(rows, [Constraint::Length(2), Constraint::Min(12)])
            .style(theme.text())
            .row_highlight_style(theme.selected());
        let selected = shown.iter().position(|index| *index == app.theme_selected);
        let mut state = TableState::default().with_selected(selected);
        frame.render_stateful_widget(table, chunks[1], &mut state);
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ↑↓", theme.accent()),
            Span::styled(" preview   ", theme.muted()),
            Span::styled("type", theme.accent()),
            Span::styled(" filter   ", theme.muted()),
            Span::styled("Enter", theme.accent()),
            Span::styled(" keep   ", theme.muted()),
            Span::styled("Esc", theme.accent()),
            Span::styled(" cancel", theme.muted()),
        ])),
        chunks[2],
    );
}

/// Received attachment browser: Yazi-like relative numbers and marks on the
/// left, automatic inline preview on the right, mouse/keyboard actions below.
fn draw_attachments(frame: &mut Frame<'_>, app: &mut App) {
    let theme = app.theme().clone();
    let (area, list_area, preview_area, footer) = super::attachment_regions(frame.area());
    frame.render_widget(Clear, area);
    let count = app.attachment_files.len();
    let mark_count = app.attachment_marked.len();
    let title = if mark_count == 0 { format!(" Attachments · {count} ") }
        else { format!(" Attachments · {count} · {mark_count} marked ") };
    frame.render_widget(Block::default().title(title).borders(Borders::ALL)
        .border_style(theme.accent()), area);

    // Left pane: header row + viewport-centered rows, with relative line
    // numbers (absolute number on the cursor line) and persistent marks.
    frame.render_widget(Block::default().borders(Borders::RIGHT)
        .border_style(theme.border()), list_area);
    frame.render_widget(Paragraph::new("  Files")
        .style(theme.accent()), Rect::new(list_area.x, list_area.y, list_area.width, list_area.height.min(1)));
    let rows = usize::from(list_area.height.saturating_sub(1));
    let selected = app.attachment_selection.min(count.saturating_sub(1));
    let start = selected.saturating_sub(rows / 2).min(count.saturating_sub(rows));
    let end = (start + rows).min(count);
    let digits = count.max(1).to_string().len();
    for index in start..end {
        let file = &app.attachment_files[index];
        let active = index == selected;
        let marked = app.attachment_marked.contains(&index);
        let visual = app.attachment_visual_anchor.is_some_and(|anchor|
            (anchor.min(selected)..=anchor.max(selected)).contains(&index));
        let number = if active { index + 1 } else { index.abs_diff(selected) };
        let prefix = format!("{} {:>digits$} {} ",
            if visual { "▍" } else { " " }, number,
            if marked { "✓" } else if file.content_type.starts_with("image/") { "▧" }
                else if file.content_type == "application/pdf" { "▤" } else { "▫" });
        let available = usize::from(list_area.width.saturating_sub(1))
            .saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
        let name = truncate_display_width(&attachment_display_name(&file.filename), available);
        let style = if active { theme.current_line().add_modifier(Modifier::BOLD) }
            else if visual { theme.selected() }
            else if marked { theme.unread() } else { theme.text() };
        // Paragraph background fills the entire row instead of ending at the
        // filename. This matches the Vim/Yazi full-width cursor highlight.
        let rect = Rect::new(list_area.x, list_area.y.saturating_add(1 + (index - start) as u16),
            list_area.width.saturating_sub(1), 1);
        frame.render_widget(Paragraph::new(Line::from(format!("{prefix}{name}"))).style(style), rect);
    }
    if start == end {
        frame.render_widget(Paragraph::new(if app.attachment_pending_loading() {
            " Loading attachments…"
        } else { " No attachments" }).style(theme.muted()), Rect::new(list_area.x,
            list_area.y.saturating_add(1), list_area.width.saturating_sub(1), 1));
    }

    // Right pane: preview the selected image/text automatically. Image decode
    // happens at most once per selection and viewport size, not every frame.
    let preview_w = preview_area.width.saturating_sub(3);
    let preview_h = preview_area.height.saturating_sub(6);
    app.ensure_attachment_preview(preview_w, preview_h);
    let pleft = preview_area.x.saturating_add(2);
    let pwidth = preview_area.width.saturating_sub(3);
    if let Some(file) = app.attachment_files.get(selected) {
        frame.render_widget(Paragraph::new(truncate_display_width(&attachment_display_name(&file.filename), usize::from(pwidth)))
            .style(theme.accent()), Rect::new(pleft, preview_area.y, pwidth, 1));
        frame.render_widget(Paragraph::new(format!("{}  ·  {}", file.content_type, attachment_human_size(file.data.len())))
            .style(theme.muted()), Rect::new(pleft, preview_area.y.saturating_add(1), pwidth, 1));
        let content = Rect::new(pleft, preview_area.y.saturating_add(3), pwidth,
            preview_area.height.saturating_sub(4));
        if let Some(protocol) = app.attachment_preview() {
            let size = protocol.size();
            if size.width <= content.width && size.height <= content.height {
                let rect = Rect::new(content.x + content.width.saturating_sub(size.width)/2,
                    content.y, size.width, size.height);
                frame.render_widget(TerminalImage::new(protocol), rect);
            }
        } else if let Some(error) = app.attachment_preview_error() {
            frame.render_widget(Paragraph::new(error.to_string()).style(theme.muted())
                .wrap(Wrap { trim: false }), content);
        } else if file.content_type == "application/pdf"
            || file.filename.rsplit('.').next().is_some_and(|ext| ext.eq_ignore_ascii_case("pdf")) {
            let message = if app.attachment_preview_decoded() {
                "PDF thumbnail ready, but this terminal cannot display it.\nPress o to open the PDF."
            } else { "Rendering PDF page 1…\n\nPreview runs off the UI thread using Poppler (pdftoppm)." };
            frame.render_widget(Paragraph::new(message)
                .style(theme.muted()).wrap(Wrap { trim: false }), content);
        } else if file.content_type.starts_with("image/") {
            let message = if app.attachment_preview_decoded() {
                "Image ready, but this terminal cannot display it.\nPress o to open."
            } else { "Preparing image preview…" };
            frame.render_widget(Paragraph::new(message)
                .style(theme.muted()).wrap(Wrap { trim: false }), content);
        } else if let Some(preview) = attachment_text_preview(file) {
            frame.render_widget(Paragraph::new(preview).style(theme.text())
                .wrap(Wrap { trim: false }), content);
        } else {
            frame.render_widget(Paragraph::new("Binary attachment\n\nPress o to open or s to save.\nNo file is opened automatically.")
                .style(theme.muted()).wrap(Wrap { trim: false }), content);
        }
    } else {
        frame.render_widget(Paragraph::new("Select an attachment to preview it")
            .style(theme.muted()), preview_area);
    }

    let footer_first = Rect::new(footer.x, footer.y, footer.width, footer.height.min(1));
    let footer_second = Rect::new(footer.x, footer.y.saturating_add(1), footer.width, footer.height.saturating_sub(1));
    frame.render_widget(Paragraph::new(" [o] Open    [s] Browse & save    [Space] Mark")
        .style(theme.accent()), footer_first);
    frame.render_widget(Paragraph::new(" j/k · 5j/5k · gg/G · Ctrl-j/k smooth · v visual · a all · Esc close")
        .style(theme.muted()), footer_second);
}

/// Save received attachments into a browsed directory (same Yazi-like
/// parent/current/preview layout as the compose attachment picker). Users
/// choose the destination instead of typing arbitrary paths.
fn draw_attachment_destination_picker(frame: &mut Frame<'_>, app: &App) {
    let Some(picker) = app.attachment_destination_picker.as_ref() else { return; };
    let (area, parent_area, files_area, preview_area, footer) =
        super::attachment_destination_regions(frame.area());
    let theme = app.theme();
    frame.render_widget(Clear, area);
    let count = if app.attachment_marked.is_empty() { usize::from(!app.attachment_files.is_empty()) }
        else { app.attachment_marked.len() };
    let block = Block::default().title(format!(" Save {count} attachment(s) · choose destination "))
        .borders(Borders::ALL).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 12 || inner.height < 5 { return; }
    frame.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(" 󰉋 ", theme.accent()),
        Span::styled(truncate_display_width(&picker.directory.display().to_string(),
            usize::from(inner.width.saturating_sub(4))), theme.text().add_modifier(Modifier::BOLD)),
    ])), Rect::new(inner.x, inner.y, inner.width, 1));
    if parent_area.width > 0 && parent_area.height > 0 {
        frame.render_widget(Block::default().borders(Borders::RIGHT).border_style(theme.border()), parent_area);
        compose_picker_rows(frame, Rect::new(parent_area.x, parent_area.y,
            parent_area.width.saturating_sub(1), parent_area.height),
            &picker.parent_entries, Some(picker.parent_selected), false, &[], None, theme);
    }
    if files_area.width > 0 && files_area.height > 0 {
        frame.render_widget(Block::default().borders(Borders::RIGHT).border_style(theme.border()), files_area);
        compose_picker_rows(frame, Rect::new(files_area.x, files_area.y,
            files_area.width.saturating_sub(1), files_area.height),
            &picker.entries, Some(picker.selected), true, &[], None, theme);
    }
    if preview_area.width > 2 && preview_area.height > 0 {
        let preview_rect = Rect::new(preview_area.x.saturating_add(1), preview_area.y,
            preview_area.width.saturating_sub(2), preview_area.height);
        let lines: Vec<Line<'static>> = match &picker.preview {
            crate::app::ComposeFilePreview::Directory(entries) => entries.iter()
                .take(usize::from(preview_rect.height)).map(|entry| Line::styled(
                    format!("{}{}", if entry.is_directory { "󰉋 " } else { "  " }, entry.name),
                    if entry.is_directory { theme.accent() } else { theme.text() })).collect(),
            crate::app::ComposeFilePreview::Binary(bytes) => vec![
                Line::styled("File (choose a folder to enter)", theme.muted()),
                Line::styled(format!("{bytes} bytes"), theme.text())],
            crate::app::ComposeFilePreview::Empty => vec![Line::styled("Empty directory", theme.muted())],
            crate::app::ComposeFilePreview::Error(error) => vec![Line::styled(error.clone(), theme.error())],
            _ => vec![Line::styled("Select a directory", theme.muted())],
        };
        frame.render_widget(Paragraph::new(lines), preview_rect);
    }
    if let Some(name) = &app.attachment_new_folder {
        frame.render_widget(Paragraph::new(format!(" New folder name: {name}▏  Enter create · Esc cancel"))
            .style(theme.accent()), footer);
    } else {
        frame.render_widget(Paragraph::new(" [s] Save here  [Enter/l] Open folder  [h] Parent  [n] New folder  [Esc] Cancel")
            .style(theme.accent()), footer);
    }
}

fn attachment_display_name(name: &str) -> String {
    name.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).collect()
}

fn attachment_human_size(bytes: usize) -> String {
    if bytes < 1024 { format!("{bytes} B") }
    else if bytes < 1024 * 1024 { format!("{:.1} KiB", bytes as f64 / 1024.0) }
    else { format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)) }
}

fn attachment_text_preview(file: &crate::model::OutgoingAttachment) -> Option<String> {
    let name = file.filename.to_ascii_lowercase();
    let textual = file.content_type.starts_with("text/")
        || file.content_type.contains("json")
        || file.content_type.contains("xml")
        || [".txt", ".csv", ".html", ".md", ".rs", ".py", ".toml", ".ini", ".cfg", ".yaml", ".yml", ".log", ".diff", ".patch", ".eml"]
            .iter().any(|suffix| name.ends_with(suffix));
    if !textual { return None; }
    let sample = &file.data[..file.data.len().min(32 * 1024)];
    if sample.contains(&0) { return None; }
    let preview = String::from_utf8_lossy(sample);
    let mut text = preview.lines().take(200).map(|line| {
        line.chars().map(|ch| if ch.is_control() && ch != '\t' { ' ' } else { ch })
            .collect::<String>()
    }).collect::<Vec<_>>().join("\n");
    if file.data.len() > sample.len() { text.push_str("\n… truncated to 32 KiB"); }
    Some(text)
}

fn draw_compose_exit_prompt(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme();
    // A compact dialog avoids a large empty area on tall terminals. The
    // action hints always occupy the last line inside the dialog, not the
    // second line below the prompt as they did on fullscreen terminals.
    let viewport = frame.area();
    let width = viewport.width.min(72).max(1);
    let height = viewport.height.min(9).max(1);
    let area = Rect::new(
        viewport.x + viewport.width.saturating_sub(width) / 2,
        viewport.y + viewport.height.saturating_sub(height) / 2,
        width, height,
    );
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Save unsent email? ")
        .borders(Borders::ALL).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 { return; }
    let prompt = "Save this email to Drafts before closing?";
    frame.render_widget(
        Paragraph::new(prompt).style(theme.text()).wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1)),
    );
    let footer = Line::from(vec![
        Span::styled("Y / Enter", theme.accent()), Span::styled(" Save    ", theme.text()),
        Span::styled("D / N", theme.accent()), Span::styled(" Discard    ", theme.text()),
        Span::styled("Esc", theme.accent()), Span::styled(" Keep editing", theme.text()),
    ]);
    frame.render_widget(Paragraph::new(footer),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
}

fn draw_confirmation(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let Some(confirmation) = app.confirmation.as_ref() else {
        return;
    };
    let area = centered_rect(frame.area(), 72, 24);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Confirm mail action ")
        .borders(Borders::ALL)
        .border_style(theme.error());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(confirmation.prompt.as_str())
            .style(theme.text())
            .wrap(Wrap { trim: false }),
        inner,
    );
}

// A full outgoing-message preview, using the same colored headers and rich
// HTML renderer as the message reader. Enter sends; Esc returns to the editor.
fn draw_send_confirmation(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme();
    let Some(composer) = app.composer.as_ref().filter(|composer| composer.confirm_send) else { return; };
    let area = centered_rect(frame.area(), frame.area().width.saturating_sub(4).min(100), 88);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Review outgoing email ")
        .borders(Borders::ALL).border_style(theme.accent());
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
        Span::styled("T", theme.accent()), Span::styled(format!(" {mode}   "), theme.muted()),
        Span::styled("O", theme.accent()), Span::styled(" Browser   ", theme.muted()),
        Span::styled("j/k", theme.accent()), Span::styled(" Scroll", theme.muted()),
    ]);
    frame.render_widget(Paragraph::new(footer), Rect::new(inner.x, footer_y, inner.width, 1));
}

fn compose_attachment_names(composer: &crate::app::Composer) -> Vec<String> {
    let mut names = composer.attachment_paths.iter().map(|path| path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())).collect::<Vec<_>>();
    names.extend(composer.attachments.split(';').map(str::trim).filter(|path| !path.is_empty())
        .map(|path| std::path::Path::new(path).file_name()
            .map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_owned())));
    names
}

/// Wrap on word boundaries, and truncate only if the available rows run out.
/// This also handles one single filename longer than the entire line.
fn clipped_wrap_text(value: &str, width: usize, max_rows: usize) -> Vec<String> {
    if width == 0 || max_rows == 0 { return Vec::new(); }
    let mut rows = vec![String::new()];
    let mut clipped = false;
    for word in value.split_whitespace() {
        let mut word = word.to_owned();
        if UnicodeWidthStr::width(word.as_str()) > width {
            word = truncate_display_width(&word, width);
        }
        let last = rows.last_mut().expect("at least one row");
        let separator = if last.is_empty() { "" } else { " " };
        if UnicodeWidthStr::width(last.as_str()) + separator.len() + UnicodeWidthStr::width(word.as_str()) <= width {
            last.push_str(separator);
            last.push_str(&word);
        } else if rows.len() < max_rows {
            rows.push(word);
        } else {
            clipped = true;
            break;
        }
    }
    if clipped {
        let last = rows.last_mut().expect("at least one row");
        let budget = width.saturating_sub(2);
        let mut clipped_row = String::new();
        for grapheme in UnicodeSegmentation::graphemes(last.as_str(), true) {
            if UnicodeWidthStr::width(clipped_row.as_str()) + UnicodeWidthStr::width(grapheme) > budget { break; }
            clipped_row.push_str(grapheme);
        }
        *last = clipped_row;
        last.push_str(if width == 1 { "…" } else { " …" });
    }
    rows
}

fn compose_picker_rows(
    frame: &mut Frame<'_>, rect: Rect, entries: &[crate::app::ComposeFileEntry],
    selected: Option<usize>, show_numbers: bool, attachments: &[std::path::PathBuf],
    visual: Option<(usize, usize)>, theme: &Theme,
) {
    if rect.width == 0 || rect.height == 0 { return; }
    if entries.is_empty() {
        frame.render_widget(Paragraph::new(" Empty directory ").style(theme.muted()), rect);
        return;
    }
    let selected_index = selected.unwrap_or(0).min(entries.len().saturating_sub(1));
    // The selection stays near the middle, like the lazygitrs/Yazi file picker.
    let start = selected_index.saturating_sub(usize::from(rect.height) / 2)
        .min(entries.len().saturating_sub(usize::from(rect.height)));
    let end = start.saturating_add(usize::from(rect.height)).min(entries.len());
    let digits = entries.len().to_string().len();
    let mut items = Vec::new();
    for (index, entry) in entries.iter().enumerate().take(end).skip(start) {
        let is_selected = selected == Some(index);
        let number = if show_numbers {
            let value = if is_selected { index + 1 } else { index.abs_diff(selected_index) };
            format!("{value:>digits$} ")
        } else { String::new() };
        let visually_selected = visual.is_some_and(|(a, b)| (a.min(b)..=a.max(b)).contains(&index));
        let marker = if entry.is_directory { "󰉋 " }
            else if attachments.contains(&entry.path) { "✓ " }
            else { "  " };
        let style = if entry.is_directory { theme.accent() }
            else if attachments.contains(&entry.path) { theme.unread() }
            else { theme.text() };
        let left_width = UnicodeWidthStr::width(number.as_str()) + UnicodeWidthStr::width(marker) + 1;
        let available = usize::from(rect.width).saturating_sub(left_width);
        let name = truncate_display_width(&entry.name, available);
        let line = Line::from(vec![
            Span::styled(if visually_selected { "▍" } else { " " },
                if visually_selected { Style::default().fg(theme.accent) } else { theme.muted() }),
            Span::styled(number, if is_selected { theme.accent() } else { theme.muted() }),
            Span::styled(marker.to_owned(), style),
            Span::styled(name, style),
        ]);
        let item = if is_selected { ListItem::new(line).style(theme.current_line()) } else { ListItem::new(line) };
        items.push(item);
    }
    frame.render_widget(List::new(items), rect);
}

fn draw_compose_file_picker(frame: &mut Frame<'_>, app: &mut App) {
    let area = centered_rect(frame.area(), frame.area().width.saturating_sub(4).min(110), 82);
    let inner_width = area.width.saturating_sub(2);
    let left_width = (inner_width * 24 / 100).max(12).min(inner_width);
    let middle_width = (inner_width * 34 / 100).max(18).min(inner_width.saturating_sub(left_width));
    let preview_width = inner_width.saturating_sub(left_width + middle_width).saturating_sub(2);
    let preview_height = area.height.saturating_sub(3);
    app.ensure_compose_file_preview(preview_width, preview_height);
    let Some(composer) = app.composer.as_ref() else { return; };
    let Some(picker) = composer.file_picker.as_ref() else { return; };
    let theme = app.theme();
    frame.render_widget(Clear, area);
    let block = Block::default().title(format!(" Attach files · {} selected ", composer.attachment_count()))
        .borders(Borders::ALL).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 5 || inner.width < 12 { return; }
    frame.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(" 󰉋 ", theme.accent()),
        Span::styled(picker.directory.display().to_string(), theme.text().add_modifier(Modifier::BOLD)),
    ])), Rect::new(inner.x, inner.y, inner.width, 1));
    let search_height = u16::from(picker.search_query.is_some());
    let content = Rect::new(inner.x, inner.y + 1, inner.width,
        inner.height.saturating_sub(1 + search_height));
    let left_width = (content.width * 24 / 100).max(12).min(content.width);
    let middle_width = (content.width * 34 / 100).max(18).min(content.width.saturating_sub(left_width));
    let right_width = content.width.saturating_sub(left_width + middle_width);
    let parent_area = Rect::new(content.x, content.y, left_width, content.height);
    let files_area = Rect::new(content.x + left_width, content.y, middle_width, content.height);
    let preview_area = Rect::new(files_area.right(), content.y, right_width, content.height);
    if parent_area.width > 0 {
        frame.render_widget(Block::default().borders(Borders::RIGHT).border_style(theme.border()), parent_area);
        compose_picker_rows(frame, Rect::new(parent_area.x, parent_area.y, parent_area.width.saturating_sub(1), parent_area.height),
            &picker.parent_entries, Some(picker.parent_selected), false, &[], None, theme);
    }
    if files_area.width > 0 {
        frame.render_widget(Block::default().borders(Borders::RIGHT).border_style(theme.border()), files_area);
        compose_picker_rows(frame, Rect::new(files_area.x, files_area.y, files_area.width.saturating_sub(1), files_area.height),
            &picker.entries, Some(picker.selected), true, &composer.attachment_paths,
            picker.visual_anchor.map(|anchor| (anchor, picker.selected)), theme);
    }
    if preview_area.width > 0 {
        let preview = Rect::new(preview_area.x + 1, preview_area.y,
            preview_area.width.saturating_sub(2), preview_area.height);
        let lines: Vec<Line<'static>> = match &picker.preview {
            crate::app::ComposeFilePreview::Empty => vec![Line::styled("Empty directory", theme.muted())],
            crate::app::ComposeFilePreview::Loading(message) => vec![Line::styled(*message, theme.muted())],
            crate::app::ComposeFilePreview::Directory(entries) => entries.iter().map(|entry| {
                Line::styled(format!("{}{}", if entry.is_directory { "󰉋 " } else { "  " }, entry.name),
                    if entry.is_directory { theme.accent() } else { theme.text() })
            }).collect(),
            crate::app::ComposeFilePreview::Text { lines, truncated } => {
                let mut result: Vec<_> = lines.iter().map(|line| Line::styled(line.clone(), theme.text())).collect();
                if *truncated { result.push(Line::styled("… truncated …", theme.muted())); }
                result
            }
            crate::app::ComposeFilePreview::Patch(patch) => {
                use crate::patch_preview::PatchTone;
                let mut lines: Vec<Line<'static>> = patch.lines.iter().map(|fragments| {
                    let spans = fragments.iter().map(|fragment| {
                        let style = match fragment.tone {
                            PatchTone::Default => theme.text(),
                            PatchTone::Meta => theme.accent().add_modifier(Modifier::BOLD),
                            PatchTone::Hunk => theme.accent().add_modifier(Modifier::BOLD),
                            PatchTone::Add => theme.unread(),
                            PatchTone::Remove => theme.error(),
                            PatchTone::Keyword => Style::default().fg(theme.visual),
                            PatchTone::String => Style::default().fg(theme.warning),
                            PatchTone::Comment => theme.muted(),
                            PatchTone::Number => Style::default().fg(theme.warning),
                            PatchTone::Function => theme.accent(),
                            PatchTone::Type => theme.accent(),
                        };
                        Span::styled(fragment.text.clone(), style)
                    }).collect::<Vec<_>>();
                    Line::from(spans)
                }).collect();
                if patch.truncated { lines.push(Line::styled("… truncated …", theme.muted())); }
                lines
            }
            crate::app::ComposeFilePreview::Image(_) => {
                if app.compose_file_preview_image().is_some() { vec![] }
                else { vec![Line::styled("Unable to display image in this terminal", theme.muted())] }
            },
            crate::app::ComposeFilePreview::Binary(bytes) => vec![
                Line::styled("Binary file", theme.muted()),
                Line::styled(format!("{bytes} bytes"), theme.text()),
            ],
            crate::app::ComposeFilePreview::Error(error) => vec![Line::styled(error.clone(), theme.error())],
        };
        frame.render_widget(Paragraph::new(lines), preview);
        if let Some(protocol) = app.compose_file_preview_image() {
            let size = protocol.size();
            if size.width <= preview.width && size.height <= preview.height {
                let image_area = Rect::new(
                    preview.x + (preview.width - size.width) / 2,
                    preview.y + (preview.height - size.height) / 2,
                    size.width, size.height,
                );
                frame.render_widget(TerminalImage::new(protocol), image_area);
            }
        }
    }
    if let Some(query) = &picker.search_query {
        let matches = picker.search_matches_for_display();
        let position = matches.iter().position(|index| *index == picker.selected);
        let counter = position.map_or_else(|| format!("0/{}", matches.len()),
            |index| format!("{}/{}", index + 1, matches.len()));
        let prompt = Line::from(vec![
            Span::styled(" / ", theme.accent()),
            Span::styled(query.clone(), theme.text()),
            Span::styled(format!("  {counter}"), theme.muted()),
        ]);
        frame.render_widget(Paragraph::new(prompt), Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
    }
    if picker.help_visible { draw_compose_file_picker_help(frame, app, picker); }
}

fn draw_compose_file_picker_help(frame: &mut Frame<'_>, app: &App, picker: &crate::app::ComposeFilePicker) {
    let theme = app.theme();
    let area = centered_rect(frame.area(), 83, 72);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" File picker keybindings ")
        .borders(Borders::ALL).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    draw_search_header(frame, chunks[0], &picker.help_query, app);
    let matches = picker.help_matches();
    let start = picker.help_scroll.min(matches.len().saturating_sub(usize::from(chunks[1].height)));
    let shown: Vec<_> = matches.into_iter().skip(start).take(usize::from(chunks[1].height)).collect();
    let rows = shown.iter().map(|&index| {
        let entry = PICKER_HELP_ENTRIES[index];
        Row::new(vec![Cell::from(entry.category), Cell::from(entry.key), Cell::from(entry.description)])
    });
    let table = Table::new(rows, [Constraint::Length(11), Constraint::Length(23), Constraint::Min(18)])
        .style(theme.text()).column_spacing(1).row_highlight_style(theme.selected());
    let mut state = TableState::default().with_selected(shown.iter().position(|&index| index == picker.help_selected));
    frame.render_stateful_widget(table, chunks[1], &mut state);
    // All picker bindings live in this searchable popup, not a footer.
}

fn draw_compose_attachment_manager(frame: &mut Frame<'_>, app: &App) {
    let Some(composer) = app.composer.as_ref() else { return; };
    let theme = app.theme();
    let area = centered_rect(frame.area(), 84, 66);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" Attached files · {} ", composer.attachment_count()))
        .borders(Borders::ALL).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 { return; }
    let mut files = composer.attachment_paths.clone();
    files.extend(composer.attachments.split(';').map(str::trim)
        .filter(|part| !part.is_empty()).map(std::path::PathBuf::from));
    if files.is_empty() {
        frame.render_widget(Paragraph::new(" No files attached").style(theme.muted()), inner);
        return;
    }
    let selected = composer.attachment_manager_selected.min(files.len() - 1);
    let start = selected.saturating_sub(usize::from(inner.height) / 2)
        .min(files.len().saturating_sub(usize::from(inner.height)));
    let rows: Vec<ListItem<'_>> = files.iter().enumerate().skip(start)
        .take(usize::from(inner.height)).map(|(index, file)| {
            let name = file.file_name().unwrap_or(file.as_os_str()).to_string_lossy();
            let parent = file.parent().map_or_else(String::new, |path| path.display().to_string());
            let name_width = usize::from(inner.width).saturating_sub(5);
            let line = Line::from(vec![
                Span::styled(if index == selected { "▸ " } else { "  " }, theme.accent()),
                Span::styled(truncate_display_width(&name, name_width.min(32)), theme.text()),
                Span::styled(format!("  {}", truncate_display_width(&parent, name_width.saturating_sub(32))), theme.muted()),
            ]);
            ListItem::new(line).style(if index == selected { theme.current_line() } else { theme.text() })
        }).collect();
    frame.render_widget(List::new(rows), inner);
}

fn draw_compose_advanced_options(frame: &mut Frame<'_>, app: &App) {
    let Some(composer) = app.composer.as_ref() else { return; };
    let theme = app.theme();
    let area = centered_rect(frame.area(), 82, 32);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Advanced compose options ")
        .borders(Borders::ALL).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2), Constraint::Length(1), Constraint::Min(1),
    ]).split(inner);
    frame.render_widget(Paragraph::new(vec![
        Line::styled("Custom email headers (optional)", theme.accent()),
        Line::styled("Use Name: value; Name: value. Standard fields are already handled.", theme.muted()),
    ]), chunks[0]);
    let mut spans = vec![Span::styled("  Headers: ", theme.accent())];
    spans.extend(editor_spans(&composer.headers, composer, true, theme));
    frame.render_widget(Paragraph::new(Line::from(spans)), chunks[1]);
    frame.render_widget(Paragraph::new(" Enter / Esc / Ctrl+O: return to compose")
        .style(theme.muted()), chunks[2]);
}

fn compose_mode_colors(mode: ComposeMode, theme: &Theme) -> (Color, Color) {
    match mode {
        ComposeMode::Normal => (Color::Blue, Color::Black),
        ComposeMode::Insert => (Color::LightGreen, Color::Black),
        ComposeMode::Visual => (theme.visual, Color::Black),
    }
}

fn draw_compose_help(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let Some(composer) = app.composer.as_ref() else { return; };
    let area = centered_rect(frame.area(), 85, 73);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Compose keybindings ")
        .borders(Borders::ALL).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2), Constraint::Min(1), Constraint::Length(1),
    ]).split(inner);
    draw_search_header(frame, chunks[0], &composer.help_query, app);
    let matches = composer.compose_help_matches();
    let max_start = matches.len().saturating_sub(usize::from(chunks[1].height));
    let start = composer.help_scroll.min(max_start);
    let shown: Vec<usize> = matches.into_iter().skip(start)
        .take(usize::from(chunks[1].height)).collect();
    let rows = shown.iter().map(|&index| {
        let entry = COMPOSE_HELP_ENTRIES[index];
        Row::new(vec![Cell::from(entry.category), Cell::from(entry.key), Cell::from(entry.description)])
    });
    let table = Table::new(rows, [
        Constraint::Length(10), Constraint::Length(22), Constraint::Min(18),
    ]).style(theme.text()).column_spacing(1).row_highlight_style(theme.selected());
    let selected = shown.iter().position(|&index| index == composer.help_selected);
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, chunks[1], &mut state);
    frame.render_widget(Paragraph::new(" Type to filter   Backspace to edit   Esc to close ")
        .style(theme.muted()), chunks[2]);
}

fn draw_composer(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let area = centered_rect(frame.area(), 96, 82);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(if composer.sending { " Compose • sending… " } else { " Compose " })
        .borders(Borders::ALL)
        .border_style(theme.border());
    let inside = block.inner(area);
    frame.render_widget(block, area);

    let selected_names = compose_attachment_names(composer);
    let attachments_label = format!("  Attachments ({}):", composer.attachment_count());
    let summary = if selected_names.is_empty() { " none".to_owned() }
        else { format!(" {}", selected_names.join("  ·  ")) };
    let width = usize::from(inside.width).max(1);
    let attachment_lines = clipped_wrap_text(&format!("{attachments_label}{summary}"), width, 3);
    let attachment_rows = attachment_lines.len().max(1) as u16;
    let fields = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), Constraint::Length(1), Constraint::Length(1),
            Constraint::Length(1), Constraint::Length(attachment_rows),
            Constraint::Length(1), Constraint::Min(3), Constraint::Length(1),
        ])
        .split(inside);

    draw_compose_field(frame, fields[0], "To", ComposeField::To, composer, app);
    draw_compose_field(frame, fields[1], "Cc", ComposeField::Cc, composer, app);
    draw_compose_field(frame, fields[2], "Bcc", ComposeField::Bcc, composer, app);
    draw_compose_field(frame, fields[3], "Subject", ComposeField::Subject, composer, app);
    let attachments_style = if selected_names.is_empty() { theme.muted() } else { theme.text() };
    let attachment_text = attachment_lines.into_iter().enumerate().map(|(i, line)| {
        if i == 0 && line.starts_with(&attachments_label) {
            Line::from(vec![
                Span::styled(attachments_label.clone(), if selected_names.is_empty() { theme.muted() } else { theme.accent() }),
                Span::styled(line[attachments_label.len()..].to_owned(), attachments_style),
            ])
        } else { Line::styled(line, attachments_style) }
    }).collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(attachment_text), fields[4]);
    let body_active = composer.field == ComposeField::Body;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if body_active { "▸ Body:" } else { "  Body:" },
            if body_active || !composer.body.trim().is_empty() { theme.accent() } else { theme.muted() },
        ))).style(if body_active { theme.current_line() } else { theme.text() }),
        fields[5],
    );

    let mut body = editor_text(&composer.body, composer, body_active, &theme);
    let active_line = composer.body.chars().take(composer.cursor)
        .filter(|&ch| ch == '\n').count();
    let (mode_color, _) = compose_mode_colors(composer.mode, &theme);
    for (i, line) in body.lines.iter_mut().enumerate() {
        let active_number = body_active && i == active_line;
        let number = if active_number { (i + 1).to_string() }
            else { i.abs_diff(active_line).to_string() };
        let style = if active_number {
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD)
        } else { theme.muted() };
        line.spans.insert(0, Span::styled(format!("{number:>3} "), style));
    }
    let viewport_height = usize::from(fields[6].height).max(1);
    let scroll = if body_active { active_line.saturating_sub(viewport_height.saturating_sub(3)) } else { 0 };
    if body_active && active_line >= scroll && active_line - scroll < viewport_height {
        // Paint the entire row first, including trailing whitespace and blank lines.
        let y = fields[6].y.saturating_add((active_line - scroll) as u16);
        frame.render_widget(Paragraph::new(" ").style(theme.current_line()),
            Rect::new(fields[6].x, y, fields[6].width, 1));
    }
    frame.render_widget(
        Paragraph::new(body).style(theme.text())
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
            .wrap(Wrap { trim: false }), fields[6],
    );
    if body_active && composer.mode == ComposeMode::Insert && fields[6].width > 4
        && composer.file_picker.is_none() && !composer.attachment_manager_open
        && !composer.advanced_headers_open && !composer.help_visible
        && !composer.confirm_send && !composer.exit_prompt {
        // A real terminal caret does not insert a cell into the edited text.
        let before = composer.body.chars().take(composer.cursor).collect::<String>();
        let current_line = before.rsplit('\n').next().unwrap_or("");
        let column = UnicodeWidthStr::width(current_line);
        let available = usize::from(fields[6].width.saturating_sub(4)).max(1);
        let wrapped_rows = column / available;
        let visual_row = active_line.saturating_sub(scroll).saturating_add(wrapped_rows);
        if visual_row < usize::from(fields[6].height) {
            let x = fields[6].x.saturating_add(4).saturating_add((column % available) as u16);
            let y = fields[6].y.saturating_add(visual_row as u16);
            if x < fields[6].right() { frame.set_cursor_position((x, y)); }
        }
    }
    // Same compact, filled mode indicator as the lazygitrs commit editor.
    // The compose keymap is searchable with `?`, not repeated in this footer.
    let label = format!(" {} ", composer.mode.label());
    let width = (UnicodeWidthStr::width(label.as_str()) as u16).min(fields[7].width);
    let mode_area = Rect::new(fields[7].right().saturating_sub(width), fields[7].y, width, 1);
    let (bg, fg) = compose_mode_colors(composer.mode, &theme);
    frame.render_widget(Paragraph::new(label)
        .style(Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD)), mode_area);
    let hint_width = fields[7].width.saturating_sub(width + 1);
    let footer = if app.status.is_error { app.status.text.as_str() }
        else { "? keybindings" };
    frame.render_widget(Paragraph::new(footer)
        .style(if app.status.is_error { theme.error() } else { theme.muted() }),
        Rect::new(fields[7].x, fields[7].y, hint_width, 1));

}

fn draw_compose_field(
    frame: &mut Frame<'_>,
    area: Rect,
    name: &str,
    field: ComposeField,
    composer: &crate::app::Composer,
    app: &App,
) {
    let theme = app.theme().clone();
    let active = composer.field == field;
    let marker = if active { "▸ " } else { "  " };
    let value = match field {
        ComposeField::To => &composer.to,
        ComposeField::Cc => &composer.cc,
        ComposeField::Bcc => &composer.bcc,
        ComposeField::Subject => &composer.subject,
        ComposeField::Attachments => &composer.attachments,
        ComposeField::Headers => &composer.headers,
        ComposeField::Body => &composer.body,
    };
    let mut spans = vec![Span::styled(
        format!("{marker}{name}: "),
        if active || !value.trim().is_empty() { theme.accent() } else { theme.muted() },
    )];
    spans.extend(editor_spans(value, composer, active, &theme));
    frame.render_widget(Paragraph::new(Line::from(spans))
        .style(if active { theme.current_line() } else { theme.text() }), area);
    if active && composer.mode == ComposeMode::Insert && area.width > 0 && area.height > 0
        && composer.file_picker.is_none() && !composer.attachment_manager_open
        && !composer.advanced_headers_open && !composer.help_visible
        && !composer.confirm_send && !composer.exit_prompt {
        let prefix = UnicodeWidthStr::width(format!("{marker}{name}: ").as_str());
        let before = value.chars().take(composer.cursor).collect::<String>();
        let column = prefix.saturating_add(UnicodeWidthStr::width(before.as_str()));
        let x = area.x.saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
        if x < area.right() { frame.set_cursor_position((x, area.y)); }
    }
}

fn editor_spans(
    value: &str,
    composer: &crate::app::Composer,
    active: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = composer.cursor.min(chars.len());
    let selection = active.then(|| composer.visual_range()).flatten();
    let mut spans = Vec::with_capacity(chars.len().saturating_add(1));
    let block = Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD);
    for (index, ch) in chars.iter().copied().enumerate() {
        let at_cursor = active && index == cursor;
        let selected = selection.is_some_and(|(start, end)| (start..end).contains(&index));
        spans.push(Span::styled(ch.to_string(),
            if at_cursor && composer.mode != ComposeMode::Insert { block }
            else if selected { theme.selected() }
            else if value.trim().is_empty() { theme.muted() } else { theme.text() }));
    }
    if active && cursor == chars.len() && composer.mode != ComposeMode::Insert {
        spans.push(Span::styled(" ", block));
    }
    spans
}

fn is_quoted_line(text: &str) -> bool {
    text.trim_start().starts_with('>')
}

fn quoted_line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut global = 0usize;
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content_len = content.chars().count();
        if is_quoted_line(content) {
            ranges.push((global, global.saturating_add(content_len)));
        }
        global = global.saturating_add(line.chars().count());
    }
    if !text.is_empty() && !text.ends_with('\n') && ranges.is_empty() && is_quoted_line(text) {
        ranges.push((0, text.chars().count()));
    }
    ranges
}

fn editor_text(
    value: &str,
    composer: &crate::app::Composer,
    active: bool,
    theme: &Theme,
) -> Text<'static> {
    let selection = active.then(|| composer.visual_range()).flatten();
    let cursor = composer.cursor.min(value.chars().count());
    let quote_ranges = quoted_line_ranges(value);
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut global = 0usize;
    let block = Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD);
    for ch in value.chars() {
        let at_cursor = active && global == cursor;
        if ch == '\n' {
            if at_cursor && composer.mode != ComposeMode::Insert {
                spans.push(Span::styled(" ", block));
            }
            lines.push(Line::from(std::mem::take(&mut spans)));
            global += 1;
            continue;
        }
        let selected = selection.is_some_and(|(start, end)| (start..end).contains(&global));
        let quoted = quote_ranges.iter().any(|&(start, end)| global >= start && global < end);
        spans.push(Span::styled(ch.to_string(),
            if at_cursor && composer.mode != ComposeMode::Insert { block }
            else if selected { theme.selected() }
            else if quoted { Style::default().fg(theme.muted) }
            else { theme.text() }));
        global += 1;
    }
    if active && global == cursor && composer.mode != ComposeMode::Insert {
        spans.push(Span::styled(" ", block));
    }
    lines.push(Line::from(spans));
    Text::from(lines)
}

fn centered_rect(area: Rect, max_width: u16, percent_height: u16) -> Rect {
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

fn mailbox_row_text(name: &str, count: &str, width: usize) -> String {
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

fn truncate_display_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width == 1 {
        return "…".to_owned();
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
    out
}

fn truncate_chars(text: &str, width: usize) -> String {
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

fn subject_for_list(subject: &str) -> String {
    // Prefer monochrome text presentation ONLY where the character has a known
    // text-style glyph. Do not rewrite emoji-only characters such as 👋, flags,
    // skin-tone variants or joined sequences: the old blanket FE0E insertion
    // made the terminal paint half a wide glyph over the next cell. This keeps
    // full-width subject cells and one terminal row per message.
    let mut output = String::with_capacity(subject.len());
    for cluster in UnicodeSegmentation::graphemes(subject, true) {
        let mut chars = cluster.chars();
        let base = chars.next();
        if let Some(base) = base {
            let simple = chars.all(|ch| ch == '\u{fe0e}' || ch == '\u{fe0f}');
            if simple && supports_safe_text_presentation(base) {
                output.push(base);
                output.push('\u{fe0e}');
                continue;
            }
        }
        output.push_str(cluster);
    }
    output
}

/// Unicode characters with established text/emoji variants whose *text*
/// presentation can fit a normal terminal cell. Emoji-only pictographs stay
/// untouched because not all terminals/font stacks provide a text glyph.
fn supports_safe_text_presentation(ch: char) -> bool {
    matches!(ch, '☀' | '☁' | '☂' | '☃' | '☎' | '☘' | '☺' | '☹'
        | '✂' | '✈' | '✉' | '✌' | '❤' | '❄' | '✔' | '✖')
}

fn relative_number(index: usize, current: usize) -> String {
    if index == current {
        (index + 1).to_string()
    } else {
        index.abs_diff(current).to_string()
    }
}

fn relative_number_width(len: usize) -> u16 {
    let digits = len.max(1).ilog10() + 1;
    u16::try_from(digits + 1).unwrap_or(6).max(3)
}

#[cfg(test)]
mod tests {
    use super::{
        mailbox_row_text, pad_reader_line, reader_word_at, relative_number, rich_html_style,
        subject_for_list, truncate_display_width, word_match_ranges,
    };
    use crate::model::HtmlSpanStyle;
    use crate::theme::load_themes;
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn message_attachment_subtitle_has_no_extra_indentation() {
        let names = vec!["config.example.toml".to_owned(), "LICENSE".to_owned()];
        assert_eq!(super::attachment_subtitle(&names), "config.example.toml · LICENSE");
        assert_eq!(super::attachment_subtitle(&[]), "Attachment");
    }

    #[test]
    fn insert_cursor_does_not_insert_a_character_into_editor_spans() {
        use crate::app::{Composer, ComposeMode};
        let mut composer = Composer::new();
        composer.mode = ComposeMode::Insert;
        composer.cursor = 1;
        let theme = load_themes().into_iter().next().unwrap();
        let rendered = super::editor_spans("abc", &composer, true, &theme);
        assert_eq!(rendered.iter().map(|span| span.content.as_ref()).collect::<String>(), "abc");
        let rendered = super::editor_text("abc", &composer, true, &theme);
        assert_eq!(rendered.lines[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>(), "abc");
    }

    #[test]
    fn mailed_diff_add_remove_and_hunk_have_distinct_styles() {
        use super::diff_line_style;
        let theme = load_themes().into_iter().next().unwrap();
        let add = diff_line_style("+new", &theme).unwrap();
        let quoted_add = diff_line_style("> +new", &theme).unwrap();
        let remove = diff_line_style("-old", &theme).unwrap();
        let hunk = diff_line_style("@@ -1,2 +1,3 @@", &theme).unwrap();
        assert_eq!(add.fg, quoted_add.fg);
        assert_ne!(add.fg, remove.fg);
        assert_ne!(hunk.fg, add.fg);
        assert!(diff_line_style("normal prose", &theme).is_none());
    }

    #[test]
    fn relative_numbers_match_vim_style() {
        assert_eq!(relative_number(7, 7), "8");
        assert_eq!(relative_number(2, 7), "5");
        assert_eq!(relative_number(11, 7), "4");
    }

    #[test]
    fn mailbox_rows_keep_counts_at_the_right_edge() {
        assert_eq!(mailbox_row_text("[Gmail]/Trash", "5", 20), "[Gmail]/Trash      5");
        assert_eq!(mailbox_row_text("INBOX", "", 10), "INBOX     ");
    }

    #[test]
    fn sender_foreground_does_not_override_terminal_theme() {
        let theme = load_themes().into_iter().next().expect("built-in theme");
        let style = HtmlSpanStyle {
            foreground: Some((255, 255, 255)),
            ..HtmlSpanStyle::default()
        };
        let rendered = rich_html_style(&style, "body text", &theme);
        assert_eq!(rendered, theme.text());
    }

    #[test]
    fn browse_subject_requests_monochrome_only_for_text_capable_symbols() {
        assert_eq!(subject_for_list("❤️ Mail ☀️"), "❤\u{fe0e} Mail ☀\u{fe0e}");
        // Preserve emoji-only/wide glyphs and joined emoji. FE0E fallback
        // caused half-glyph clipping with the user's Apple Color Emoji font.
        assert_eq!(subject_for_list("👋 Welcome to KLIPY!"), "👋 Welcome to KLIPY!");
        assert_eq!(subject_for_list("👨\u{200d}💻 Status"), "👨\u{200d}💻 Status");
    }

    #[test]
    fn emoji_subject_keeps_unclipped_grapheme() {
        let subject = subject_for_list("👋 Welcome to KLIPY!");
        let display = truncate_display_width(&subject, 25);
        assert_eq!(display, subject);
        assert!(UnicodeWidthStr::width(display.as_str()) <= 25);
    }

    #[test]
    fn attachment_summary_wraps_and_clips_without_splitting_emoji() {
        let rows = super::clipped_wrap_text(
            "Attachments (4): a.txt · image👨\u{200d}💻.png · very-long-filename.pdf · z.zip", 22, 2,
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| UnicodeWidthStr::width(row.as_str()) <= 22));
        assert!(rows.last().unwrap().ends_with('…'));
        assert!(!rows.iter().any(|row| row.ends_with('\u{200d}')));
    }

    #[test]
    fn compose_mode_badges_use_distinct_filled_colors() {
        use crate::app::ComposeMode;
        let theme = crate::theme::load_themes().into_iter().find(|t| t.id == "tokyo-night").unwrap();
        let normal = super::compose_mode_colors(ComposeMode::Normal, &theme);
        let insert = super::compose_mode_colors(ComposeMode::Insert, &theme);
        let visual = super::compose_mode_colors(ComposeMode::Visual, &theme);
        assert_eq!(visual.0, theme.visual);
        assert_eq!(normal, (Color::Blue, Color::Black));
        assert_eq!(insert, (Color::LightGreen, Color::Black));
        assert_ne!(visual.0, normal.0);
        assert_ne!(visual.0, insert.0);
        assert_eq!(visual.1, Color::Black);
    }

    #[test]
    fn browse_subject_truncation_never_splits_emoji_graphemes() {
        let subject = subject_for_list("Status 👨\u{200d}💻 complete");
        let truncated = truncate_display_width(&subject, 10);
        assert!(!truncated.ends_with('\u{200d}'));
        assert!(!truncated.ends_with('\u{fe0e}'));
        assert!(truncated.ends_with('…'));
        assert!(UnicodeWidthStr::width(truncated.as_str()) <= 10);
    }

    #[test]
    fn reader_illuminate_finds_word_under_cursor() {
        assert_eq!(
            reader_word_at("alpha beta\nnext", 0, 7),
            Some("beta".to_owned())
        );
        assert_eq!(reader_word_at("alpha beta", 0, 5), None);
    }

    #[test]
    fn reader_illuminate_matches_whole_words_only() {
        assert_eq!(
            word_match_ranges("word wordy word word_word", "word"),
            vec![(0, 4), (11, 15)]
        );
    }

    #[test]
    fn reader_illuminate_uses_colorscheme_style_reference_highlight() {
        let theme = load_themes().into_iter().next().expect("theme");
        let fragments = super::reader_fragments(
            "word",
            theme.text(),
            0,
            4,
            1,
            0,
            0,
            None,
            true,
            &theme,
            &[(0, 4)],
        );
        assert_eq!(fragments.len(), 1);
        assert!(!fragments[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
        assert_eq!(fragments[0].style.bg, Some(super::reader_illuminate_bg(&theme)));
        assert_eq!(fragments[0].style.fg, theme.text().fg);
    }

    #[test]
    fn reader_illuminate_matches_tokyonight_reference_tone() {
        let mut theme = load_themes().into_iter().next().expect("theme");
        theme.popup_bg = Color::Rgb(36, 40, 59);
        theme.border = Color::Rgb(65, 72, 104);
        assert_eq!(super::reader_illuminate_bg(&theme), Color::Rgb(60, 66, 96));
    }

    #[test]
    fn reader_cursor_stays_white_over_illuminated_word() {
        let theme = load_themes().into_iter().next().expect("theme");
        let fragments = super::reader_fragments(
            "word",
            theme.text(),
            0,
            4,
            0,
            0,
            0,
            None,
            true,
            &theme,
            &[(0, 4)],
        );
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].style.bg, Some(Color::White));
        assert_eq!(fragments[0].style.fg, Some(Color::Black));
        assert_eq!(fragments[1].style.bg, Some(super::reader_illuminate_bg(&theme)));
    }

    #[test]
    fn reader_cursor_remains_visible_on_blank_lines() {
        let theme = load_themes().into_iter().next().expect("theme");
        let mut spans = Vec::new();
        pad_reader_line(&mut spans, 0, 8, 2, 2, 0, None, true, &theme);

        assert!(!spans.is_empty());
        assert_eq!(spans[0].content.as_ref(), " ");
        assert_eq!(spans[0].style.bg, Some(Color::White));
        assert_eq!(spans[0].style.fg, Some(Color::Black));
    }

}

#[cfg(test)]
mod attachment_preview_tests {
    use super::*;
    use crate::model::OutgoingAttachment;

    #[test]
    fn preview_text_is_bounded_and_does_not_display_binary() {
        let file = OutgoingAttachment {
            filename: "notes.txt".into(), content_type: "text/plain".into(),
            data: vec![b'a'; 64 * 1024],
        };
        let preview = attachment_text_preview(&file).unwrap();
        assert!(preview.len() < file.data.len());
        assert!(preview.contains("truncated"));
        let binary = OutgoingAttachment {
            filename: "file.bin".into(), content_type: "application/octet-stream".into(),
            data: vec![0, 1, 2],
        };
        assert!(attachment_text_preview(&binary).is_none());
    }
}
