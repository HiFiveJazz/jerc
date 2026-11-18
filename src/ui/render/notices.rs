//! Mail-browser-style Notices list and full-detail reader.
use super::*;

pub(super) fn draw_notices(frame: &mut Frame<'_>, app: &mut App) {
    let theme = app.theme().clone();
    let rows = app.filtered_notices();
    let area = centered_rect(frame.area(), 116, 88);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" {} Notices ", crate::ui::NOTICE_HAZARD_ICON))
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(Style::default().fg(theme.warning))
        .style(theme.text());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 18 || inner.height < 5 {
        return;
    }
    let content = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    if let Some(ref id) = app.notices.detail {
        draw_notice_reader(
            frame,
            &theme,
            content,
            rows.iter().find(|row| &row.id == id),
            app.notices.detail_scroll,
        );
    } else {
        draw_notice_list(frame, app, &theme, content, &rows);
    }
    if !app.notices.confirming.is_empty() {
        draw_notice_confirmation(frame, app, &theme);
    }
    if app.notices.help {
        let entries = NOTICE_HELP_ENTRIES;
        let matches = ranked_help_matches(entries, &app.notices.help_query);
        draw_help_palette(
            frame,
            &theme,
            " Notices keybindings ",
            entries,
            &app.notices.help_query,
            &matches,
            (app.notices.help_selected, app.notices.help_scroll),
        );
    }
}

/// Mail and Notices both use ratatui's Table for column sizing. Header and
/// body must receive the exact same widths and spacing, including mark cells.
fn notice_columns(width: u16, count: usize) -> (Vec<Constraint>, usize, usize, bool) {
    let account_width = if width >= 72 { 16 } else { 10 };
    let level_width = if width >= 60 { 9 } else { 7 };
    let show_type = width >= 80;
    let mut columns = vec![
        Constraint::Length(relative_number_width(count).saturating_add(1)),
        Constraint::Length(1), // Contacts/mail checkmark column
        Constraint::Length(account_width as u16),
        Constraint::Length(level_width as u16),
    ];
    if show_type {
        columns.push(Constraint::Length(12));
    }
    columns.push(Constraint::Min(1));
    (columns, account_width, level_width, show_type)
}

fn draw_notice_list(
    frame: &mut Frame<'_>,
    app: &mut App,
    theme: &Theme,
    area: Rect,
    rows: &[crate::app::NoticeRow],
) {
    // Search uses the same extra input row as Contacts and mail.
    let (search_row, list_area) = contacts_search_layout(area, app.notices.searching);
    if let Some(search_row) = search_row {
        draw_search_prompt(frame, search_row, &app.notices.query, theme);
    }
    let area = list_area;
    let header_height = 1;
    // The mode badge and keybinding palette are application-wide. Notices only
    // needs a safety explanation and the item count inside its own modal.
    let footer_height = 2;
    let list_height = area.height.saturating_sub(header_height + footer_height);
    let app_modal: &mut crate::app::NoticeBrowser = &mut app.notices;
    if let Some(id) = &app_modal.focus_id
        && let Some(position) = rows.iter().position(|row| &row.id == id)
    {
        app_modal.selected = position;
    }
    app_modal.view_height = usize::from(list_height);
    app_modal.ensure_visible(rows.len());
    let (columns, account_width, _level_width, show_type) = notice_columns(area.width, rows.len());
    let number_width = usize::from(relative_number_width(rows.len()));
    let mut headings = vec![
        Cell::from(format!(" {:>number_width$}", "#")),
        Cell::from(""),
        Cell::from("Account"),
        Cell::from("Level"),
    ];
    if show_type {
        headings.push(Cell::from("Type"));
    }
    headings.push(Cell::from("Summary"));
    frame.render_widget(
        Table::new([Row::new(headings)], columns.clone())
            .column_spacing(1)
            .style(theme.muted()),
        Rect::new(area.x, area.y, area.width, header_height),
    );

    let visible = rows
        .iter()
        .enumerate()
        .skip(app_modal.scroll)
        .take(usize::from(list_height));
    let table_rows = visible.map(|(position, notice)| {
        let active = position == app_modal.selected;
        let visual = app_modal.in_visual_range(position);
        let marked = app_modal.marked.contains(&notice.id);
        let number = relative_number(position, app_modal.selected);
        let level_color = match notice.level {
            "Error" => theme.error,
            _ => theme.warning,
        };
        let mut cells = vec![
            Cell::from(Line::from(vec![
                contact_gutter(theme, visual, marked),
                Span::raw(format!("{number:>number_width$}")),
            ])),
            Cell::from(Line::from(Span::styled(
                contact_list_icon(false, false, visual, marked),
                selection_check_style(theme, app.visual_selection_style, visual, marked, active),
            ))),
            Cell::from(truncate_display_width(&notice.account, account_width)),
            Cell::from(Line::from(Span::styled(
                notice.level,
                Style::default().fg(level_color),
            ))),
        ];
        if show_type {
            cells.push(Cell::from(notice.kind));
        }
        cells.push(Cell::from(notice.summary.replace('\n', " ")));
        Row::new(cells).style(browser_row_style(
            theme,
            app.visual_selection_style,
            visual,
            marked,
            active,
            false,
        ))
    });
    frame.render_widget(
        Table::new(table_rows, columns)
            .column_spacing(1)
            .style(theme.text()),
        Rect::new(
            area.x,
            area.y.saturating_add(header_height),
            area.width,
            list_height,
        ),
    );
    if rows.is_empty() {
        let empty = if app_modal.query.is_empty() {
            " No active notices"
        } else {
            " No matching notices"
        };
        frame.render_widget(
            Paragraph::new(empty).style(theme.muted()),
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        );
    }
    let bottom = area.y + area.height.saturating_sub(footer_height);
    let guidance =
        "Queued work syncs when safe. Manual-review work is never retried automatically.";
    frame.render_widget(
        Paragraph::new(truncate_display_width(guidance, usize::from(area.width)))
            .style(theme.muted()),
        Rect::new(area.x, bottom, area.width, 1),
    );
    let noun = if rows.len() == 1 { "notice" } else { "notices" };
    let counter = if app_modal.query.is_empty() {
        format!("{} {noun}", rows.len())
    } else {
        format!("{} matching {noun}", rows.len())
    };
    frame.render_widget(
        Paragraph::new(counter)
            .alignment(Alignment::Right)
            .style(theme.muted()),
        Rect::new(area.x, bottom + 1, area.width, 1),
    );
}

