//! Compose rendering.

use super::*;

pub(super) fn compose_attachment_names(composer: &crate::app::Composer) -> Vec<String> {
    let mut names = composer
        .attachment_paths
        .iter()
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        })
        .collect::<Vec<_>>();
    names.extend(
        composer
            .attachments
            .split(';')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| {
                std::path::Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_owned())
            }),
    );
    names.extend(composer.draft_attachment_names.iter().cloned());
    names
}

/// Wrap on word boundaries, and truncate only if the available rows run out.
/// This also handles one single filename longer than the entire line.
pub(super) fn clipped_wrap_text(value: &str, width: usize, max_rows: usize) -> Vec<String> {
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }
    let mut rows = vec![String::new()];
    let mut clipped = false;
    for word in value.split_whitespace() {
        let mut word = word.to_owned();
        if UnicodeWidthStr::width(word.as_str()) > width {
            word = truncate_display_width(&word, width);
        }
        let last = rows.last_mut().expect("at least one row");
        let separator = if last.is_empty() { "" } else { " " };
        if UnicodeWidthStr::width(last.as_str())
            + separator.len()
            + UnicodeWidthStr::width(word.as_str())
            <= width
        {
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
            if UnicodeWidthStr::width(clipped_row.as_str()) + UnicodeWidthStr::width(grapheme)
                > budget
            {
                break;
            }
            clipped_row.push_str(grapheme);
        }
        *last = clipped_row;
        last.push_str(if width == 1 { "…" } else { " …" });
    }
    rows
}

pub(super) struct ComposePickerOptions {
    pub(super) selected: Option<usize>,
    pub(super) show_numbers: bool,
    pub(super) visual: Option<(usize, usize)>,
    pub(super) mode: VisualSelectionStyle,
}

pub(super) fn compose_picker_rows(
    frame: &mut Frame<'_>,
    rect: Rect,
    entries: &[crate::app::ComposeFileEntry],
    attachments: &[std::path::PathBuf],
    options: ComposePickerOptions,
    theme: &Theme,
) {
    let ComposePickerOptions {
        selected,
        show_numbers,
        visual,
        mode,
    } = options;
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new(" Empty directory ").style(theme.muted()),
            rect,
        );
        return;
    }
    let selected_index = selected.unwrap_or(0).min(entries.len().saturating_sub(1));
    // The selection stays near the middle, in the three-pane file picker.
    let start = selected_index
        .saturating_sub(usize::from(rect.height) / 2)
        .min(entries.len().saturating_sub(usize::from(rect.height)));
    let end = start
        .saturating_add(usize::from(rect.height))
        .min(entries.len());
    let digits = entries.len().to_string().len();
    let mut items = Vec::new();
    for (index, entry) in entries.iter().enumerate().take(end).skip(start) {
        let is_selected = selected == Some(index);
        let number = if show_numbers {
            let value = if is_selected {
                index + 1
            } else {
                index.abs_diff(selected_index)
            };
            format!("{value:>digits$} ")
        } else {
            String::new()
        };
        let visually_selected = visual.is_some_and(|(a, b)| (a.min(b)..=a.max(b)).contains(&index));
        let marker = if entry.is_directory {
            "󰉋 "
        } else if attachments.contains(&entry.path) {
            "✓ "
        } else {
            "  "
        };
        let style = if entry.is_directory {
            theme.accent()
        } else if attachments.contains(&entry.path) {
            theme.unread()
        } else {
            theme.text()
        };
        let left_width =
            UnicodeWidthStr::width(number.as_str()) + UnicodeWidthStr::width(marker) + 1;
        let available = usize::from(rect.width).saturating_sub(left_width);
        let name = truncate_display_width(&entry.name, available);
        let line = Line::from(vec![
            browser_gutter(theme, visually_selected, false),
            Span::styled(
                number,
                if is_selected {
                    theme.accent()
                } else {
                    theme.muted()
                },
            ),
            Span::styled(marker.to_owned(), style),
            Span::styled(name, style),
        ]);
        let item = ListItem::new(line).style(browser_row_style(
            theme,
            mode,
            visually_selected,
            false,
            is_selected,
            false,
        ));
        items.push(item);
    }
    frame.render_widget(List::new(items), rect);
}

