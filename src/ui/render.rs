use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui_image::Image as TerminalImage;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{
    App, ComposeField, ComposeMode, HELP_ENTRIES, ReaderSelection,
};
use crate::model::{HtmlLine, HtmlSpanStyle, MessageFlags};

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
    if app.theme_picker_visible {
        draw_theme_picker(frame, app);
    }
    if app.confirmation.is_some() {
        draw_confirmation(frame, app);
    }
    if app.composer.is_some() {
        draw_composer(frame, app);
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
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.text()), area);
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
            let mailbox_percent = app.mailbox_width_percent.clamp(20, 50);
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(mailbox_percent),
                    Constraint::Percentage(100_u16.saturating_sub(mailbox_percent)),
                ])
                .split(area);
            draw_mailboxes(frame, app, columns[0]);
            draw_messages(frame, app, columns[1]);
        }
        (false, false) => draw_messages(frame, app, area),
        (true, true) => {
            let mailbox_percent = app.mailbox_width_percent.clamp(18, 32);
            let remaining = 100_u16.saturating_sub(mailbox_percent);
            let list_percent = remaining.saturating_mul(43) / 100;
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(mailbox_percent),
                    Constraint::Percentage(list_percent),
                    Constraint::Percentage(remaining.saturating_sub(list_percent)),
                ])
                .split(area);
            draw_mailboxes(frame, app, columns[0]);
            draw_messages(frame, app, columns[1]);
            draw_message(frame, app, columns[2]);
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
        .border_style(theme.border());
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
                theme.selected()
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
    let start = offset.min(visible.len().saturating_sub(1));
    let end = start.saturating_add(height).min(visible.len());
    let shown = &visible[start..end];
    let current_position = current_position.unwrap_or(0);
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
    // Keep one terminal cell clear at the right edge of the subject column.
    // Some monochrome emoji glyphs are double-cell/wide even in text
    // presentation; allowing the final grapheme to touch the clipping edge can
    // leave only half the glyph visible in terminals/font fallback stacks.
    let subject_width = column_areas
        .get(3)
        .map_or(0, |area| usize::from(area.width.saturating_sub(1)));
    let rows = shown.iter().enumerate().map(|(row_position, index)| {
        let account = app.account().expect("account exists");
        let message = &account.messages[*index];
        let absolute_position = start + row_position;
        let number = relative_number(absolute_position, current_position);
        let visual_selected = app.message_is_visual_selected(*index);
        let current = absolute_position == current_position;
        let mark = if visual_selected {
            "✓"
        } else if message.flags.contains(MessageFlags::SEEN) {
            " "
        } else {
            "●"
        };
        let sender = if message.from_name.is_empty() {
            &message.from_address
        } else {
            &message.from_name
        };
        let style = if current || visual_selected {
            theme.selected()
        } else if message.unread() {
            theme.unread()
        } else {
            theme.text()
        };
        Row::new(vec![
            Cell::from(number),
            Cell::from(mark),
            Cell::from(sender.clone()),
            Cell::from(format!(
                "{} ",
                truncate_display_width(&subject_for_list(&message.subject), subject_width)
            )),
            Cell::from(message.date_label.clone()),
        ])
        .style(style)
    });

    let table = Table::new(rows, message_columns)
    .style(theme.text())
    .column_spacing(1);
    frame.render_widget(table, list_area);

}

