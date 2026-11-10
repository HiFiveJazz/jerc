use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui_image::Image as TerminalImage;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{
    ATTACHMENT_HELP_ENTRIES, App, COMPOSE_HELP_ENTRIES, CONTACT_HELP_ENTRIES, ComposeField,
    ComposeMode, DESTINATION_HELP_ENTRIES, HELP_ENTRIES, HelpEntry, PICKER_HELP_ENTRIES,
    ReaderSelection, ranked_help_matches,
};
use crate::config::VisualSelectionStyle;
use crate::model::{HtmlLine, HtmlSpanStyle, MessageFlags};
use crate::theme::Theme;

/// Reader cursor and appearance state shared by the plain-text and HTML
/// rendering paths. Keeping it together avoids argument-order mistakes.
#[derive(Clone, Copy)]
struct ReaderView<'a> {
    cursor: usize,
    column: usize,
    visual: Option<ReaderSelection>,
    active: bool,
    quoted_diff_highlighting: bool,
    theme: &'a Theme,
}

// Feature renderers share the original theme and layout helpers through this
// module. Keep the entry point here so overlay stacking remains explicit.
mod attachments;
mod common;
mod compose;
mod contacts;
mod dialogs;
mod help;
mod mail_list;
mod quote;
mod reader;
mod setup;
mod status;

use self::attachments::*;
use self::common::*;
use self::compose::*;
use self::contacts::*;
use self::dialogs::*;
use self::help::*;
use self::mail_list::*;
use self::quote::*;
use self::reader::*;
use self::setup::*;
use self::status::*;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_tabs(frame, app, outer[0]);
    draw_main(frame, app, outer[1]);
    draw_status(frame, app, outer[2]);

    if app.show_startup_warnings {
        draw_startup_warnings(frame, app);
    }
    if app.attachment_picker_visible {
        draw_attachments(frame, app);
        if app.attachment_destination_picker.is_some() {
            draw_attachment_destination_picker(frame, app);
        }
    }
    if app.help_visible {
        // Above the attachment viewer; otherwise `?` was hidden behind it.
        draw_help(frame, app);
    }
    if app.theme_picker_visible {
        draw_theme_picker(frame, app);
    }
    if app.confirmation.is_some() {
        draw_confirmation(frame, app);
    }
    if app.remove_account_confirmation.is_some() {
        draw_remove_account_confirmation(frame, app);
    }
    if app.composer.is_some() {
        draw_composer(frame, app);
        if app
            .composer
            .as_ref()
            .is_some_and(|composer| composer.exit_prompt)
        {
            draw_compose_exit_prompt(frame, app.theme());
        } else if app
            .composer
            .as_ref()
            .is_some_and(|composer| composer.confirm_send)
        {
            draw_send_confirmation(frame, app);
        } else if app
            .composer
            .as_ref()
            .is_some_and(|composer| composer.help_visible)
        {
            draw_compose_help(frame, app);
        } else if app
            .composer
            .as_ref()
            .is_some_and(|composer| composer.attachment_manager_open)
        {
            draw_compose_attachment_manager(frame, app);
        } else if app
            .composer
            .as_ref()
            .is_some_and(|composer| composer.file_picker.is_some())
        {
            draw_compose_file_picker(frame, app);
        } else if app
            .composer
            .as_ref()
            .is_some_and(|composer| composer.advanced_headers_open)
        {
            draw_compose_advanced_options(frame, app);
        }
    }
    if app.setup.is_some() {
        draw_setup_wizard(frame, app);
    }
    if app.contacts_modal.is_some() {
        draw_contacts(frame, app);
    } else if app.composer.is_some() {
        draw_contact_suggestions(frame, app);
    }
    if app.detach_confirmation.is_some() {
        draw_detach_confirmation(frame, app);
    }
    if app.draft_resume_prompt_open() {
        draw_draft_resume_confirmation(frame, app.theme());
    }
}