pub(super) fn draw_compose_file_picker(frame: &mut Frame<'_>, app: &mut App) {
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(110),
        82,
    );
    let inner_width = area.width.saturating_sub(2);
    let left_width = (inner_width * 24 / 100).max(12).min(inner_width);
    let middle_width = (inner_width * 34 / 100)
        .max(18)
        .min(inner_width.saturating_sub(left_width));
    let preview_width = inner_width
        .saturating_sub(left_width + middle_width)
        .saturating_sub(2);
    let preview_height = area.height.saturating_sub(3);
    app.ensure_compose_file_preview(preview_width, preview_height);
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let Some(picker) = composer.file_picker.as_ref() else {
        return;
    };
    let theme = app.theme();
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(
            " Attach files · {} selected ",
            composer.attachment_count()
        ))
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 5 || inner.width < 12 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 󰉋 ", theme.accent()),
            Span::styled(
                picker.directory.display().to_string(),
                theme.text().add_modifier(Modifier::BOLD),
            ),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let search_height = u16::from(picker.search_query.is_some());
    let content = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1 + search_height),
    );
    let left_width = (content.width * 24 / 100).max(12).min(content.width);
    let middle_width = (content.width * 34 / 100)
        .max(18)
        .min(content.width.saturating_sub(left_width));
    let right_width = content.width.saturating_sub(left_width + middle_width);
    let parent_area = Rect::new(content.x, content.y, left_width, content.height);
    let files_area = Rect::new(
        content.x + left_width,
        content.y,
        middle_width,
        content.height,
    );
    let preview_area = Rect::new(files_area.right(), content.y, right_width, content.height);
    if parent_area.width > 0 {
        frame.render_widget(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(theme.border()),
            parent_area,
        );
        compose_picker_rows(
            frame,
            Rect::new(
                parent_area.x,
                parent_area.y,
                parent_area.width.saturating_sub(1),
                parent_area.height,
            ),
            &picker.parent_entries,
            &[],
            ComposePickerOptions {
                selected: Some(picker.parent_selected),
                show_numbers: false,
                visual: None,
                mode: app.visual_selection_style,
            },
            theme,
        );
    }
    if files_area.width > 0 {
        frame.render_widget(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(theme.border()),
            files_area,
        );
        compose_picker_rows(
            frame,
            Rect::new(
                files_area.x,
                files_area.y,
                files_area.width.saturating_sub(1),
                files_area.height,
            ),
            &picker.entries,
            &composer.attachment_paths,
            ComposePickerOptions {
                selected: Some(picker.selected),
                show_numbers: true,
                visual: picker.visual_anchor.map(|anchor| (anchor, picker.selected)),
                mode: app.visual_selection_style,
            },
            theme,
        );
    }
    if preview_area.width > 0 {
        let preview = Rect::new(
            preview_area.x + 1,
            preview_area.y,
            preview_area.width.saturating_sub(2),
            preview_area.height,
        );
        let lines: Vec<Line<'static>> = match &picker.preview {
            crate::app::ComposeFilePreview::Empty => {
                vec![Line::styled("Empty directory", theme.muted())]
            }
            crate::app::ComposeFilePreview::Loading(message) => {
                vec![Line::styled(*message, theme.muted())]
            }
            crate::app::ComposeFilePreview::Directory(entries) => entries
                .iter()
                .map(|entry| {
                    Line::styled(
                        format!(
                            "{}{}",
                            if entry.is_directory { "󰉋 " } else { "  " },
                            entry.name
                        ),
                        if entry.is_directory {
                            theme.accent()
                        } else {
                            theme.text()
                        },
                    )
                })
                .collect(),
            crate::app::ComposeFilePreview::Text { lines, truncated } => {
                let mut result: Vec<_> = lines
                    .iter()
                    .map(|line| Line::styled(line.clone(), theme.text()))
                    .collect();
                if *truncated {
                    result.push(Line::styled("… truncated …", theme.muted()));
                }
                result
            }
            crate::app::ComposeFilePreview::Patch(patch) => {
                use crate::patch_preview::PatchTone;
                let mut lines: Vec<Line<'static>> = patch
                    .lines
                    .iter()
                    .map(|fragments| {
                        // Apply a consistent row background even where tree-sitter
                        // overlays source syntax colors on added code.
                        let patch_background = match fragments.first().map(|f| f.tone) {
                            Some(PatchTone::Add) => diff_line_style(PatchLineKind::Add, theme).bg,
                            Some(PatchTone::Remove) => {
                                diff_line_style(PatchLineKind::Remove, theme).bg
                            }
                            _ => None,
                        };
                        let mut spans = fragments
                            .iter()
                            .map(|fragment| {
                                let mut style = match fragment.tone {
                                    PatchTone::Default => theme.text(),
                                    PatchTone::Meta => theme.accent().add_modifier(Modifier::BOLD),
                                    PatchTone::Hunk => theme.accent().add_modifier(Modifier::BOLD),
                                    PatchTone::Add => diff_line_style(PatchLineKind::Add, theme),
                                    PatchTone::Remove => {
                                        diff_line_style(PatchLineKind::Remove, theme)
                                    }
                                    PatchTone::Keyword => Style::default().fg(theme.visual),
                                    PatchTone::String => Style::default().fg(theme.warning),
                                    PatchTone::Comment => theme.muted(),
                                    PatchTone::Number => Style::default().fg(theme.warning),
                                    PatchTone::Function => theme.accent(),
                                    PatchTone::Type => theme.accent(),
                                };
                                if let Some(bg) = patch_background {
                                    style = style.bg(bg);
                                }
                                Span::styled(fragment.text.clone(), style)
                            })
                            .collect::<Vec<_>>();
                        if let Some(bg) = patch_background {
                            let used = fragments
                                .iter()
                                .map(|fragment| UnicodeWidthStr::width(fragment.text.as_str()))
                                .sum::<usize>();
                            let padding = usize::from(preview.width).saturating_sub(used);
                            if padding > 0 {
                                spans.push(Span::styled(
                                    " ".repeat(padding),
                                    Style::default().bg(bg),
                                ));
                            }
                        }
                        Line::from(spans)
                    })
                    .collect();
                if patch.truncated {
                    lines.push(Line::styled("… truncated …", theme.muted()));
                }
                lines
            }
            crate::app::ComposeFilePreview::Image(_) => {
                if app.compose_file_preview_image().is_some() {
                    vec![]
                } else {
                    vec![Line::styled(
                        "Unable to display image in this terminal",
                        theme.muted(),
                    )]
                }
            }
            crate::app::ComposeFilePreview::Binary(bytes) => vec![
                Line::styled("Binary file", theme.muted()),
                Line::styled(format!("{bytes} bytes"), theme.text()),
            ],
            crate::app::ComposeFilePreview::Error(error) => {
                vec![Line::styled(error.clone(), theme.error())]
            }
        };
        frame.render_widget(Paragraph::new(lines), preview);
        if let Some(protocol) = app.compose_file_preview_image() {
            let size = protocol.size();
            if size.width <= preview.width && size.height <= preview.height {
                let image_area = Rect::new(
                    preview.x + (preview.width - size.width) / 2,
                    preview.y + (preview.height - size.height) / 2,
                    size.width,
                    size.height,
                );
                frame.render_widget(TerminalImage::new(protocol), image_area);
            }
        }
    }
    if let Some(query) = &picker.search_query {
        let matches = picker.search_matches_for_display();
        let position = matches.iter().position(|index| *index == picker.selected);
        let counter = position.map_or_else(
            || format!("0/{}", matches.len()),
            |index| format!("{}/{}", index + 1, matches.len()),
        );
        let prompt = Line::from(vec![
            Span::styled(" / ", theme.accent()),
            Span::styled(query.clone(), theme.text()),
            Span::styled(format!("  {counter}"), theme.muted()),
        ]);
        frame.render_widget(
            Paragraph::new(prompt),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }
    if picker.help_visible {
        draw_compose_file_picker_help(frame, app, picker);
    }
}

