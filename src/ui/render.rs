use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};

use crate::app::{App, Focus};
use crate::model::MessageFlags;

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)])
        .split(frame.area());

    draw_tabs(frame, app, outer[0]);
    draw_main(frame, app, outer[1]);
    draw_status(frame, app, outer[2]);
}

fn draw_tabs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans = Vec::new();
    for (index, account) in app.accounts.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if index == app.account_index {
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(account.name.as_str(), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_main(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(_account) = app.account() else {
        frame.render_widget(Paragraph::new("No accounts configured"), area);
        return;
    };

    let sidebar = app.sidebar_width.min(area.width.saturating_sub(20));
    let rest = area.width.saturating_sub(sidebar);
    let message_width = (u32::from(rest) * u32::from(app.message_width_percent.min(80)) / 100) as u16;
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(sidebar),
            Constraint::Length(message_width.max(20)),
            Constraint::Min(20),
        ])
        .split(area);

    draw_mailboxes(frame, app, panes[0]);
    draw_messages(frame, app, panes[1]);
    draw_preview(frame, app, panes[2]);
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let mut style = Style::default();
    if focused {
        style = style.add_modifier(Modifier::BOLD);
    }
    Block::default().title(title).borders(Borders::ALL).border_style(style)
}

fn draw_mailboxes(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let account = app.account().expect("account exists");
    let rows = account.mailboxes.iter().map(|mailbox| {
        let unread = if mailbox.unread == 0 {
            String::new()
        } else {
            mailbox.unread.to_string()
        };
        Row::new(vec![Cell::from(mailbox.name.clone()), Cell::from(unread)])
    });
    let table = Table::new(rows, [Constraint::Min(4), Constraint::Length(5)])
        .block(pane_block(" Mailboxes ".to_owned(), app.focus == Focus::Mailboxes))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = TableState::default().with_selected(Some(account.mailbox_index));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_messages(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let account = app.account().expect("account exists");
    let rows = account.messages.iter().map(|message| {
        let mark = if message.flags.contains(MessageFlags::SEEN) { " " } else { "●" };
        let from = if message.from_name.is_empty() {
            &message.from_address
        } else {
            &message.from_name
        };
        let style = if message.unread() {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(mark),
            Cell::from(message.date_label.clone()),
            Cell::from(from.to_owned()),
            Cell::from(message.subject.clone()),
        ])
        .style(style)
    });

    let title = account.current_mailbox.as_deref().map_or_else(
        || " Messages ".to_owned(),
        |name| format!(" {name} ({}) ", account.messages.len()),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Percentage(35),
            Constraint::Percentage(65),
        ],
    )
    .block(pane_block(title, app.focus == Focus::Messages))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = TableState::default().with_selected(Some(account.message_index));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_preview(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let account = app.account().expect("account exists");
    let Some(message) = account.selected_message() else {
        frame.render_widget(
            Paragraph::new("No message selected")
                .block(pane_block(" Preview ".to_owned(), app.focus == Focus::Preview)),
            area,
        );
        return;
    };

    let from = if message.from_name.is_empty() {
        message.from_address.clone()
    } else if message.from_address.is_empty() {
        message.from_name.clone()
    } else {
        format!("{} <{}>", message.from_name, message.from_address)
    };
    let mut lines = vec![
        Line::from(vec![Span::styled("From: ", Style::default().add_modifier(Modifier::BOLD)), Span::raw(from)]),
        Line::from(vec![Span::styled("Subject: ", Style::default().add_modifier(Modifier::BOLD)), Span::raw(message.subject.clone())]),
    ];
    if let Some(message_id) = &message.message_id {
        lines.push(Line::from(vec![
            Span::styled("Message-ID: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("<{message_id}>")),
        ]));
    }
    lines.push(Line::raw(""));

    if let Some(body) = account.selected_body() {
        lines.extend(Text::from(body.text.clone()).lines);
        if body.attachment_count > 0 {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("{} attachment(s)", body.attachment_count),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
    } else {
        lines.push(Line::styled(
            "Loading message body…",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    let viewport_height = usize::from(area.height.saturating_sub(2));
    let max_scroll = u16::try_from(lines.len().saturating_sub(viewport_height)).unwrap_or(u16::MAX);
    let scroll = account.preview_scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(pane_block(" Preview ".to_owned(), app.focus == Focus::Preview))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let account = app.account();
    let perf = account
        .and_then(|a| a.last_scan_micros)
        .map_or_else(String::new, |us| format!("  scan {:.2} ms", us as f64 / 1000.0));
    let help = "  j/k move/scroll  g/G top/bottom  Tab pane  Enter open  u unread  r refresh  q quit";
    let mut style = Style::default();
    if app.status.is_error {
        style = style.add_modifier(Modifier::BOLD);
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(app.status.text.as_str(), style),
            Span::raw(perf),
            Span::raw(help),
        ])),
        area,
    );
}
