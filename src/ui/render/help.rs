//! Help rendering.

use super::*;

pub(super) fn draw_help(frame: &mut Frame<'_>, app: &App) {
    let attachment_help = app.attachment_picker_visible;
    let entries = if attachment_help {
        ATTACHMENT_HELP_ENTRIES
    } else {
        HELP_ENTRIES
    };
    let matches = app.help_matches();
    draw_help_palette(
        frame,
        app.theme(),
        if attachment_help {
            " Attachment keybindings "
        } else {
            " Keybindings "
        },
        entries,
        &app.help_query,
        &matches,
        (app.help_selected, app.help_scroll),
    );
}

// At a standard 80-column terminal, the 86-column modal is clipped to 80.
// After borders (2), the category (13), key (27) and two gaps (2), 36
// terminal cells remain for each description. On narrow screens, shrink
// category/key columns instead of allowing description cells to overflow.
const HELP_CATEGORY_COLUMNS: u16 = 13;
const HELP_KEY_COLUMNS: u16 = 27;

pub(super) fn help_columns(inner_width: u16) -> (u16, u16, u16) {
    let category = HELP_CATEGORY_COLUMNS.min(inner_width / 4);
    let key = HELP_KEY_COLUMNS.min(inner_width.saturating_sub(category + 2) / 2);
    let description = inner_width.saturating_sub(category + key + 2);
    (category, key, description)
}

/// Identical help table, search field, highlighting and footer for mail and contacts.
pub(super) fn draw_help_palette(
    frame: &mut Frame<'_>,
    theme: &Theme,
    title: &str,
    entries: &[HelpEntry],
    query: &str,
    matches: &[usize],
    navigation: (usize, usize),
) {
    let (selected, scroll) = navigation;
    let area = centered_rect(frame.area(), 86, 70);
    let inner = popup_inner(frame, theme, area, title, theme.accent());
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let prompt = if query.is_empty() {
        "type to search…"
    } else {
        query
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" / ", theme.accent()),
            Span::styled(
                prompt,
                if query.is_empty() {
                    theme.muted()
                } else {
                    theme.text()
                },
            ),
        ])),
        chunks[0],
    );

    let shown = visible_filtered(matches, selected, scroll, usize::from(chunks[1].height));
    if shown.is_empty() {
        frame.render_widget(
            Paragraph::new(" No matching keybindings").style(theme.muted()),
            chunks[1],
        );
    } else {
        let (category_width, key_width, description_width) = help_columns(chunks[1].width);
        let rows = shown.iter().map(|&index| {
            let entry = entries[index];
            Row::new(vec![
                Cell::from(truncate_display_width(
                    entry.category,
                    usize::from(category_width),
                )),
                Cell::from(truncate_display_width(entry.key, usize::from(key_width))),
                Cell::from(truncate_display_width(
                    entry.description,
                    usize::from(description_width),
                )),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(category_width),
                Constraint::Length(key_width),
                Constraint::Length(description_width),
            ],
        )
        .style(theme.text())
        .column_spacing(1)
        .row_highlight_style(theme.selected());
        let selected = shown.iter().position(|&index| index == selected);
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

pub(super) fn draw_theme_picker(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let area = centered_rect(frame.area(), 52, 46);
    let inner = popup_inner(frame, &theme, area, " Themes ", theme.accent());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    draw_search_header(frame, chunks[0], &app.theme_query, app);

    let matches = app.theme_matches();
    let shown = visible_filtered(
        &matches,
        app.theme_selected,
        app.theme_scroll,
        usize::from(chunks[1].height),
    );
    if shown.is_empty() {
        frame.render_widget(
            Paragraph::new(" No matching themes").style(theme.muted()),
            chunks[1],
        );
    } else {
        let rows = shown.iter().map(|index| {
            let candidate = &app.themes[*index];
            let marker = if *index == app.theme_selected {
                "●"
            } else {
                " "
            };
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
