//! Contacts rendering.

use super::*;

/// Contacts are overlays over the *terminal* surface, not an opaque gray
/// popup. Match the message list and reader; reserve selected backgrounds for
/// the currently highlighted row only.
pub(super) fn contact_surface_style(theme: &Theme) -> Style {
    theme.text()
}

/// Autocomplete is an overlay, not a new compose field: users keep their
/// normal Vim cursor, and Tab/Enter accept the highlighted address.
pub(super) fn draw_contact_suggestions(frame: &mut Frame<'_>, app: &App) {
    let ids = app.contact_suggestions();
    if ids.is_empty() {
        return;
    }
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let field = match composer.field {
        ComposeField::To => 0,
        ComposeField::Cc => 1,
        ComposeField::Bcc => 2,
        _ => return,
    };
    let theme = app.theme();
    let compose = centered_rect(frame.area(), 96, 82);
    let inner_x = compose.x.saturating_add(1);
    let inner_y = compose.y.saturating_add(1);
    let width = compose.width.saturating_sub(8).min(68);
    let height = (ids.len() as u16 + 2).min(frame.area().height.saturating_sub(2));
    if width < 12 || height < 3 {
        return;
    }
    let x = inner_x
        .saturating_add(9)
        .min(frame.area().right().saturating_sub(width));
    // Recipient fields may now occupy multiple wrapped rows. Anchor the
    // suggestions below the active field rather than at a fixed line offset.
    let composer_width = compose.width.saturating_sub(2);
    let preview_theme = theme;
    let requested = [
        (ComposeField::To, "To", composer.to.as_str()),
        (ComposeField::Cc, "Cc", composer.cc.as_str()),
        (ComposeField::Bcc, "Bcc", composer.bcc.as_str()),
        (ComposeField::Subject, "Subject", composer.subject.as_str()),
    ]
    .map(|(f, name, value)| {
        compose_header_display(
            f,
            name,
            value,
            composer,
            preview_theme,
            usize::from(composer_width),
        )
        .lines
        .len()
    });
    let selected_names = compose_attachment_names(composer);
    let attachment_label = format!("  Attachments ({}):", composer.attachment_count());
    let summary = if selected_names.is_empty() {
        " none".to_owned()
    } else {
        format!(" {}", selected_names.join("  ·  "))
    };
    let attachment_rows = clipped_wrap_text(
        &format!("{attachment_label}{summary}"),
        usize::from(composer_width).max(1),
        3,
    )
    .len()
    .max(1) as u16;
    let field_heights =
        compose_header_heights(requested, compose.height.saturating_sub(2), attachment_rows);
    let y_offset: u16 = field_heights[..=field].iter().copied().sum();
    let y = inner_y
        .saturating_add(y_offset)
        .min(frame.area().bottom().saturating_sub(height));
    let area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(theme.border_type)
            .border_style(theme.border())
            .style(contact_surface_style(theme)),
        area,
    );
    for (position, id) in ids
        .iter()
        .take(usize::from(height.saturating_sub(2)))
        .enumerate()
    {
        let contact = &app.contacts.entries[*id];
        let content = format!(
            "{}  {}",
            if contact.favorite { "★" } else { " " },
            contact.display()
        );
        let truncated = truncate_display_width(&content, usize::from(width.saturating_sub(4)));
        let style = if position == composer.contact_suggestion.min(ids.len() - 1) {
            theme.selected()
        } else {
            theme.text()
        };
        frame.render_widget(
            Paragraph::new(format!(" {truncated}")).style(style),
            Rect::new(area.x + 1, area.y + 1 + position as u16, width - 2, 1),
        );
    }
}

/// SEARCH uses the theme's visual/purple color, while VISUAL uses its
/// selected/blue color. Both badges occupy the same mode slot in the footer.
pub(super) fn search_mode_style(theme: &Theme) -> Style {
    mode_style("SEARCH", theme)
}

