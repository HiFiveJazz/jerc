//! Outgoing email review layout and rendering.

use super::*;
use crate::app::SendPreviewMode;

/// Wrap a styled logical line without discarding spaces or losing its MIME
/// renderer's colors. Use the same word/Unicode boundaries as the editor.
fn wrap_review_line(line: Line<'static>, width: usize, output: &mut Vec<Line<'static>>) {
    let plain = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let rows = crate::app::compose_body_rows(&plain, width);
    let line_style = line.style;
    let mut chars = line.spans.iter().flat_map(|span| {
        span.content
            .chars()
            .map(move |ch| (ch, line_style.patch(span.style)))
    });
    let mut position = 0;
    for row in rows {
        for _ in position..row.start {
            let _ = chars.next();
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (ch, style) in chars.by_ref().take(row.end.saturating_sub(row.start)) {
            if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
                last.content.to_mut().push(ch);
            } else {
                spans.push(Span::styled(ch.to_string(), style));
            }
        }
        position = row.end;
        output.push(Line::from(spans).style(line_style));
    }
}

fn build_send_review_headers(
    composer: &crate::app::Composer,
    from: &str,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for (label, value) in [
        ("From", from),
        ("To", composer.to.as_str()),
        ("Cc", composer.cc.as_str()),
        ("Bcc", composer.bcc.as_str()),
        ("Subject", composer.subject.as_str()),
    ] {
        let value = if value.trim().is_empty() {
            "—"
        } else {
            value
        };
        wrap_review_line(
            Line::from(vec![
                Span::styled(format!("{label:<8} "), theme.muted()),
                Span::styled(value.to_owned(), theme.accent()),
            ]),
            width,
            &mut rows,
        );
    }
    let mut filenames = compose_attachment_names(composer);
    filenames.extend(
        composer
            .send_preview
            .document
            .forwarded_attachments
            .iter()
            .filter(|name| !composer.draft_attachment_names.contains(name))
            .cloned(),
    );
    let files = if filenames.is_empty() {
        "none".to_owned()
    } else {
        filenames.join(" · ")
    };
    wrap_review_line(
        Line::styled(
            format!("Attachments ({})  {files}", filenames.len()),
            if filenames.is_empty() {
                theme.muted()
            } else {
                theme.accent()
            },
        ),
        width,
        &mut rows,
    );
    rows
}

fn append_send_html_rows(source: &str, theme: &Theme, width: usize, rows: &mut Vec<Line<'static>>) {
    let (_, lines, _) = crate::mail::render_html_for_terminal(source, width);
    for line in lines {
        let spans = line
            .spans
            .into_iter()
            .map(|span| {
                let style = rich_html_style(&span.style, &span.text, theme);
                Span::styled(span.text, style)
            })
            .collect::<Vec<_>>();
        wrap_review_line(Line::from(spans), width, rows);
    }
}