pub(super) fn draw_compose_file_picker_help(
    frame: &mut Frame<'_>,
    app: &App,
    picker: &crate::app::ComposeFilePicker,
) {
    let theme = app.theme();
    let area = centered_rect(frame.area(), 83, 72);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" File picker keybindings ")
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    draw_search_header(frame, chunks[0], &picker.help_query, app);
    let matches = picker.help_matches();
    let start = picker
        .help_scroll
        .min(matches.len().saturating_sub(usize::from(chunks[1].height)));
    let shown: Vec<_> = matches
        .into_iter()
        .skip(start)
        .take(usize::from(chunks[1].height))
        .collect();
    let (category_width, key_width, description_width) = help_columns(chunks[1].width);
    let rows = shown.iter().map(|&index| {
        let entry = PICKER_HELP_ENTRIES[index];
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
    let mut state = TableState::default().with_selected(
        shown
            .iter()
            .position(|&index| index == picker.help_selected),
    );
    frame.render_stateful_widget(table, chunks[1], &mut state);
    // All picker bindings live in this searchable popup, not a footer.
}

pub(super) fn draw_compose_attachment_manager(frame: &mut Frame<'_>, app: &App) {
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let theme = app.theme();
    let area = centered_rect(frame.area(), 84, 66);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(
            " Attached files · {} ",
            composer.attachment_count()
        ))
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mut files = composer.attachment_paths.clone();
    files.extend(
        composer
            .attachments
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(std::path::PathBuf::from),
    );
    if files.is_empty() {
        let label = if composer.draft_attachment_names.is_empty() {
            " No files attached".to_owned()
        } else {
            format!(" Saved draft attachment(s) retained: {}", composer.draft_attachment_names.join(", "))
        };
        frame.render_widget(
            Paragraph::new(label).style(theme.muted()),
            inner,
        );
        return;
    }
    let selected = composer.attachment_manager_selected.min(files.len() - 1);
    let start = selected
        .saturating_sub(usize::from(inner.height) / 2)
        .min(files.len().saturating_sub(usize::from(inner.height)));
    let rows: Vec<ListItem<'_>> = files
        .iter()
        .enumerate()
        .skip(start)
        .take(usize::from(inner.height))
        .map(|(index, file)| {
            let name = file
                .file_name()
                .unwrap_or(file.as_os_str())
                .to_string_lossy();
            let parent = file
                .parent()
                .map_or_else(String::new, |path| path.display().to_string());
            let name_width = usize::from(inner.width).saturating_sub(5);
            let visual = composer
                .attachment_manager_visual_anchor
                .is_some_and(|anchor| {
                    (anchor.min(selected)..=anchor.max(selected)).contains(&index)
                });
            let marked = composer.attachment_manager_marked.contains(&index);
            let line = Line::from(vec![
                browser_gutter(theme, visual, marked),
                Span::styled(if index == selected { "▸ " } else { "  " }, theme.accent()),
                Span::styled(
                    truncate_display_width(&name, name_width.min(32)),
                    theme.text(),
                ),
                Span::styled(
                    format!(
                        "  {}",
                        truncate_display_width(&parent, name_width.saturating_sub(32))
                    ),
                    theme.muted(),
                ),
            ]);
            ListItem::new(line).style(browser_row_style(
                theme,
                app.visual_selection_style,
                visual,
                marked,
                index == selected,
                false,
            ))
        })
        .collect();
    frame.render_widget(List::new(rows), inner);
}