fn draw_tabs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = app.theme().clone();
    let mut spans = Vec::new();
    for (index, account) in app.accounts.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if index == app.account_index {
            theme.selected()
        } else {
            theme.muted()
        };
        spans.push(Span::styled(format!(" {} ", account.name), style));
    }
    if !app.startup_warnings.is_empty() {
        spans.push(Span::styled(
            format!(
                "  ! {} config issue(s) [! to inspect]",
                app.startup_warnings.len()
            ),
            theme.error(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.text()), area);
}

fn draw_main(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(_account) = app.account() else {
        frame.render_widget(
            Paragraph::new("No accounts configured").style(app.theme().text()),
            area,
        );
        return;
    };

    if app.message_open {
        draw_message(frame, app, area);
        return;
    }

    match (app.sidebar_visible, app.preview_split) {
        (true, false) => {
            let mailbox_width = super::mailbox_column_width(area.width, app.mailbox_width_percent);
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(mailbox_width), Constraint::Min(1)])
                .split(area);
            draw_mailboxes(frame, app, columns[0]);
            draw_messages(frame, app, columns[1]);
        }
        (false, false) => draw_messages(frame, app, area),
        (true, true) => {
            let mailbox_width = super::mailbox_column_width(area.width, app.mailbox_width_percent);
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(mailbox_width), Constraint::Min(1)])
                .split(area);
            let right = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(super::preview_column_width(
                        columns[1].width,
                        app.preview_width_percent,
                    )),
                ])
                .split(columns[1]);
            draw_mailboxes(frame, app, columns[0]);
            draw_messages(frame, app, right[0]);
            draw_message(frame, app, right[1]);
        }
        (false, true) => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(super::preview_column_width(
                        area.width,
                        app.preview_width_percent,
                    )),
                ])
                .split(area);
            draw_messages(frame, app, columns[0]);
            draw_message(frame, app, columns[1]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ReaderView, mailbox_row_text, pad_reader_line, reader_word_at, relative_number,
        rich_html_style, subject_for_list, truncate_display_width, word_match_ranges,
    };
    use crate::app::ComposeMode;
    use crate::model::HtmlSpanStyle;
    use crate::theme::load_themes;
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn visual_ranges_are_purple_and_marks_remain_blue() {
        use crate::config::VisualSelectionStyle;
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        assert_eq!(
            super::browser_row_style(
                &theme,
                VisualSelectionStyle::Gutter,
                true,
                false,
                true,
                false
            ),
            theme.current_line()
        );
        assert_eq!(
            super::browser_row_style(
                &theme,
                VisualSelectionStyle::Gutter,
                true,
                false,
                false,
                false
            ),
            theme.text()
        );
        assert_eq!(
            super::browser_row_style(&theme, VisualSelectionStyle::Row, true, false, false, false)
                .bg,
            Some(theme.visual)
        );
        assert_eq!(
            super::browser_row_style(&theme, VisualSelectionStyle::Row, false, true, true, false),
            theme.current_line()
        );
        assert_eq!(
            super::browser_row_style(&theme, VisualSelectionStyle::Row, true, true, true, false),
            theme.current_line()
        );
        assert_eq!(
            super::browser_row_style(&theme, VisualSelectionStyle::Row, false, true, true, true),
            theme.unread().bg(theme.popup_bg)
        );
        let visual = super::browser_gutter(&theme, true, false);
        assert_eq!(visual.content.as_ref(), "▍");
        assert_eq!(visual.style.fg, Some(theme.visual));
        let marked = super::browser_gutter(&theme, false, true);
        assert_eq!(marked.style.fg, Some(theme.accent));
        let normal = super::browser_gutter(&theme, false, false);
        assert_eq!(normal.content.as_ref(), "▏");
        assert_eq!(normal.style.fg, Some(theme.border));
        assert_eq!(
            super::selection_check_style(&theme, VisualSelectionStyle::Gutter, true, false, false)
                .fg,
            Some(theme.visual)
        );
        assert_eq!(
            super::selection_check_style(&theme, VisualSelectionStyle::Row, true, false, true).fg,
            Some(theme.visual)
        );
        assert_eq!(
            super::selection_check_style(&theme, VisualSelectionStyle::Row, false, true, true).fg,
            Some(theme.accent)
        );
    }

    #[test]
    fn contact_check_reuses_favorite_or_saved_icon_cell() {
        assert_eq!(super::contact_list_icon(false, false, false, false), "  ");
        assert_eq!(super::contact_list_icon(true, false, false, false), "★ ");
        assert_eq!(super::contact_list_icon(false, true, false, false), "● ");
        assert_eq!(super::contact_list_icon(true, true, true, false), "✓ ");
        assert_eq!(super::contact_list_icon(true, true, false, true), "✓ ");
    }

    #[test]
    fn contacts_have_markers_only_on_selected_rows() {
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        assert_eq!(
            super::contact_gutter(&theme, false, false).content.as_ref(),
            " "
        );
        let visual = super::contact_gutter(&theme, true, false);
        assert_eq!(visual.content.as_ref(), "▍");
        assert_eq!(visual.style.fg, Some(theme.visual));
        let marked = super::contact_gutter(&theme, false, true);
        assert_eq!(marked.content.as_ref(), "▍");
        assert_eq!(marked.style.fg, Some(theme.accent));
    }

    #[test]
    fn contact_editing_shows_a_block_cursor_in_normal_and_visual_modes() {
        use crate::contacts::ContactEdit;
        let theme = load_themes().into_iter().next().expect("theme");
        let mut edit = ContactEdit::new(None);
        edit.name = "Jazz".into();
        edit.cursor = 2;
        edit.mode = ComposeMode::Normal;
        let normal = super::contact_editor_field_spans("Name", &edit.name, &edit, false, &theme);
        assert_eq!(normal[3].content.as_ref(), "z");
        assert_eq!(normal[3].style.bg, Some(Color::White));
        assert_eq!(normal[3].style.fg, Some(Color::Black));
        edit.cursor = edit.name.chars().count();
        let end = super::contact_editor_field_spans("Name", &edit.name, &edit, false, &theme);
        assert_eq!(end.last().unwrap().content.as_ref(), " ");
        assert_eq!(end.last().unwrap().style.bg, Some(Color::White));
        edit.mode = ComposeMode::Insert;
        let insert = super::contact_editor_field_spans("Name", &edit.name, &edit, false, &theme);
        assert_ne!(insert.last().unwrap().style.bg, Some(Color::White));
        edit.mode = ComposeMode::Visual;
        edit.visual_anchor = Some(0);
        edit.cursor = 2;
        let visual = super::contact_editor_field_spans("Name", &edit.name, &edit, false, &theme);
        assert_eq!(visual[3].style.bg, Some(Color::White));
        edit.active_email = true;
        edit.email = "a@example.org".into();
        edit.cursor = 0;
        let email = super::contact_editor_field_spans("Email", &edit.email, &edit, true, &theme);
        assert_eq!(email[1].style.bg, Some(Color::White));
    }

    #[test]
    fn editor_highlight_keeps_ordinary_text_unbolded() {
        use ratatui::style::Modifier;
        let theme = load_themes()
            .into_iter()
            .find(|t| t.id == "tokyo-night")
            .unwrap();
        let style = super::editor_current_line_style(&theme);
        assert_eq!(style.bg, Some(theme.popup_bg));
        assert!(!style.add_modifier.contains(Modifier::BOLD));
        assert!(theme.current_line().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn help_descriptions_fit_standard_terminal_and_narrow_layout() {
        let width = super::centered_rect(ratatui::layout::Rect::new(0, 0, 80, 30), 86, 70).width;
        let inner = width - 2; // outer popup borders
        let (category, shortcut, max_description) = super::help_columns(inner);
        assert_eq!((category, shortcut, max_description), (13, 27, 36));
        for entries in [
            crate::app::HELP_ENTRIES,
            crate::app::CONTACT_HELP_ENTRIES,
            crate::app::ATTACHMENT_HELP_ENTRIES,
            crate::app::DESTINATION_HELP_ENTRIES,
            crate::app::PICKER_HELP_ENTRIES,
            crate::app::COMPOSE_HELP_ENTRIES,
        ] {
            for entry in entries {
                assert!(
                    UnicodeWidthStr::width(entry.description) <= usize::from(max_description),
                    "key {} description too wide: {}",
                    entry.key,
                    entry.description
                );
                assert!(
                    UnicodeWidthStr::width(entry.key) <= usize::from(shortcut),
                    "key label too wide: {}",
                    entry.key
                );
                assert!(
                    UnicodeWidthStr::width(entry.category) <= usize::from(category),
                    "category too wide: {}",
                    entry.category
                );
            }
        }
        for screen_width in 3u16..80 {
            let (category, key, description) = super::help_columns(screen_width.saturating_sub(2));
            assert!(
                category + key + description + 2 <= screen_width,
                "columns overflow width {screen_width}"
            );
        }
    }

    #[test]
    fn preview_resize_highlights_the_existing_divider_only_while_dragging() {
        let theme = load_themes()
            .into_iter()
            .find(|t| t.id == "tokyo-night")
            .unwrap();
        assert_eq!(
            super::preview_split_border_style(&theme, false).fg,
            Some(theme.border)
        );
        assert_eq!(
            super::preview_split_border_style(&theme, true).fg,
            Some(theme.accent)
        );
    }

    #[test]
    fn mail_marker_is_thin_and_stays_right_of_sidebar_divider() {
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let visual = super::mail_gutter(&theme, true, false, true);
        assert_eq!(visual.content.as_ref(), "▍");
        assert_eq!(visual.style.fg, Some(theme.visual));
        assert_eq!(
            super::mail_gutter(&theme, false, false, true)
                .content
                .as_ref(),
            " "
        );
        assert_eq!(
            super::mail_gutter(&theme, false, false, false)
                .content
                .as_ref(),
            "▏"
        );
    }

    #[test]
    fn compose_exit_uses_configured_square_or_rounded_corners() {
        use ratatui::{Terminal, backend::TestBackend, widgets::BorderType};
        for (border_type, expected) in [(BorderType::Plain, '┌'), (BorderType::Rounded, '╭')] {
            let mut theme = load_themes().into_iter().next().unwrap();
            theme.border_type = border_type;
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| super::draw_compose_exit_prompt(frame, &theme))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let actual = (0u16..24)
                .flat_map(|y| (0u16..80).map(move |x| (x, y)))
                .find_map(|(x, y)| {
                    let symbol = buffer[(x, y)].symbol();
                    matches!(symbol, "┌" | "╭").then(|| symbol.chars().next().unwrap())
                });
            assert_eq!(actual, Some(expected), "wrong corner for {border_type:?}");
        }
    }

    #[test]
    fn compose_exit_uses_yes_no_buttons_and_no_escape_hint() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| super::draw_compose_exit_prompt(frame, &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let lines = (0u16..24)
            .map(|y| {
                (0u16..80)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<Vec<_>>()
                    .concat()
            })
            .collect::<Vec<_>>();
        let rendered = lines.join("\n");
        assert!(rendered.contains("[Y]es"));
        assert!(rendered.contains("[N]o"));
        assert!(!rendered.contains("Esc"));
        assert!(!rendered.contains("Keep editing"));
        assert!(!rendered.contains("Discard"));
    }

    #[test]
    fn delete_review_names_align_for_single_and_double_digit_indices() {
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let one = super::review_item_line(1, 2, "First", 40, &theme);
        let ten = super::review_item_line(10, 2, "Tenth", 40, &theme);
        assert_eq!(one.spans.len(), 2); // no colored selection gutter
        assert_eq!(ten.spans.len(), 2);
        assert_eq!(one.spans[0].content.as_ref(), "  1  ");
        assert_eq!(ten.spans[0].content.as_ref(), " 10  ");
        assert_eq!(one.spans[1].content.as_ref(), "First");
        assert_eq!(ten.spans[1].content.as_ref(), "Tenth");
        assert_eq!(
            super::destructive_button_style(&theme).bg,
            Some(theme.error)
        );
    }

    #[test]
    fn contacts_status_uses_account_and_same_mode_slot_as_mail() {
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let mut modal = crate::contacts::ContactsModal::default();
        let normal = super::contact_status_spans(&theme, Some("Gmail"), &modal);
        assert_eq!(normal.len(), 2);
        assert_eq!(normal[0].content.as_ref(), "[Gmail] ");
        assert_eq!(normal[1].content.as_ref(), " NORMAL ");
        assert_eq!(normal[1].style.bg, Some(theme.accent));
        modal.visual_anchor = Some(0);
        let visual = super::contact_status_spans(&theme, Some("Gmail"), &modal);
        assert_eq!(visual[0].content.as_ref(), "[Gmail] ");
        assert_eq!(visual[1].content.as_ref(), " VISUAL ");
        assert_eq!(visual[1].style.bg, Some(theme.visual));
        modal.visual_anchor = None;
        modal.searching = true;
        modal.query = "john".to_owned();
        modal.matches = vec![0, 1];
        let search = super::contact_status_spans(&theme, Some("Gmail"), &modal);
        assert_eq!(search[0].content.as_ref(), "[Gmail] ");
        assert_eq!(search[1].content.as_ref(), " SEARCH ");
        assert_eq!(search[1].style, super::search_mode_style(&theme));
        assert_eq!(search[1].style.bg, visual[1].style.bg);
        assert_eq!(search[2].content.as_ref(), " 2 matches");
        assert!(
            !search.iter().any(|span| span.content.contains("/john")),
            "the editable query belongs above the contacts list"
        );
        let details = super::contact_footer_details(&theme, &modal);
        assert!(
            details.is_empty(),
            "matches belong beside SEARCH, not at mid-footer"
        );
        modal.searching = false;
        assert_eq!(
            super::contact_status_spans(&theme, Some("Gmail"), &modal).len(),
            2
        );
        let filtered = super::contact_footer_details(&theme, &modal);
        assert!(filtered[0].content.contains("filter:/john"));
        modal.marked.insert("test@example.com".to_owned());
        let mut status = super::contact_status_spans(&theme, Some("Gmail"), &modal);
        status.extend(super::contact_footer_details(&theme, &modal));
        assert_eq!(status[0].content.as_ref(), "[Gmail] ");
        assert_eq!(status[1].content.as_ref(), " NORMAL ");
        assert_eq!(
            status[2].content.as_ref(),
            " 1 marked ",
            "marked count should follow the mode immediately at the left"
        );
    }

    #[test]
    fn searching_contacts_opens_same_extra_row_as_message_search() {
        use ratatui::layout::Rect;
        let inner = Rect::new(2, 3, 70, 19);
        let (search, list) = super::contacts_search_layout(inner, true);
        assert_eq!(search, Some(Rect::new(2, 3, 70, 1)));
        assert_eq!(list, Rect::new(2, 4, 70, 18));
        let (search, list) = super::contacts_search_layout(inner, false);
        assert!(search.is_none());
        assert_eq!(list, inner);
        let tiny = Rect::new(0, 0, 10, 1);
        assert_eq!(super::contacts_search_layout(tiny, true), (None, tiny));
    }

    #[test]
    fn mail_and_contacts_use_same_purple_search_badge() {
        for theme in load_themes() {
            let badge = super::search_mode_style(&theme);
            assert_eq!(badge.bg, Some(theme.visual));
            assert_eq!(badge.fg, Some(theme.selected_fg));
        }
    }

    #[test]
    fn contact_count_is_not_part_of_global_status_mode() {
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let modal = crate::contacts::ContactsModal::default();
        let spans = super::contact_status_spans(&theme, Some("Gmail"), &modal);
        assert!(spans.iter().all(|span| !span.content.contains("contacts")));
        assert_eq!(spans[1].content.as_ref(), " NORMAL ");
    }

    #[test]
    fn contact_modals_use_the_terminal_background() {
        let theme = load_themes().into_iter().next().expect("theme");
        let style = super::contact_surface_style(&theme);
        assert_eq!(style.bg, None, "contact UI must not force popup gray");
        assert_eq!(style.fg, theme.text().fg);
    }

    #[test]
    fn active_diff_row_gets_a_distinct_highlight() {
        use super::{PatchLineKind, diff_line_style, plain_reader_line};
        let theme = load_themes().into_iter().next().expect("theme");
        let diff = diff_line_style(PatchLineKind::Remove, &theme);
        let normal = plain_reader_line(
            "- old text",
            0,
            10,
            40,
            ReaderView {
                cursor: 9,
                column: 0,
                visual: None,
                active: true,
                quoted_diff_highlighting: false,
                theme: &theme,
            },
            Some(PatchLineKind::Remove),
            None,
        );
        let active = plain_reader_line(
            "- old text",
            0,
            10,
            40,
            ReaderView {
                cursor: 0,
                column: 0,
                visual: None,
                active: true,
                quoted_diff_highlighting: false,
                theme: &theme,
            },
            Some(PatchLineKind::Remove),
            None,
        );
        assert_eq!(normal.spans.last().and_then(|s| s.style.bg), diff.bg);
        assert_eq!(
            active.spans.last().and_then(|s| s.style.bg),
            theme.current_line().bg
        );
        assert!(
            !active.spans.last().is_some_and(|s| s
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)),
            "only the current relative line number should be bold"
        );

        let added = plain_reader_line(
            "+ new text",
            0,
            10,
            40,
            ReaderView {
                cursor: 0,
                column: 0,
                visual: None,
                active: true,
                quoted_diff_highlighting: false,
                theme: &theme,
            },
            Some(PatchLineKind::Add),
            None,
        );
        assert_eq!(
            added.spans.last().and_then(|s| s.style.bg),
            theme.current_line().bg
        );
    }

    #[test]
    fn mailing_list_quotations_are_distinct_from_the_authors_reply() {
        let theme = load_themes().into_iter().next().expect("theme");
        let view = ReaderView {
            cursor: 0,
            column: 0,
            visual: None,
            active: false,
            quoted_diff_highlighting: false,
            theme: &theme,
        };
        let own = super::plain_reader_line("My response", 0, 4, 30, view, None, None);
        let first = super::plain_reader_line("> Original", 1, 4, 30, view, None, None);
        let nested = super::plain_reader_line("> > Earlier", 2, 4, 30, view, None, None);
        let intro =
            super::plain_reader_line("On Monday, Phillip wrote:", 3, 4, 30, view, None, None);
        assert_eq!(own.spans[0].style.fg, Some(theme.foreground));
        assert_eq!(
            first
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "> Original"
        );
        assert!(
            first
                .spans
                .iter()
                .all(|span| span.style.fg == Some(theme.visual))
        );
        assert_eq!(
            nested
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "> > Earlier"
        );
        assert!(
            nested
                .spans
                .iter()
                .all(|span| span.style.fg == Some(theme.warning))
        );
        assert_eq!(intro.spans[0].style.fg, Some(theme.visual));
        let patch = super::plain_reader_line(
            "> + added",
            3,
            4,
            30,
            view,
            Some(super::PatchLineKind::Add),
            None,
        );
        assert!(
            patch
                .spans
                .iter()
                .all(|span| span.style.fg == Some(theme.visual))
        );
        assert!(patch.spans.iter().all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn wrapped_body_line_cursor_row_matches_paragraph_layout() {
        use super::reader_visual_cursor_row;
        use ratatui::text::{Line, Span, Text};
        use ratatui::widgets::{Paragraph, Wrap};
        let theme = load_themes().into_iter().next().expect("theme");
        let lines = vec![
            Line::raw("From: Sender"),
            Line::raw("first body line that wraps through several screen rows"),
            Line::from(vec![
                Span::raw("a very long body line with "),
                Span::styled("X", super::reader_cursor_style()),
                Span::raw(" trailing content"),
            ]),
        ];
        let preceding_rows = Paragraph::new(Text::from(lines[..2].to_vec()))
            .wrap(Wrap { trim: false })
            .line_count(20);
        let cursor_row = reader_visual_cursor_row(&lines, 1, 1, 20).unwrap();
        assert!(cursor_row >= preceding_rows);
        assert!(
            cursor_row > 2,
            "wrapped rows must contribute to the scroll position"
        );
        assert_eq!(
            super::reader_active_diff_bg(&theme, theme.error),
            theme.popup_bg
        );
    }

    #[test]
    fn compact_headers_hide_addresses_but_preserve_all_recipient_names() {
        use crate::model::{MessageFlags, MessageId, MessageSummary};
        let theme = load_themes().into_iter().next().expect("theme");
        let message = MessageSummary {
            id: MessageId("local-id".into()),
            message_id: Some("message@example.org".into()),
            in_reply_to: None,
            references: vec![],
            from_name: "Alex Sender".into(),
            from_address: "alex@example.org".into(),
            to: "Recipient One <one@example.org>, Recipient Two <two@example.org>".into(),
            cc: "CC Person <cc@example.org>".into(),
            bcc: "BCC Person <bcc@example.org>".into(),
            subject: "Hello".into(),
            timestamp: 0,
            date_label: String::new(),
            size: 0,
            flags: MessageFlags::empty(),
            has_attachment_hint: false,
            attachment_names_hint: vec![],
        };
        let text = |show_details| {
            super::reader_header_lines(&message, show_details, &theme)
                .into_iter()
                .map(|line| {
                    line.spans
                        .into_iter()
                        .map(|span| span.content.into_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let compact = text(false);
        let expanded = text(true);
        assert!(compact.contains("From: Alex Sender\n"));
        assert!(!compact.contains("alex@example.org"));
        assert!(!compact.contains("Message-ID:"));
        for header in [
            "To: Recipient One, Recipient Two",
            "Cc: CC Person",
            "Bcc: BCC Person",
            "Subject: Hello",
            "Date: unknown",
        ] {
            assert!(compact.contains(header), "missing compact header: {header}");
        }
        for address in [
            "one@example.org",
            "two@example.org",
            "cc@example.org",
            "bcc@example.org",
        ] {
            assert!(!compact.contains(address), "leaked address: {address}");
            assert!(
                expanded.contains(address),
                "missing expanded address: {address}"
            );
        }
        assert!(expanded.contains("From: Alex Sender <alex@example.org>"));
        assert!(expanded.contains("Message-ID: <message@example.org>"));
    }

    #[test]
    fn compact_recipients_support_quoted_commas_and_bare_addresses() {
        assert_eq!(
            super::compact_recipient_header(
                "\"Doe, Jane\" <jane@example.com>, bare@example.com, <only@example.com>"
            ),
            "Doe, Jane, bare@example.com, <only@example.com>"
        );
        assert_eq!(
            super::compact_recipient_header("Research Team <team@example.com>;"),
            "Research Team;"
        );
    }

    #[test]
    fn missing_sender_name_keeps_address_even_when_compact() {
        assert_eq!(
            super::sender_header_value("", "nobody@example.org", false),
            "nobody@example.org"
        );
        assert_eq!(
            super::sender_header_value("Alex", "alex@example.org", false),
            "Alex"
        );
        assert_eq!(
            super::sender_header_value("Alex", "alex@example.org", true),
            "Alex <alex@example.org>"
        );
    }

    #[test]
    fn message_attachment_subtitle_has_no_extra_indentation() {
        let names = vec!["config.example.toml".to_owned(), "LICENSE".to_owned()];
        assert_eq!(
            super::attachment_subtitle(&names),
            "config.example.toml · LICENSE"
        );
        assert_eq!(super::attachment_subtitle(&[]), "Attachment");
    }

    #[test]
    fn composer_recipient_headers_wrap_without_truncating_or_losing_cursor() {
        use crate::app::{ComposeField, ComposeMode, Composer};
        let mut composer = Composer::new();
        composer.field = ComposeField::To;
        composer.mode = ComposeMode::Insert;
        composer.to =
            "Jane Doe <jane@example.org>, Björk <bjork@example.org>, Zoe <zoe@example.org>".into();
        composer.cursor = composer.to.chars().count();
        let theme = load_themes().into_iter().next().unwrap();
        let display = super::compose_header_display(
            ComposeField::To,
            "To",
            &composer.to,
            &composer,
            &theme,
            24,
        );
        assert!(display.lines.len() > 2);
        // Strip only layout indentation, never characters from the input.
        let mut reconstructed = String::new();
        for (row_number, line) in display.lines.iter().enumerate() {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            if row_number == 0 {
                reconstructed.push_str(text.strip_prefix("▸ To: ").unwrap());
            } else {
                reconstructed.push_str(text.strip_prefix("      ").unwrap());
            }
        }
        assert_eq!(reconstructed, composer.to);
        assert!(display.cursor_row > 0);
        assert!(display.cursor_col < 24);
        for row in &display.lines {
            let text = row
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(
                UnicodeWidthStr::width(text.as_str()) <= 24,
                "overflow: {text:?}"
            );
        }
    }

    #[test]
    fn composer_wrap_keeps_emoji_grapheme_intact() {
        use crate::app::{ComposeField, ComposeMode, Composer};
        let mut composer = Composer::new();
        composer.field = ComposeField::Subject;
        composer.mode = ComposeMode::Insert;
        composer.subject = "Hello 👨\u{200d}💻 Zoe".into();
        composer.cursor = composer.subject.chars().count();
        let theme = load_themes().into_iter().next().unwrap();
        let display = super::compose_header_display(
            ComposeField::Subject,
            "Subject",
            &composer.subject,
            &composer,
            &theme,
            15,
        );
        assert!(display.lines.len() >= 2);
        let rows = display
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rows.iter().any(|row| row.contains("👨\u{200d}💻")));
        assert!(
            rows.iter()
                .all(|row| UnicodeWidthStr::width(row.as_str()) <= 15)
        );
    }

    #[test]
    fn compose_header_budget_preserves_body_space_and_shares_rows() {
        let heights = super::compose_header_heights([8, 7, 1, 2], 22, 2);
        assert!(heights.iter().all(|&height| height >= 1));
        assert!(heights.iter().copied().sum::<u16>() <= 16);
        assert!(heights[0] > 1);
        assert!(heights[1] > 1);
        assert_eq!(heights[2], 1);
    }

    #[test]
    fn insert_cursor_does_not_insert_a_character_into_editor_spans() {
        use crate::app::{ComposeMode, Composer};
        let mut composer = Composer::new();
        composer.mode = ComposeMode::Insert;
        composer.cursor = 1;
        let theme = load_themes().into_iter().next().unwrap();
        let rendered = super::editor_spans("abc", &composer, true, &theme);
        assert_eq!(
            rendered
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "abc"
        );
        let rendered = super::editor_text("abc", &composer, true, &theme, false);
        assert_eq!(
            rendered.lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "abc"
        );
    }

    #[test]
    fn notification_indicator_has_unicode_fallback_and_nerd_opt_in() {
        let config: crate::config::Config = toml::from_str(
            "[ui]\nnew_mail_notifications = true\nnotification_icon_style = \"unicode\"",
        )
        .unwrap();
        let app = crate::app::App::new(&config, vec![]);
        let theme = load_themes().into_iter().next().unwrap();
        assert!(super::notification_indicator(&app, &theme).0.contains('●'));
        let nerd_config: crate::config::Config = toml::from_str(
            "[ui]\nnew_mail_notifications = true\nnotification_icon_style = \"nerd\"",
        )
        .unwrap();
        let nerd_app = crate::app::App::new(&nerd_config, vec![]);
        assert!(
            super::notification_indicator(&nerd_app, &theme)
                .0
                .contains("󰂚")
        );
    }

    #[test]
    fn quoted_patch_examples_are_not_tinted_like_active_diffs() {
        use super::{PatchLineKind, classify_patch_lines, diff_line_style};
        let theme = load_themes().into_iter().next().unwrap();
        let lines = [
            "On Tue, Takashi wrote:",
            "> > This results in -EIO.",
            "> ---",
            "> --",
            "> -EIO",
            "> diff --git a/card.c b/card.c",
            "> index 123..456 100644",
            "> --- a/card.c",
            "> +++ b/card.c",
            "> @@ -1,2 +1,2 @@",
            ">  context",
            "> -old",
            "> +new",
            "> This is ordinary follow-up prose, -EIO should stay plain.",
        ];
        let kinds = classify_patch_lines(&lines);
        assert!(kinds.iter().all(Option::is_none));
        // When a real, unquoted patch follows, it must still be recognized.
        let real_patch = [
            "diff --git a/a.c b/a.c",
            "--- a/a.c",
            "+++ b/a.c",
            "@@ -1 +1 @@",
            "-old",
            "+new",
        ];
        let real_kinds = classify_patch_lines(&real_patch);
        assert_eq!(real_kinds[0], Some(PatchLineKind::Meta));
        assert_eq!(real_kinds[3], Some(PatchLineKind::Hunk));
        assert_eq!(real_kinds[4], Some(PatchLineKind::Remove));
        assert_eq!(real_kinds[5], Some(PatchLineKind::Add));
        let add = diff_line_style(PatchLineKind::Add, &theme);
        let remove = diff_line_style(PatchLineKind::Remove, &theme);
        assert_ne!(add.fg, remove.fg);
        assert!(add.bg.is_some());
        assert!(remove.bg.is_some());
        assert_ne!(add.bg, remove.bg);
    }

    #[test]
    fn quoted_patch_diff_mode_recognizes_only_structural_hunks() {
        use super::{PatchLineKind, classify_patch_lines_with_quotes};
        let lines = [
            "On Mon, Phillip wrote:",
            "> The command returned -EIO.",
            "> diff --git a/a.c b/a.c",
            "> index 0123..abcd 100644",
            "> --- a/a.c",
            "> +++ b/a.c",
            "> @@ -1,2 +1,2 @@",
            ">  context",
            "> -old",
            "> +new",
            ">> +not a hunk in another reply",
            "My response follows.",
            "+not a patch line",
        ];
        let in_conversation_mode = classify_patch_lines_with_quotes(&lines, false);
        assert!(in_conversation_mode.iter().all(Option::is_none));
        let in_diff_mode = classify_patch_lines_with_quotes(&lines, true);
        assert_eq!(in_diff_mode[1], None);
        assert_eq!(in_diff_mode[2], Some(PatchLineKind::Meta));
        assert_eq!(in_diff_mode[6], Some(PatchLineKind::Hunk));
        assert_eq!(in_diff_mode[7], None);
        assert_eq!(in_diff_mode[8], Some(PatchLineKind::Remove));
        assert_eq!(in_diff_mode[9], Some(PatchLineKind::Add));
        assert!(in_diff_mode[10..].iter().all(Option::is_none));
        // A hunk-only quotation from a review is also a valid diff fragment.
        let hunk_only = [
            "> @@ -3,2 +3,2 @@",
            ">  unchanged",
            "> -prior",
            "> +updated",
            "I agree with the change.",
        ];
        let kinds = classify_patch_lines_with_quotes(&hunk_only, true);
        assert_eq!(kinds[0], Some(PatchLineKind::Hunk));
        assert_eq!(kinds[1], None);
        assert_eq!(kinds[2], Some(PatchLineKind::Remove));
        assert_eq!(kinds[3], Some(PatchLineKind::Add));
        assert_eq!(kinds[4], None);
    }

    #[test]
    fn quote_diff_toggle_colors_only_quoted_patch_content() {
        use super::{PatchLineKind, plain_reader_line, rich_html_line};
        use crate::model::{HtmlLine, HtmlSpan};
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .expect("TokyoNight theme");
        let view = ReaderView {
            cursor: 99,
            column: 0,
            visual: None,
            active: false,
            quoted_diff_highlighting: true,
            theme: &theme,
        };
        let line = plain_reader_line("> +added", 0, 1, 40, view, Some(PatchLineKind::Add), None);
        // The reader pads each visual row to its viewport width. Verify the
        // actual message text and the padding separately, rather than treating
        // the intentional trailing spaces as part of the quotation.
        let rendered = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(rendered.trim_end_matches(' '), "> +added");
        assert_eq!(rendered.chars().count(), 40);
        assert_eq!(line.spans[0].style.fg, Some(theme.visual));
        assert_eq!(line.spans[1].style.fg, Some(theme.unread));
        assert!(line.spans[1].style.bg.is_some());
        let plain_quote = plain_reader_line("> +mentioned in prose", 0, 1, 40, view, None, None);
        assert!(
            plain_quote
                .spans
                .iter()
                .all(|span| span.style.fg == Some(theme.visual))
        );
        assert!(plain_quote.spans.iter().all(|span| span.style.bg.is_none()));
        let html_line = HtmlLine {
            spans: vec![
                HtmlSpan {
                    text: "> ".into(),
                    style: HtmlSpanStyle::default(),
                },
                HtmlSpan {
                    text: "-removed".into(),
                    style: HtmlSpanStyle::default(),
                },
            ],
        };
        let rich = rich_html_line(
            &html_line,
            0,
            1,
            40,
            view,
            Some(PatchLineKind::Remove),
            None,
        );
        let rich_rendered = rich
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(rich_rendered.trim_end_matches(' '), "> -removed");
        assert_eq!(rich_rendered.chars().count(), 40);
        assert_eq!(rich.spans[0].style.fg, Some(theme.visual));
        assert_eq!(rich.spans[1].style.fg, Some(theme.error));
        assert!(rich.spans[1].style.bg.is_some());
    }

    #[test]
    fn quoted_diff_state_does_not_leak_into_following_reply_text() {
        let lines = [
            "> diff --git a/a.c b/a.c",
            "> --- a/a.c",
            "> +++ b/a.c",
            "> @@ -1 +1 @@",
            "> -old",
            "Thanks for reviewing the patch.",
            "+not a code addition",
        ];
        assert!(
            super::classify_patch_lines(&lines)
                .iter()
                .all(Option::is_none)
        );
    }

    #[test]
    fn quoted_plain_mail_colors_the_entire_reply_not_just_the_marker() {
        use super::{PatchLineKind, plain_reader_line, quoted_body_style};
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        for (depth, input) in [
            (1, "> This is Phillip's previous response."),
            (2, ">> Older response with -EIO."),
            (3, ">  > > Even older résumé and ✅."),
        ] {
            let rendered = plain_reader_line(
                input,
                0,
                3,
                72,
                ReaderView {
                    cursor: 99,
                    column: 0,
                    visual: None,
                    active: false,
                    quoted_diff_highlighting: false,
                    theme: &theme,
                },
                // A quote must not become a green diff even if a patch
                // classifier has recognized a previously quoted hunk.
                Some(PatchLineKind::Add),
                None,
            );
            assert_eq!(
                rendered
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>(),
                input,
            );
            let expected = quoted_body_style(depth, &theme).fg;
            assert!(
                rendered
                    .spans
                    .iter()
                    .all(|s| s.style.fg == expected && s.style.bg.is_none()),
                "quote depth {depth} lost styling on part of the line"
            );
        }
        let unquoted_diff = plain_reader_line(
            "+actual addition",
            0,
            3,
            72,
            ReaderView {
                cursor: 99,
                column: 0,
                visual: None,
                active: false,
                quoted_diff_highlighting: false,
                theme: &theme,
            },
            Some(PatchLineKind::Add),
            None,
        );
        assert_eq!(unquoted_diff.spans[0].style.fg, Some(theme.unread));
        assert!(unquoted_diff.spans[0].style.bg.is_some());
    }

    #[test]
    fn rich_html_quote_spanning_multiple_spans_keeps_whole_line_colored() {
        use super::{PatchLineKind, quoted_body_style, rich_html_line};
        use crate::model::{HtmlLine, HtmlSpan};
        use ratatui::style::Modifier;
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let line = HtmlLine {
            spans: vec![
                HtmlSpan {
                    text: ">".into(),
                    style: HtmlSpanStyle::default(),
                },
                HtmlSpan {
                    text: "> From ".into(),
                    style: HtmlSpanStyle::default(),
                },
                HtmlSpan {
                    text: "Jérôme: quoted full text".into(),
                    style: HtmlSpanStyle {
                        foreground: Some((255, 0, 0)),
                        bold: true,
                        ..Default::default()
                    },
                },
            ],
        };
        let rendered = rich_html_line(
            &line,
            0,
            2,
            70,
            ReaderView {
                cursor: 99,
                column: 0,
                visual: None,
                active: false,
                quoted_diff_highlighting: false,
                theme: &theme,
            },
            Some(PatchLineKind::Remove),
            None,
        );
        let expected = quoted_body_style(2, &theme).fg;
        assert_eq!(
            rendered
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            ">> From Jérôme: quoted full text"
        );
        assert!(rendered.spans.iter().all(|span| span.style.fg == expected));
        assert!(rendered.spans.iter().all(|span| span.style.bg.is_none()));
        assert!(
            rendered.spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn composer_quoted_attribution_keeps_depth_color() {
        use super::{editor_text, quoted_body_style};
        use crate::app::Composer;
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let mut composer = Composer::new();
        composer.body = "> On Mon, Phillip wrote:\n>> Earlier reply".to_string();
        let text = editor_text(&composer.body, &composer, false, &theme, false);
        for (depth, line) in text.lines.iter().enumerate() {
            let expected = quoted_body_style(depth + 1, &theme).fg;
            assert!(line.spans.iter().all(|s| s.style.fg == expected));
        }
    }

    #[test]
    fn composer_quote_diff_mode_preserves_quote_markers_and_only_colors_patch_hunks() {
        use super::{PatchLineKind, diff_line_style, editor_text, quoted_body_style};
        use crate::app::{ComposeMode, Composer};
        let theme = load_themes()
            .into_iter()
            .find(|theme| theme.id == "tokyo-night")
            .unwrap();
        let mut composer = Composer::new();
        composer.mode = ComposeMode::Insert; // Do not mask text with the Normal block cursor.
        composer.body = "> @@ -1 +1 @@\n> -before\n> +after\n> plain prose".into();
        let conversation = editor_text(&composer.body, &composer, false, &theme, false);
        assert_eq!(conversation.lines.len(), 4);
        for line in &conversation.lines {
            assert!(line.spans.iter().all(|span| span.style.bg.is_none()));
            assert_eq!(line.spans[2].style.fg, quoted_body_style(1, &theme).fg);
        }
        let diff = editor_text(&composer.body, &composer, false, &theme, true);
        assert_eq!(
            diff.lines[0].spans[2].style.fg,
            diff_line_style(PatchLineKind::Hunk, &theme).fg
        );
        assert_eq!(
            diff.lines[1].spans[2].style.bg,
            diff_line_style(PatchLineKind::Remove, &theme).bg
        );
        assert_eq!(
            diff.lines[2].spans[2].style.bg,
            diff_line_style(PatchLineKind::Add, &theme).bg
        );
        assert_eq!(
            diff.lines[3].spans[2].style.fg,
            quoted_body_style(1, &theme).fg
        );
        // The leading '>' and the following space are still conversation styling.
        assert_eq!(
            diff.lines[1].spans[0].style.fg,
            quoted_body_style(1, &theme).fg
        );
        assert!(diff.lines[1].spans[0].style.bg.is_none());
    }

    #[test]
    fn adjacent_unified_headers_allow_non_git_patches() {
        use super::{PatchLineKind, classify_patch_lines};
        let lines = [
            "--- a/foo",
            "+++ b/foo",
            "@@ -1 +1 @@",
            "-before",
            "+after",
            "--",
            "-EIO",
        ];
        let kinds = classify_patch_lines(&lines);
        assert_eq!(kinds[3], Some(PatchLineKind::Remove));
        assert_eq!(kinds[4], Some(PatchLineKind::Add));
        assert_eq!(kinds[5], None);
        assert_eq!(kinds[6], None);
    }

    #[test]
    fn message_viewport_matches_full_height_scan() {
        // Reference: previous implementation, which scanned/allotted heights
        // for the entire mailbox on every terminal redraw.
        fn old_viewport(
            heights: &[usize],
            offset: usize,
            current: usize,
            height: usize,
        ) -> (usize, usize) {
            let mut start = offset.min(heights.len() - 1);
            if current < start {
                start = current;
            }
            let mut rows = heights[start..=current].iter().sum::<usize>();
            while start < current && rows > height {
                rows -= heights[start];
                start += 1;
            }
            let mut end = start;
            let mut used = 0;
            while end < heights.len() && used + heights[end] <= height {
                used += heights[end];
                end += 1;
            }
            if end == start {
                end = (start + 1).min(heights.len());
            }
            (start, end)
        }
        for mask in 0..256usize {
            let heights: Vec<_> = (0..8).map(|index| 1 + ((mask >> index) & 1)).collect();
            let visible: Vec<_> = (0..heights.len()).collect();
            for height in 1..=10 {
                for offset in 0..=heights.len() {
                    for current in 0..heights.len() {
                        assert_eq!(
                            super::message_list_viewport(
                                &visible,
                                offset,
                                current,
                                height,
                                |index| heights[index]
                            ),
                            old_viewport(&heights, offset, current, height),
                            "mask={mask} height={height} offset={offset} current={current}"
                        );
                    }
                }
            }
        }
        assert_eq!(super::message_list_viewport(&[], 0, 0, 1, |_| 1), (0, 0));
    }

    #[test]
    fn visual_mail_selection_uses_view_positions_without_repeated_searches() {
        let visible = [6, 2, 9, 1];
        assert_eq!(
            super::message_list_visual_range(&visible, Some(9), Some(1)),
            Some((1, 2))
        );
        assert_eq!(
            super::message_list_visual_range(&visible, Some(6), Some(3)),
            Some((0, 3))
        );
        assert_eq!(
            super::message_list_visual_range(&visible, Some(8), Some(1)),
            None
        );
        assert_eq!(
            super::message_list_visual_range(&visible, None, Some(1)),
            None
        );
    }

    #[test]
    fn mail_list_borrows_unchanged_subjects_and_truncation() {
        use std::borrow::Cow;
        assert!(matches!(
            super::subject_for_list_cow("Hello 👋"),
            Cow::Borrowed(_)
        ));
        assert_eq!(super::subject_for_list_cow("❤️"), "❤\u{fe0e}");
        assert!(matches!(
            super::truncate_display_width_cow("Hello", 10),
            Cow::Borrowed(_)
        ));
        assert_eq!(super::truncate_display_width_cow("Hello", 4), "Hel…");
    }

    #[test]
    fn relative_numbers_match_vim_style() {
        assert_eq!(relative_number(7, 7), "8");
        assert_eq!(relative_number(2, 7), "5");
        assert_eq!(relative_number(11, 7), "4");
    }

    #[test]
    fn mailbox_rows_keep_counts_at_the_right_edge() {
        assert_eq!(
            mailbox_row_text("[Gmail]/Trash", "5", 20),
            "[Gmail]/Trash      5"
        );
        assert_eq!(mailbox_row_text("INBOX", "", 10), "INBOX     ");
    }

    #[test]
    fn sender_foreground_does_not_override_terminal_theme() {
        let theme = load_themes().into_iter().next().expect("built-in theme");
        let style = HtmlSpanStyle {
            foreground: Some((255, 255, 255)),
            ..HtmlSpanStyle::default()
        };
        let rendered = rich_html_style(&style, "body text", &theme);
        assert_eq!(rendered, theme.text());
    }

    #[test]
    fn browse_subject_requests_monochrome_only_for_text_capable_symbols() {
        assert_eq!(subject_for_list("❤️ Mail ☀️"), "❤\u{fe0e} Mail ☀\u{fe0e}");
        // Preserve emoji-only/wide glyphs and joined emoji. FE0E fallback
        // caused half-glyph clipping with the user's Apple Color Emoji font.
        assert_eq!(
            subject_for_list("👋 Welcome to KLIPY!"),
            "👋 Welcome to KLIPY!"
        );
        assert_eq!(
            subject_for_list("👨\u{200d}💻 Status"),
            "👨\u{200d}💻 Status"
        );
    }

    #[test]
    fn emoji_subject_keeps_unclipped_grapheme() {
        let subject = subject_for_list("👋 Welcome to KLIPY!");
        let display = truncate_display_width(&subject, 25);
        assert_eq!(display, subject);
        assert!(UnicodeWidthStr::width(display.as_str()) <= 25);
    }

    #[test]
    fn attachment_summary_wraps_and_clips_without_splitting_emoji() {
        let rows = super::clipped_wrap_text(
            "Attachments (4): a.txt · image👨\u{200d}💻.png · very-long-filename.pdf · z.zip",
            22,
            2,
        );
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| UnicodeWidthStr::width(row.as_str()) <= 22)
        );
        assert!(rows.last().unwrap().ends_with('…'));
        assert!(!rows.iter().any(|row| row.ends_with('\u{200d}')));
    }

    #[test]
    fn compose_mode_badges_use_distinct_filled_colors() {
        use crate::app::ComposeMode;
        let theme = crate::theme::load_themes()
            .into_iter()
            .find(|t| t.id == "tokyo-night")
            .unwrap();
        let normal = super::compose_mode_colors(ComposeMode::Normal, &theme);
        let insert = super::compose_mode_colors(ComposeMode::Insert, &theme);
        let visual = super::compose_mode_colors(ComposeMode::Visual, &theme);
        assert_eq!(visual.0, theme.visual);
        assert_eq!(normal, (theme.accent, theme.selected_fg));
        assert_eq!(insert, (theme.unread, theme.selected_fg));
        assert_ne!(visual.0, normal.0);
        assert_ne!(visual.0, insert.0);
        assert_eq!(visual.1, theme.selected_fg);
    }

    #[test]
    fn mode_badges_are_consistent_in_browser_contacts_and_compose() {
        for theme in load_themes() {
            assert_eq!(super::mode_style("NORMAL", &theme).bg, Some(theme.accent));
            assert_eq!(super::mode_style("VISUAL", &theme).bg, Some(theme.visual));
            assert_eq!(super::mode_style("SEARCH", &theme).bg, Some(theme.visual));
            assert_eq!(super::mode_style("INSERT", &theme).bg, Some(theme.unread));
            assert_eq!(
                super::compose_mode_style(crate::app::ComposeMode::Insert, &theme).bg,
                Some(theme.unread)
            );
        }
    }

    #[test]
    fn browse_subject_truncation_never_splits_emoji_graphemes() {
        let subject = subject_for_list("Status 👨\u{200d}💻 complete");
        let truncated = truncate_display_width(&subject, 10);
        assert!(!truncated.ends_with('\u{200d}'));
        assert!(!truncated.ends_with('\u{fe0e}'));
        assert!(truncated.ends_with('…'));
        assert!(UnicodeWidthStr::width(truncated.as_str()) <= 10);
    }

    #[test]
    fn reader_illuminate_finds_word_under_cursor() {
        assert_eq!(
            reader_word_at("alpha beta\nnext", 0, 7),
            Some("beta".to_owned())
        );
        assert_eq!(reader_word_at("alpha beta", 0, 5), None);
    }

    #[test]
    fn reader_illuminate_matches_whole_words_only() {
        assert_eq!(
            word_match_ranges("word wordy word word_word", "word"),
            vec![(0, 4), (11, 15)]
        );
    }

    #[test]
    fn reader_illuminate_uses_colorscheme_style_reference_highlight() {
        let theme = load_themes().into_iter().next().expect("theme");
        let fragments = super::reader_fragments(
            "word",
            theme.text(),
            0,
            4,
            1,
            ReaderView {
                cursor: 0,
                column: 0,
                visual: None,
                active: true,
                quoted_diff_highlighting: false,
                theme: &theme,
            },
            &[(0, 4)],
        );
        assert_eq!(fragments.len(), 1);
        assert!(
            !fragments[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        );
        assert_eq!(
            fragments[0].style.bg,
            Some(super::reader_illuminate_bg(&theme))
        );
        assert_eq!(fragments[0].style.fg, theme.text().fg);
    }

    #[test]
    fn reader_illuminate_matches_tokyonight_reference_tone() {
        let mut theme = load_themes().into_iter().next().expect("theme");
        theme.popup_bg = Color::Rgb(36, 40, 59);
        theme.border = Color::Rgb(65, 72, 104);
        assert_eq!(super::reader_illuminate_bg(&theme), Color::Rgb(60, 66, 96));
    }

    #[test]
    fn reader_cursor_stays_white_over_illuminated_word() {
        let theme = load_themes().into_iter().next().expect("theme");
        let fragments = super::reader_fragments(
            "word",
            theme.text(),
            0,
            4,
            0,
            ReaderView {
                cursor: 0,
                column: 0,
                visual: None,
                active: true,
                quoted_diff_highlighting: false,
                theme: &theme,
            },
            &[(0, 4)],
        );
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].style.bg, Some(Color::White));
        assert_eq!(fragments[0].style.fg, Some(Color::Black));
        assert_eq!(
            fragments[1].style.bg,
            Some(super::reader_illuminate_bg(&theme))
        );
    }

    #[test]
    fn reader_cursor_remains_visible_on_blank_lines() {
        let theme = load_themes().into_iter().next().expect("theme");
        let mut spans = Vec::new();
        pad_reader_line(
            &mut spans,
            0,
            8,
            2,
            ReaderView {
                cursor: 2,
                column: 0,
                visual: None,
                active: true,
                quoted_diff_highlighting: false,
                theme: &theme,
            },
            None,
        );

        assert!(!spans.is_empty());
        assert_eq!(spans[0].content.as_ref(), " ");
        assert_eq!(spans[0].style.bg, Some(Color::White));
        assert_eq!(spans[0].style.fg, Some(Color::Black));
    }
}

#[cfg(test)]
mod attachment_preview_tests {
    use super::*;
    use crate::model::OutgoingAttachment;

    #[test]
    fn preview_text_is_bounded_and_does_not_display_binary() {
        let file = OutgoingAttachment {
            filename: "notes.txt".into(),
            content_type: "text/plain".into(),
            data: vec![b'a'; 64 * 1024],
        };
        let preview = attachment_text_preview(&file).unwrap();
        assert!(preview.len() < file.data.len());
        assert!(preview.contains("truncated"));
        let binary = OutgoingAttachment {
            filename: "file.bin".into(),
            content_type: "application/octet-stream".into(),
            data: vec![0, 1, 2],
        };
        assert!(attachment_text_preview(&binary).is_none());
    }
}