/// The mode color is the same in the reader, message list, Contacts and Compose.
/// Normal = blue, Visual/Search = purple, Insert = green.
pub(super) fn mode_style(label: &str, theme: &Theme) -> Style {
    let bg = match label {
        "VISUAL" | "SEARCH" => theme.visual,
        "INSERT" => theme.unread,
        _ => theme.accent,
    };
    Style::default()
        .fg(theme.selected_fg)
        .bg(bg)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn compose_mode_style(mode: ComposeMode, theme: &Theme) -> Style {
    mode_style(mode.label(), theme)
}

/// Prefix for the shared bottom status row when Contacts is open. Keep the
/// account label and mode position identical to the mail browser.
pub(super) fn contact_status_spans(
    theme: &Theme,
    account_name: Option<&str>,
    modal: &crate::contacts::ContactsModal,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        format!("[{}] ", account_name.unwrap_or("Contacts")),
        theme.accent(),
    )];
    spans.extend(contact_footer_mode(theme, modal));
    if modal.searching {
        spans.push(Span::styled(
            format!(" {} matches", modal.matches.len()),
            theme.muted(),
        ));
    }
    spans
}

/// Contact mode badges rendered in the shared bottom status bar. The query
/// itself lives in the dedicated search row above the contacts list.
pub(super) fn contact_footer_mode(
    theme: &Theme,
    modal: &crate::contacts::ContactsModal,
) -> Vec<Span<'static>> {
    if let Some(edit) = modal.edit.as_ref() {
        vec![Span::styled(
            format!(" {} ", edit.mode.label()),
            compose_mode_style(edit.mode, theme),
        )]
    } else if modal.searching {
        vec![Span::styled(" SEARCH ", search_mode_style(theme))]
    } else if modal.visual_anchor.is_some() {
        vec![Span::styled(" VISUAL ", mode_style("VISUAL", theme))]
    } else {
        vec![Span::styled(" NORMAL ", mode_style("NORMAL", theme))]
    }
}

/// Contact filters and marks follow the mode at the left of the global status
/// bar. The contact total stays in the quiet footer inside Contacts itself.
pub(super) fn contact_footer_details(
    theme: &Theme,
    modal: &crate::contacts::ContactsModal,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if !modal.marked.is_empty() {
        spans.push(Span::styled(
            format!(" {} marked ", modal.marked.len()),
            theme.accent(),
        ));
    }
    if !modal.searching && !modal.query.is_empty() {
        spans.push(Span::styled(
            format!(" filter:/{} ", modal.query),
            theme.accent(),
        ));
    }
    spans
}

/// Contact editing uses the same white block cursor as Compose in
/// Normal/Visual mode. The terminal's native cursor is only used in Insert.
/// Keep selection highlighting underneath the cursor, just as in Compose.
pub(super) fn contact_editor_field_spans(
    label: &str,
    value: &str,
    editor: &crate::contacts::ContactEdit,
    is_email: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let active = editor.active_email == is_email;
    let style = if active { theme.current_line() } else { theme.text() };
    let mut spans = vec![Span::styled(format!("{label}: "), style)];
    let block = Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD);
    let length = value.chars().count();
    let cursor = editor.cursor.min(length);
    let selection = if active && editor.mode == ComposeMode::Visual {
        editor.selected_range()
    } else {
        None
    };
    for (index, ch) in value.chars().enumerate() {
        let at_cursor = active && cursor == index && editor.mode != ComposeMode::Insert;
        let selected = selection.is_some_and(|(start, end)| (start..end).contains(&index));
        spans.push(Span::styled(
            ch.to_string(),
            if at_cursor { block } else if selected { theme.selected() } else { style },
        ));
    }
    if active && cursor == length && editor.mode != ComposeMode::Insert {
        spans.push(Span::styled(" ", block));
    }
    spans
}

/// Contacts get the same extra top search row as the message list, only while
/// actively editing a search. Normal browsing gets the full list height.
pub(super) fn contacts_search_layout(inner: Rect, searching: bool) -> (Option<Rect>, Rect) {
    if !searching || inner.height <= 1 {
        return (None, inner);
    }
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    (Some(chunks[0]), chunks[1])
}