pub(super) fn draw_compose_advanced_options(frame: &mut Frame<'_>, app: &App) {
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let theme = app.theme();
    let area = centered_rect(frame.area(), 82, 32);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Advanced compose options ")
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled("Custom email headers (optional)", theme.accent()),
            Line::styled(
                "Use Name: value; Name: value. Standard fields are already handled.",
                theme.muted(),
            ),
        ]),
        chunks[0],
    );
    let mut spans = vec![Span::styled("  Headers: ", theme.accent())];
    spans.extend(editor_spans(&composer.headers, composer, true, theme));
    frame.render_widget(Paragraph::new(Line::from(spans)), chunks[1]);
    frame.render_widget(
        Paragraph::new(" Enter / Esc / Ctrl+O: return to compose").style(theme.muted()),
        chunks[2],
    );
}

pub(super) fn compose_mode_colors(mode: ComposeMode, theme: &Theme) -> (Color, Color) {
    match mode {
        ComposeMode::Normal => (theme.accent, theme.selected_fg),
        ComposeMode::Insert => (theme.unread, theme.selected_fg),
        ComposeMode::Visual => (theme.visual, theme.selected_fg),
    }
}

pub(super) fn draw_compose_help(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let area = centered_rect(frame.area(), 85, 73);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Compose keybindings ")
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    draw_search_header(frame, chunks[0], &composer.help_query, app);
    let matches = composer.compose_help_matches();
    let max_start = matches.len().saturating_sub(usize::from(chunks[1].height));
    let start = composer.help_scroll.min(max_start);
    let shown: Vec<usize> = matches
        .into_iter()
        .skip(start)
        .take(usize::from(chunks[1].height))
        .collect();
    let (category_width, key_width, description_width) = help_columns(chunks[1].width);
    let rows = shown.iter().map(|&index| {
        let entry = COMPOSE_HELP_ENTRIES[index];
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
    let selected = shown
        .iter()
        .position(|&index| index == composer.help_selected);
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, chunks[1], &mut state);
    frame.render_widget(
        Paragraph::new(" Type to filter   Backspace to edit   Esc to close ").style(theme.muted()),
        chunks[2],
    );
}
/// Layout of a single composer header. We wrap on grapheme boundaries
/// rather than relying on Paragraph word-wrap, so the visual caret position
/// cannot drift when a recipient contains spaces, wide characters or emoji.
pub(super) struct ComposeHeaderDisplay {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) cursor_row: usize,
    pub(super) cursor_col: usize,
    pub(super) active: bool,
}

