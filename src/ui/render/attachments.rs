//! Attachments rendering.

use super::*;

/// Received attachment browser: Yazi-like relative numbers and marks on the
/// left, automatic inline preview on the right, mouse/keyboard actions below.
pub(super) fn draw_attachments(frame: &mut Frame<'_>, app: &mut App) {
    let theme = app.theme().clone();
    let (area, list_area, preview_area, _) = crate::ui::attachment_regions(frame.area());
    frame.render_widget(Clear, area);
    let count = app.attachment_files.len();
    let mark_count = app.attachment_marked.len();
    let title = if mark_count == 0 { format!(" Attachments · {count} ") }
        else { format!(" Attachments · {count} · {mark_count} marked ") };
    frame.render_widget(Block::default().title(title).borders(Borders::ALL).border_type(theme.border_type)
        .border_style(theme.accent()), area);

    // Left pane: header row + viewport-centered rows, with relative line
    // numbers (absolute number on the cursor line) and persistent marks.
    frame.render_widget(Block::default().borders(Borders::RIGHT)
        .border_style(theme.border()), list_area);
    frame.render_widget(Paragraph::new("  Files")
        .style(theme.accent()), Rect::new(list_area.x, list_area.y, list_area.width, list_area.height.min(1)));
    let rows = usize::from(list_area.height.saturating_sub(1));
    let selected = app.attachment_selection.min(count.saturating_sub(1));
    let start = selected.saturating_sub(rows / 2).min(count.saturating_sub(rows));
    let end = (start + rows).min(count);
    let digits = count.max(1).to_string().len();
    for index in start..end {
        let file = &app.attachment_files[index];
        let active = index == selected;
        let marked = app.attachment_marked.contains(&index);
        let visual = app.attachment_visual_anchor.is_some_and(|anchor|
            (anchor.min(selected)..=anchor.max(selected)).contains(&index));
        let number = if active { index + 1 } else { index.abs_diff(selected) };
        let selected = visual || marked;
        let prefix = format!(" {:>digits$} ", number);
        let mark = if selected { "✓" }
            else if file.content_type.starts_with("image/") { "▧" }
            else if file.content_type == "application/pdf" { "▤" } else { "▫" };
        let available = usize::from(list_area.width.saturating_sub(1))
            .saturating_sub(UnicodeWidthStr::width(prefix.as_str()) + 3);
        let name = truncate_display_width(&attachment_display_name(&file.filename), available);
        let style = browser_row_style(&theme, app.visual_selection_style,
            visual, marked, active, false);
        let rect = Rect::new(list_area.x, list_area.y.saturating_add(1 + (index - start) as u16),
            list_area.width.saturating_sub(1), 1);
        frame.render_widget(Paragraph::new(Line::from(vec![
            browser_gutter(&theme, visual, marked),
            Span::raw(prefix),
            Span::styled(mark, selection_check_style(&theme,
                app.visual_selection_style, visual, marked, active)),
            Span::raw(format!(" {name}")),
        ])).style(style), rect);
    }
    if start == end {
        frame.render_widget(Paragraph::new(if app.attachment_pending_loading() {
            " Loading attachments…"
        } else { " No attachments" }).style(theme.muted()), Rect::new(list_area.x,
            list_area.y.saturating_add(1), list_area.width.saturating_sub(1), 1));
    }

    // Right pane: preview the selected image/text automatically. Image decode
    // happens at most once per selection and viewport size, not every frame.
    let preview_w = preview_area.width.saturating_sub(3);
    let preview_h = preview_area.height.saturating_sub(6);
    // Kitty graphics are terminal-side resources. Do not draw the image
    // underneath a help popup whose Clear can erase it without invalidating
    // ratatui-image's cached protocol. Rebuild when help closes instead.
    if !app.help_visible { app.ensure_attachment_preview(preview_w, preview_h); }
    let pleft = preview_area.x.saturating_add(2);
    let pwidth = preview_area.width.saturating_sub(3);
    if let Some(file) = app.attachment_files.get(selected) {
        frame.render_widget(Paragraph::new(truncate_display_width(&attachment_display_name(&file.filename), usize::from(pwidth)))
            .style(theme.accent()), Rect::new(pleft, preview_area.y, pwidth, 1));
        frame.render_widget(Paragraph::new(format!("{}  ·  {}", file.content_type, attachment_human_size(file.data.len())))
            .style(theme.muted()), Rect::new(pleft, preview_area.y.saturating_add(1), pwidth, 1));
        let content = Rect::new(pleft, preview_area.y.saturating_add(3), pwidth,
            preview_area.height.saturating_sub(4));
        if let Some(protocol) = (!app.help_visible).then(|| app.attachment_preview()).flatten() {
            let size = protocol.size();
            if size.width <= content.width && size.height <= content.height {
                let rect = Rect::new(content.x + content.width.saturating_sub(size.width)/2,
                    content.y, size.width, size.height);
                frame.render_widget(TerminalImage::new(protocol), rect);
            }
        } else if let Some(error) = app.attachment_preview_error() {
            frame.render_widget(Paragraph::new(error.to_string()).style(theme.muted())
                .wrap(Wrap { trim: false }), content);
        } else if file.content_type == "application/pdf"
            || file.filename.rsplit('.').next().is_some_and(|ext| ext.eq_ignore_ascii_case("pdf")) {
            let message = if app.attachment_preview_decoded() {
                "PDF thumbnail ready, but this terminal cannot display it.\nPress o to open the PDF."
            } else { "Rendering PDF page 1…\n\nPreview runs off the UI thread using Poppler (pdftoppm)." };
            frame.render_widget(Paragraph::new(message)
                .style(theme.muted()).wrap(Wrap { trim: false }), content);
        } else if file.content_type.starts_with("image/") {
            let message = if app.attachment_preview_decoded() {
                "Image ready, but this terminal cannot display it.\nPress o to open."
            } else { "Preparing image preview…" };
            frame.render_widget(Paragraph::new(message)
                .style(theme.muted()).wrap(Wrap { trim: false }), content);
        } else if let Some(preview) = attachment_text_preview(file) {
            frame.render_widget(Paragraph::new(preview).style(theme.text())
                .wrap(Wrap { trim: false }), content);
        } else {
            frame.render_widget(Paragraph::new("Binary attachment\n\nPress o to open or s to save.\nNo file is opened automatically.")
                .style(theme.muted()).wrap(Wrap { trim: false }), content);
        }
    } else {
        frame.render_widget(Paragraph::new("Select an attachment to preview it")
            .style(theme.muted()), preview_area);
    }

}