/// Body labels use the same styled field spans as mail reader headers.
/// Restrict recognized labels so colons inside arbitrary errors/URLs aren't
/// accidentally formatted as structured metadata.
fn notice_detail_line(line: &str, theme: &Theme) -> Line<'static> {
    const FIELDS: &[&str] = &[
        "Operation: ",
        "Source: ",
        "Destination: ",
        "UIDVALIDITY: ",
        "Message IDs: ",
        "To: ",
        "Subject: ",
        "Outbox ID: ",
        "Error: ",
        "Reason: ",
        "Server: ",
        "Code: ",
        "Status: ",
    ];
    for &field in FIELDS {
        if let Some(value) = line.strip_prefix(field) {
            return reader_labeled_line(
                field,
                value.to_owned(),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            );
        }
    }
    Line::raw(line.to_owned())
}

/// Both readers have a labeled header, divider and a wrapped scrolling body.
/// The mail reader's header and divider helpers keep their styling consistent
/// while Notices retains its amber warning semantic color.
fn notice_reader_lines(
    row: &crate::app::NoticeRow,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let label_style = Style::default()
        .fg(theme.warning)
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![
        reader_labeled_line("Level: ", row.level, label_style),
        reader_labeled_line("Account: ", row.account.clone(), label_style),
        reader_labeled_line("Type: ", row.kind, label_style),
        reader_divider(theme, width),
        Line::raw(""),
        Line::styled(
            row.summary.clone(),
            theme.text().add_modifier(Modifier::BOLD),
        ),
    ];
    // Manual-review details already carry an account heading, which the new
    // reader header provides. Don't print it twice.
    let prefix = format!("{}  |  Manual review\n\n", row.account);
    let details = row.details.strip_prefix(&prefix).unwrap_or(&row.details);
    if details != row.summary {
        lines.push(Line::raw(""));
        lines.extend(details.lines().map(|line| notice_detail_line(line, theme)));
    }
    if row.review.is_some() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("IMAP/SMTP result uncertain. Dismissing this notice will not retry or remove the saved operation.",
            Style::default().fg(theme.warning)));
    } else if row.kind == "Queue" {
        lines.push(Line::raw(""));
        lines.push(Line::styled("Queued offline work synchronizes automatically only when safe. Manual-review work is never retried automatically.", theme.muted()));
    }
    lines
}

fn draw_notice_reader(
    frame: &mut Frame<'_>,
    theme: &Theme,
    area: Rect,
    row: Option<&crate::app::NoticeRow>,
    scroll: u16,
) {
    let Some(row) = row else {
        frame.render_widget(
            Paragraph::new("Notice is no longer active. Press Esc to return."),
            area,
        );
        return;
    };
    // Identical full-document scrolling model to the mail reader: headers and
    // body scroll together, and long wrapped errors do not overlap the header.
    let paragraph = Paragraph::new(Text::from(notice_reader_lines(row, theme, area.width)))
        .style(theme.text())
        .wrap(Wrap { trim: false });
    let visible_lines = paragraph.line_count(area.width.max(1));
    let max_scroll =
        u16::try_from(visible_lines.saturating_sub(usize::from(area.height))).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((scroll.min(max_scroll), 0)), area);
}