pub(super) fn compose_header_display(
    field: ComposeField,
    name: &str,
    value: &str,
    composer: &crate::app::Composer,
    theme: &Theme,
    width: usize,
) -> ComposeHeaderDisplay {
    let active = composer.field == field;
    let marker = if active { "▸ " } else { "  " };
    let prefix = truncate_display_width(&format!("{marker}{name}: "), width);
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let prefix_style = if active || !value.trim().is_empty() {
        theme.accent()
    } else {
        theme.muted()
    };
    // Leave room for a wide (two-cell) grapheme even in narrow terminals.
    let continuation = prefix_width.min(width.saturating_sub(2));
    let spans = editor_spans(value, composer, active, theme);
    let mut lines = Vec::new();
    let mut row = vec![Span::styled(prefix, prefix_style)];
    let mut column = prefix_width;
    let mut cursor_row = 0;
    let mut cursor_col = column;
    let mut index = 0usize;

    for grapheme in UnicodeSegmentation::graphemes(value, true) {
        let glyph_width = UnicodeWidthStr::width(grapheme);
        if grapheme == "\n"
            || (width > 0 && column > continuation && column.saturating_add(glyph_width) > width)
        {
            lines.push(Line::from(std::mem::take(&mut row)));
            row.push(Span::raw(" ".repeat(continuation)));
            column = continuation;
        }
        if index == composer.cursor && active {
            cursor_row = lines.len();
            cursor_col = column;
        }
        if grapheme != "\n" {
            let n = grapheme.chars().count();
            row.extend(spans[index..index + n].iter().cloned());
            column = column.saturating_add(glyph_width);
        }
        index += grapheme.chars().count();
    }
    // A caret after the last character also needs an on-screen cell. The
    // normal-mode block at EOF is already included in editor_spans().
    if active && composer.cursor >= index {
        if width > 0 && column >= width {
            lines.push(Line::from(std::mem::take(&mut row)));
            row.push(Span::raw(" ".repeat(continuation)));
            column = continuation;
        }
        cursor_row = lines.len();
        cursor_col = column;
    }
    if spans.len() > index {
        row.extend(spans[index..].iter().cloned());
    }
    lines.push(Line::from(row));
    ComposeHeaderDisplay {
        lines,
        cursor_row,
        cursor_col,
        active,
    }
}

