use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};

use crate::app::{App, ComposeField, Focus, HELP_ENTRIES};
use crate::model::MessageFlags;
use crate::theme::THEMES;

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
    if app.composer.is_some() {
        draw_composer(frame, app);
    }
}

fn draw_tabs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = *app.theme();
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

fn draw_mailboxes(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = *app.theme();
    let account = app.account().expect("account exists");
    let mut rows = Vec::with_capacity(account.mailboxes.len().max(1));
    if account.mailboxes.is_empty() && (account.connecting || account.mailboxes_loading) {
        rows.push(Row::new(vec![Cell::from("Loading mailboxes…")]).style(theme.muted()));
    } else {
        for mailbox in &account.mailboxes {
            let count = if !mailbox.counts_known {
                "…".to_owned()
            } else if mailbox.unread > 0 {
                mailbox.unread.to_string()
            } else {
                String::new()
            };
            rows.push(Row::new(vec![
                Cell::from(mailbox.name.clone()),
                Cell::from(count),
            ]));
        }
    }

    let border_style = if app.focus == Focus::Mailboxes {
        theme.accent()
    } else {
        theme.border()
    };
    let table = Table::new(rows, [Constraint::Min(4), Constraint::Length(6)])
        .style(theme.text())
        .block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(border_style),
        )
        .row_highlight_style(theme.selected());
    let selected = (!account.mailboxes.is_empty()).then_some(account.mailbox_index);
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_messages(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = *app.theme();
    let account = app.account().expect("account exists");

    if account.messages_loading && account.messages.is_empty() {
        frame.render_widget(
            Paragraph::new(" Loading messages…").style(theme.muted()),
            area,
        );
        return;
    }

    let current = account.message_index;
    let rows = account.messages.iter().enumerate().map(|(index, message)| {
        let number = relative_number(index, current);
        let mark = if message.flags.contains(MessageFlags::SEEN) {
            " "
        } else {
            "●"
        };
        let sender = if message.from_name.is_empty() {
            &message.from_address
        } else {
            &message.from_name
        };
        let style = if message.unread() {
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
            Constraint::Length(relative_number_width(account.messages.len())),
            Constraint::Length(2),
            Constraint::Percentage(28),
            Constraint::Min(20),
            Constraint::Length(6),
        ],
    )
    .style(theme.text())
    .column_spacing(1)
    .row_highlight_style(theme.selected());
    let selected = (!account.messages.is_empty()).then_some(account.message_index);
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_message(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let theme = *app.theme();
    if app.account().and_then(|account| account.selected_message()).is_none() {
        frame.render_widget(Paragraph::new("No message selected").style(theme.muted()), area);
        app.set_preview_max_scroll(0);
        return;
    }
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
        lines.push(Line::styled("─".repeat(usize::from(area.width)), theme.border()));
        lines.push(Line::raw(""));

        if let Some(body) = account.selected_body() {
            lines.extend(Text::from(body.display_text(account.prefer_html).to_owned()).lines);
            if body.has_html_alternative() {
                let mode = if account.prefer_html || body.plain_is_empty() {
                    "HTML rendered"
                } else {
                    "plain text"
                };
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!("[{mode}; press h to toggle alternative]"),
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

    // Paragraph::line_count accounts for soft wrapping, so the scroll limit is
    // the *visual* last row rather than the number of logical source lines.
    let paragraph = Paragraph::new(Text::from(lines))
        .style(theme.text())
        .wrap(Wrap { trim: false });
    let visual_lines = paragraph.line_count(area.width.max(1));
    let viewport_height = usize::from(area.height.max(1));
    let max_scroll = u16::try_from(visual_lines.saturating_sub(viewport_height)).unwrap_or(u16::MAX);
    app.set_preview_max_scroll(max_scroll);
    let scroll = app.account().map_or(0, |account| account.preview_scroll);
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = *app.theme();
    let mut spans = Vec::new();
    if let Some(account) = app.account() {
        spans.push(Span::styled(format!("[{}] ", account.name), theme.accent()));
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
    } else {
        spans.push(Span::raw(app.status.text.as_str()));
    }
    if let Some(count) = app.count_prefix {
        spans.push(Span::styled(format!("    {count}"), theme.accent()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.status()), area);
}

fn draw_search_header(frame: &mut Frame<'_>, area: Rect, query: &str, app: &App) {
    let theme = *app.theme();
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
    let theme = *app.theme();
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
    let theme = *app.theme();
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
            let candidate = &THEMES[*index];
            let marker = if *index == app.theme_selected { "●" } else { " " };
            Row::new(vec![Cell::from(marker), Cell::from(candidate.name)])
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

fn draw_composer(frame: &mut Frame<'_>, app: &App) {
    let theme = *app.theme();
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let area = centered_rect(frame.area(), 86, 72);
    frame.render_widget(Clear, area);
    let inner = Block::default()
        .title(if composer.sending {
            " Compose — sending… "
        } else {
            " Compose — Ctrl-s send, Esc cancel "
        })
        .borders(Borders::ALL)
        .border_style(theme.accent());
    let inside = inner.inner(area);
    frame.render_widget(inner, area);

    let fields = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .split(inside);

    draw_compose_field(frame, fields[0], "To", &composer.to, composer.field == ComposeField::To, app);
    draw_compose_field(frame, fields[1], "Cc", &composer.cc, composer.field == ComposeField::Cc, app);
    draw_compose_field(
        frame,
        fields[2],
        "Subject",
        &composer.subject,
        composer.field == ComposeField::Subject,
        app,
    );
    frame.render_widget(Paragraph::new(""), fields[3]);
    let body_style = if composer.field == ComposeField::Body {
        theme.selected()
    } else {
        theme.text()
    };
    frame.render_widget(
        Paragraph::new(composer.body.as_str())
            .style(body_style)
            .wrap(Wrap { trim: false }),
        fields[4],
    );
}

fn draw_compose_field(
    frame: &mut Frame<'_>,
    area: Rect,
    name: &str,
    value: &str,
    active: bool,
    app: &App,
) {
    let theme = *app.theme();
    let style = if active { theme.selected() } else { theme.text() };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{name}: "), theme.accent()),
            Span::raw(value),
        ]))
        .style(style),
        area,
    );
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
