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
    ATTACHMENT_HELP_ENTRIES, App, CONTACT_HELP_ENTRIES, ComposeField, ComposeMode,
    DESTINATION_HELP_ENTRIES, HELP_ENTRIES, HelpEntry, MailReviewItem, NOTICE_HELP_ENTRIES,
    PICKER_HELP_ENTRIES, ReaderSelection, ranked_help_matches,
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

impl ReaderView<'_> {
    fn highlights_current_line(self, line: usize) -> bool {
        self.active && self.cursor == line && self.visual.is_none()
    }
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
mod notices;
mod quote;
mod reader;
mod send_review;
mod setup;
mod status;

use self::attachments::*;
use self::common::*;
use self::compose::*;
use self::contacts::*;
use self::dialogs::*;
use self::help::*;
use self::mail_list::*;
use self::notices::*;
use self::quote::*;
use self::reader::*;
use self::send_review::*;
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
        draw_notices(frame, app);
    }
    if app.manual_review_visible {
        draw_manual_review(frame, app);
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
            if app
                .composer
                .as_ref()
                .is_some_and(|composer| composer.help_visible)
            {
                draw_compose_help(frame, app);
            }
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
    // Only show the amber hazard icon while at least one notice is active.
    // Acknowledging it does not resolve uncertain mailbox or Outbox work.
    if let Some(label) = app.warning_banner_label() {
        spans.push(Span::styled(
            label,
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
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

    // A browser without a split previews the next fullscreen opening. The
    // split renderer overrides this hint with its exact pane width.
    app.remember_reader_html_width(usize::from(area.width).saturating_sub(5));
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
mod tests;

#[cfg(test)]
mod attachment_preview_tests;