fn build_send_review_rows(
    composer: &crate::app::Composer,
    theme: &Theme,
    width: usize,
    quoted_diff_highlighting: bool,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    if composer.send_preview.mode == SendPreviewMode::Html
        && let Some((before, attribution, source, after)) = composer.reply_html_review_parts()
    {
        // Render the actual received HTML, not an HTML conversion of the
        // flattened plain quotation. Keep the editable reply around it.
        for line in before.lines() {
            wrap_review_line(
                Line::raw(line.replace('\t', crate::app::COMPOSE_TAB_SPACES)),
                width,
                &mut rows,
            );
        }
        wrap_review_line(
            Line::styled(attribution.to_owned(), attribution_style(theme)),
            width,
            &mut rows,
        );
        append_send_html_rows(source, theme, width, &mut rows);
        for line in after.lines() {
            wrap_review_line(
                Line::raw(line.replace('\t', crate::app::COMPOSE_TAB_SPACES)),
                width,
                &mut rows,
            );
        }
    } else if composer.send_preview.mode == SendPreviewMode::Html {
        // Only generate a plain-to-HTML projection while building the HTML
        // layout. Ordinary drafts never retain a second full body string.
        let generated;
        let source = if let Some(source) = composer.send_preview.document.html_source.as_deref() {
            source
        } else {
            generated = composer.plain_review_html();
            &generated
        };
        append_send_html_rows(source, theme, width, &mut rows);
    } else {
        let lines = composer.plain_review_lines();
        let texts: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect();
        let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let diffs = classify_patch_lines_with_quotes(&text_refs, quoted_diff_highlighting);
        // Use the ordinary rich reader's quote, attribution, URL and diff
        // styles. Disable its cursor/gutter here: the review adds those after
        // wrapping, so both its navigation and current line remain accurate.
        let view = ReaderView {
            cursor: 0,
            column: 0,
            visual: None,
            active: false,
            quoted_diff_highlighting,
            theme,
        };
        for (index, line) in lines.iter().enumerate() {
            let styled = rich_html_line(line, index, lines.len(), 0, view, diffs[index], None);
            wrap_review_line(styled, width, &mut rows);
        }
    }
    // An empty HTML alternative still has a readable plain-text fallback.
    if rows.is_empty() {
        for line in composer.outgoing_plain_body().split('\n') {
            wrap_review_line(
                Line::raw(
                    line.trim_end_matches('\r')
                        .replace('\t', crate::app::COMPOSE_TAB_SPACES),
                ),
                width,
                &mut rows,
            );
        }
    }
    if rows.is_empty() {
        rows.push(Line::raw(""));
    }
    rows
}