/// Save received attachments into a browsed directory (same Yazi-like
/// parent/current/preview layout as the compose attachment picker). Users
/// choose the destination instead of typing arbitrary paths.
pub(super) fn draw_attachment_destination_picker(frame: &mut Frame<'_>, app: &App) {
    let Some(picker) = app.attachment_destination_picker.as_ref() else { return; };
    let (area, parent_area, files_area, preview_area, footer) =
        crate::ui::attachment_destination_regions(frame.area());
    let theme = app.theme();
    frame.render_widget(Clear, area);
    let count = if app.attachment_marked.is_empty() { usize::from(!app.attachment_files.is_empty()) }
        else { app.attachment_marked.len() };
    let block = Block::default().title(format!(" Save {count} attachment(s) · choose destination "))
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 12 || inner.height < 5 { return; }
    frame.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(" 󰉋 ", theme.accent()),
        Span::styled(truncate_display_width(&picker.directory.display().to_string(),
            usize::from(inner.width.saturating_sub(4))), theme.text().add_modifier(Modifier::BOLD)),
    ])), Rect::new(inner.x, inner.y, inner.width, 1));
    if parent_area.width > 0 && parent_area.height > 0 {
        frame.render_widget(Block::default().borders(Borders::RIGHT).border_style(theme.border()), parent_area);
        compose_picker_rows(frame, Rect::new(parent_area.x, parent_area.y,
            parent_area.width.saturating_sub(1), parent_area.height),
            &picker.parent_entries, Some(picker.parent_selected), false, &[], None,
            app.visual_selection_style, theme);
    }
    if files_area.width > 0 && files_area.height > 0 {
        frame.render_widget(Block::default().borders(Borders::RIGHT).border_style(theme.border()), files_area);
        compose_picker_rows(frame, Rect::new(files_area.x, files_area.y,
            files_area.width.saturating_sub(1), files_area.height),
            &picker.entries, Some(picker.selected), true, &[], None,
            app.visual_selection_style, theme);
    }
    if preview_area.width > 2 && preview_area.height > 0 {
        let preview_rect = Rect::new(preview_area.x.saturating_add(1), preview_area.y,
            preview_area.width.saturating_sub(2), preview_area.height);
        let lines: Vec<Line<'static>> = match &picker.preview {
            crate::app::ComposeFilePreview::Directory(entries) => entries.iter()
                .take(usize::from(preview_rect.height)).map(|entry| Line::styled(
                    format!("{}{}", if entry.is_directory { "󰉋 " } else { "  " }, entry.name),
                    if entry.is_directory { theme.accent() } else { theme.text() })).collect(),
            crate::app::ComposeFilePreview::Binary(bytes) => vec![
                Line::styled("File (choose a folder to enter)", theme.muted()),
                Line::styled(format!("{bytes} bytes"), theme.text())],
            crate::app::ComposeFilePreview::Empty => vec![Line::styled("Empty directory", theme.muted())],
            crate::app::ComposeFilePreview::Error(error) => vec![Line::styled(error.clone(), theme.error())],
            _ => vec![Line::styled("Select a directory", theme.muted())],
        };
        frame.render_widget(Paragraph::new(lines), preview_rect);
    }
    if let Some(name) = &app.attachment_new_folder {
        frame.render_widget(Paragraph::new(format!(" New folder name: {name}▏  Enter create · Esc cancel"))
            .style(theme.accent()), footer);
    } else {
        frame.render_widget(Paragraph::new(" [s] Save here  [Enter/l] Open folder  [h] Parent  [n] New folder  [Esc] Cancel")
            .style(theme.accent()), footer);
    }
}

