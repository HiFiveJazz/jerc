//! Reader rendering.

use super::*;

/// In compact mode only the sender email address is hidden when a display
/// name is available. Mail with no sender display name must remain identifiable.
pub(super) fn sender_header_value(
    from_name: &str,
    from_address: &str,
    show_details: bool,
) -> String {
    if from_name.is_empty() {
        from_address.to_owned()
    } else if from_address.is_empty() || !show_details {
        from_name.to_owned()
    } else {
        format!("{from_name} <{from_address}>")
    }
}

/// Preserve all To/Cc/Bcc header rows in compact mode while hiding each
/// mailbox's address when it has a readable display name. Bare addresses
/// remain visible instead of turning a header into an empty label. Commas
/// inside quoted names or angle addresses are not recipient separators.
pub(super) fn compact_recipient_header(value: &str) -> String {
    let mut recipients = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut angle_depth = 0usize;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quoted {
            escaped = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        match ch {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            ',' if angle_depth == 0 => {
                recipients.push(compact_recipient(value[start..index].trim()));
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    recipients.push(compact_recipient(value[start..].trim()));
    recipients.join(", ")
}

pub(super) fn compact_recipient(value: &str) -> String {
    let Some(start) = value.rfind('<') else {
        return value.to_owned();
    };
    let Some(end) = value[start..].find('>').map(|end| start + end) else {
        return value.to_owned();
    };
    if !value[start + 1..end].contains('@') {
        return value.to_owned();
    }
    // Only discard the address when we have an actual name to show. Leave
    // e.g. <someone@example.com> and malformed input untouched.
    let name = value[..start].trim();
    if name.is_empty() {
        return value.to_owned();
    }
    let unquoted = name
        .strip_prefix('"')
        .and_then(|name| name.strip_suffix('"'));
    let name = unquoted.unwrap_or(name);
    let trailing = value[end + 1..].trim();
    format!("{name}{trailing}")
}

/// Only addresses and Message-ID collapse with `m`. From, To, Cc and Bcc
/// stay visible, as do all display names, subjects and dates.
pub(super) fn reader_header_lines(
    message: &crate::model::MessageSummary,
    show_details: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let from = sender_header_value(&message.from_name, &message.from_address, show_details);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("From: ", theme.accent()),
            Span::raw(from),
        ]),
        Line::from(vec![
            Span::styled("Date: ", theme.accent()),
            Span::raw(if message.timestamp > 0 {
                chrono::DateTime::<chrono::Utc>::from_timestamp(message.timestamp, 0)
                    .map(|date| {
                        date.with_timezone(&chrono::Local)
                            .format("%a, %b %-d, %Y at %-I:%M %p %Z")
                            .to_string()
                    })
                    .unwrap_or_else(|| "unknown".to_owned())
            } else {
                "unknown".to_owned()
            }),
        ]),
        Line::from(vec![
            Span::styled("To: ", theme.accent()),
            Span::raw(if message.to.is_empty() {
                "(not supplied)".to_owned()
            } else if show_details {
                message.to.clone()
            } else {
                compact_recipient_header(&message.to)
            }),
        ]),
        Line::from(vec![
            Span::styled("Subject: ", theme.accent()),
            Span::raw(message.subject.clone()),
        ]),
    ];
    if !message.cc.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Cc: ", theme.accent()),
            Span::raw(if show_details {
                message.cc.clone()
            } else {
                compact_recipient_header(&message.cc)
            }),
        ]));
    }
    if !message.bcc.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Bcc: ", theme.accent()),
            Span::raw(if show_details {
                message.bcc.clone()
            } else {
                compact_recipient_header(&message.bcc)
            }),
        ]));
    }
    if show_details && let Some(message_id) = &message.message_id {
        lines.push(Line::from(vec![
            Span::styled("Message-ID: ", theme.accent()),
            Span::raw(format!("<{message_id}>")),
        ]));
    }
    lines
}

pub(super) fn preview_split_border_style(theme: &Theme, resizing: bool) -> Style {
    if resizing {
        theme.accent()
    } else {
        theme.border()
    }
}