// Headers stay pinned; only the body participates in reader navigation.
pub(super) fn draw_send_confirmation(frame: &mut Frame<'_>, app: &mut App) {
    let theme = app.theme().clone();
    let quoted_diff_highlighting = app.quoted_diff_highlighting;
    let from = app
        .account()
        .map(|account| account.from.clone())
        .unwrap_or_default();
    let Some(composer) = app
        .composer
        .as_mut()
        .filter(|composer| composer.confirm_send)
    else {
        return;
    };
    let title = match composer.send_preview.mode {
        SendPreviewMode::Html => " Review Outgoing Email — HTML ",
        SendPreviewMode::Plain => " Review Outgoing Email — Plain ",
    };
    let mut area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(100),
        88,
    );
    if area.height < 5 || area.width < 8 {
        composer.send_preview.layout.height = 0;
        popup_inner(frame, &theme, area, title, theme.accent());
        return;
    }
    let inner_width = area.width.saturating_sub(2);
    if composer.send_preview.layout.width != inner_width
        || composer.send_preview.layout.rows.is_empty()
    {
        composer.clear_preview_visual();
        composer.send_preview.layout.headers =
            build_send_review_headers(composer, &from, &theme, usize::from(inner_width));
        // Cache visual rows, including the plain projection used by shared Vim
        // word/paragraph motions. Only rebuild on width or representation changes.
        let mut gutter = 4usize.min(usize::from(inner_width - 1));
        loop {
            let width = usize::from(inner_width).saturating_sub(gutter).max(1);
            let rows = build_send_review_rows(composer, &theme, width, quoted_diff_highlighting);
            let needed = usize::from(relative_number_width(rows.len()))
                .max(gutter)
                .min(usize::from(inner_width - 1));
            if needed == gutter {
                // Build the navigation projection directly: avoid allocating
                // a temporary String per row and a Vec just to join them.
                let mut text = String::new();
                for (index, line) in rows.iter().enumerate() {
                    if index != 0 {
                        text.push('\n');
                    }
                    for span in &line.spans {
                        text.push_str(span.content.as_ref());
                    }
                }
                composer.send_preview.layout.text = text;
                composer.send_preview.layout.rows = rows;
                composer.send_preview.layout.gutter = gutter;
                break;
            }
            gutter = needed;
        }
        composer.send_preview.layout.width = inner_width;
    }
    // Expand the popup before clipping headers on a short terminal. If even
    // the full terminal cannot fit them, reserve a body row and say so.
    let minimum_height = composer.send_preview.layout.headers.len().saturating_add(6);
    if usize::from(area.height) < minimum_height {
        let height = u16::try_from(minimum_height)
            .unwrap_or(u16::MAX)
            .min(frame.area().height);
        area.y = frame.area().y + frame.area().height.saturating_sub(height) / 2;
        area.height = height;
    }
    let inner = popup_inner(frame, &theme, area, title, theme.accent());
    let header_capacity = usize::from(inner.height.saturating_sub(4));
    let header_height = composer
        .send_preview
        .layout
        .headers
        .len()
        .min(header_capacity);
    let clipped = composer.send_preview.layout.headers.len() > header_capacity;
    for (index, line) in composer
        .send_preview
        .layout
        .headers
        .iter()
        .take(header_height)
        .enumerate()
    {
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        if clipped && index + 1 == header_height {
            frame.render_widget(
                Paragraph::new("More headers — enlarge terminal").style(theme.muted()),
                row,
            );
        } else {
            frame.render_widget(Paragraph::new(line.clone()).style(theme.text()), row);
        }
    }
    let separator_y = inner.y + header_height as u16;
    let body_y = separator_y + 1;
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(inner.width))).style(theme.border()),
        Rect::new(inner.x, separator_y, inner.width, 1),
    );
    let height = usize::from(inner.bottom().saturating_sub(2).saturating_sub(body_y));
    composer.update_preview_viewport(height);
    let rows = &composer.send_preview.layout.rows;
    let current = composer.send_preview.navigation.row;
    let view = ReaderView {
        cursor: current,
        column: composer.send_preview.navigation.column,
        visual: composer.preview_visual_range(),
        active: true,
        quoted_diff_highlighting,
        theme: &theme,
    };
    for (index, line) in rows
        .iter()
        .enumerate()
        .skip(composer.send_preview.layout.top)
        .take(height)
    {
        let active = index == current;
        let number = relative_number(index, current);
        let digits = composer.send_preview.layout.gutter.saturating_sub(1);
        let number = format!("{:>digits$} ", truncate_display_width(&number, digits));
        let mut spans = vec![Span::styled(number, reader_number_style(index, view))];
        let len = line
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum::<usize>();
        let mut start = 0;
        for span in &line.spans {
            spans.extend(reader_fragments(
                &span.content,
                span.style,
                start,
                len,
                index,
                view,
                &[],
            ));
            start += span.content.chars().count();
        }
        if active && len == 0 {
            spans.push(Span::styled(" ", reader_cursor_style()));
        }
        let row = Rect::new(
            inner.x,
            body_y + (index - composer.send_preview.layout.top) as u16,
            inner.width,
            1,
        );
        let style = if view.highlights_current_line(index) {
            editor_current_line_style(&theme)
        } else if let Some(bg) = line.style.bg {
            theme.text().bg(bg)
        } else {
            theme.text()
        };
        frame.render_widget(Paragraph::new(Line::from(spans)).style(style), row);
    }
    draw_action_buttons(
        frame,
        &theme,
        Rect::new(inner.x, inner.bottom() - 2, inner.width, 2),
        setup_button_style(&theme),
        theme.current_line(),
        (" Send (Enter) ", " Edit (Esc) "),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outgoing_review_visual_has_no_extra_current_line_background() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        app.setup = None;
        let mut composer = crate::app::Composer::new();
        composer.confirm_send = true;
        composer.send_preview.mode = SendPreviewMode::Plain;
        composer.body = "VISUAL_SENTINEL\nsecond row".into();
        app.composer = Some(composer);
        let theme = app.theme().clone();
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        app.handle_key(key('v'));
        app.handle_key(key('l'));
        terminal
            .draw(|frame| {
                draw_send_confirmation(frame, &mut app);
                let area = frame.area();
                let buffer = frame.buffer_mut();
                let (x, y) = (0..area.height)
                    .find_map(|y| {
                        let row = (0..area.width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>();
                        row.find("VISUAL_SENTINEL")
                            .map(|byte| (UnicodeWidthStr::width(&row[..byte]) as u16, y))
                    })
                    .expect("preview body");
                assert_eq!(buffer[(x, y)].symbol(), "V");
                assert_eq!(buffer[(x + 1, y)].symbol(), "I");
                assert_eq!(buffer[(x + 2, y)].symbol(), "S");
                assert_eq!(buffer[(x, y)].bg, theme.visual_selection_bg);
                assert_eq!(buffer[(x + 1, y)].bg, Color::White);
                assert_eq!(buffer[(x + 2, y)].bg, Color::Reset);
                assert_eq!(buffer[(x + 25, y)].bg, Color::Reset);
            })
            .unwrap();
    }

    #[test]
    fn wrapped_send_preview_reaches_last_row_in_short_and_narrow_terminals() {
        use ratatui::{Terminal, backend::TestBackend};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        let mut composer = crate::app::Composer::new();
        composer.confirm_send = true;
        composer.send_preview.mode = SendPreviewMode::Plain;
        composer.to = "to@example.org".into();
        composer.subject = "Multiline preview".into();
        composer.body = format!(
            "{}\nPREVIEW_TAIL_SENTINEL",
            "A long wrapped line with additional text beyond the short viewport. ".repeat(25)
        );
        composer.send_preview.navigation.row = usize::MAX;
        app.composer = Some(composer);
        for (width, height) in [(60u16, 19u16), (100u16, 30u16)] {
            app.composer.as_mut().unwrap().send_preview.navigation.row = usize::MAX;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw_send_confirmation(frame, &mut app))
                .unwrap();
            let rendered = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                rendered.contains("PREVIEW_TAIL_SENTINEL"),
                "{width}x{height}: {rendered}"
            );
        }
    }

    #[test]
    fn outgoing_review_bounds_navigation_mouse_help_and_resize() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        // Exercise the actual input dispatcher, not the first-run setup wizard.
        app.setup = None;
        let mut composer = crate::app::Composer::new();
        composer.confirm_send = true;
        composer.body = (0..60).map(|i| format!("row {i}\n")).collect();
        app.composer = Some(composer);
        let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let last = app
            .composer
            .as_ref()
            .unwrap()
            .send_preview
            .layout
            .rows
            .len()
            - 1;
        let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        app.handle_key(key('G'));
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            last
        );
        for _ in 0..50 {
            app.handle_key(key('j'));
        }
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            last
        );
        app.handle_key(key('k'));
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            last - 1
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(app.animation_active());
        for _ in 0..22 {
            app.advance_animation();
        }
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            last - 23
        );
        assert!(!app.animation_active());
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 20,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
            70,
            24,
        );
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            last - 24
        );
        assert!(!app.animation_active());
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 20,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
            70,
            24,
        );
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            last - 23
        );
        app.handle_key(key('?'));
        assert!(app.composer.as_ref().unwrap().help_visible);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.composer.as_ref().unwrap().confirm_send);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.composer.as_ref().unwrap().help_visible);
        assert!(app.composer.as_ref().unwrap().confirm_send);
        // A pending destination must be clamped when the visual row count shrinks.
        app.handle_key(key('g'));
        app.handle_key(key('g'));
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let composer = app.composer.as_mut().unwrap();
        composer.send_preview.navigation.clamp(2);
        composer
            .send_preview
            .navigation
            .advance(&composer.send_preview.layout.text, 2);
        assert_eq!(composer.send_preview.navigation.row, 1);
        assert!(!composer.send_preview.navigation.active());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let composer = app.composer.as_ref().unwrap();
        assert!(!composer.confirm_send);
        assert!(composer.send_preview.layout.rows.is_empty());
    }

    #[test]
    fn outgoing_review_pins_headers_and_uses_reader_motions_and_real_html_toggle() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        app.setup = None;
        let mut composer = crate::app::Composer::new();
        composer.confirm_send = true;
        composer.subject = "PINNED_SUBJECT".into();
        composer.send_preview.mode = SendPreviewMode::Plain;
        composer.body = (0..50).map(|i| format!("row {i} next\n")).collect();
        app.composer = Some(composer);
        let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        for ch in ['1', '0', 'j'] {
            app.handle_key(key(ch));
        }
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            10
        );
        app.handle_key(key('w'));
        assert_eq!(
            app.composer
                .as_ref()
                .unwrap()
                .send_preview
                .navigation
                .column,
            4
        );
        app.handle_key(key('t'));
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            10
        );
        assert_eq!(
            app.composer
                .as_ref()
                .unwrap()
                .send_preview
                .navigation
                .column,
            4
        );
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.mode,
            SendPreviewMode::Html
        );
        // The read-only surface must not send or edit on a Vim yank key.
        app.handle_key(key('y'));
        assert!(app.composer.as_ref().unwrap().confirm_send);
        app.handle_key(key('g'));
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            10
        );
        app.handle_key(key('g'));
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            0
        );
        for ch in ['2', '0', 'G'] {
            app.handle_key(key(ch));
        }
        assert_eq!(
            app.composer.as_ref().unwrap().send_preview.navigation.row,
            19
        );
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let rendered = (0..24)
            .map(|y| {
                (0..70)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("PINNED_SUBJECT"));
        assert!(!rendered.contains("? Help"));
        assert!(app.composer.as_ref().unwrap().send_preview.layout.top > 0);
        let composer = app.composer.as_mut().unwrap();
        composer.body = (0..50).map(|i| format!("rich {i}\n")).collect();
        composer.send_preview.document.html_source = Some(format!(
            "<pre>{}</pre>",
            (0..50).map(|i| format!("html {i}\n")).collect::<String>()
        ));
        composer.send_preview.mode = SendPreviewMode::Html;
        app.handle_key(key('t'));
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let composer = app.composer.as_ref().unwrap();
        assert_eq!(composer.send_preview.mode, SendPreviewMode::Plain);
        assert!(composer.send_preview.layout.text.contains("rich 19"));
        assert_eq!(composer.send_preview.navigation.row, 19);
        app.handle_key(key('t'));
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let composer = app.composer.as_ref().unwrap();
        assert_eq!(composer.send_preview.mode, SendPreviewMode::Html);
        assert!(composer.send_preview.layout.text.contains("html 19"));
        assert_eq!(composer.send_preview.navigation.row, 19);
        app.handle_key(key('t'));
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let composer = app.composer.as_ref().unwrap();
        assert_eq!(composer.send_preview.mode, SendPreviewMode::Plain);
        assert!(composer.send_preview.layout.text.contains("rich 19"));
        assert_eq!(composer.send_preview.navigation.row, 19);
    }

    #[test]
    fn outgoing_review_toggles_highlighted_plain_and_html() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        app.setup = None;
        let mut composer = crate::app::Composer::new();
        composer.to = "friend@example.org".into();
        composer.body = "first line\nhttps://example.org\n\nOn Monday, James wrote:\n> quoted line\n>> older line\nlast line".into();
        composer.mode = ComposeMode::Normal;
        app.composer = Some(composer);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Enter));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let plain_text = app
            .composer
            .as_ref()
            .unwrap()
            .send_preview
            .layout
            .text
            .clone();
        app.handle_key(key(KeyCode::Char('j')));
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let composer = app.composer.as_ref().unwrap();
        assert_eq!(composer.send_preview.mode, SendPreviewMode::Plain);
        assert_eq!(composer.send_preview.navigation.row, 1);
        let quote_row = &composer.send_preview.layout.rows[4];
        assert!(quote_row.spans.iter().any(|span| {
            span.content.contains("quoted line") && span.style.fg == Some(app.theme().visual)
        }));
        let title = (0..24)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(title.contains("Review Outgoing Email — Plain"));
        assert_eq!(composer.send_preview.navigation.row, 1);
        assert!(
            composer
                .send_preview
                .layout
                .rows
                .iter()
                .flat_map(|row| &row.spans)
                .any(|span| {
                    span.content.contains("https://example.org")
                        && span.style.add_modifier.contains(Modifier::UNDERLINED)
                })
        );
        app.handle_key(key(KeyCode::Char('t')));
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let composer = app.composer.as_ref().unwrap();
        assert_eq!(composer.send_preview.mode, SendPreviewMode::Html);
        assert!(composer.send_preview.layout.text.contains("first line"));
        assert!(
            composer
                .send_preview
                .layout
                .rows
                .iter()
                .flat_map(|row| &row.spans)
                .any(|span| {
                    span.content.contains("https://example.org")
                        && span.style.add_modifier.contains(Modifier::UNDERLINED)
                })
        );
        assert_eq!(composer.send_preview.navigation.row, 1);
        app.handle_key(key(KeyCode::Char('t')));
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let composer = app.composer.as_ref().unwrap();
        assert_eq!(composer.send_preview.mode, SendPreviewMode::Plain);
        assert_eq!(composer.send_preview.layout.text, plain_text);
        assert!(
            composer.send_preview.layout.rows[4]
                .spans
                .iter()
                .any(|span| { span.style.fg == Some(app.theme().visual) })
        );
        assert_eq!(composer.send_preview.navigation.row, 1);
        assert!(
            composer
                .send_preview
                .layout
                .rows
                .iter()
                .flat_map(|row| &row.spans)
                .any(|span| { span.style.add_modifier.contains(Modifier::UNDERLINED) })
        );
    }

    #[test]
    fn outgoing_review_html_uses_received_mime_alternative_in_reply() {
        use crate::model::MessageId;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let raw = b"From: system <system@example.org>\r\nTo: friend@example.org\r\nSubject: Hidden post\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=example\r\n\r\n--example\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nPlain [post](https://example.org/post)\r\n--example\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><p><b>Framework Logo</b></p><p>HTML_ONLY_SENTINEL</p><a href=\"https://example.org/post\">Visit Message</a></body></html>\r\n--example--\r\n";
        let id = MessageId("reply-html".into());
        let summary =
            crate::mail::parse_summary(id.clone(), raw, raw.len() as u64, MessageFlags::empty())
                .unwrap();
        let mut composer = crate::app::Composer::prepare_original(
            &summary,
            id,
            raw.to_vec(),
            crate::app::ComposeIntent::Reply {
                quote: true,
                audience: crate::outgoing::ReplyAudience::Sender,
            },
            &[],
        )
        .unwrap();
        composer.body.insert_str(0, "My reply");
        composer.mode = ComposeMode::Normal;
        let wire = composer.outgoing_plain_body();
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        app.setup = None;
        app.composer = Some(composer);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Enter));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        for mode in [SendPreviewMode::Plain, SendPreviewMode::Html] {
            terminal
                .draw(|frame| draw_send_confirmation(frame, &mut app))
                .unwrap();
            let composer = app.composer.as_ref().unwrap();
            assert_eq!(composer.send_preview.mode, mode);
            assert_eq!(composer.outgoing_plain_body(), wire);
            assert!(composer.send_preview.layout.text.contains("My reply"));
            if mode == SendPreviewMode::Html {
                assert!(composer.send_preview.layout.text.contains("Framework Logo"));
                assert!(
                    composer
                        .send_preview
                        .layout
                        .text
                        .contains("HTML_ONLY_SENTINEL")
                );
                assert!(composer.send_preview.layout.text.contains("Visit Message"));
                assert!(!composer.send_preview.layout.text.contains("Plain [post]"));
            } else {
                assert!(composer.send_preview.layout.text.contains("Plain [post]"));
                assert!(
                    !composer
                        .send_preview
                        .layout
                        .text
                        .contains("HTML_ONLY_SENTINEL")
                );
            }
            app.handle_key(key(KeyCode::Char('t')));
        }
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        app.handle_key(key(KeyCode::Char('v')));
        app.handle_key(key(KeyCode::Char('l')));
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        assert!(
            app.composer
                .as_ref()
                .unwrap()
                .preview_visual_range()
                .is_some()
        );
        assert!((0..24).any(|y| (0..80).any(|x| {
            terminal.backend().buffer()[(x, y)].bg == app.theme().visual_selection_bg
        })));
    }

    #[test]
    fn outgoing_review_cycles_actual_html_forward_without_changing_wire_body() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        app.setup = None;
        let mut composer = crate::app::Composer::new();
        composer.to = "friend@example.org".into();
        composer.mode = ComposeMode::Normal;
        composer.body = "PLAIN_DRAFT_SENTINEL".into();
        composer.forwarded_raw = Some(
            concat!(
                "From: sender@example.org\r\n",
                "To: friend@example.org\r\n",
                "Subject: HTML forward\r\n",
                "MIME-Version: 1.0\r\n",
                "Content-Type: text/html; charset=utf-8\r\n\r\n",
                "<html><body><p><strong>HTML_ONLY_SENTINEL</strong></p></body></html>\r\n"
            )
            .as_bytes()
            .to_vec(),
        );
        let expected_plain = composer.outgoing_plain_body();
        let expected_html = composer.outgoing_html_preview().unwrap().unwrap();
        app.composer = Some(composer);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Enter));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        for mode in [
            SendPreviewMode::Html,
            SendPreviewMode::Plain,
            SendPreviewMode::Html,
        ] {
            terminal
                .draw(|frame| draw_send_confirmation(frame, &mut app))
                .unwrap();
            let composer = app.composer.as_ref().unwrap();
            assert_eq!(composer.send_preview.mode, mode);
            assert_eq!(composer.outgoing_plain_body(), expected_plain);
            assert_eq!(
                composer.outgoing_html_preview().unwrap().as_deref(),
                Some(expected_html.as_str())
            );
            if mode == SendPreviewMode::Html {
                assert!(
                    composer
                        .send_preview
                        .layout
                        .text
                        .contains("HTML_ONLY_SENTINEL")
                );
                assert!(
                    composer
                        .send_preview
                        .layout
                        .rows
                        .iter()
                        .flat_map(|row| &row.spans)
                        .any(|span| {
                            span.content.contains("HTML_ONLY_SENTINEL")
                                && span.style.add_modifier.contains(Modifier::BOLD)
                        })
                );
            } else {
                assert_eq!(composer.send_preview.layout.text, expected_plain);
            }
            app.handle_key(key(KeyCode::Char('t')));
        }
    }

    #[test]
    fn outgoing_review_diff_continuations_fill_right_edge_without_yank_padding() {
        use ratatui::{Terminal, backend::TestBackend};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        app.quoted_diff_highlighting = true;
        let mut composer = crate::app::Composer::new();
        composer.confirm_send = true;
        composer.body = ">> @@ -1 +1 @@\n>> -\told words with a long suffix that wraps onto another screen row\n>> +\tnew words with a long suffix that wraps onto another screen row\nordinary prose".into();
        let expected = composer.body.replace('\t', crate::app::COMPOSE_TAB_SPACES);
        app.composer = Some(composer);
        let (width, height) = (60u16, 32u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let composer = app.composer.as_ref().unwrap();
        let area = centered_rect(Rect::new(0, 0, width, height), width - 4, 88);
        let y = area.y + 2 + composer.send_preview.layout.headers.len() as u16;
        let buffer = terminal.backend().buffer();
        let mut diff_count = 0;
        for (index, line) in composer.send_preview.layout.rows.iter().enumerate() {
            if let Some(bg) = line.style.bg {
                diff_count += 1;
                assert_eq!(buffer[(area.right() - 2, y + index as u16)].bg, bg);
            }
        }
        assert!(diff_count >= 4, "both additions and deletions must wrap");
        assert_eq!(
            composer.send_preview.layout.text.replace('\n', ""),
            expected.replace('\n', "")
        );
    }

    #[test]
    fn outgoing_review_visual_mode_updates_global_footer_and_current_number() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        app.setup = None;
        let mut composer = crate::app::Composer::new();
        // The review owns its mode, not the underlying editor.
        composer.mode = ComposeMode::Insert;
        composer.confirm_send = true;
        composer.send_preview.mode = SendPreviewMode::Plain;
        composer.to = "friend@example.org".into();
        composer.body = "first line\nsecond line".into();
        app.composer = Some(composer);
        let mut terminal = Terminal::new(TestBackend::new(80, 32)).unwrap();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for (action, visual) in [
            (None, false),
            (Some(KeyCode::Char('v')), true),
            (Some(KeyCode::Esc), false),
            (Some(KeyCode::Char('V')), true),
            (Some(KeyCode::Esc), false),
        ] {
            if let Some(code) = action {
                app.handle_key(key(code));
            }
            terminal
                .draw(|frame| {
                    draw_status(frame, &app, Rect::new(0, 31, 80, 1));
                    draw_send_confirmation(frame, &mut app);
                })
                .unwrap();
            let composer = app.composer.as_ref().unwrap();
            assert!(composer.confirm_send);
            assert_eq!(composer.mode, ComposeMode::Insert);
            assert_eq!(composer.preview_visual_range().is_some(), visual);
            let area = centered_rect(Rect::new(0, 0, 80, 32), 76, 88);
            let y = area.y + 2 + composer.send_preview.layout.headers.len() as u16;
            let x = area.x + composer.send_preview.layout.gutter as u16 - 1;
            let buffer = terminal.backend().buffer();
            let expected = if visual {
                app.theme().visual
            } else {
                app.theme().accent
            };
            assert_eq!(buffer[(x, y)].symbol(), "1");
            assert_eq!(buffer[(x, y)].fg, expected);
            let footer = (0..80)
                .map(|x| buffer[(x, 31)].symbol())
                .collect::<String>();
            let label = if visual { "VISUAL" } else { "NORMAL" };
            let start = footer.find(label).expect("review mode badge") as u16;
            assert_eq!(buffer[(start, 31)].bg, expected);
        }
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.composer.as_ref().unwrap().confirm_send);
        terminal
            .draw(|frame| draw_status(frame, &app, Rect::new(0, 31, 80, 1)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let footer = (0..80)
            .map(|x| buffer[(x, 31)].symbol())
            .collect::<String>();
        assert!(footer.contains("INSERT"));
    }

    #[test]
    fn outgoing_review_wraps_all_headers_and_draws_relative_rows_and_buttons() {
        use ratatui::{Terminal, backend::TestBackend};
        let config: crate::config::Config = toml::from_str("[ui]\n").unwrap();
        let mut app = App::new(&config, vec![]);
        let mut composer = crate::app::Composer::new();
        composer.confirm_send = true;
        composer.to = "first@example.org, second@example.org, TO_TAIL".into();
        composer.cc = "first@example.org, second@example.org, CC_TAIL".into();
        composer.bcc = "first@example.org, second@example.org, BCC_TAIL".into();
        composer.subject = "Long subject with multiple words and SUBJECT_TAIL".into();
        composer.body = "body".into();
        app.composer = Some(composer);
        let mut terminal = Terminal::new(TestBackend::new(40, 22)).unwrap();
        terminal
            .draw(|frame| draw_send_confirmation(frame, &mut app))
            .unwrap();
        let composer = app.composer.as_ref().unwrap();
        let all = composer
            .send_preview
            .layout
            .headers
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for sentinel in ["TO_TAIL", "CC_TAIL", "BCC_TAIL", "SUBJECT_TAIL"] {
            assert!(all.contains(sentinel), "{sentinel}: {all}");
        }
        let rendered = (0..22)
            .map(|y| {
                (0..40)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Review Outgoing Email"), "{rendered}");
        assert!(rendered.contains("Send (Enter)"), "{rendered}");
        assert!(rendered.contains("Edit (Esc)"), "{rendered}");
        assert!(rendered.contains("From"), "{rendered}");
        assert!(!rendered.contains("  1 From"), "{rendered}");
        assert!(!rendered.contains("? Help"), "{rendered}");
        assert_eq!(composer.send_preview.layout.text, "body");
        assert_eq!(composer.send_preview.layout.rows.len(), 1);
        let header_height = composer.send_preview.layout.headers.len();
        let body_y = (0..22)
            .find(|&y| {
                (0..40)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
                    .contains("body")
            })
            .unwrap();
        assert!(usize::from(body_y) > header_height);
        let cell = &terminal.backend().buffer()[(3, body_y)];
        assert_eq!(cell.bg, app.theme().popup_bg);
    }
}