pub(super) fn attachment_display_name(name: &str) -> String {
    name.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).collect()
}

pub(super) fn attachment_human_size(bytes: usize) -> String {
    if bytes < 1024 { format!("{bytes} B") }
    else if bytes < 1024 * 1024 { format!("{:.1} KiB", bytes as f64 / 1024.0) }
    else { format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)) }
}

pub(super) fn attachment_text_preview(file: &crate::model::OutgoingAttachment) -> Option<String> {
    let name = file.filename.to_ascii_lowercase();
    let textual = file.content_type.starts_with("text/")
        || file.content_type.contains("json")
        || file.content_type.contains("xml")
        || [".txt", ".csv", ".html", ".md", ".rs", ".py", ".toml", ".ini", ".cfg", ".yaml", ".yml", ".log", ".diff", ".patch", ".eml"]
            .iter().any(|suffix| name.ends_with(suffix));
    if !textual { return None; }
    let sample = &file.data[..file.data.len().min(32 * 1024)];
    if sample.contains(&0) { return None; }
    let preview = String::from_utf8_lossy(sample);
    let mut text = preview.lines().take(200).map(|line| {
        line.chars().map(|ch| if ch.is_control() && ch != '\t' { ' ' } else { ch })
            .collect::<String>()
    }).collect::<Vec<_>>().join("\n");
    if file.data.len() > sample.len() { text.push_str("\n… truncated to 32 KiB"); }
    Some(text)
}