/// Reserve space for the message body and distribute remaining header rows
/// fairly. When a header is taller than its assigned height, its active caret
/// stays in view via vertical scrolling inside that field.
pub(super) fn compose_header_heights(
    requested: [usize; 4],
    height: u16,
    attachment_rows: u16,
) -> [u16; 4] {
    let reserved = attachment_rows.saturating_add(1 + 3); // body label and body; footer is global
    let budget = height.saturating_sub(reserved).max(4) as usize;
    let mut heights = [1u16; 4];
    while heights.iter().map(|&h| usize::from(h)).sum::<usize>() < budget {
        let mut increased = false;
        for (i, desired) in requested.iter().copied().enumerate() {
            if usize::from(heights[i]) < desired
                && heights.iter().map(|&h| usize::from(h)).sum::<usize>() < budget
            {
                heights[i] += 1;
                increased = true;
            }
        }
        if !increased {
            break;
        }
    }
    heights
}

pub(super) fn render_compose_header(
    frame: &mut Frame<'_>,
    area: Rect,
    display: ComposeHeaderDisplay,
    composer: &crate::app::Composer,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let scroll = if display.active {
        display
            .cursor_row
            .saturating_sub(usize::from(area.height).saturating_sub(1))
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(display.lines)
            .style(if display.active {
                editor_current_line_style(theme)
            } else {
                theme.text()
            })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        area,
    );
    if display.active
        && composer.mode == ComposeMode::Insert
        && composer.file_picker.is_none()
        && !composer.attachment_manager_open
        && !composer.advanced_headers_open
        && !composer.help_visible
        && !composer.confirm_send
        && !composer.exit_prompt
    {
        let visual_row = display.cursor_row.saturating_sub(scroll);
        if visual_row < usize::from(area.height) && display.cursor_col < usize::from(area.width) {
            frame.set_cursor_position((
                area.x + display.cursor_col as u16,
                area.y + visual_row as u16,
            ));
        }
    }
}