fn draw_message(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let theme = app.theme().clone();
    let content_area = if !app.message_open {
        let block = Block::default()
            .title(" Preview ")
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

    let selected_link = app.account().and_then(|account| {
        let body = account.selected_body()?;
        let links = body.html_links();
        links
            .get(app.reader_link_index.min(links.len().saturating_sub(1)))
            .cloned()
    });
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
            Line::from(vec![
                Span::styled("Subject: ", theme.accent()),
                Span::raw(message.subject.clone()),
            ]),
        ];
        if let Some(message_id) = &message.message_id {
            lines.push(Line::from(vec![
                Span::styled("Message-ID: ", theme.accent()),
                Span::raw(format!("<{message_id}>")),
            ]));
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
                            selected_link.as_deref(),
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
            if body.has_html_alternative() {
                let mode = if showing_html {
                    format!(
                        "HTML via {}",
                        body.html_renderer.as_deref().unwrap_or("rich renderer")
                    )
                } else {
                    "plain text".to_owned()
                };
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!(
                        "[{mode}; T rich/plain; Tab/Shift-Tab links; o opens link; O opens original in browser; I images]"
                    ),
                    theme.muted(),
                ));
            }
            if body.attachment_count > 0 {
                lines.push(Line::styled(
                    format!("[{} attachment(s)]", body.attachment_count),
                    theme.muted(),
                ));
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
    let base = theme.text();
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
    selected_link: Option<&str>,
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
    let mut column = 0usize;
    for span in &line.spans {
        let mut style = rich_html_style(&span.style, &span.text, theme);
        if span
            .style
            .link
            .as_deref()
            .is_some_and(|url| Some(url) == selected_link)
        {
            style = style
                .fg(theme.accent)
                .add_modifier(Modifier::UNDERLINED | Modifier::BOLD);
        }
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
            // Match vim-illuminate's default IlluminatedWord* groups: keep the
            // existing syntax/rich-text colour and mark references with an
            // underline instead of painting them with an accent-derived fill.
            fragment_style = fragment_style.add_modifier(Modifier::UNDERLINED);
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
            spans.push(Span::styled(format!("/{}▏  ", account.search_query), theme.accent()));
        } else if !account.search_query.is_empty() {
            spans.push(Span::styled(format!("filter:/{}  ", account.search_query), theme.accent()));
        }
        if account.connecting {
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

fn draw_composer(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let area = centered_rect(frame.area(), 96, 82);
    frame.render_widget(Clear, area);
    let title = if composer.sending {
        " Compose • sending… ".to_owned()
    } else if composer.confirm_send {
        " Send message? • Enter/y send • Esc/n cancel ".to_owned()
    } else {
        format!(
            " Compose • {} • Tab fields • Enter confirms send from NORMAL ",
            composer.mode.label()
        )
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(match composer.mode {
            ComposeMode::Normal => theme.accent(),
            ComposeMode::Insert => theme.unread(),
            ComposeMode::Visual => theme.error(),
        });
    let inside = block.inner(area);
    frame.render_widget(block, area);

    let fields = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inside);

    draw_compose_field(frame, fields[0], "To", ComposeField::To, composer, app);
    draw_compose_field(frame, fields[1], "Cc", ComposeField::Cc, composer, app);
    draw_compose_field(frame, fields[2], "Subject", ComposeField::Subject, composer, app);
    let body_active = composer.field == ComposeField::Body;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if body_active { "▸ Body:" } else { "  Body:" },
            if body_active { theme.accent() } else { theme.muted() },
        ))),
        fields[3],
    );

    let body = editor_text(&composer.body, composer, body_active, &theme);
    frame.render_widget(
        Paragraph::new(body)
            .style(theme.text())
            .wrap(Wrap { trim: false }),
        fields[4],
    );

    let mode_hint = match composer.mode {
        ComposeMode::Insert => "INSERT  Esc normal • Tab next field",
        ComposeMode::Normal => {
            "NORMAL  i/a/I/A insert • o/O lines • v visual • hjkl/wb move • Enter send"
        }
        ComposeMode::Visual => "VISUAL  hjkl select • d/x delete • c change • Esc normal",
    };
    let footer = if app.status.is_error {
        vec![
            Span::styled(" ERROR ", theme.error()),
            Span::styled(app.status.text.clone(), theme.error()),
        ]
    } else {
        vec![
            Span::styled(format!(" {} ", composer.mode.label()), theme.accent()),
            Span::styled(mode_hint, theme.muted()),
        ]
    };
    frame.render_widget(Paragraph::new(Line::from(footer)), fields[5]);
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
        ComposeField::Subject => &composer.subject,
        ComposeField::Body => &composer.body,
    };
    let mut spans = vec![Span::styled(
        format!("{marker}{name}: "),
        if active { theme.accent() } else { theme.muted() },
    )];
    spans.extend(editor_spans(value, composer, active, &theme));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn editor_spans(
    value: &str,
    composer: &crate::app::Composer,
    active: bool,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = composer.cursor.min(chars.len());
    let selection = active.then(|| composer.visual_range()).flatten();
    let mut spans = Vec::with_capacity(chars.len().saturating_add(1));
    for index in 0..=chars.len() {
        if active && index == cursor {
            spans.push(Span::styled("▏".to_owned(), theme.accent()));
        }
        if let Some(ch) = chars.get(index).copied() {
            let selected = selection.is_some_and(|(start, end)| (start..end).contains(&index));
            spans.push(Span::styled(
                ch.to_string(),
                if selected { theme.selected() } else { theme.text() },
            ));
        }
    }
    spans
}

fn editor_text(
    value: &str,
    composer: &crate::app::Composer,
    active: bool,
    theme: &crate::theme::Theme,
) -> Text<'static> {
    let selection = active.then(|| composer.visual_range()).flatten();
    let cursor = composer.cursor.min(value.chars().count());
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut global = 0usize;

    for ch in value.chars() {
        if active && global == cursor {
            spans.push(Span::styled("▏".to_owned(), theme.accent()));
        }
        if ch == '\n' {
            lines.push(Line::from(std::mem::take(&mut spans)));
            global += 1;
            continue;
        }
        let selected = selection.is_some_and(|(start, end)| (start..end).contains(&global));
        spans.push(Span::styled(
            ch.to_string(),
            if selected { theme.selected() } else { theme.text() },
        ));
        global += 1;
    }
    if active && global == cursor {
        spans.push(Span::styled("▏".to_owned(), theme.accent()));
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
    let mut out = String::with_capacity(subject.len());
    let mut chars = subject.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{fe0f}' {
            out.push('\u{fe0e}');
            continue;
        }

        out.push(ch);
        if !is_emoji_text_candidate(ch) {
            continue;
        }

        match chars.peek().copied() {
            Some('\u{fe0e}') => {
                out.push(chars.next().expect("peeked variation selector"));
            }
            Some('\u{fe0f}') => {
                let _ = chars.next();
                out.push('\u{fe0e}');
            }
            Some('\u{200d}') | Some('\u{1f3fb}'..='\u{1f3ff}') => {}
            _ => out.push('\u{fe0e}'),
        }
    }

    out
}

fn is_emoji_text_candidate(ch: char) -> bool {
    matches!(
        ch,
        '\u{2600}'..='\u{26ff}'
            | '\u{2700}'..='\u{27bf}'
            | '\u{1f300}'..='\u{1f3fa}'
            | '\u{1f400}'..='\u{1faff}'
    )
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
    fn browse_subjects_request_text_emoji_presentation() {
        assert_eq!(subject_for_list("🎉 Sale"), "🎉\u{fe0e} Sale");
        assert!(!subject_for_list("🎉️ Sale").contains('\u{fe0f}'));
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
    fn reader_illuminate_uses_vim_illuminate_default_underline() {
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
        assert!(fragments[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
        assert_eq!(fragments[0].style.bg, None);
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