fn draw_notice_confirmation(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    let count = app.notices.confirming.len();
    let prompt = if count == 1 {
        "Dismiss this notice? It will disappear from the list.".to_owned()
    } else {
        format!("Dismiss {count} notices? They will disappear from the list.")
    };
    // Use the same centered Yes/No component used by mail confirmations.
    draw_short_confirmation(
        frame,
        theme,
        68,
        if count == 1 {
            " Dismiss notice "
        } else {
            " Dismiss notices "
        },
        &prompt,
        Style::default().fg(theme.warning),
        true,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::NoticeRow;
    use crate::theme::load_themes;

    fn review_row() -> NoticeRow {
        NoticeRow {
            id: "test".into(),
            level: "Action",
            account: "Burner".into(),
            kind: "Review",
            summary: "Move to [Gmail]/Trash · INBOX · result uncertain".into(),
            details: "Burner  |  Manual review\n\nOperation: Move to [Gmail]/Trash\nSource: INBOX\nUIDVALIDITY: 1\nMessage IDs: imap-uid:1:4225".into(),
            review: None,
            first_seen: 0,
        }
    }

    #[test]
    fn notice_table_header_matches_visible_row_offsets() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = load_themes().into_iter().next().unwrap();
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        let rows = vec![review_row()];
        let mut terminal = Terminal::new(TestBackend::new(120, 16)).unwrap();
        terminal
            .draw(|frame| {
                draw_notice_list(frame, &mut app, &theme, Rect::new(0, 0, 120, 16), &rows)
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y| {
            (0u16..120)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let header = row_text(0);
        let first = row_text(1);
        assert_eq!(header.find('#'), first.find('1'));
        assert_eq!(header.find("Account"), first.find("Burner"));
        assert_eq!(header.find("Level"), first.find("Action"));
        assert_eq!(header.find("Type"), first.find("Review"));
        assert_eq!(header.find("Summary"), first.find("Move to"));
    }

    #[test]
    fn notice_reader_uses_mail_header_and_bold_field_labels() {
        let theme = load_themes().into_iter().next().unwrap();
        let lines = notice_reader_lines(&review_row(), &theme, 80);
        let contents = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(contents.iter().any(|line| line == "Level: Action"));
        assert!(contents.iter().any(|line| line == "Account: Burner"));
        assert!(
            !contents
                .iter()
                .any(|line| line.contains("Burner  |  Manual review"))
        );
        for label in ["Operation: ", "Source: ", "UIDVALIDITY: ", "Message IDs: "] {
            let line = lines
                .iter()
                .find(|line| line.spans.first().is_some_and(|span| span.content == label))
                .unwrap();
            assert!(
                line.spans[0].style.add_modifier.contains(Modifier::BOLD),
                "{label} should be bold"
            );
            assert_eq!(line.spans[0].style.fg, Some(theme.warning));
        }
    }

    #[test]
    fn unstructured_errors_and_urls_are_not_rewritten_as_labels() {
        let theme = load_themes().into_iter().next().unwrap();
        let line = notice_detail_line("https://example.org:9443 failed", &theme);
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "https://example.org:9443 failed");
    }

    #[test]
    fn notices_header_and_rows_share_exact_table_columns() {
        let (columns, _account_width, _level_width, with_type) = notice_columns(120, 24);
        assert!(with_type);
        assert_eq!(columns.len(), 6); // #, check, account, level, type, summary
        let (narrow, _, _, with_type) = notice_columns(60, 24);
        assert!(!with_type);
        assert_eq!(narrow.len(), 5);
    }

    #[test]
    fn notice_counter_uses_singular_plural_at_bottom_right() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = load_themes().into_iter().next().unwrap();
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        for (count, label) in [(1usize, "1 notice"), (2, "2 notices")] {
            let rows = vec![review_row(); count];
            let mut terminal = Terminal::new(TestBackend::new(80, 15)).unwrap();
            terminal
                .draw(|frame| {
                    draw_notice_list(frame, &mut app, &theme, Rect::new(0, 0, 80, 15), &rows)
                })
                .unwrap();
            let bottom = terminal.backend().buffer();
            let line = (0..80)
                .map(|x| bottom[(x, 14)].symbol())
                .collect::<String>();
            assert!(line.trim_end().ends_with(label), "{line:?}");
        }
    }

    #[test]
    fn notice_dismissal_reuses_mail_confirmation_buttons() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = load_themes().into_iter().next().unwrap();
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        app.notices.confirming = vec!["notice-1".to_owned()];
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw_notice_confirmation(frame, &app, &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let all = (0..24)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("[Y]es"), "{all}");
        assert!(all.contains("[N]o"), "{all}");
        assert!(all.contains("Dismiss this notice?"), "{all}");
    }

    #[test]
    fn monochrome_notice_icon_is_shared_with_mail_header() {
        assert_eq!(crate::ui::NOTICE_HAZARD_ICON, "!");
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(crate::ui::NOTICE_HAZARD_ICON),
            1
        );
    }
}