pub(super) fn draw_composer(frame: &mut Frame<'_>, app: &mut App) {
    let theme = app.theme().clone();
    let Some(composer) = app.composer.as_mut() else {
        return;
    };
    let area = centered_rect(frame.area(), 96, 82);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(if composer.sending {
            " Compose • sending… "
        } else {
            " Compose "
        })
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.border());
    let inside = block.inner(area);
    frame.render_widget(block, area);

    let selected_names = compose_attachment_names(composer);
    let attachments_label = format!("  Attachments ({}):", composer.total_attachment_count());
    let summary = if selected_names.is_empty() {
        " none".to_owned()
    } else {
        format!(" {}", selected_names.join("  ·  "))
    };
    let width = usize::from(inside.width).max(1);
    let attachment_lines = clipped_wrap_text(&format!("{attachments_label}{summary}"), width, 3);
    let attachment_rows = attachment_lines.len().max(1) as u16;
    let header_specs = [
        (ComposeField::To, "To", composer.to.as_str()),
        (ComposeField::Cc, "Cc", composer.cc.as_str()),
        (ComposeField::Bcc, "Bcc", composer.bcc.as_str()),
        (ComposeField::Subject, "Subject", composer.subject.as_str()),
    ];
    let headers = header_specs
        .into_iter()
        .map(|(field, name, value)| {
            compose_header_display(field, name, value, composer, &theme, width)
        })
        .collect::<Vec<_>>();
    let requested = std::array::from_fn(|i| headers[i].lines.len());
    let header_heights = compose_header_heights(requested, inside.height, attachment_rows);
    let fields = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_heights[0]),
            Constraint::Length(header_heights[1]),
            Constraint::Length(header_heights[2]),
            Constraint::Length(header_heights[3]),
            Constraint::Length(attachment_rows),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .split(inside);

    for (i, header) in headers.into_iter().enumerate() {
        render_compose_header(frame, fields[i], header, composer, &theme);
    }
    let attachments_style = if selected_names.is_empty() {
        theme.muted()
    } else {
        theme.text()
    };
    let attachment_text = attachment_lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 && line.starts_with(&attachments_label) {
                Line::from(vec![
                    Span::styled(
                        attachments_label.clone(),
                        if selected_names.is_empty() {
                            theme.muted()
                        } else {
                            theme.accent()
                        },
                    ),
                    Span::styled(
                        line[attachments_label.len()..].to_owned(),
                        attachments_style,
                    ),
                ])
            } else {
                Line::styled(line, attachments_style)
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(attachment_text), fields[4]);
    let body_active = composer.field == ComposeField::Body;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if body_active { "▸ Body:" } else { "  Body:" },
            if body_active || !composer.body.trim().is_empty() {
                theme.accent()
            } else {
                theme.muted()
            },
        )))
        .style(if body_active {
            editor_current_line_style(&theme)
        } else {
            theme.text()
        }),
        fields[5],
    );

    // Compose, reply and forward use exactly the same physical-row model for
    // rendering, j/k, and Ctrl-j/k. Explicit wrap rows avoid treating a long
    // forwarded line as a single invisible logical line during navigation.
    composer.body_view_width = usize::from(fields[6].width.saturating_sub(4)).max(1);
    let viewport_height = usize::from(fields[6].height).max(1);
    let rows = composer.body_rows();
    let cursor_row = rows
        .iter()
        .rposition(|row| row.start <= composer.cursor)
        .unwrap_or(0);
    let active_line = rows[cursor_row].logical_line;
    let raw = editor_text(&composer.body, composer, body_active, &theme);
    let (mode_color, _) = compose_mode_colors(composer.mode, &theme);
    let mut display = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let current = body_active && index == cursor_row;
        let number = if row.continuation {
            "    ".to_owned()
        } else {
            let value = if row.logical_line == active_line {
                row.logical_line + 1
            } else {
                row.logical_line.abs_diff(active_line)
            };
            format!("{value:>3} ")
        };
        let number_style = if current {
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD)
        } else {
            theme.muted()
        };
        let mut spans = vec![Span::styled(number, number_style)];
        if let Some(line) = raw.lines.get(row.logical_line) {
            let offset = row.start.saturating_sub(row.line_start);
            let length = row.end.saturating_sub(row.start);
            spans.extend(line.spans.iter().skip(offset).take(length).cloned());
            // At the end of a newline-terminated or final line, editor_text
            // creates a separate block cursor cell in Normal/Visual mode.
            if current
                && composer.mode != ComposeMode::Insert
                && rows
                    .get(index + 1)
                    .is_none_or(|next| next.logical_line != row.logical_line)
                && let Some(cell) = line.spans.get(offset + length)
            {
                spans.push(cell.clone());
            }
        }
        display.push(Line::from(spans));
    }
    let mut scroll = composer.body_scroll;
    if body_active {
        if cursor_row < scroll {
            scroll = cursor_row;
        }
        if cursor_row >= scroll.saturating_add(viewport_height) {
            scroll = cursor_row + 1 - viewport_height;
        }
    }
    scroll = scroll.min(rows.len().saturating_sub(viewport_height));
    composer.body_scroll = scroll;
    if body_active && cursor_row >= scroll && cursor_row - scroll < viewport_height {
        let y = fields[6].y.saturating_add((cursor_row - scroll) as u16);
        frame.render_widget(
            Block::default().style(editor_current_line_style(&theme)),
            Rect::new(fields[6].x, y, fields[6].width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(Text::from(display))
            .style(theme.text())
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        fields[6],
    );
    if body_active
        && composer.mode == ComposeMode::Insert
        && fields[6].width > 4
        && composer.file_picker.is_none()
        && !composer.attachment_manager_open
        && !composer.advanced_headers_open
        && !composer.help_visible
        && !composer.confirm_send
        && !composer.exit_prompt
        && cursor_row >= scroll
        && cursor_row - scroll < viewport_height
    {
        let before = composer
            .body
            .chars()
            .skip(rows[cursor_row].start)
            .take(composer.cursor.saturating_sub(rows[cursor_row].start))
            .collect::<String>();
        let column = UnicodeWidthStr::width(before.as_str());
        let x = fields[6]
            .x
            .saturating_add(4)
            .saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
        let y = fields[6].y.saturating_add((cursor_row - scroll) as u16);
        if x < fields[6].right() {
            frame.set_cursor_position((x, y));
        }
    }
    // Mode and status are rendered in draw_status(), the application-wide
    // bottom bar. Compose no longer creates a second competing footer.
}

pub(super) fn editor_spans(
    value: &str,
    composer: &crate::app::Composer,
    active: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = composer.cursor.min(chars.len());
    let selection = active.then(|| composer.visual_range()).flatten();
    let mut spans = Vec::with_capacity(chars.len().saturating_add(1));
    let block = Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD);
    for (index, ch) in chars.iter().copied().enumerate() {
        let at_cursor = active && index == cursor;
        let selected = selection.is_some_and(|(start, end)| (start..end).contains(&index));
        spans.push(Span::styled(
            ch.to_string(),
            if at_cursor && composer.mode != ComposeMode::Insert {
                block
            } else if selected {
                theme.selected()
            } else if value.trim().is_empty() {
                theme.muted()
            } else {
                theme.text()
            },
        ));
    }
    if active && cursor == chars.len() && composer.mode != ComposeMode::Insert {
        spans.push(Span::styled(" ", block));
    }
    spans
}