pub(super) fn draw_message(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let theme = app.theme().clone();
    let content_area = if !app.message_open {
        let block = Block::default()
            .borders(Borders::LEFT)
            .border_style(preview_split_border_style(&theme, app.preview_resizing));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };

    if app
        .account()
        .and_then(|account| account.selected_message())
        .is_none()
    {
        frame.render_widget(
            Paragraph::new("No message selected").style(theme.muted()),
            content_area,
        );
        app.set_reader_layout(0, usize::from(content_area.height.max(1)), 0, None);
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
            reader_word_at(
                body.display_text(showing_html),
                reader_cursor,
                reader_column,
            )
        })
        .flatten();

    let (lines, body_start, body_line_count, image_start) = {
        let account = app.account().expect("account exists");
        let message = account.selected_message().expect("message checked above");

        let mut lines = reader_header_lines(message, app.show_header_details, &theme);
        if let Some(body) = account.selected_body() {
            if body.attachment_count > 0 {
                let preview = body
                    .attachment_names
                    .iter()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("  ·  ");
                let overflow = body.attachment_names.len().saturating_sub(2);
                let label = if preview.is_empty() {
                    format!("{} attachment(s)", body.attachment_count)
                } else if overflow > 0 {
                    format!("{preview}  +{overflow} more")
                } else {
                    preview
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        "⧉  ",
                        Style::default()
                            .fg(theme.warning)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(label, theme.text()),
                    Span::styled("   Ctrl-a: view", theme.muted()),
                ]));
            }
        } else if message.has_attachment_hint {
            let names = crate::mail::visible_attachment_hints(&message.attachment_names_hint)
                .into_iter()
                .take(2)
                .collect::<Vec<_>>()
                .join("  ·  ");
            lines.push(Line::from(vec![
                Span::styled(
                    "⧉  ",
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if names.is_empty() {
                        "Attachments (Ctrl-a: browse)".to_owned()
                    } else {
                        names
                    },
                    theme.text(),
                ),
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
                    let rendered = html_lines
                        .iter()
                        .map(|line| {
                            line.spans
                                .iter()
                                .map(|span| span.text.as_str())
                                .collect::<String>()
                        })
                        .collect::<Vec<_>>();
                    let texts = rendered.iter().map(String::as_str).collect::<Vec<_>>();
                    let diff_lines = classify_patch_lines(&texts);
                    lines.extend(html_lines.iter().enumerate().map(|(line_index, line)| {
                        rich_html_line(
                            line,
                            line_index,
                            body_line_count,
                            body_width,
                            ReaderView {
                                cursor: reader_cursor,
                                column: reader_column,
                                visual: reader_visual,
                                active: reader_active,
                                theme: &theme,
                            },
                            diff_lines[line_index],
                            reader_illuminate.as_deref(),
                        )
                    }));
                } else {
                    let rendered = body.display_text(true).lines().collect::<Vec<_>>();
                    body_line_count = rendered.len();
                    let diff_lines = classify_patch_lines(&rendered);
                    lines.extend(rendered.into_iter().enumerate().map(|(line_index, line)| {
                        plain_reader_line(
                            line,
                            line_index,
                            body_line_count,
                            body_width,
                            ReaderView {
                                cursor: reader_cursor,
                                column: reader_column,
                                visual: reader_visual,
                                active: reader_active,
                                theme: &theme,
                            },
                            diff_lines[line_index],
                            reader_illuminate.as_deref(),
                        )
                    }));
                }
            } else {
                let rendered = body.display_text(false).lines().collect::<Vec<_>>();
                body_line_count = rendered.len();
                let diff_lines = classify_patch_lines(&rendered);
                lines.extend(rendered.into_iter().enumerate().map(|(line_index, line)| {
                    plain_reader_line(
                        line,
                        line_index,
                        body_line_count,
                        body_width,
                        ReaderView {
                            cursor: reader_cursor,
                            column: reader_column,
                            visual: reader_visual,
                            active: reader_active,
                            theme: &theme,
                        },
                        diff_lines[line_index],
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
    // Cursor motions address logical message lines, but Paragraph wraps those
    // lines into *terminal rows*. Using body_start + reader_cursor for the
    // scroll position loses all preceding wraps and eventually leaves the
    // cursor below the viewport. Measure the row with Paragraph's own layout.
    let visual_cursor_row =
        if reader_active && body_line_count > 0 && app.reader_needs_scroll_follow() {
            reader_visual_cursor_row(
                &lines,
                body_start,
                reader_cursor.min(body_line_count - 1),
                content_area.width,
            )
        } else {
            None
        };
    let paragraph = Paragraph::new(Text::from(lines))
        .style(theme.text())
        .wrap(Wrap { trim: false });
    let visual_lines = paragraph.line_count(content_area.width.max(1));
    let viewport_height = usize::from(content_area.height.max(1));
    let max_scroll =
        u16::try_from(visual_lines.saturating_sub(viewport_height)).unwrap_or(u16::MAX);
    app.set_reader_layout(
        body_line_count,
        viewport_height,
        max_scroll,
        visual_cursor_row,
    );
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

/// Physical terminal row containing the Vim cursor, including wrapped headers,
/// images, long body lines and the portion of the active line before the cursor.
/// Use exactly the same word-wrap settings as the reader itself.
pub(super) fn reader_visual_cursor_row(
    lines: &[Line<'_>],
    body_start: usize,
    cursor_line: usize,
    width: u16,
) -> Option<usize> {
    let index = body_start.checked_add(cursor_line)?;
    let current = lines.get(index)?;
    let width = width.max(1);
    let preceding_rows = if index == 0 {
        0
    } else {
        Paragraph::new(Text::from(lines[..index].to_vec()))
            .wrap(Wrap { trim: false })
            .line_count(width)
    };

    // The rendered block cursor is a white-on-black cell. Measuring the prefix
    // up to that cell follows the cursor even when a single logical line spans
    // several screen rows. When no cursor cell is present, use its first row.
    let mut prefix = Vec::new();
    for span in &current.spans {
        if span.style == reader_cursor_style() {
            break;
        }
        prefix.push(span.clone());
    }
    let within_line = if prefix.is_empty() {
        0
    } else {
        Paragraph::new(Line::from(prefix))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .saturating_sub(1)
    };
    Some(preceding_rows.saturating_add(within_line))
}

/// Only color *real patch hunks*. An email quote containing `-EIO`, a
/// Markdown `---` divider, or a plain `+text` line is not a Git diff.
/// Classify the entire body at once so hunk ranges and quoted patches are
/// recognized without falsely coloring nearby prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PatchLineKind {
    Meta,
    Hunk,
    Add,
    Remove,
}

pub(super) fn strip_mail_quotes(mut text: &str) -> &str {
    while let Some(after) = text.strip_prefix('>') {
        text = after.strip_prefix(' ').unwrap_or(after);
    }
    text
}

pub(super) fn patch_hunk_counts(line: &str) -> Option<(usize, usize)> {
    let text = line.strip_prefix("@@ -")?;
    let (old, tail) = text.split_once(" +")?;
    let (new, _) = tail.split_once(" @@")?;
    fn parse_range(spec: &str) -> Option<usize> {
        let (start, count) = spec.split_once(',').unwrap_or((spec, "1"));
        start.parse::<usize>().ok()?;
        count.parse::<usize>().ok()
    }
    Some((parse_range(old)?, parse_range(new)?))
}

pub(super) fn is_patch_source(text: &str) -> bool {
    text.starts_with("--- a/") || text == "--- /dev/null"
}
pub(super) fn is_patch_target(text: &str) -> bool {
    text.starts_with("+++ b/") || text == "+++ /dev/null"
}

pub(super) fn classify_patch_lines(lines: &[&str]) -> Vec<Option<PatchLineKind>> {
    let mut result = Vec::with_capacity(lines.len());
    let mut in_patch = false;
    let mut in_hunk = false;
    let (mut old_left, mut new_left) = (0usize, 0usize);
    for (index, line) in lines.iter().enumerate() {
        let text = strip_mail_quotes(line);
        let next_is_target = lines
            .get(index + 1)
            .is_some_and(|next| is_patch_target(strip_mail_quotes(next)));
        let kind = if text.starts_with("diff --git ") || is_patch_source(text) && next_is_target {
            in_patch = true;
            in_hunk = false;
            Some(PatchLineKind::Meta)
        } else if !in_patch {
            None
        } else if text.starts_with("index ")
            || is_patch_source(text)
            || is_patch_target(text)
            || text.starts_with("new file mode ")
            || text.starts_with("deleted file mode ")
            || text.starts_with("old mode ")
            || text.starts_with("new mode ")
            || text.starts_with("similarity index ")
            || text.starts_with("rename from ")
            || text.starts_with("rename to ")
        {
            in_hunk = false;
            Some(PatchLineKind::Meta)
        } else if let Some((old, new)) = patch_hunk_counts(text) {
            old_left = old;
            new_left = new;
            in_hunk = old > 0 || new > 0;
            Some(PatchLineKind::Hunk)
        } else if in_hunk && text.starts_with('!') {
            // Combined diffs and context diffs have different formats. They
            // need explicit support, not arbitrary +/- colorization.
            in_hunk = false;
            None
        } else if in_hunk {
            let kind = if text.starts_with('+') && new_left > 0 {
                new_left -= 1;
                Some(PatchLineKind::Add)
            } else if text.starts_with('-') && old_left > 0 {
                old_left -= 1;
                Some(PatchLineKind::Remove)
            } else if text.starts_with(' ') && old_left > 0 && new_left > 0 {
                old_left -= 1;
                new_left -= 1;
                None
            } else if text.starts_with("\\ No newline at end of file") {
                Some(PatchLineKind::Meta)
            } else {
                // A reply is allowed to contain prose immediately after a
                // quoted patch. Stop highlighting on malformed hunk content.
                in_hunk = false;
                None
            };
            if old_left == 0 && new_left == 0 {
                in_hunk = false;
            }
            kind
        } else {
            if !text.trim().is_empty() && !text.starts_with("\\ No newline") {
                in_patch = false;
            }
            None
        };
        result.push(kind);
    }
    result
}

/// The active diff row uses the same neutral gray as every other Vim row.
/// Normal diff rows keep their red/green backgrounds, while the selected row
/// keeps the diff foreground and uses the theme's current-line background.
pub(super) fn reader_active_diff_bg(theme: &Theme, _diff_bg: Color) -> Color {
    theme.popup_bg
}

pub(super) fn diff_line_style(kind: PatchLineKind, theme: &Theme) -> Style {
    // Blend the addition/removal background with the active theme's surface.
    // Preserves readability on light and dark themes, including TokyoNight.
    let bg = |fg: Color| match (theme.popup_bg, fg) {
        (Color::Rgb(sr, sg, sb), Color::Rgb(fr, fg, fb)) => Color::Rgb(
            mix_channel(sr, fr, 23),
            mix_channel(sg, fg, 23),
            mix_channel(sb, fb, 23),
        ),
        _ => theme.popup_bg,
    };
    match kind {
        PatchLineKind::Meta => theme.accent(),
        PatchLineKind::Hunk => Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
        PatchLineKind::Add => Style::default().fg(theme.unread).bg(bg(theme.unread)),
        PatchLineKind::Remove => Style::default().fg(theme.error).bg(bg(theme.error)),
    }
}

pub(super) fn plain_reader_line(
    text: &str,
    line_index: usize,
    line_count: usize,
    content_width: usize,
    view: ReaderView<'_>,
    diff_kind: Option<PatchLineKind>,
    illuminate_word: Option<&str>,
) -> Line<'static> {
    let ReaderView {
        cursor: reader_cursor,
        column: reader_column,
        visual: reader_visual,
        active: reader_active,
        theme,
    } = view;
    let diff = diff_kind.map(|kind| diff_line_style(kind, theme));
    let (quote_depth, quote_prefix_end) = quote_prefix(text);
    let base = diff.unwrap_or_else(|| {
        if quote_depth > 0 {
            quoted_body_style(quote_depth, theme)
        } else if is_reply_attribution(text) {
            attribution_style(theme)
        } else {
            theme.text()
        }
    });
    let illuminate_ranges = illuminate_word
        .map(|word| word_match_ranges(text, word))
        .unwrap_or_default();
    let mut spans = reader_gutter(line_index, line_count, reader_cursor, reader_active, theme);
    let line_len = text.chars().count();
    let view = ReaderView {
        cursor: reader_cursor,
        column: reader_column,
        visual: reader_visual,
        active: reader_active,
        theme,
    };
    if quote_depth > 0 && diff.is_none() {
        // Differentiate the `>` markers from the quoted body without losing
        // cursor, visual selection, or illuminated-word highlighting.
        let prefix = &text[..quote_prefix_end];
        spans.extend(reader_fragments(
            prefix,
            quoted_prefix_style(theme),
            0,
            line_len,
            line_index,
            view,
            &illuminate_ranges,
        ));
        spans.extend(reader_fragments(
            &text[quote_prefix_end..],
            base,
            prefix.chars().count(),
            line_len,
            line_index,
            view,
            &illuminate_ranges,
        ));
    } else {
        spans.extend(reader_fragments(
            text,
            base,
            0,
            line_len,
            line_index,
            view,
            &illuminate_ranges,
        ));
    }
    pad_reader_line(
        &mut spans,
        text.chars().count(),
        content_width,
        line_index,
        ReaderView {
            cursor: reader_cursor,
            column: reader_column,
            visual: reader_visual,
            active: reader_active,
            theme,
        },
        diff.and_then(|style| style.bg),
    );
    Line::from(spans)
}

pub(super) fn rich_html_line(
    line: &HtmlLine,
    line_index: usize,
    line_count: usize,
    content_width: usize,
    view: ReaderView<'_>,
    diff_kind: Option<PatchLineKind>,
    illuminate_word: Option<&str>,
) -> Line<'static> {
    let ReaderView {
        cursor: reader_cursor,
        column: reader_column,
        visual: reader_visual,
        active: reader_active,
        theme,
    } = view;
    let mut spans = reader_gutter(line_index, line_count, reader_cursor, reader_active, theme);
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
    let (quote_depth, _) = quote_prefix(&line_text);
    let attribution = is_reply_attribution(&line_text);
    let diff = diff_kind.map(|kind| diff_line_style(kind, theme));
    let mut column = 0usize;
    for span in &line.spans {
        let mut style = rich_html_style(&span.style, &span.text, theme);
        if let Some(diff) = diff {
            style = style.fg(diff.fg.unwrap_or(theme.foreground));
            if let Some(bg) = diff.bg {
                style = style.bg(bg);
            }
        } else if quote_depth > 0 {
            style = style.fg(quoted_body_style(quote_depth, theme).fg.unwrap_or(theme.muted));
        } else if attribution {
            style = attribution_style(theme);
        }
        spans.extend(reader_fragments(
            &span.text,
            style,
            column,
            line_len,
            line_index,
            ReaderView {
                cursor: reader_cursor,
                column: reader_column,
                visual: reader_visual,
                active: reader_active,
                theme,
            },
            &illuminate_ranges,
        ));
        column = column.saturating_add(span.text.chars().count());
    }
    pad_reader_line(
        &mut spans,
        column,
        content_width,
        line_index,
        ReaderView {
            cursor: reader_cursor,
            column: reader_column,
            visual: reader_visual,
            active: reader_active,
            theme,
        },
        diff.and_then(|style| style.bg),
    );
    Line::from(spans)
}

pub(super) fn reader_gutter(
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

pub(super) fn reader_fragments(
    text: &str,
    style: Style,
    global_start: usize,
    line_len: usize,
    line_index: usize,
    view: ReaderView<'_>,
    illuminate_ranges: &[(usize, usize)],
) -> Vec<Span<'static>> {
    let ReaderView {
        cursor: reader_cursor,
        column: reader_column,
        visual: reader_visual,
        active: reader_active,
        theme,
    } = view;
    if text.is_empty() {
        return Vec::new();
    }
    let len = text.chars().count();
    let global_end = global_start.saturating_add(len);
    let mut boundaries = vec![global_start, global_end];

    let selected_columns =
        reader_visual.and_then(|selection| selection.columns_for_line(line_index, line_len));
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
            // Diff semantics remain tinted, but selection gets a stronger
            // background than ordinary additions/removals.
            let bg = fragment_style
                .bg
                .map(|diff_bg| reader_active_diff_bg(theme, diff_bg))
                .unwrap_or(theme.popup_bg);
            fragment_style = fragment_style.bg(bg);
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

pub(super) fn reader_word_at(text: &str, line_index: usize, column: usize) -> Option<String> {
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

pub(super) fn word_match_ranges(text: &str, word: &str) -> Vec<(usize, usize)> {
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

pub(super) fn is_reader_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

pub(super) fn reader_illuminate_bg(theme: &Theme) -> Color {
    match (theme.popup_bg, theme.border) {
        (Color::Rgb(sr, sg, sb), Color::Rgb(br, bg, bb)) => Color::Rgb(
            mix_channel(sr, br, 82),
            mix_channel(sg, bg, 82),
            mix_channel(sb, bb, 82),
        ),
        _ => Color::Rgb(59, 66, 97),
    }
}

pub(super) fn mix_channel(from: u8, to: u8, toward_to_percent: u16) -> u8 {
    let toward_to_percent = toward_to_percent.min(100);
    let toward_from_percent = 100 - toward_to_percent;
    ((u16::from(from) * toward_from_percent + u16::from(to) * toward_to_percent + 50) / 100) as u8
}

pub(super) fn reader_cursor_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn pad_reader_line(
    spans: &mut Vec<Span<'static>>,
    content_len: usize,
    content_width: usize,
    line_index: usize,
    view: ReaderView<'_>,
    diff_background: Option<Color>,
) {
    let ReaderView {
        cursor: reader_cursor,
        column: reader_column,
        visual: reader_visual,
        active: reader_active,
        theme,
    } = view;
    if !reader_active && diff_background.is_none() {
        return;
    }
    let visual_line_selected = reader_visual.is_some_and(|selection| {
        selection.start.line <= line_index && line_index <= selection.end.line
    });
    let linewise_visual = reader_visual
        .is_some_and(|selection| selection.start.column == 0 && selection.end.column == usize::MAX);
    if (line_index != reader_cursor || !reader_active)
        && !visual_line_selected
        && diff_background.is_none()
    {
        return;
    }
    let mut style = theme.text();
    if let Some(bg) = diff_background {
        style = style.bg(bg);
    }
    if reader_active && line_index == reader_cursor {
        style = style.bg(diff_background
            .map(|bg| reader_active_diff_bg(theme, bg))
            .unwrap_or(theme.popup_bg));
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

pub(super) fn char_slice(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

pub(super) fn rich_html_style(
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
        rendered = rendered.fg(theme.unread).add_modifier(Modifier::BOLD);
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

pub(super) fn is_box_drawing_span(text: &str) -> bool {
    let mut saw_box = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if matches!(
            ch,
            '─' | '│'
                | '┌'
                | '┐'
                | '└'
                | '┘'
                | '├'
                | '┤'
                | '┬'
                | '┴'
                | '┼'
                | '╭'
                | '╮'
                | '╰'
                | '╯'
                | '═'
                | '║'
                | '╔'
                | '╗'
                | '╚'
                | '╝'
                | '╠'
                | '╣'
                | '╦'
                | '╩'
                | '╬'
        ) {
            saw_box = true;
        } else {
            return false;
        }
    }
    saw_box
}