pub(super) fn contacts_list_width(inner_width: u16) -> u16 {
    (inner_width.saturating_mul(47) / 100)
        .max(12)
        .min(inner_width)
}

/// Use the existing favorite/saved icon cell for visual and marked checks.
/// This keeps contact names aligned whether the row is selected or not.
pub(super) fn contact_list_icon(
    favorite: bool,
    saved: bool,
    visual: bool,
    marked: bool,
) -> &'static str {
    if visual || marked {
        "✓ "
    } else if favorite {
        "★ "
    } else if saved {
        "● "
    } else {
        "  "
    }
}

pub(super) fn draw_contacts(frame: &mut Frame<'_>, app: &mut App) {
    let theme = app.theme().clone();
    let selection_style = app.visual_selection_style;
    let Some(modal) = app.contacts_modal.as_mut() else {
        return;
    };
    let area = centered_rect(frame.area(), 116, 88);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .title(" Contacts ")
            .borders(Borders::ALL)
            .border_type(theme.border_type)
            .style(contact_surface_style(&theme))
            .border_style(theme.border()),
        area,
    );
    let inner = Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(1),
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    if inner.width < 5 || inner.height < 4 {
        return;
    }
    // Reserve a quiet internal count row. It never uses the global status bar.
    let content_bounds = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let (search_row, content) = contacts_search_layout(content_bounds, modal.searching);
    if let Some(search_row) = search_row {
        draw_search_prompt(frame, search_row, &modal.query, &theme);
    }
    let left_width = contacts_list_width(content.width);
    let left = Rect::new(content.x, content.y, left_width, content.height);
    let right = Rect::new(
        left.right(),
        content.y,
        content.width.saturating_sub(left_width),
        content.height,
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(theme.border()),
        left,
    );
    let list_width = left.width.saturating_sub(1);
    if left.height == 0 {
        return;
    }
    modal.view_height = usize::from(left.height);
    let item_count = modal.matches.len();
    modal.ensure_visible(item_count);
    let items = &modal.matches;
    let number_width = (items.len().max(1).to_string().len() + 1).max(3);
    for (visual, &id) in items
        .iter()
        .enumerate()
        .skip(modal.scroll)
        .take(modal.view_height)
    {
        let contact = &app.contacts.entries[id];
        let active = visual == modal.selected;
        let is_visual = modal.in_visual_range(visual);
        let is_marked = modal.marked.contains(&contact.email.to_ascii_lowercase());
        let number = if active {
            (visual + 1).to_string()
        } else {
            visual.abs_diff(modal.selected).to_string()
        };
        let icon = contact_list_icon(contact.favorite, contact.saved, is_visual, is_marked);
        let label = if contact.name.is_empty() {
            &contact.email
        } else {
            &contact.name
        };
        let max_label = usize::from(list_width).saturating_sub(number_width + 4);
        let line = Line::from(vec![
            contact_gutter(&theme, is_visual, is_marked),
            Span::raw(format!("{number:>number_width$} ")),
            Span::styled(
                icon,
                selection_check_style(&theme, selection_style, is_visual, is_marked, active),
            ),
            Span::raw(truncate_display_width(label, max_label)),
        ]);
        let row_style =
            browser_row_style(&theme, selection_style, is_visual, is_marked, active, false);
        frame.render_widget(
            Paragraph::new(line).style(row_style),
            Rect::new(
                left.x,
                left.y + (visual - modal.scroll) as u16,
                list_width,
                1,
            ),
        );
    }
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(if modal.query.is_empty() {
                " No contacts yet"
            } else {
                " No matching contacts"
            })
            .style(theme.muted()),
            Rect::new(left.x, left.y, list_width, 1),
        );
    }
    if right.width > 5 {
        let x = right.x.saturating_add(2);
        let width = right.width.saturating_sub(3);
        if let Some(&id) = items.get(modal.selected) {
            let contact = &app.contacts.entries[id];
            let details = [
                ("Name", contact.name.as_str()),
                ("Email", contact.email.as_str()),
                (
                    "Source",
                    if contact.saved {
                        "Saved locally"
                    } else {
                        "Learned from mail"
                    },
                ),
                ("Favorite", if contact.favorite { "Yes" } else { "No" }),
            ];
            for (pos, (label, value)) in details.iter().enumerate() {
                let y = right.y.saturating_add((pos * 3) as u16);
                if y + 1 >= right.bottom() {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(*label).style(theme.accent()),
                    Rect::new(x, y, width, 1),
                );
                frame.render_widget(
                    Paragraph::new(truncate_display_width(value, width as usize))
                        .style(theme.text()),
                    Rect::new(x, y + 1, width, 1),
                );
            }
        } else {
            frame.render_widget(
                Paragraph::new("No contact selected").style(theme.muted()),
                Rect::new(x, right.y, width, 1),
            );
        }
    }
    let count = app.contacts.entries.len();
    let count_label = if modal.query.is_empty() {
        format!("{count} contacts")
    } else {
        format!("{} of {count} contacts", modal.matches.len())
    };
    let count_rect = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    frame.render_widget(
        Paragraph::new(count_label)
            .alignment(Alignment::Right)
            .style(theme.muted()),
        count_rect,
    );
    if modal.help {
        let matches = ranked_help_matches(CONTACT_HELP_ENTRIES, &modal.help_query);
        draw_help_palette(
            frame,
            &theme,
            " Contacts keybindings ",
            CONTACT_HELP_ENTRIES,
            &modal.help_query,
            &matches,
            (modal.help_selected, modal.help_scroll),
        );
        return;
    }
    if modal.confirm_delete {
        let ids = modal.action_ids(&app.contacts);
        let names = ids
            .iter()
            .filter_map(|&id| app.contacts.entries.get(id))
            .map(|contact| contact.display())
            .collect::<Vec<_>>();
        draw_review_dialog(
            frame,
            &theme,
            "Delete contacts",
            "Delete these contacts from your local address book?",
            &names,
            modal.delete_review.position,
        );
        return;
    }
    if let Some(edit) = &modal.edit {
        let edit_area = centered_rect(frame.area(), 74, 35);
        frame.render_widget(Clear, edit_area);
        frame.render_widget(
            Block::default()
                .title(if edit.original_email.is_some() {
                    " Edit contact "
                } else {
                    " New contact "
                })
                .borders(Borders::ALL)
                .border_type(theme.border_type)
                .border_style(theme.border())
                .style(contact_surface_style(&theme)),
            edit_area,
        );
        let width = edit_area.width.saturating_sub(4);
        let name_y = edit_area.y.saturating_add(2);
        let email_y = name_y.saturating_add(2);
        for (is_email, value, y, label) in [
            (false, edit.name.as_str(), name_y, "Name"),
            (true, edit.email.as_str(), email_y, "Email"),
        ] {
            if y >= edit_area.bottom().saturating_sub(1) {
                continue;
            }
            let label_text = format!("{label}: {value}");
            let row_style = if edit.active_email == is_email {
                theme.current_line()
            } else {
                theme.text()
            };
            let line = if edit.active_email == is_email {
                Line::from(contact_editor_field_spans(label, value, edit, is_email, &theme))
            } else {
                Line::from(Span::styled(
                    truncate_display_width(&label_text, width as usize),
                    row_style,
                ))
            };
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(edit_area.x + 2, y, width, 1),
            );
            if edit.active_email == is_email && edit.mode == ComposeMode::Insert {
                let prefix = UnicodeWidthStr::width(format!("{label}: ").as_str());
                let before: String = value.chars().take(edit.cursor).collect();
                let offset = prefix + UnicodeWidthStr::width(before.as_str());
                if offset < width as usize {
                    frame.set_cursor_position((edit_area.x + 2 + offset as u16, y));
                }
            }
        }
        if edit_area.height > 8 {
            frame.render_widget(
                Paragraph::new("Tab switch · Esc Normal/cancel · i Insert · v Visual · Enter next/save")
                    .style(theme.muted()),
                Rect::new(
                    edit_area.x + 2,
                    edit_area.bottom().saturating_sub(2),
                    width,
                    1,
                ),
            );
        }
    }
}
