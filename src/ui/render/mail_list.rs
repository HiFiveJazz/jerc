//! Mail list rendering.

use super::*;

pub(super) fn draw_mailboxes(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.set_mailbox_view_height(usize::from(area.height.max(1)));
    let theme = app.theme().clone();
    let account = app.account().expect("account exists");
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(if app.mailbox_resizing {
            theme.accent()
        } else {
            theme.border()
        });
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

/// Visual ranges are purple, while persistent Space marks retain the theme's
/// blue accent. In gutter mode the active row stays the usual dark gray.
pub(super) fn visual_row_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.selected_fg)
        .bg(theme.visual)
        .add_modifier(Modifier::BOLD)
}

/// Editor/reader current-line background without the list row's bold weight.
pub(super) fn editor_current_line_style(theme: &Theme) -> Style {
    theme.text().bg(theme.popup_bg)
}

pub(super) fn browser_row_style(
    theme: &Theme,
    mode: VisualSelectionStyle,
    visual: bool,
    marked: bool,
    active: bool,
    unread: bool,
) -> Style {
    if active && unread {
        theme.unread().bg(theme.popup_bg)
    } else if active {
        theme.current_line()
    } else if mode == VisualSelectionStyle::Row && visual {
        visual_row_style(theme)
    } else if mode == VisualSelectionStyle::Row && marked {
        theme.selected()
    } else if unread {
        theme.unread()
    } else {
        theme.text()
    }
}

pub(super) fn selection_check_style(
    theme: &Theme,
    mode: VisualSelectionStyle,
    visual: bool,
    marked: bool,
    active: bool,
) -> Style {
    if mode == VisualSelectionStyle::Row && (visual || marked) && !active {
        // On a filled row, retain contrast rather than purple-on-purple.
        Style::default()
            .fg(theme.selected_fg)
            .add_modifier(Modifier::BOLD)
    } else if visual {
        Style::default()
            .fg(theme.visual)
            .add_modifier(Modifier::BOLD)
    } else if marked {
        theme.accent()
    } else {
        Style::default() // Keep the parent row's unread/current-line color.
    }
}

/// Shared thin selection marker for attachments and the mail number column.
pub(super) fn browser_gutter(theme: &Theme, visual: bool, marked: bool) -> Span<'static> {
    Span::styled(
        if visual || marked { "▍" } else { "▏" },
        if visual {
            Style::default().fg(theme.visual)
        } else if marked {
            theme.accent()
        } else {
            theme.border()
        },
    )
}

/// Contacts share the mail marker's size and colors but never draw a
/// continuous line through unmarked rows.
pub(super) fn contact_gutter(theme: &Theme, visual: bool, marked: bool) -> Span<'static> {
    if visual || marked {
        browser_gutter(theme, visual, marked)
    } else {
        Span::raw(" ")
    }
}

/// With the mailbox sidebar visible, the marker is on the *right* of its
/// divider, in the message number column; unselected rows add no extra rule.
pub(super) fn mail_gutter(
    theme: &Theme,
    visual: bool,
    marked: bool,
    sidebar: bool,
) -> Span<'static> {
    let mut marker = browser_gutter(theme, visual, marked);
    if sidebar && !visual && !marked {
        marker.content = " ".into();
    }
    marker
}

