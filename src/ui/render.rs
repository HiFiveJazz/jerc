use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};

use crate::app::{App, ComposeField, ComposeMode, HELP_ENTRIES};
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

    if account.mailboxes.is_empty() {
        let text = if account.connecting || account.mailboxes_loading {
            " Loading mailboxes…"
        } else {
            " No mailboxes"
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(theme.muted())
                .block(Block::default().borders(Borders::RIGHT).border_style(theme.border())),
            area,
        );
        return;
    }

    let height = usize::from(area.height.max(1));
    let start = account
        .mailbox_view_offset
        .min(account.mailboxes.len().saturating_sub(1));
    let end = start.saturating_add(height).min(account.mailboxes.len());
    let rows = account.mailboxes[start..end]
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
            Row::new(vec![Cell::from(mailbox.name.clone()), Cell::from(count)]).style(
                if absolute_index == account.mailbox_index {
                    theme.selected()
                } else {
                    theme.text()
                },
            )
        });

    let table = Table::new(rows, [Constraint::Min(4), Constraint::Length(6)])
        .style(theme.text())
        .block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(theme.border()),
        );
    frame.render_widget(table, area);
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
    let mut list_area = search_split.as_ref().map_or(area, |chunks| chunks[1]);
    if let Some(chunks) = &search_split {
        draw_search_header(frame, chunks[0], &search_query, app);
    }

    let (visible, current_position, offset, loading, has_more, current_mailbox) = {
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
            account.has_more,
            account
                .current_mailbox
                .clone()
                .unwrap_or_else(|| "mailbox".to_owned()),
        )
    };

    // Reserve the final line for the pagination hint instead of painting over
    // the last message row. Besides being clearer, this avoids clipping wide
    // glyphs/emoji on the bottom row when older mail is available.
    let hint_area = if has_more && list_area.height > 1 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(list_area);
        list_area = chunks[0];
        Some(chunks[1])
    } else {
        None
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
            Cell::from(message.subject.clone()),
            Cell::from(message.date_label.clone()),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(number_width),
            Constraint::Length(2),
            Constraint::Percentage(28),
            Constraint::Min(12),
            Constraint::Length(6),
        ],
    )
    .style(theme.text())
    .column_spacing(1);
    frame.render_widget(table, list_area);

    if let Some(hint_area) = hint_area {
        frame.render_widget(
            Paragraph::new(" older mail loads automatically near the bottom…")
                .style(theme.muted()),
            hint_area,
        );
    }
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
        app.set_preview_max_scroll(0);
        return;
    }
    let selected_link = app.account().and_then(|account| {
        let body = account.selected_body()?;
        let links = body.html_links();
        links
            .get(app.reader_link_index.min(links.len().saturating_sub(1)))
            .cloned()
    });
    let lines = {
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

        if let Some(body) = account.selected_body() {
            let showing_html = body.has_html_alternative()
                && (account.prefer_html || body.plain_is_empty());
            if showing_html {
                if let Some(html_lines) = &body.html_lines {
                    lines.extend(html_lines.iter().map(|line| {
                        rich_html_line(line, &theme, selected_link.as_deref())
                    }));
                } else {
                    lines.extend(
                        body.display_text(true)
                            .lines()
                            .map(|line| Line::styled(line.to_owned(), theme.text())),
                    );
                }
            } else {
                lines.extend(Text::from(body.display_text(false).to_owned()).lines);
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
                    format!("[{mode}; h toggles alternative; Tab/Shift-Tab selects links; o opens]"),
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
        lines
    };

    let paragraph = Paragraph::new(Text::from(lines))
        .style(theme.text())
        .wrap(Wrap { trim: false });
    let visual_lines = paragraph.line_count(content_area.width.max(1));
    let viewport_height = usize::from(content_area.height.max(1));
    let max_scroll =
        u16::try_from(visual_lines.saturating_sub(viewport_height)).unwrap_or(u16::MAX);
    app.set_preview_max_scroll(max_scroll);
    let scroll = app.account().map_or(0, |account| account.preview_scroll);
    frame.render_widget(paragraph.scroll((scroll, 0)), content_area);
}

fn rich_html_line(
    line: &HtmlLine,
    theme: &crate::theme::Theme,
    selected_link: Option<&str>,
) -> Line<'static> {
    let spans = line
        .spans
        .iter()
        .map(|span| {
            let mut style = rich_html_style(&span.style, &span.text, theme);
            if span
                .style
                .link
                .as_deref()
                .is_some_and(|url| Some(url) == selected_link)
            {
                style = theme.selected().add_modifier(Modifier::UNDERLINED);
            }
            Span::styled(span.text.clone(), style)
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn rich_html_style(
    style: &HtmlSpanStyle,
    text: &str,
    theme: &crate::theme::Theme,
) -> Style {
    // w3m is easy to scan because structure and interactive elements do not all
    // collapse into the same colour. Keep that idea while staying inside
    // Ratatui: table chrome follows the theme border, links/buttons use accent,
    // emphasis is distinct, and images/placeholders stay deliberately subdued.
    let mut rendered = if is_box_drawing_span(text) {
        theme.border()
    } else {
        theme.text()
    };
    if let Some((red, green, blue)) = style.foreground {
        rendered = rendered.fg(Color::Rgb(red, green, blue));
    }
    if let Some((red, green, blue)) = style.background {
        rendered = rendered.bg(Color::Rgb(red, green, blue));
    }
    if style.link.is_some() {
        // Links remain unmistakably interactive. Author CSS can still provide
        // backgrounds, but foreground follows the active terminal theme so a
        // low-contrast marketing colour cannot make the URL disappear.
        rendered = rendered
            .fg(theme.accent)
            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD);
    } else if style.bold && style.foreground.is_none() {
        rendered = rendered.fg(theme.unread);
    }
    if style.image.is_some() {
        rendered = theme.muted().add_modifier(Modifier::ITALIC);
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
        if app.visual_anchor.is_some() {
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
        } else if account.page_inflight.is_some() {
            spans.push(Span::raw("Loading older mail…"));
        } else if account.is_loading_body() {
            spans.push(Span::raw("Loading message…"));
        } else {
            let style = if app.status.is_error { theme.error() } else { theme.text() };
            spans.push(Span::styled(app.status.text.as_str(), style));
        }
        if account.has_more {
            spans.push(Span::styled(
                format!("    {} loaded • more", account.messages.len()),
                theme.muted(),
            ));
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
        ComposeMode::Normal => "NORMAL  i/a/I/A insert • o/O lines • v visual • hjkl/wb move • Enter send",
        ComposeMode::Visual => "VISUAL  hjkl select • d/x delete • c change • Esc normal",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {} ", composer.mode.label()), theme.accent()),
            Span::styled(mode_hint, theme.muted()),
        ])),
        fields[5],
    );
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
    use super::relative_number;

    #[test]
    fn relative_numbers_match_vim_style() {
        assert_eq!(relative_number(7, 7), "8");
        assert_eq!(relative_number(2, 7), "5");
        assert_eq!(relative_number(11, 7), "4");
    }
}
