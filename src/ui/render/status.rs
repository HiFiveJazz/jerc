//! Status rendering.

use super::*;

pub(super) fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = app.theme().clone();
    let mut spans = Vec::new();
    if let Some(composer) = app
        .composer
        .as_ref()
        .filter(|_| app.contacts_modal.is_none())
    {
        spans.push(Span::styled(
            format!(
                "[{}] ",
                app.account()
                    .map_or("Compose", |account| account.name.as_str())
            ),
            theme.accent(),
        ));
        spans.push(Span::styled(
            format!(" {} ", composer.mode.label()),
            compose_mode_style(composer.mode, &theme),
        ));
        spans.push(Span::raw(" "));
        if !app.status.text.is_empty() {
            spans.push(Span::styled(
                app.status.text.as_str(),
                if app.status.is_error {
                    theme.error()
                } else {
                    theme.muted()
                },
            ));
        }
    } else if let Some(account) = app.account() {
        spans.push(Span::styled(format!("[{}] ", account.name), theme.accent()));
        let mode = if app.message_open {
            if app.reader_visual_anchor.is_some() {
                "VISUAL"
            } else {
                "NORMAL"
            }
        } else if account.search_editing {
            "SEARCH"
        } else if app.visual_anchor.is_some() {
            "VISUAL"
        } else {
            "NORMAL"
        };
        spans.push(Span::styled(format!(" {mode} "), mode_style(mode, &theme)));
        spans.push(Span::raw(" "));
        if account.search_editing {
            let matches = account.visible_message_indices().len();
            spans.push(Span::styled(
                format!("{matches} match{}  ", if matches == 1 { "" } else { "es" }),
                theme.muted(),
            ));
        } else if !account.search_query.is_empty() {
            spans.push(Span::styled(
                format!("filter:/{}  ", account.search_query),
                theme.accent(),
            ));
        }
        if account.search_editing {
            // The editable query is already rendered in the message-list search
            // header. Avoid echoing the status text below it a second time.
        } else if app.message_open
            && let Some(url) = app.link_under_reader_cursor()
        {
            // The actual destination has priority over transient status text
            // while the Vim reader cursor rests on a hyperlink. Never follow it
            // automatically: the user can inspect the complete URL first.
            spans.push(Span::styled(format!("URL: {url}"), theme.accent()));
        } else if account.syncing_with_server() {
            spans.push(Span::styled(
                format!("{} Syncing with server…", app.sync_spinner()),
                theme.muted(),
            ));
        } else if account.is_loading_body() {
            spans.push(Span::raw("Loading message…"));
        } else {
            let style = if app.status.is_error {
                theme.error()
            } else {
                theme.text()
            };
            spans.push(Span::styled(app.status.text.as_str(), style));
        }
        if !app.message_marked.is_empty() {
            spans.push(Span::styled(
                format!("    {} marked", app.message_marked.len()),
                theme.accent(),
            ));
        }
        if app.preview_split {
            spans.push(Span::styled("    SPLIT", theme.muted()));
        }
    } else {
        spans.push(Span::raw(app.status.text.as_str()));
    }
    if let Some(count) = app.count_prefix {
        spans.push(Span::styled(format!("    {count}"), theme.accent()));
    }
    // Reserve the far-right footer cells so long status text never covers the
    // notification state. The glyph can also be clicked to toggle alerts.
    let indicator_width = crate::ui::NOTIFICATION_INDICATOR_WIDTH.min(area.width);
    let status_area = Rect::new(area.x, area.y, area.width - indicator_width, area.height);
    if let Some(modal) = app.contacts_modal.as_ref() {
        // Contacts use the same global status row and mode slot as mail.
        // The contact count belongs inside the Contacts window, not here.
        let mut mode_spans = contact_status_spans(
            &theme,
            app.account().map(|account| account.name.as_str()),
            modal,
        );
        mode_spans.extend(contact_footer_details(&theme, modal));
        frame.render_widget(
            Paragraph::new(Line::from(mode_spans)).style(theme.status()),
            status_area,
        );
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(theme.status()),
            status_area,
        );
    }
    if indicator_width == crate::ui::NOTIFICATION_INDICATOR_WIDTH {
        let (glyph, style) = notification_indicator(app, &theme);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(glyph, style)))
                .alignment(Alignment::Right)
                .style(theme.status()),
            Rect::new(
                area.right().saturating_sub(indicator_width),
                area.y,
                indicator_width,
                area.height,
            ),
        );
    }
}

/// Never guess whether the terminal has a Nerd Font installed. Users opt in
/// with `ui.notification_icon_style = "nerd"`; Unicode is the safe default.
pub(super) fn notification_indicator(app: &App, theme: &Theme) -> (String, Style) {
    let enabled = app.notifications_enabled();
    let glyph = match (app.notification_icon_style(), enabled) {
        (crate::config::NotificationIconStyle::Nerd, true) => "󰂚",
        (crate::config::NotificationIconStyle::Nerd, false) => "󰂛",
        (crate::config::NotificationIconStyle::Unicode, true) => "●",
        (crate::config::NotificationIconStyle::Unicode, false) => "○",
    };
    let count = if enabled { app.new_mail_count() } else { 0 };
    let label = if count > 99 {
        format!("{glyph} 99+")
    } else if count > 0 {
        format!("{glyph} {count}")
    } else {
        glyph.to_owned()
    };
    (
        format!(" {label} "),
        if enabled {
            theme.accent()
        } else {
            theme.muted()
        },
    )
}

pub(super) fn draw_search_header(frame: &mut Frame<'_>, area: Rect, query: &str, app: &App) {
    draw_search_prompt(frame, area, query, app.theme());
}

/// Reused by the mail browser and Contacts so / opens the same dedicated
/// search row without borrowing App while the contacts modal is mutable.
pub(super) fn draw_search_prompt(frame: &mut Frame<'_>, area: Rect, query: &str, theme: &Theme) {
    let prompt = if query.is_empty() {
        "type to search…"
    } else {
        query
    };
    let style = if query.is_empty() {
        theme.muted()
    } else {
        theme.text()
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" / ", theme.accent()),
            Span::styled(prompt, style),
        ])),
        area,
    );
}