pub(super) fn draw_messages(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
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

    // Cached rows remain normal interactive mail rows. Sync is a footer-only
    // activity; don't reserve a row or visually disable the message list.
    app.set_message_view_height(usize::from(list_area.height.max(1)));
    let account = app.account().expect("account exists");
    let visible = account.visible_message_indices();
    let selected_position = visible
        .iter()
        .position(|&index| index == account.message_index);

    if visible.is_empty() {
        let message = if !search_query.is_empty() {
            format!(" No loaded messages match /{search_query}")
        } else if account.syncing_with_server() {
            // An empty *cached* mailbox is not proof the server mailbox is
            // empty. The footer spinner already indicates background work.
            String::new()
        } else {
            format!(
                " No messages in {}.",
                account.current_mailbox.as_deref().unwrap_or("mailbox")
            )
        };
        frame.render_widget(Paragraph::new(message).style(theme.muted()), list_area);
        return;
    }

    let height = usize::from(list_area.height.max(1));
    let current_position = selected_position.unwrap_or(0);
    // Inspect only the few rows around the viewport. The old code allocated a
    // height vector and checked attachments for every loaded message per draw.
    let (start, end) = message_list_viewport(
        &visible,
        account.message_view_offset,
        current_position,
        height,
        |index| {
            let message = &account.messages[index];
            if message.has_attachment(account.bodies.get(&message.id)) {
                2
            } else {
                1
            }
        },
    );
    // Resolve the visual range once; calling message_is_visual_selected() for
    // every row previously rebuilt and searched the entire filtered index list.
    let visual_range = message_list_visual_range(&visible, app.visual_anchor, selected_position);
    let in_junk = account.current_mailbox.as_ref().is_some_and(|source| {
        account.mailboxes.iter().any(|mailbox| {
            mailbox.name == *source && mailbox.role == crate::model::MailboxRole::Junk
        })
    });
    let shown = &visible[start..end];
    let number_width = relative_number_width(visible.len());
    let message_columns = [
        Constraint::Length(number_width.saturating_add(1)),
        Constraint::Length(3),
        Constraint::Percentage(28),
        Constraint::Min(12),
        Constraint::Length(6),
    ];
    let column_areas = Layout::horizontal(message_columns)
        .spacing(1)
        .split(list_area);
    // Use the actual subject width to preserve wide emoji graphemes.
    let subject_width = column_areas
        .get(3)
        .map_or(0, |area| usize::from(area.width));
    let rows = shown.iter().enumerate().map(|(row_position, &index)| {
        let message = &account.messages[index];
        let absolute_position = start + row_position;
        let number = relative_number(absolute_position, current_position);
        let visual_selected =
            visual_range.is_some_and(|(first, last)| (first..=last).contains(&absolute_position));
        let marked = app.message_marked.contains(&message.id);
        let is_selected = visual_selected || marked;
        let current = absolute_position == current_position;
        let unread_mark = if is_selected {
            "✓"
        } else if message.flags.contains(MessageFlags::SEEN) {
            " "
        } else {
            "●"
        };
        let attachment_mark = message.has_attachment(account.bodies.get(&message.id));
        let mark = Line::from(vec![
            Span::styled(
                unread_mark,
                selection_check_style(
                    &theme,
                    app.visual_selection_style,
                    visual_selected,
                    marked,
                    current,
                ),
            ),
            if in_junk {
                Span::styled(
                    "!",
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(" ")
            },
            if attachment_mark {
                Span::styled(
                    "⧉",
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(" ")
            },
        ]);
        let sender = if message.from_name.is_empty() {
            message.from_address.as_str()
        } else {
            message.from_name.as_str()
        };
        let style = browser_row_style(
            &theme,
            app.visual_selection_style,
            visual_selected,
            marked,
            current,
            message.unread(),
        );
        // Borrow unchanged subjects and strings from the message. Own text
        // only when normalization or display-width truncation actually changes it.
        let subject_line = match subject_for_list_cow(&message.subject) {
            std::borrow::Cow::Borrowed(subject) => {
                Line::raw(truncate_display_width_cow(subject, subject_width))
            }
            std::borrow::Cow::Owned(normalized) => {
                Line::raw(truncate_display_width_cow(&normalized, subject_width).into_owned())
            }
        };
        let subject_cell = if attachment_mark {
            let subtitle = account.bodies.get(&message.id).map_or_else(
                || {
                    attachment_subtitle(&crate::mail::visible_attachment_hints(
                        &message.attachment_names_hint,
                    ))
                },
                |body| attachment_subtitle(&body.attachment_names),
            );
            Cell::from(Text::from(vec![
                subject_line,
                Line::styled(
                    truncate_display_width(&subtitle, subject_width),
                    theme.muted(),
                ),
            ]))
        } else {
            Cell::from(subject_line)
        };
        // Keep the mailbox border intact. The thin purple selection marker
        // occupies the first cell to its right, followed by aligned numbers.
        let number_line = Line::from(vec![
            mail_gutter(&theme, visual_selected, marked, app.sidebar_visible),
            Span::raw(format!(
                "{number:>width$}",
                width = usize::from(number_width)
            )),
        ]);
        let number_cell = if attachment_mark {
            let subtitle_gutter = Line::from(mail_gutter(
                &theme,
                visual_selected,
                marked,
                app.sidebar_visible,
            ));
            Cell::from(Text::from(vec![number_line, subtitle_gutter]))
        } else {
            Cell::from(number_line)
        };
        Row::new(vec![
            number_cell,
            Cell::from(mark),
            Cell::from(sender),
            subject_cell,
            Cell::from(message.date_label.as_str()),
        ])
        .height(if attachment_mark { 2 } else { 1 })
        .style(style)
    });

    let table = Table::new(rows, message_columns)
        .style(theme.text())
        .column_spacing(1);
    // Paint every selected terminal row, including table gaps and attachment
    // subtitles. Visual ranges are purple; independently marked rows are blue.
    if app.visual_selection_style == VisualSelectionStyle::Row {
        let mut y = list_area.y;
        for (row_position, &index) in shown.iter().enumerate() {
            let message = &account.messages[index];
            let position = start + row_position;
            let visual =
                visual_range.is_some_and(|(first, last)| (first..=last).contains(&position));
            let marked = app.message_marked.contains(&message.id);
            let row_height = if message.has_attachment(account.bodies.get(&message.id)) {
                2
            } else {
                1
            };
            if (visual || marked) && y < list_area.bottom() {
                let current = position == current_position;
                let style = if current {
                    browser_row_style(
                        &theme,
                        app.visual_selection_style,
                        visual,
                        marked,
                        true,
                        message.unread(),
                    )
                } else if visual {
                    visual_row_style(&theme)
                } else {
                    theme.selected()
                };
                frame.render_widget(
                    Block::default().style(style),
                    Rect::new(
                        list_area.x,
                        y,
                        list_area.width,
                        row_height.min(list_area.bottom().saturating_sub(y)),
                    ),
                );
            }
            y = y.saturating_add(row_height);
        }
    }
    frame.render_widget(table, list_area);
    if let Some(account) = app.account_mut() {
        account.message_view_offset = start;
    }
}

/// Compute the same terminal-row-aware viewport as the original list without
/// allocating a row-height vector or visiting messages far from the viewport.
pub(super) fn message_list_viewport(
    visible: &[usize],
    offset: usize,
    current_position: usize,
    height: usize,
    mut row_height: impl FnMut(usize) -> usize,
) -> (usize, usize) {
    if visible.is_empty() {
        return (0, 0);
    }
    let current = current_position.min(visible.len() - 1);
    let min_start = offset.min(visible.len() - 1).min(current);
    let mut start = current;
    let mut used = row_height(visible[current]);
    while start > min_start {
        let previous = row_height(visible[start - 1]);
        if used.saturating_add(previous) > height {
            break;
        }
        used += previous;
        start -= 1;
    }
    let mut end = start;
    used = 0;
    while end < visible.len() {
        let next = row_height(visible[end]);
        if used.saturating_add(next) > height {
            break;
        }
        used += next;
        end += 1;
    }
    if end == start {
        end = (start + 1).min(visible.len());
    }
    (start, end)
}

/// Positions are indices in the filtered/sorted view, not message IDs.
pub(super) fn message_list_visual_range(
    visible: &[usize],
    anchor: Option<usize>,
    current_position: Option<usize>,
) -> Option<(usize, usize)> {
    let anchor = anchor?;
    let current_position = current_position?;
    let anchor_position = visible.iter().position(|&index| index == anchor)?;
    Some((
        anchor_position.min(current_position),
        anchor_position.max(current_position),
    ))
}

/// Attachment chips share the same left edge as the first subject character.
pub(super) fn attachment_subtitle(names: &[String]) -> String {
    if names.is_empty() {
        "Attachment".to_owned()
    } else {
        names
            .iter()
            .take(2)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}