pub(super) fn editor_text(
    value: &str,
    composer: &crate::app::Composer,
    active: bool,
    theme: &Theme,
) -> Text<'static> {
    let selection = active.then(|| composer.visual_range()).flatten();
    let cursor = composer.cursor.min(value.chars().count());
    // Quote classification is computed once per logical line; composer text
    // remains editable, including nested mailing-list reply history.
    let quote_lines = value
        .split('\n')
        .map(|line| {
            let (depth, prefix_bytes) = quote_prefix(line);
            (depth, prefix_bytes, is_reply_attribution(line))
        })
        .collect::<Vec<_>>();
    let mut logical_line = 0usize;
    let mut column = 0usize;
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut global = 0usize;
    let block = Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD);
    for ch in value.chars() {
        let at_cursor = active && global == cursor;
        if ch == '\n' {
            if at_cursor && composer.mode != ComposeMode::Insert {
                spans.push(Span::styled(" ", block));
            }
            lines.push(Line::from(std::mem::take(&mut spans)));
            global += 1;
            logical_line += 1;
            column = 0;
            continue;
        }
        let selected = selection.is_some_and(|(start, end)| (start..end).contains(&global));
        let (depth, quote_marker_len, attribution) = quote_lines
            .get(logical_line)
            .copied()
            .unwrap_or((0, 0, false));
        spans.push(Span::styled(
            ch.to_string(),
            if at_cursor && composer.mode != ComposeMode::Insert {
                block
            } else if selected {
                theme.selected()
            } else if depth > 0 && column < quote_marker_len {
                quoted_prefix_style(theme)
            } else if attribution {
                attribution_style(theme)
            } else if depth > 0 {
                quoted_body_style(depth, theme)
            } else {
                theme.text()
            },
        ));
        global += 1;
        column += 1;
    }
    if active && global == cursor && composer.mode != ComposeMode::Insert {
        spans.push(Span::styled(" ", block));
    }
    lines.push(Line::from(spans));
    Text::from(lines)
}
