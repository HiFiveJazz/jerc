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
    App, ComposeField, ComposeMode, ATTACHMENT_HELP_ENTRIES, COMPOSE_HELP_ENTRIES, CONTACT_HELP_ENTRIES, PICKER_HELP_ENTRIES, HELP_ENTRIES, HelpEntry, ranked_help_matches, ReaderSelection,
};
use crate::model::{HtmlLine, HtmlSpanStyle, MessageFlags};
use crate::config::VisualSelectionStyle;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
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
        if app.composer.as_ref().is_some_and(|composer| composer.exit_prompt) {
            draw_compose_exit_prompt(frame, app.theme());
        } else if app.composer.as_ref().is_some_and(|composer| composer.confirm_send) {
            draw_send_confirmation(frame, app);
        } else if app.composer.as_ref().is_some_and(|composer| composer.help_visible) {
            draw_compose_help(frame, app);
        } else if app.composer.as_ref().is_some_and(|composer| composer.attachment_manager_open) {
            draw_compose_attachment_manager(frame, app);
        } else if app.composer.as_ref().is_some_and(|composer| composer.file_picker.is_some()) {
            draw_compose_file_picker(frame, app);
        } else if app.composer.as_ref().is_some_and(|composer| composer.advanced_headers_open) {
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
            format!("  ! {} config issue(s) [! to inspect]", app.startup_warnings.len()),
            theme.error(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.text()), area);
}

/// Two-step account setup: identity first, discovered and editable settings
/// second. Each selected row paints its entire rectangle, not just the text.
fn draw_setup_wizard(frame: &mut Frame<'_>, app: &App) {
    use crate::setup::{SetupStep, SetupDisplayRow, EMAIL, NAME, PROVIDER, CLIENT_ID, GOOGLE_SECRET};

    let Some(wizard) = app.setup.as_ref() else { return; };
    let theme = app.theme().clone();
    let bounds = frame.area();
    let (width, height) = wizard.popup_size(bounds.width, bounds.height);
    let popup = Rect::new(
        bounds.x + bounds.width.saturating_sub(width) / 2,
        bounds.y + bounds.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let stages = ["Add Email", "Review and Connect", "Browser Sign-In", "Confirmation"];
    let active = wizard.progress_stage();
    let mut progress = Vec::new();
    for (i, stage) in stages.iter().enumerate() {
        if i != 0 { progress.push(Span::styled("  →  ", theme.muted())); }
        let style = if i == active { theme.accent() } else { theme.muted() };
        progress.push(Span::styled(*stage, style));
    }
    // A single border title avoids the two titles overlapping at smaller widths.
    let block = Block::default()
        .title(Line::from(progress).centered())
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 7 || inner.width < 15 { return; }
    let content = inner;
    if wizard.saved {
        let description = if wizard.authenticated {
            "Your account is configured and signed in."
        } else { "Your account was saved. Sign in later to access mail." };
        let saved_regions = Layout::default().direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
            .split(content);
        let lines = vec![
            Line::styled("✓ Account saved successfully", setup_success_style()),
            Line::raw(""),
            Line::styled(format!("{}  ·  {}", wizard.fields[NAME], wizard.fields[EMAIL]), theme.text()),
            Line::raw(""),
            Line::styled(description, theme.muted()),
        ];
        // The confirmation's success banner already conveys that the account
        // was saved. Avoid repeating an additional "Account saved" message.
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), saved_regions[0]);
        let button_area = Rect::new(saved_regions[1].x + saved_regions[1].width.saturating_sub(26) / 2,
            saved_regions[1].y, 26.min(saved_regions[1].width), 1);
        frame.render_widget(Paragraph::new(" Open account ").alignment(Alignment::Center)
            .style(setup_button_style(&theme)), button_area);
        return;
    }
    if wizard.step == SetupStep::Authenticating {
        // This is a real stage for *both* Google and Microsoft, including
        // when sign-in is launched from the editable review page.
        let provider = if wizard.provider_hint() == "google" { "Google" }
            else { "Microsoft 365" };
        let lines = vec![
            Line::styled(format!("Sign in with {provider}"), theme.accent()),
            Line::raw(""),
            Line::styled(wizard.fields[EMAIL].clone(), theme.text()),
            Line::raw(""),
            Line::styled(format!("{}  Complete sign-in in your browser…", wizard.spinner()), theme.text()),
            Line::raw(""),
            Line::styled("This page will update when authentication finishes.", theme.muted()),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), content);
        return;
    }

    let identity_step = wizard.step == SetupStep::Identity;
    let header_height = if identity_step { 1 } else { 3 };
    let regions = Layout::default().direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(3),
        ]).split(content);

    if identity_step {
        frame.render_widget(Paragraph::new(
            Line::styled("Enter your account name and email address.", theme.muted())
        ), regions[0]);
    } else {
        let found = if wizard.source.is_some() { "Account settings found" }
            else { "Manual account settings" };
        let source = wizard.source.as_deref().unwrap_or("Edit the settings below");
        let found_style = if wizard.source.is_some() { setup_success_style() }
            else { theme.accent() };
        // Render as a full-width success strip, not merely green text.
        frame.render_widget(Paragraph::new(format!(" ✓ {found} ")).style(found_style),
            Rect::new(regions[0].x, regions[0].y, regions[0].width, 1));
        frame.render_widget(Paragraph::new(
            Line::styled(format!("{}  ·  {}", wizard.fields[NAME], wizard.fields[EMAIL]), theme.text())
        ), Rect::new(regions[0].x, regions[0].y + 1, regions[0].width, 1));
        frame.render_widget(Paragraph::new(
            Line::styled(format!("{}  ·  {}", wizard.fields[PROVIDER], source), theme.muted())
        ), Rect::new(regions[0].x, regions[0].y + 2, regions[0].width, 1));
    }

    let display_rows = wizard.display_rows();
    let list_height = usize::from(regions[1].height);
    let selected_pos = display_rows.iter().position(|row|
        matches!(row, SetupDisplayRow::Field(index) if *index == wizard.selected))
        .unwrap_or(0);
    let first = selected_pos.saturating_sub(list_height.saturating_sub(1))
        .min(display_rows.len().saturating_sub(list_height));
    for (row, entry) in display_rows.iter().skip(first).take(list_height).enumerate() {
        let row_area = Rect::new(regions[1].x, regions[1].y + row as u16,
            regions[1].width, 1);
        let width = usize::from(row_area.width);
        match entry {
            SetupDisplayRow::BoxTop(title) => {
                let (left, right) = match theme.border_type {
                    ratatui::widgets::BorderType::Rounded => ('╭', '╮'),
                    _ => ('┌', '┐'),
                };
                let start = format!("{left}─ {title} ");
                let line = format!("{}{}{right}", start,
                    "─".repeat(width.saturating_sub(start.chars().count() + 1)));
                frame.render_widget(Paragraph::new(Line::styled(line, theme.border())), row_area);
            }
            SetupDisplayRow::BoxBottom => {
                let (left, right) = match theme.border_type {
                    ratatui::widgets::BorderType::Rounded => ('╰', '╯'),
                    _ => ('└', '┘'),
                };
                let line = format!("{left}{}{right}", "─".repeat(width.saturating_sub(2)));
                frame.render_widget(Paragraph::new(Line::styled(line, theme.border())), row_area);
            }
            SetupDisplayRow::Field(index) => {
                let selected = *index == wizard.selected;
                let style = if selected { theme.current_line() } else { theme.text() };
                // Paint full cell width, not just as many columns as the label.
                frame.render_widget(Block::default().style(style), row_area);
                let label = crate::setup::LABELS[*index];
                if identity_step && wizard.fields[*index].is_empty() {
                    // Examples are visual placeholders, never persisted values.
                    // Keep them dim even when the selection paints the row.
                    let example = if *index == NAME { "e.g. Personal" }
                        else { "name@example.com" };
                    let prefix = format!(" {} {label:<20} ", if selected { "›" } else { " " });
                    let caret = if selected { "▏" } else { "" };
                    let available = width.saturating_sub(prefix.chars().count() + caret.chars().count());
                    let placeholder = example.chars().take(available).collect::<String>();
                    let placeholder_style = if selected {
                        Style::default().fg(theme.muted).bg(theme.popup_bg)
                    } else { Style::default().fg(theme.muted) };
                    frame.render_widget(Paragraph::new(Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(caret, style),
                        Span::styled(placeholder, placeholder_style),
                    ])), row_area);
                    continue;
                }
                let value = wizard.fields[*index].as_str();
                let value = if *index == CLIENT_ID && value.is_empty()
                    && crate::oauth::embedded_client_id(wizard.provider_hint()).is_some() {
                    "(publisher-configured)"
                } else if *index == GOOGLE_SECRET && value.is_empty()
                    && crate::oauth::embedded_google_client_secret().is_some() {
                    "(publisher-configured)"
                } else if value.is_empty() { "(enter value)" } else { value };
                let value = if *index == GOOGLE_SECRET && !wizard.fields[*index].is_empty() {
                    "(configured)".to_owned()
                } else { value.to_owned() };
                let value = if *index == PROVIDER {
                    format!("◀ {value} ▶")
                } else if selected && !wizard.authenticating && !wizard.saved {
                    format!("{value}▏")
                } else { value };
                let boxed = !identity_step && *index != PROVIDER;
                let maximum = width.saturating_sub(if boxed { 28 } else { 24 });
                let visible = value.chars().take(maximum).collect::<String>();
                let body = format!(" {} {label:<20} {visible}",
                    if selected { "›" } else { " " });
                let content_width = width.saturating_sub(if boxed { 2 } else { 0 });
                let mut text = body.chars().take(content_width).collect::<String>();
                text.push_str(&" ".repeat(content_width.saturating_sub(text.chars().count())));
                if boxed && width >= 2 {
                    // Keep vertical borders the same muted color as the top
                    // and bottom, even on a selected row. The row background
                    // is still painted across the entire width.
                    let border_style = if selected { theme.border().bg(theme.popup_bg) }
                        else { theme.border() };
                    frame.render_widget(Paragraph::new(Line::from(vec![
                        Span::styled("│", border_style),
                        Span::styled(text, style),
                        Span::styled("│", border_style),
                    ])), row_area);
                } else {
                    frame.render_widget(Paragraph::new(Line::styled(text, style)), row_area);
                }
            }
        }
    }

    // No repeated instructions or keybinding cheat sheet in the wizard.
    // Discovery and authentication failures remain visible and actionable.
    if wizard.error || wizard.discovering {
        let status = if wizard.discovering {
            format!("{} Looking up secure email settings…", wizard.spinner())
        } else { wizard.message.clone() };
        let status_style = if wizard.error { theme.error() } else { theme.muted() };
        frame.render_widget(Paragraph::new(status).style(status_style),
            Rect::new(regions[2].x, regions[2].y, regions[2].width, 1));
    }
    let action = if identity_step { "Continue" }
        else if wizard.authenticated { "Save and open account" }
        else if wizard.login_failed { "Retry sign-in" }
        else { "Continue to Browser Sign-In" };
    if !wizard.authenticating && !wizard.discovering {
        let button_width = (action.chars().count() as u16).saturating_add(6).min(regions[2].width);
        let button_area = Rect::new(regions[2].x + regions[2].width.saturating_sub(button_width) / 2,
            regions[2].y + 2, button_width, 1);
        frame.render_widget(Paragraph::new(format!("  {action}  ")).alignment(Alignment::Center)
            .style(setup_button_style(&theme)), button_area);
    }
}

/// Consistent high-contrast success treatment without changing the theme's
/// accent, error, or currently selected row colors.
fn setup_success_style() -> Style {
    Style::default().fg(Color::Rgb(158, 236, 184))
        .bg(Color::Rgb(24, 69, 46))
        .add_modifier(Modifier::BOLD)
}

fn setup_button_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.popup_bg)
        .bg(theme.accent)
        .add_modifier(Modifier::BOLD)
}

fn destructive_button_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.selected_fg)
        .bg(theme.error)
        .add_modifier(Modifier::BOLD)
}

fn draw_startup_warnings(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let area = centered_rect(frame.area(), 85, 55);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Account configuration warnings ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.error());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let details = app.startup_warnings.iter().enumerate()
        .map(|(i, issue)| format!("{}. {}\n", i + 1, issue))
        .collect::<Vec<_>>().join("\n");
    frame.render_widget(Paragraph::new(format!(
        "Other usable accounts remain active. Fix config.toml and restart to re-enable affected accounts or sending.\n\n{details}\nEsc / q / ! to close"
    )).style(theme.text()).wrap(Wrap { trim: false }), inner);
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
                .constraints([Constraint::Min(1), Constraint::Length(super::preview_column_width(columns[1].width, app.preview_width_percent))])
                .split(columns[1]);
            draw_mailboxes(frame, app, columns[0]);
            draw_messages(frame, app, right[0]);
            draw_message(frame, app, right[1]);
        }
        (false, true) => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(super::preview_column_width(area.width, app.preview_width_percent))])
                .split(area);
            draw_messages(frame, app, columns[0]);
            draw_message(frame, app, columns[1]);
        }
    }
}

fn draw_mailboxes(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.set_mailbox_view_height(usize::from(area.height.max(1)));
    let theme = app.theme().clone();
    let account = app.account().expect("account exists");
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(if app.mailbox_resizing { theme.accent() } else { theme.border() });
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
fn visual_row_style(theme: &Theme) -> Style {
    Style::default().fg(theme.selected_fg).bg(theme.visual)
        .add_modifier(Modifier::BOLD)
}

/// Editor/reader current-line background without the list row's bold weight.
fn editor_current_line_style(theme: &Theme) -> Style {
    theme.text().bg(theme.popup_bg)
}

fn browser_row_style(theme: &Theme, mode: VisualSelectionStyle,
    visual: bool, marked: bool, active: bool, unread: bool) -> Style {
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

fn selection_check_style(theme: &Theme, mode: VisualSelectionStyle,
    visual: bool, marked: bool, active: bool) -> Style {
    if mode == VisualSelectionStyle::Row && (visual || marked) && !active {
        // On a filled row, retain contrast rather than purple-on-purple.
        Style::default().fg(theme.selected_fg).add_modifier(Modifier::BOLD)
    } else if visual {
        Style::default().fg(theme.visual).add_modifier(Modifier::BOLD)
    } else if marked {
        theme.accent()
    } else {
        Style::default() // Keep the parent row's unread/current-line color.
    }
}

/// Shared thin selection marker for attachments and the mail number column.
fn browser_gutter(theme: &Theme, visual: bool, marked: bool) -> Span<'static> {
    Span::styled(if visual || marked { "▍" } else { "▏" },
        if visual { Style::default().fg(theme.visual) }
        else if marked { theme.accent() } else { theme.border() })
}

/// Contacts share the mail marker's size and colors but never draw a
/// continuous line through unmarked rows.
fn contact_gutter(theme: &Theme, visual: bool, marked: bool) -> Span<'static> {
    if visual || marked { browser_gutter(theme, visual, marked) }
    else { Span::raw(" ") }
}

/// With the mailbox sidebar visible, the marker is on the *right* of its
/// divider, in the message number column; unselected rows add no extra rule.
fn mail_gutter(theme: &Theme, visual: bool, marked: bool, sidebar: bool) -> Span<'static> {
    let mut marker = browser_gutter(theme, visual, marked);
    if sidebar && !visual && !marked { marker.content = " ".into(); }
    marker
}

fn draw_messages(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
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

    app.set_message_view_height(usize::from(list_area.height.max(1)));
    let account = app.account().expect("account exists");
    let visible = account.visible_message_indices();
    let selected_position = visible
        .iter()
        .position(|&index| index == account.message_index);

    if visible.is_empty() {
        let message = if account.messages_loading {
            " Loading messages…".to_owned()
        } else if !search_query.is_empty() {
            format!(" No loaded messages match /{search_query}")
        } else {
            format!(" No messages in {}.", account.current_mailbox.as_deref().unwrap_or("mailbox"))
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
            if message.has_attachment(account.bodies.get(&message.id)) { 2 } else { 1 }
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
    let column_areas = Layout::horizontal(message_columns.clone())
        .spacing(1)
        .split(list_area);
    // Use the actual subject width to preserve wide emoji graphemes.
    let subject_width = column_areas.get(3).map_or(0, |area| usize::from(area.width));
    let rows = shown.iter().enumerate().map(|(row_position, &index)| {
        let message = &account.messages[index];
        let absolute_position = start + row_position;
        let number = relative_number(absolute_position, current_position);
        let visual_selected = visual_range.is_some_and(|(first, last)| {
            (first..=last).contains(&absolute_position)
        });
        let marked = app.message_marked.contains(&message.id);
        let is_selected = visual_selected || marked;
        let current = absolute_position == current_position;
        let unread_mark = if is_selected { "✓" } else if message.flags.contains(MessageFlags::SEEN) { " " } else { "●" };
        let attachment_mark = message.has_attachment(account.bodies.get(&message.id));
        let mark = Line::from(vec![
            Span::styled(unread_mark, selection_check_style(&theme,
                app.visual_selection_style, visual_selected, marked, current)),
            if in_junk {
                Span::styled("!", Style::default().fg(theme.error).add_modifier(Modifier::BOLD))
            } else { Span::raw(" ") },
            if attachment_mark {
                Span::styled("⧉", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))
            } else { Span::raw(" ") },
        ]);
        let sender = if message.from_name.is_empty() {
            message.from_address.as_str()
        } else {
            message.from_name.as_str()
        };
        let style = browser_row_style(&theme, app.visual_selection_style,
            visual_selected, marked, current, message.unread());
        // Borrow unchanged subjects and strings from the message. Own text
        // only when normalization or display-width truncation actually changes it.
        let subject_line = match subject_for_list_cow(&message.subject) {
            std::borrow::Cow::Borrowed(subject) =>
                Line::raw(truncate_display_width_cow(subject, subject_width)),
            std::borrow::Cow::Owned(normalized) =>
                Line::raw(truncate_display_width_cow(&normalized, subject_width).into_owned()),
        };
        let subject_cell = if attachment_mark {
            let subtitle = account.bodies.get(&message.id).map_or_else(
                || attachment_subtitle(&crate::mail::visible_attachment_hints(&message.attachment_names_hint)),
                |body| attachment_subtitle(&body.attachment_names),
            );
            Cell::from(Text::from(vec![
                subject_line,
                Line::styled(truncate_display_width(&subtitle, subject_width), theme.muted()),
            ]))
        } else {
            Cell::from(subject_line)
        };
        // Keep the mailbox border intact. The thin purple selection marker
        // occupies the first cell to its right, followed by aligned numbers.
        let number_line = Line::from(vec![
            mail_gutter(&theme, visual_selected, marked, app.sidebar_visible),
            Span::raw(format!("{number:>width$}", width = usize::from(number_width))),
        ]);
        let number_cell = if attachment_mark {
            let subtitle_gutter = Line::from(mail_gutter(
                &theme, visual_selected, marked, app.sidebar_visible));
            Cell::from(Text::from(vec![number_line, subtitle_gutter]))
        } else { Cell::from(number_line) };
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
            let visual = visual_range.is_some_and(|(first, last)|
                (first..=last).contains(&position));
            let marked = app.message_marked.contains(&message.id);
            let row_height = if message.has_attachment(account.bodies.get(&message.id)) { 2 } else { 1 };
            if (visual || marked) && y < list_area.bottom() {
                let current = position == current_position;
                let style = if current {
                    browser_row_style(&theme, app.visual_selection_style,
                        visual, marked, true, message.unread())
                } else if visual { visual_row_style(&theme) } else { theme.selected() };
                frame.render_widget(Block::default().style(style),
                    Rect::new(list_area.x, y, list_area.width,
                        row_height.min(list_area.bottom().saturating_sub(y))));
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
fn message_list_viewport(
    visible: &[usize],
    offset: usize,
    current_position: usize,
    height: usize,
    mut row_height: impl FnMut(usize) -> usize,
) -> (usize, usize) {
    if visible.is_empty() { return (0, 0); }
    let current = current_position.min(visible.len() - 1);
    let min_start = offset.min(visible.len() - 1).min(current);
    let mut start = current;
    let mut used = row_height(visible[current]);
    while start > min_start {
        let previous = row_height(visible[start - 1]);
        if used.saturating_add(previous) > height { break; }
        used += previous;
        start -= 1;
    }
    let mut end = start;
    used = 0;
    while end < visible.len() {
        let next = row_height(visible[end]);
        if used.saturating_add(next) > height { break; }
        used += next;
        end += 1;
    }
    if end == start { end = (start + 1).min(visible.len()); }
    (start, end)
}

/// Positions are indices in the filtered/sorted view, not message IDs.
fn message_list_visual_range(
    visible: &[usize],
    anchor: Option<usize>,
    current_position: Option<usize>,
) -> Option<(usize, usize)> {
    let anchor = anchor?;
    let current_position = current_position?;
    let anchor_position = visible.iter().position(|&index| index == anchor)?;
    Some((anchor_position.min(current_position), anchor_position.max(current_position)))
}

/// Attachment chips share the same left edge as the first subject character.
fn attachment_subtitle(names: &[String]) -> String {
    if names.is_empty() { "Attachment".to_owned() }
    else { names.iter().take(2).map(String::as_str).collect::<Vec<_>>().join(" · ") }
}

/// In compact mode only the sender email address is hidden when a display
/// name is available. Mail with no sender display name must remain identifiable.
fn sender_header_value(from_name: &str, from_address: &str, show_details: bool) -> String {
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
fn compact_recipient_header(value: &str) -> String {
    let mut recipients = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut angle_depth = 0usize;
    for (index, ch) in value.char_indices() {
        if escaped { escaped = false; continue; }
        if ch == '\\' && quoted { escaped = true; continue; }
        if ch == '"' { quoted = !quoted; continue; }
        if quoted { continue; }
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

fn compact_recipient(value: &str) -> String {
    let Some(start) = value.rfind('<') else { return value.to_owned(); };
    let Some(end) = value[start..].find('>').map(|end| start + end) else { return value.to_owned(); };
    if !value[start + 1..end].contains('@') { return value.to_owned(); }
    // Only discard the address when we have an actual name to show. Leave
    // e.g. <someone@example.com> and malformed input untouched.
    let name = value[..start].trim();
    if name.is_empty() { return value.to_owned(); }
    let unquoted = name.strip_prefix('"').and_then(|name| name.strip_suffix('"'));
    let name = unquoted.unwrap_or(name);
    let trailing = value[end + 1..].trim();
    format!("{name}{trailing}")
}

/// Only addresses and Message-ID collapse with `m`. From, To, Cc and Bcc
/// stay visible, as do all display names, subjects and dates.
fn reader_header_lines(
    message: &crate::model::MessageSummary,
    show_details: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let from = sender_header_value(&message.from_name, &message.from_address, show_details);
    let mut lines = vec![
        Line::from(vec![Span::styled("From: ", theme.accent()), Span::raw(from)]),
        Line::from(vec![Span::styled("Date: ", theme.accent()), Span::raw(
            if message.timestamp > 0 {
                chrono::DateTime::<chrono::Utc>::from_timestamp(message.timestamp, 0)
                    .map(|date| date.with_timezone(&chrono::Local)
                        .format("%a, %b %-d, %Y at %-I:%M %p %Z").to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            } else { "unknown".to_owned() }
        )]),
        Line::from(vec![Span::styled("To: ", theme.accent()), Span::raw(
            if message.to.is_empty() { "(not supplied)".to_owned() }
            else if show_details { message.to.clone() }
            else { compact_recipient_header(&message.to) }
        )]),
        Line::from(vec![
            Span::styled("Subject: ", theme.accent()),
            Span::raw(message.subject.clone()),
        ]),
    ];
    if !message.cc.is_empty() {
        lines.push(Line::from(vec![Span::styled("Cc: ", theme.accent()), Span::raw(if show_details { message.cc.clone() }
            else { compact_recipient_header(&message.cc) })]));
    }
    if !message.bcc.is_empty() {
        lines.push(Line::from(vec![Span::styled("Bcc: ", theme.accent()), Span::raw(if show_details { message.bcc.clone() }
            else { compact_recipient_header(&message.bcc) })]));
    }
    if show_details {
        if let Some(message_id) = &message.message_id {
            lines.push(Line::from(vec![
                Span::styled("Message-ID: ", theme.accent()),
                Span::raw(format!("<{message_id}>")),
            ]));
        }
    }
    lines
}

fn preview_split_border_style(theme: &Theme, resizing: bool) -> Style {
    if resizing { theme.accent() } else { theme.border() }
}

fn draw_message(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
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

    if app.account().and_then(|account| account.selected_message()).is_none() {
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
            reader_word_at(body.display_text(showing_html), reader_cursor, reader_column)
        })
        .flatten();

    let (lines, body_start, body_line_count, image_start) = {
        let account = app.account().expect("account exists");
        let message = account.selected_message().expect("message checked above");

        let mut lines = reader_header_lines(message, app.show_header_details, &theme);
        if let Some(body) = account.selected_body() {
            if body.attachment_count > 0 {
                let preview = body.attachment_names.iter().take(2).cloned().collect::<Vec<_>>().join("  ·  ");
                let overflow = body.attachment_names.len().saturating_sub(2);
                let label = if preview.is_empty() {
                    format!("{} attachment(s)", body.attachment_count)
                } else if overflow > 0 {
                    format!("{preview}  +{overflow} more")
                } else { preview };
                lines.push(Line::from(vec![
                    Span::styled("⧉  ", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
                    Span::styled(label, theme.text()),
                    Span::styled("   Ctrl-a: view", theme.muted()),
                ]));
            }
        } else if message.has_attachment_hint {
            let names = crate::mail::visible_attachment_hints(&message.attachment_names_hint)
                .into_iter().take(2).collect::<Vec<_>>().join("  ·  ");
            lines.push(Line::from(vec![Span::styled("⧉  ", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
                Span::styled(if names.is_empty() { "Attachments (Ctrl-a: browse)".to_owned() } else { names }, theme.text())]));
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
                    let rendered = html_lines.iter().map(|line| line.spans.iter()
                        .map(|span| span.text.as_str()).collect::<String>()).collect::<Vec<_>>();
                    let texts = rendered.iter().map(String::as_str).collect::<Vec<_>>();
                    let diff_lines = classify_patch_lines(&texts);
                    lines.extend(html_lines.iter().enumerate().map(|(line_index, line)| {
                        rich_html_line(
                            line,
                            line_index,
                            body_line_count,
                            body_width,
                            reader_cursor,
                            reader_column,
                            reader_visual,
                            reader_active,
                            &theme,
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
                            reader_cursor,
                            reader_column,
                            reader_visual,
                            reader_active,
                            &theme,
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
                        reader_cursor,
                        reader_column,
                        reader_visual,
                        reader_active,
                        &theme,
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
    let visual_cursor_row = if reader_active && body_line_count > 0
        && app.reader_needs_scroll_follow()
    {
        reader_visual_cursor_row(
            &lines,
            body_start,
            reader_cursor.min(body_line_count - 1),
            content_area.width,
        )
    } else { None };
    let paragraph = Paragraph::new(Text::from(lines))
        .style(theme.text())
        .wrap(Wrap { trim: false });
    let visual_lines = paragraph.line_count(content_area.width.max(1));
    let viewport_height = usize::from(content_area.height.max(1));
    let max_scroll =
        u16::try_from(visual_lines.saturating_sub(viewport_height)).unwrap_or(u16::MAX);
    app.set_reader_layout(body_line_count, viewport_height, max_scroll, visual_cursor_row);
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
fn reader_visual_cursor_row(
    lines: &[Line<'_>], body_start: usize, cursor_line: usize, width: u16,
) -> Option<usize> {
    let index = body_start.checked_add(cursor_line)?;
    let current = lines.get(index)?;
    let width = width.max(1);
    let preceding_rows = if index == 0 { 0 } else {
        Paragraph::new(Text::from(lines[..index].to_vec()))
            .wrap(Wrap { trim: false })
            .line_count(width)
    };

    // The rendered block cursor is a white-on-black cell. Measuring the prefix
    // up to that cell follows the cursor even when a single logical line spans
    // several screen rows. When no cursor cell is present, use its first row.
    let mut prefix = Vec::new();
    for span in &current.spans {
        if span.style == reader_cursor_style() { break; }
        prefix.push(span.clone());
    }
    let within_line = if prefix.is_empty() { 0 } else {
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
enum PatchLineKind { Meta, Hunk, Add, Remove }

fn strip_mail_quotes(mut text: &str) -> &str {
    while let Some(after) = text.strip_prefix('>') {
        text = after.strip_prefix(' ').unwrap_or(after);
    }
    text
}

fn patch_hunk_counts(line: &str) -> Option<(usize, usize)> {
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

fn is_patch_source(text: &str) -> bool {
    text.starts_with("--- a/") || text == "--- /dev/null"
}
fn is_patch_target(text: &str) -> bool {
    text.starts_with("+++ b/") || text == "+++ /dev/null"
}

fn classify_patch_lines(lines: &[&str]) -> Vec<Option<PatchLineKind>> {
    let mut result = Vec::with_capacity(lines.len());
    let mut in_patch = false;
    let mut in_hunk = false;
    let (mut old_left, mut new_left) = (0usize, 0usize);
    for (index, line) in lines.iter().enumerate() {
        let text = strip_mail_quotes(line);
        let next_is_target = lines.get(index + 1)
            .is_some_and(|next| is_patch_target(strip_mail_quotes(next)));
        let kind = if text.starts_with("diff --git ")
            || is_patch_source(text) && next_is_target
        {
            in_patch = true;
            in_hunk = false;
            Some(PatchLineKind::Meta)
        } else if !in_patch {
            None
        } else if text.starts_with("index ") || is_patch_source(text)
            || is_patch_target(text) || text.starts_with("new file mode ")
            || text.starts_with("deleted file mode ")
            || text.starts_with("old mode ") || text.starts_with("new mode ")
            || text.starts_with("similarity index ")
            || text.starts_with("rename from ") || text.starts_with("rename to ")
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
            if old_left == 0 && new_left == 0 { in_hunk = false; }
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
fn reader_active_diff_bg(theme: &Theme, _diff_bg: Color) -> Color {
    theme.popup_bg
}

fn diff_line_style(kind: PatchLineKind, theme: &Theme) -> Style {
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
        PatchLineKind::Hunk => Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        PatchLineKind::Add => Style::default().fg(theme.unread).bg(bg(theme.unread)),
        PatchLineKind::Remove => Style::default().fg(theme.error).bg(bg(theme.error)),
    }
}

fn plain_reader_line(
    text: &str,
    line_index: usize,
    line_count: usize,
    content_width: usize,
    reader_cursor: usize,
    reader_column: usize,
    reader_visual: Option<ReaderSelection>,
    reader_active: bool,
    theme: &crate::theme::Theme,
    diff_kind: Option<PatchLineKind>,
    illuminate_word: Option<&str>,
) -> Line<'static> {
    let diff = diff_kind.map(|kind| diff_line_style(kind, theme));
    let base = diff.unwrap_or_else(|| {
        if is_quoted_line(text) { Style::default().fg(theme.muted) }
        else { theme.text() }
    });
    let illuminate_ranges = illuminate_word
        .map(|word| word_match_ranges(text, word))
        .unwrap_or_default();
    let mut spans = reader_gutter(
        line_index,
        line_count,
        reader_cursor,
        reader_active,
        theme,
    );
    spans.extend(reader_fragments(
        text,
        base,
        0,
        text.chars().count(),
        line_index,
        reader_cursor,
        reader_column,
        reader_visual,
        reader_active,
        theme,
        &illuminate_ranges,
    ));
    pad_reader_line(
        &mut spans,
        text.chars().count(),
        content_width,
        line_index,
        reader_cursor,
        reader_column,
        reader_visual,
        reader_active,
        theme,
        diff.and_then(|style| style.bg),
    );
    Line::from(spans)
}

fn rich_html_line(
    line: &HtmlLine,
    line_index: usize,
    line_count: usize,
    content_width: usize,
    reader_cursor: usize,
    reader_column: usize,
    reader_visual: Option<ReaderSelection>,
    reader_active: bool,
    theme: &crate::theme::Theme,
    diff_kind: Option<PatchLineKind>,
    illuminate_word: Option<&str>,
) -> Line<'static> {
    let mut spans = reader_gutter(
        line_index,
        line_count,
        reader_cursor,
        reader_active,
        theme,
    );
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
    let quoted = is_quoted_line(&line_text);
    let diff = diff_kind.map(|kind| diff_line_style(kind, theme));
    let mut column = 0usize;
    for span in &line.spans {
        let mut style = rich_html_style(&span.style, &span.text, theme);
        if let Some(diff) = diff {
            style = style.fg(diff.fg.unwrap_or(theme.foreground));
            if let Some(bg) = diff.bg { style = style.bg(bg); }
        }
        else if quoted { style = style.fg(theme.muted); }
        spans.extend(reader_fragments(
            &span.text,
            style,
            column,
            line_len,
            line_index,
            reader_cursor,
            reader_column,
            reader_visual,
            reader_active,
            theme,
            &illuminate_ranges,
        ));
        column = column.saturating_add(span.text.chars().count());
    }
    pad_reader_line(
        &mut spans,
        column,
        content_width,
        line_index,
        reader_cursor,
        reader_column,
        reader_visual,
        reader_active,
        theme,
        diff.and_then(|style| style.bg),
    );
    Line::from(spans)
}

fn reader_gutter(
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

fn reader_fragments(
    text: &str,
    style: Style,
    global_start: usize,
    line_len: usize,
    line_index: usize,
    reader_cursor: usize,
    reader_column: usize,
    reader_visual: Option<ReaderSelection>,
    reader_active: bool,
    theme: &crate::theme::Theme,
    illuminate_ranges: &[(usize, usize)],
) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let len = text.chars().count();
    let global_end = global_start.saturating_add(len);
    let mut boundaries = vec![global_start, global_end];

    let selected_columns = reader_visual
        .and_then(|selection| selection.columns_for_line(line_index, line_len));
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
            let bg = fragment_style.bg
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

fn reader_word_at(text: &str, line_index: usize, column: usize) -> Option<String> {
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

fn word_match_ranges(text: &str, word: &str) -> Vec<(usize, usize)> {
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

fn is_reader_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn reader_illuminate_bg(theme: &Theme) -> Color {
    match (theme.popup_bg, theme.border) {
        (Color::Rgb(sr, sg, sb), Color::Rgb(br, bg, bb)) => Color::Rgb(
            mix_channel(sr, br, 82),
            mix_channel(sg, bg, 82),
            mix_channel(sb, bb, 82),
        ),
        _ => Color::Rgb(59, 66, 97),
    }
}

fn mix_channel(from: u8, to: u8, toward_to_percent: u16) -> u8 {
    let toward_to_percent = toward_to_percent.min(100);
    let toward_from_percent = 100 - toward_to_percent;
    ((u16::from(from) * toward_from_percent
        + u16::from(to) * toward_to_percent
        + 50)
        / 100) as u8
}

fn reader_cursor_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn pad_reader_line(
    spans: &mut Vec<Span<'static>>,
    content_len: usize,
    content_width: usize,
    line_index: usize,
    reader_cursor: usize,
    reader_column: usize,
    reader_visual: Option<ReaderSelection>,
    reader_active: bool,
    theme: &crate::theme::Theme,
    diff_background: Option<Color>,
) {
    if !reader_active && diff_background.is_none() {
        return;
    }
    let visual_line_selected = reader_visual.is_some_and(|selection| {
        selection.start.line <= line_index && line_index <= selection.end.line
    });
    let linewise_visual = reader_visual.is_some_and(|selection| {
        selection.start.column == 0 && selection.end.column == usize::MAX
    });
    if (line_index != reader_cursor || !reader_active) && !visual_line_selected
        && diff_background.is_none() {
        return;
    }
    let mut style = theme.text();
    if let Some(bg) = diff_background { style = style.bg(bg); }
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

fn char_slice(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn rich_html_style(
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
        rendered = rendered
            .fg(theme.unread)
            .add_modifier(Modifier::BOLD);
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

fn is_box_drawing_span(text: &str) -> bool {
    let mut saw_box = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if matches!(
            ch,
            '─' | '│' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼'
                | '╭' | '╮' | '╰' | '╯' | '═' | '║' | '╔' | '╗' | '╚' | '╝'
                | '╠' | '╣' | '╦' | '╩' | '╬'
        ) {
            saw_box = true;
        } else {
            return false;
        }
    }
    saw_box
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = app.theme().clone();
    let mut spans = Vec::new();
    if let Some(composer) = app.composer.as_ref().filter(|_| app.contacts_modal.is_none()) {
        spans.push(Span::styled(format!("[{}] ",
            app.account().map_or("Compose", |account| account.name.as_str())), theme.accent()));
        spans.push(Span::styled(format!(" {} ", composer.mode.label()),
            compose_mode_style(composer.mode, &theme)));
        spans.push(Span::raw(" "));
        if !app.status.text.is_empty() {
            spans.push(Span::styled(app.status.text.as_str(),
                if app.status.is_error { theme.error() } else { theme.muted() }));
        }
    } else if let Some(account) = app.account() {
        spans.push(Span::styled(format!("[{}] ", account.name), theme.accent()));
        let mode = if app.message_open {
            if app.reader_visual_anchor.is_some() { "VISUAL" } else { "NORMAL" }
        } else if account.search_editing {
            "SEARCH"
        } else if app.visual_anchor.is_some() {
            "VISUAL"
        } else { "NORMAL" };
        spans.push(Span::styled(format!(" {mode} "), mode_style(mode, &theme)));
        spans.push(Span::raw(" "));
        if account.search_editing {
            let matches = account.visible_message_indices().len();
            spans.push(Span::styled(
                format!("{matches} match{}  ", if matches == 1 { "" } else { "es" }),
                theme.muted(),
            ));
        } else if !account.search_query.is_empty() {
            spans.push(Span::styled(format!("filter:/{}  ", account.search_query), theme.accent()));
        }
        if account.search_editing {
            // The editable query is already rendered in the message-list search
            // header. Avoid echoing the status text below it a second time.
        } else if account.connecting {
            spans.push(Span::raw("Connecting…"));
        } else if account.mailboxes_loading {
            spans.push(Span::raw("Loading mailboxes…"));
        } else if account.messages_loading {
            spans.push(Span::raw("Loading messages…"));
        } else if account.is_loading_body() {
            spans.push(Span::raw("Loading message…"));
        } else {
            let style = if app.status.is_error { theme.error() } else { theme.text() };
            spans.push(Span::styled(app.status.text.as_str(), style));
        }
        if !app.message_marked.is_empty() {
            spans.push(Span::styled(format!("    {} marked", app.message_marked.len()), theme.accent()));
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
    let indicator_width = super::NOTIFICATION_INDICATOR_WIDTH.min(area.width);
    let status_area = Rect::new(area.x, area.y, area.width - indicator_width, area.height);
    if let Some(modal) = app.contacts_modal.as_ref() {
        // Contacts use the same global status row and mode slot as mail.
        // The contact count belongs inside the Contacts window, not here.
        let mut mode_spans = contact_status_spans(&theme,
            app.account().map(|account| account.name.as_str()), modal);
        mode_spans.extend(contact_footer_details(&theme, modal));
        frame.render_widget(Paragraph::new(Line::from(mode_spans)).style(theme.status()), status_area);
    } else {
        frame.render_widget(Paragraph::new(Line::from(spans)).style(theme.status()), status_area);
    }
    if indicator_width == super::NOTIFICATION_INDICATOR_WIDTH {
        let (glyph, style) = notification_indicator(app, &theme);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(glyph, style)))
                .alignment(Alignment::Right)
                .style(theme.status()),
            Rect::new(area.right().saturating_sub(indicator_width), area.y, indicator_width, area.height),
        );
    }
}

/// Never guess whether the terminal has a Nerd Font installed. Users opt in
/// with `ui.notification_icon_style = "nerd"`; Unicode is the safe default.
fn notification_indicator(app: &App, theme: &Theme) -> (String, Style) {
    let enabled = app.notifications_enabled();
    let glyph = match (app.notification_icon_style(), enabled) {
        (crate::config::NotificationIconStyle::Nerd, true) => "󰂚",
        (crate::config::NotificationIconStyle::Nerd, false) => "󰂛",
        (crate::config::NotificationIconStyle::Unicode, true) => "●",
        (crate::config::NotificationIconStyle::Unicode, false) => "○",
    };
    let count = if enabled { app.new_mail_count() } else { 0 };
    let label = if count > 99 { format!("{glyph} 99+") }
        else if count > 0 { format!("{glyph} {count}") }
        else { glyph.to_owned() };
    (format!(" {label} "), if enabled { theme.accent() } else { theme.muted() })
}

fn draw_search_header(frame: &mut Frame<'_>, area: Rect, query: &str, app: &App) {
    draw_search_prompt(frame, area, query, app.theme());
}

/// Reused by the mail browser and Contacts so / opens the same dedicated
/// search row without borrowing App while the contacts modal is mutable.
fn draw_search_prompt(frame: &mut Frame<'_>, area: Rect, query: &str, theme: &Theme) {
    let prompt = if query.is_empty() { "type to search…" } else { query };
    let style = if query.is_empty() { theme.muted() } else { theme.text() };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" / ", theme.accent()),
            Span::styled(prompt, style),
        ])),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, app: &App) {
    let attachment_help = app.attachment_picker_visible;
    let entries = if attachment_help { ATTACHMENT_HELP_ENTRIES } else { HELP_ENTRIES };
    let matches = app.help_matches();
    draw_help_palette(frame, app.theme(),
        if attachment_help { " Attachment keybindings " } else { " Keybindings " },
        entries, &app.help_query, &matches, app.help_selected, app.help_scroll);
}

// At a standard 80-column terminal, the 86-column modal is clipped to 80.
// After borders (2), the category (13), key (27) and two gaps (2), 36
// terminal cells remain for each description. On narrow screens, shrink
// category/key columns instead of allowing description cells to overflow.
const HELP_CATEGORY_COLUMNS: u16 = 13;
const HELP_KEY_COLUMNS: u16 = 27;

fn help_columns(inner_width: u16) -> (u16, u16, u16) {
    let category = HELP_CATEGORY_COLUMNS.min(inner_width / 4);
    let key = HELP_KEY_COLUMNS.min(inner_width.saturating_sub(category + 2) / 2);
    let description = inner_width.saturating_sub(category + key + 2);
    (category, key, description)
}

/// Identical help table, search field, highlighting and footer for mail and contacts.
fn draw_help_palette(
    frame: &mut Frame<'_>, theme: &Theme, title: &str, entries: &[HelpEntry],
    query: &str, matches: &[usize], selected: usize, scroll: usize,
) {
    let area = centered_rect(frame.area(), 86, 70);
    frame.render_widget(Clear, area);
    let block = Block::default().title(title)
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2), Constraint::Min(1), Constraint::Length(1),
    ]).split(inner);
    let prompt = if query.is_empty() { "type to search…" } else { query };
    frame.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(" / ", theme.accent()),
        Span::styled(prompt, if query.is_empty() { theme.muted() } else { theme.text() }),
    ])), chunks[0]);

    let visible = usize::from(chunks[1].height.max(1));
    let max_start = matches.len().saturating_sub(visible);
    let selected_position = matches.iter().position(|&i| i == selected).unwrap_or(0);
    let start = scroll.max(selected_position.saturating_sub(visible.saturating_sub(1)))
        .min(selected_position).min(max_start);
    let shown: Vec<usize> = matches.iter().copied().skip(start).take(visible).collect();
    if shown.is_empty() {
        frame.render_widget(Paragraph::new(" No matching keybindings").style(theme.muted()), chunks[1]);
    } else {
        let (category_width, key_width, description_width) = help_columns(chunks[1].width);
        let rows = shown.iter().map(|&index| {
            let entry = entries[index];
            Row::new(vec![
                Cell::from(truncate_display_width(entry.category, usize::from(category_width))),
                Cell::from(truncate_display_width(entry.key, usize::from(key_width))),
                Cell::from(truncate_display_width(entry.description, usize::from(description_width))),
            ])
        });
        let table = Table::new(rows, [
            Constraint::Length(category_width), Constraint::Length(key_width),
            Constraint::Length(description_width),
        ]).style(theme.text()).column_spacing(1).row_highlight_style(theme.selected());
        let selected = shown.iter().position(|&index| index == selected);
        let mut state = TableState::default().with_selected(selected);
        frame.render_stateful_widget(table, chunks[1], &mut state);
    }
    frame.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(" ↑↓", theme.accent()),
        Span::styled(" navigate   ", theme.muted()),
        Span::styled("type", theme.accent()),
        Span::styled(" filter   ", theme.muted()),
        Span::styled("Backspace", theme.accent()),
        Span::styled(" edit   ", theme.muted()),
        Span::styled("Esc/?", theme.accent()),
        Span::styled(" close", theme.muted()),
    ])), chunks[2]);
}

fn draw_theme_picker(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let area = centered_rect(frame.area(), 52, 46);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Themes ")
        .borders(Borders::ALL).border_type(theme.border_type)
        .border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    draw_search_header(frame, chunks[0], &app.theme_query, app);

    let matches = app.theme_matches();
    let visible = usize::from(chunks[1].height.max(1));
    let max_start = matches.len().saturating_sub(visible);
    let start = app.theme_scroll.min(max_start);
    let shown: Vec<usize> = matches.iter().copied().skip(start).take(visible).collect();
    if shown.is_empty() {
        frame.render_widget(Paragraph::new(" No matching themes").style(theme.muted()), chunks[1]);
    } else {
        let rows = shown.iter().map(|index| {
            let candidate = &app.themes[*index];
            let marker = if *index == app.theme_selected { "●" } else { " " };
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

/// Received attachment browser: Yazi-like relative numbers and marks on the
/// left, automatic inline preview on the right, mouse/keyboard actions below.
fn draw_attachments(frame: &mut Frame<'_>, app: &mut App) {
    let theme = app.theme().clone();
    let (area, list_area, preview_area, _) = super::attachment_regions(frame.area());
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
fn draw_attachment_destination_picker(frame: &mut Frame<'_>, app: &App) {
    let Some(picker) = app.attachment_destination_picker.as_ref() else { return; };
    let (area, parent_area, files_area, preview_area, footer) =
        super::attachment_destination_regions(frame.area());
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

fn attachment_display_name(name: &str) -> String {
    name.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).collect()
}

fn attachment_human_size(bytes: usize) -> String {
    if bytes < 1024 { format!("{bytes} B") }
    else if bytes < 1024 * 1024 { format!("{:.1} KiB", bytes as f64 / 1024.0) }
    else { format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)) }
}

fn attachment_text_preview(file: &crate::model::OutgoingAttachment) -> Option<String> {
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

/// The exit prompt uses the same two-button layout as deletion reviews. Yes
/// saves the draft; No discards it. Esc still returns to editing, without an
/// extra on-screen instruction competing with the actions.
fn draw_compose_exit_prompt(frame: &mut Frame<'_>, theme: &Theme) {
    let viewport = frame.area();
    let width = viewport.width.min(62);
    let height = viewport.height.min(7);
    let area = Rect::new(
        viewport.x + viewport.width.saturating_sub(width) / 2,
        viewport.y + viewport.height.saturating_sub(height) / 2,
        width, height,
    );
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Save unsent email? ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.border());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 10 || inner.height < 3 { return; }
    let prompt = "Save this email to Drafts before closing?";
    frame.render_widget(Paragraph::new(truncate_display_width(prompt, usize::from(inner.width)))
        .alignment(Alignment::Center).style(theme.text()),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1));
    let buttons = Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 2);
    draw_confirmation_buttons(frame, theme, buttons,
        setup_button_style(theme), destructive_button_style(theme));
}

fn draw_remove_account_confirmation(frame: &mut Frame<'_>, app: &App) {
    let Some(name) = app.remove_account_confirmation.as_ref() else { return; };
    let theme = app.theme();
    let area = centered_rect(frame.area(), 72, 27);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Remove local email account ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.error());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(format!(
        "Remove account {:?} from aerc-rs?\n\nThis edits only config.toml. \
         No messages or folders on the server are deleted. Cached OAuth tokens\nare not revoked.\n\n[y] Remove   [n/Esc] Cancel", name))
        .style(theme.text()).wrap(Wrap { trim: false }), inner);
}

/// Shared Yazi-style review: simple, aligned item numbers without a visual
/// selection gutter. The list is for reviewing, not selecting, its contents.
fn review_item_line(number: usize, digits: usize, item: &str, width: usize,
    theme: &Theme) -> Line<'static> {
    let prefix = format!(" {number:>digits$}  ");
    let available = width.saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
    Line::from(vec![
        Span::styled(prefix, theme.muted()),
        Span::styled(truncate_display_width(item, available), theme.text()),
    ])
}

/// Shared horizontal rule and centered Yes/No buttons for confirmation popups.
/// The calling dialog decides which action is destructive.
fn draw_confirmation_buttons(frame: &mut Frame<'_>, theme: &Theme, area: Rect,
    yes_style: Style, no_style: Style) {
    if area.height < 2 || area.width == 0 { return; }
    frame.render_widget(Paragraph::new("─".repeat(usize::from(area.width)))
        .style(theme.border()), Rect::new(area.x, area.y, area.width, 1));
    let gap = 3u16;
    let yes_width = 11u16;
    let no_width = 11u16;
    let total = yes_width + no_width + gap;
    let x = area.x + area.width.saturating_sub(total) / 2;
    let y = area.y.saturating_add(1);
    frame.render_widget(Paragraph::new(" [Y]es ").alignment(Alignment::Center)
        .style(yes_style), Rect::new(x, y, yes_width.min(area.width), 1));
    if area.width > yes_width + gap {
        frame.render_widget(Paragraph::new(" [N]o ").alignment(Alignment::Center)
            .style(no_style), Rect::new(x + yes_width + gap, y,
                no_width.min(area.right().saturating_sub(x + yes_width + gap)), 1));
    }
}

/// Shared Yazi-style review: visible item list, scroll position, and the same
/// filled action treatment as the setup wizard. No action is taken by drawing.
fn draw_review_dialog(frame: &mut Frame<'_>, theme: &Theme, title: &str,
    explanation: &str, items: &[String], scroll: usize) {
    let area = centered_rect(frame.area(), 86, 72);
    frame.render_widget(Clear, area);
    let block = Block::default().title(format!(" {title} "))
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.error());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 10 || inner.height < 5 { return; }
    let chunks = Layout::vertical([
        Constraint::Length(2), Constraint::Min(1),
        Constraint::Length(1), Constraint::Length(2),
    ]).split(inner);
    frame.render_widget(Paragraph::new(truncate_display_width(explanation, usize::from(chunks[0].width)))
        .style(theme.text()), chunks[0]);
    let capacity = usize::from(chunks[1].height);
    let start = scroll.min(items.len().saturating_sub(capacity));
    let digits = items.len().max(1).to_string().len();
    for (offset, item) in items.iter().skip(start).take(capacity).enumerate() {
        let row = Rect::new(chunks[1].x, chunks[1].y + offset as u16, chunks[1].width, 1);
        frame.render_widget(Paragraph::new(review_item_line(start + offset + 1, digits,
            item, usize::from(row.width), theme)), row);
    }
    frame.render_widget(Paragraph::new(format!(
        " {} items  ·  {}–{}",
        items.len(), if items.is_empty() { 0 } else { start + 1 }, (start + capacity).min(items.len()),
    )).style(theme.muted()), chunks[2]);
    draw_confirmation_buttons(frame, theme, chunks[3],
        destructive_button_style(theme), theme.current_line());
}

fn draw_confirmation(frame: &mut Frame<'_>, app: &App) {
    let Some(confirmation) = app.confirmation.as_ref() else { return; };
    draw_review_dialog(frame, app.theme(), "Review mail action", &confirmation.prompt,
        &confirmation.items, confirmation.review.position);
}

fn draw_detach_confirmation(frame: &mut Frame<'_>, app: &App) {
    let Some(confirmation) = app.detach_confirmation.as_ref() else { return; };
    let names = confirmation.paths.iter().map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    draw_review_dialog(frame, app.theme(), "Detach attachments",
        "Remove these attachments from the outgoing message?", &names,
        confirmation.review.position);
}

// A full outgoing-message preview, using the same colored headers and rich
// HTML renderer as the message reader. Enter sends; Esc returns to the editor.
fn draw_send_confirmation(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme();
    let Some(composer) = app.composer.as_ref().filter(|composer| composer.confirm_send) else { return; };
    let area = centered_rect(frame.area(), frame.area().width.saturating_sub(4).min(100), 88);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Review outgoing email ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 8 { return; }
    let footer_y = inner.bottom().saturating_sub(1);
    let headers = [
        ("From", app.account().map(|account| account.from.as_str()).unwrap_or("")),
        ("To", composer.to.as_str()),
        ("Cc", composer.cc.as_str()),
        ("Bcc", composer.bcc.as_str()),
        ("Subject", composer.subject.as_str()),
    ];
    let mut y = inner.y;
    for (label, value) in headers {
        if y >= footer_y.saturating_sub(3) { break; }
        let value_style = if value.trim().is_empty() { theme.muted() } else { theme.accent() };
        let line = Line::from(vec![
            Span::styled(format!(" {label:<8} "), theme.muted()),
            Span::styled(if value.trim().is_empty() { "—" } else { value }, value_style),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
        y += 1;
    }
    let mut filenames = compose_attachment_names(composer);
    filenames.extend(composer.preview_forwarded_attachments.iter().cloned());
    let files = if filenames.is_empty() { "none".to_owned() } else { filenames.join(" · ") };
    let attachment_text = format!(" Attachments ({})  {files}", filenames.len());
    let lines = clipped_wrap_text(&attachment_text, usize::from(inner.width), 2);
    let attachment_height = (lines.len() as u16).min(2);
    if y.saturating_add(attachment_height) < footer_y.saturating_sub(1) {
        frame.render_widget(Paragraph::new(lines.into_iter().map(|line| Line::styled(line,
            if !filenames.is_empty() { theme.accent() } else { theme.muted() })).collect::<Vec<_>>()),
            Rect::new(inner.x, y, inner.width, attachment_height));
        y += attachment_height;
    }
    if y >= footer_y { return; }
    frame.render_widget(Paragraph::new(Line::from("─".repeat(usize::from(inner.width))))
        .style(theme.border()), Rect::new(inner.x, y, inner.width, 1));
    y += 1;
    let content_height = footer_y.saturating_sub(y);
    if content_height > 0 {
        let body = if composer.preview_html {
            composer.preview_rich_lines.as_ref().map(|lines| Text::from(lines.iter().map(|line| {
                Line::from(line.spans.iter().map(|span| {
                    Span::styled(span.text.clone(), rich_html_style(&span.style, &span.text, theme))
                }).collect::<Vec<_>>())
            }).collect::<Vec<_>>()))
        } else { None };
        let body = body.unwrap_or_else(|| Text::from(composer.body.clone()));
        let max_scroll = body.lines.len().saturating_sub(usize::from(content_height));
        let scroll = composer.preview_scroll.min(max_scroll);
        frame.render_widget(Paragraph::new(body).style(theme.text())
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
            Rect::new(inner.x.saturating_add(1), y, inner.width.saturating_sub(2), content_height));
    }
    let mode = if composer.preview_html && composer.preview_rich_lines.is_some() { "HTML" } else { "Plain" };
    let footer = Line::from(vec![
        Span::styled(" Enter ", Style::default().bg(theme.accent).fg(Color::Black).add_modifier(Modifier::BOLD)),
        Span::styled(" Send   ", theme.text()),
        Span::styled("Esc", theme.accent()), Span::styled(" Edit   ", theme.muted()),
        Span::styled("t", theme.accent()), Span::styled(format!(" {mode}   "), theme.muted()),
        Span::styled("O", theme.accent()), Span::styled(" Browser   ", theme.muted()),
        Span::styled("j/k", theme.accent()), Span::styled(" Scroll", theme.muted()),
    ]);
    frame.render_widget(Paragraph::new(footer), Rect::new(inner.x, footer_y, inner.width, 1));
}

fn compose_attachment_names(composer: &crate::app::Composer) -> Vec<String> {
    let mut names = composer.attachment_paths.iter().map(|path| path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())).collect::<Vec<_>>();
    names.extend(composer.attachments.split(';').map(str::trim).filter(|path| !path.is_empty())
        .map(|path| std::path::Path::new(path).file_name()
            .map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_owned())));
    names
}

/// Wrap on word boundaries, and truncate only if the available rows run out.
/// This also handles one single filename longer than the entire line.
fn clipped_wrap_text(value: &str, width: usize, max_rows: usize) -> Vec<String> {
    if width == 0 || max_rows == 0 { return Vec::new(); }
    let mut rows = vec![String::new()];
    let mut clipped = false;
    for word in value.split_whitespace() {
        let mut word = word.to_owned();
        if UnicodeWidthStr::width(word.as_str()) > width {
            word = truncate_display_width(&word, width);
        }
        let last = rows.last_mut().expect("at least one row");
        let separator = if last.is_empty() { "" } else { " " };
        if UnicodeWidthStr::width(last.as_str()) + separator.len() + UnicodeWidthStr::width(word.as_str()) <= width {
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
            if UnicodeWidthStr::width(clipped_row.as_str()) + UnicodeWidthStr::width(grapheme) > budget { break; }
            clipped_row.push_str(grapheme);
        }
        *last = clipped_row;
        last.push_str(if width == 1 { "…" } else { " …" });
    }
    rows
}

fn compose_picker_rows(
    frame: &mut Frame<'_>, rect: Rect, entries: &[crate::app::ComposeFileEntry],
    selected: Option<usize>, show_numbers: bool, attachments: &[std::path::PathBuf],
    visual: Option<(usize, usize)>, mode: VisualSelectionStyle, theme: &Theme,
) {
    if rect.width == 0 || rect.height == 0 { return; }
    if entries.is_empty() {
        frame.render_widget(Paragraph::new(" Empty directory ").style(theme.muted()), rect);
        return;
    }
    let selected_index = selected.unwrap_or(0).min(entries.len().saturating_sub(1));
    // The selection stays near the middle, like the lazygitrs/Yazi file picker.
    let start = selected_index.saturating_sub(usize::from(rect.height) / 2)
        .min(entries.len().saturating_sub(usize::from(rect.height)));
    let end = start.saturating_add(usize::from(rect.height)).min(entries.len());
    let digits = entries.len().to_string().len();
    let mut items = Vec::new();
    for (index, entry) in entries.iter().enumerate().take(end).skip(start) {
        let is_selected = selected == Some(index);
        let number = if show_numbers {
            let value = if is_selected { index + 1 } else { index.abs_diff(selected_index) };
            format!("{value:>digits$} ")
        } else { String::new() };
        let visually_selected = visual.is_some_and(|(a, b)| (a.min(b)..=a.max(b)).contains(&index));
        let marker = if entry.is_directory { "󰉋 " }
            else if attachments.contains(&entry.path) { "✓ " }
            else { "  " };
        let style = if entry.is_directory { theme.accent() }
            else if attachments.contains(&entry.path) { theme.unread() }
            else { theme.text() };
        let left_width = UnicodeWidthStr::width(number.as_str()) + UnicodeWidthStr::width(marker) + 1;
        let available = usize::from(rect.width).saturating_sub(left_width);
        let name = truncate_display_width(&entry.name, available);
        let line = Line::from(vec![
            browser_gutter(theme, visually_selected, false),
            Span::styled(number, if is_selected { theme.accent() } else { theme.muted() }),
            Span::styled(marker.to_owned(), style),
            Span::styled(name, style),
        ]);
        let item = ListItem::new(line).style(browser_row_style(theme, mode,
            visually_selected, false, is_selected, false));
        items.push(item);
    }
    frame.render_widget(List::new(items), rect);
}

fn draw_compose_file_picker(frame: &mut Frame<'_>, app: &mut App) {
    let area = centered_rect(frame.area(), frame.area().width.saturating_sub(4).min(110), 82);
    let inner_width = area.width.saturating_sub(2);
    let left_width = (inner_width * 24 / 100).max(12).min(inner_width);
    let middle_width = (inner_width * 34 / 100).max(18).min(inner_width.saturating_sub(left_width));
    let preview_width = inner_width.saturating_sub(left_width + middle_width).saturating_sub(2);
    let preview_height = area.height.saturating_sub(3);
    app.ensure_compose_file_preview(preview_width, preview_height);
    let Some(composer) = app.composer.as_ref() else { return; };
    let Some(picker) = composer.file_picker.as_ref() else { return; };
    let theme = app.theme();
    frame.render_widget(Clear, area);
    let block = Block::default().title(format!(" Attach files · {} selected ", composer.attachment_count()))
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 5 || inner.width < 12 { return; }
    frame.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(" 󰉋 ", theme.accent()),
        Span::styled(picker.directory.display().to_string(), theme.text().add_modifier(Modifier::BOLD)),
    ])), Rect::new(inner.x, inner.y, inner.width, 1));
    let search_height = u16::from(picker.search_query.is_some());
    let content = Rect::new(inner.x, inner.y + 1, inner.width,
        inner.height.saturating_sub(1 + search_height));
    let left_width = (content.width * 24 / 100).max(12).min(content.width);
    let middle_width = (content.width * 34 / 100).max(18).min(content.width.saturating_sub(left_width));
    let right_width = content.width.saturating_sub(left_width + middle_width);
    let parent_area = Rect::new(content.x, content.y, left_width, content.height);
    let files_area = Rect::new(content.x + left_width, content.y, middle_width, content.height);
    let preview_area = Rect::new(files_area.right(), content.y, right_width, content.height);
    if parent_area.width > 0 {
        frame.render_widget(Block::default().borders(Borders::RIGHT).border_style(theme.border()), parent_area);
        compose_picker_rows(frame, Rect::new(parent_area.x, parent_area.y, parent_area.width.saturating_sub(1), parent_area.height),
            &picker.parent_entries, Some(picker.parent_selected), false, &[], None,
            app.visual_selection_style, theme);
    }
    if files_area.width > 0 {
        frame.render_widget(Block::default().borders(Borders::RIGHT).border_style(theme.border()), files_area);
        compose_picker_rows(frame, Rect::new(files_area.x, files_area.y, files_area.width.saturating_sub(1), files_area.height),
            &picker.entries, Some(picker.selected), true, &composer.attachment_paths,
            picker.visual_anchor.map(|anchor| (anchor, picker.selected)),
            app.visual_selection_style, theme);
    }
    if preview_area.width > 0 {
        let preview = Rect::new(preview_area.x + 1, preview_area.y,
            preview_area.width.saturating_sub(2), preview_area.height);
        let lines: Vec<Line<'static>> = match &picker.preview {
            crate::app::ComposeFilePreview::Empty => vec![Line::styled("Empty directory", theme.muted())],
            crate::app::ComposeFilePreview::Loading(message) => vec![Line::styled(*message, theme.muted())],
            crate::app::ComposeFilePreview::Directory(entries) => entries.iter().map(|entry| {
                Line::styled(format!("{}{}", if entry.is_directory { "󰉋 " } else { "  " }, entry.name),
                    if entry.is_directory { theme.accent() } else { theme.text() })
            }).collect(),
            crate::app::ComposeFilePreview::Text { lines, truncated } => {
                let mut result: Vec<_> = lines.iter().map(|line| Line::styled(line.clone(), theme.text())).collect();
                if *truncated { result.push(Line::styled("… truncated …", theme.muted())); }
                result
            }
            crate::app::ComposeFilePreview::Patch(patch) => {
                use crate::patch_preview::PatchTone;
                let mut lines: Vec<Line<'static>> = patch.lines.iter().map(|fragments| {
                    // Apply a consistent row background even where tree-sitter
                    // overlays source syntax colors on added code.
                    let patch_background = match fragments.first().map(|f| f.tone) {
                        Some(PatchTone::Add) => diff_line_style(PatchLineKind::Add, &theme).bg,
                        Some(PatchTone::Remove) => diff_line_style(PatchLineKind::Remove, &theme).bg,
                        _ => None,
                    };
                    let mut spans = fragments.iter().map(|fragment| {
                        let mut style = match fragment.tone {
                            PatchTone::Default => theme.text(),
                            PatchTone::Meta => theme.accent().add_modifier(Modifier::BOLD),
                            PatchTone::Hunk => theme.accent().add_modifier(Modifier::BOLD),
                            PatchTone::Add => diff_line_style(PatchLineKind::Add, &theme),
                            PatchTone::Remove => diff_line_style(PatchLineKind::Remove, &theme),
                            PatchTone::Keyword => Style::default().fg(theme.visual),
                            PatchTone::String => Style::default().fg(theme.warning),
                            PatchTone::Comment => theme.muted(),
                            PatchTone::Number => Style::default().fg(theme.warning),
                            PatchTone::Function => theme.accent(),
                            PatchTone::Type => theme.accent(),
                        };
                        if let Some(bg) = patch_background { style = style.bg(bg); }
                        Span::styled(fragment.text.clone(), style)
                    }).collect::<Vec<_>>();
                    if let Some(bg) = patch_background {
                        let used = fragments.iter()
                            .map(|fragment| UnicodeWidthStr::width(fragment.text.as_str()))
                            .sum::<usize>();
                        let padding = usize::from(preview.width).saturating_sub(used);
                        if padding > 0 {
                            spans.push(Span::styled(" ".repeat(padding), Style::default().bg(bg)));
                        }
                    }
                    Line::from(spans)
                }).collect();
                if patch.truncated { lines.push(Line::styled("… truncated …", theme.muted())); }
                lines
            }
            crate::app::ComposeFilePreview::Image(_) => {
                if app.compose_file_preview_image().is_some() { vec![] }
                else { vec![Line::styled("Unable to display image in this terminal", theme.muted())] }
            },
            crate::app::ComposeFilePreview::Binary(bytes) => vec![
                Line::styled("Binary file", theme.muted()),
                Line::styled(format!("{bytes} bytes"), theme.text()),
            ],
            crate::app::ComposeFilePreview::Error(error) => vec![Line::styled(error.clone(), theme.error())],
        };
        frame.render_widget(Paragraph::new(lines), preview);
        if let Some(protocol) = app.compose_file_preview_image() {
            let size = protocol.size();
            if size.width <= preview.width && size.height <= preview.height {
                let image_area = Rect::new(
                    preview.x + (preview.width - size.width) / 2,
                    preview.y + (preview.height - size.height) / 2,
                    size.width, size.height,
                );
                frame.render_widget(TerminalImage::new(protocol), image_area);
            }
        }
    }
    if let Some(query) = &picker.search_query {
        let matches = picker.search_matches_for_display();
        let position = matches.iter().position(|index| *index == picker.selected);
        let counter = position.map_or_else(|| format!("0/{}", matches.len()),
            |index| format!("{}/{}", index + 1, matches.len()));
        let prompt = Line::from(vec![
            Span::styled(" / ", theme.accent()),
            Span::styled(query.clone(), theme.text()),
            Span::styled(format!("  {counter}"), theme.muted()),
        ]);
        frame.render_widget(Paragraph::new(prompt), Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
    }
    if picker.help_visible { draw_compose_file_picker_help(frame, app, picker); }
}

fn draw_compose_file_picker_help(frame: &mut Frame<'_>, app: &App, picker: &crate::app::ComposeFilePicker) {
    let theme = app.theme();
    let area = centered_rect(frame.area(), 83, 72);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" File picker keybindings ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    draw_search_header(frame, chunks[0], &picker.help_query, app);
    let matches = picker.help_matches();
    let start = picker.help_scroll.min(matches.len().saturating_sub(usize::from(chunks[1].height)));
    let shown: Vec<_> = matches.into_iter().skip(start).take(usize::from(chunks[1].height)).collect();
    let (category_width, key_width, description_width) = help_columns(chunks[1].width);
    let rows = shown.iter().map(|&index| {
        let entry = PICKER_HELP_ENTRIES[index];
        Row::new(vec![
            Cell::from(truncate_display_width(entry.category, usize::from(category_width))),
            Cell::from(truncate_display_width(entry.key, usize::from(key_width))),
            Cell::from(truncate_display_width(entry.description, usize::from(description_width))),
        ])
    });
    let table = Table::new(rows, [
        Constraint::Length(category_width), Constraint::Length(key_width),
        Constraint::Length(description_width),
    ]).style(theme.text()).column_spacing(1).row_highlight_style(theme.selected());
    let mut state = TableState::default().with_selected(shown.iter().position(|&index| index == picker.help_selected));
    frame.render_stateful_widget(table, chunks[1], &mut state);
    // All picker bindings live in this searchable popup, not a footer.
}

fn draw_compose_attachment_manager(frame: &mut Frame<'_>, app: &App) {
    let Some(composer) = app.composer.as_ref() else { return; };
    let theme = app.theme();
    let area = centered_rect(frame.area(), 84, 66);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" Attached files · {} ", composer.attachment_count()))
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 { return; }
    let mut files = composer.attachment_paths.clone();
    files.extend(composer.attachments.split(';').map(str::trim)
        .filter(|part| !part.is_empty()).map(std::path::PathBuf::from));
    if files.is_empty() {
        frame.render_widget(Paragraph::new(" No files attached").style(theme.muted()), inner);
        return;
    }
    let selected = composer.attachment_manager_selected.min(files.len() - 1);
    let start = selected.saturating_sub(usize::from(inner.height) / 2)
        .min(files.len().saturating_sub(usize::from(inner.height)));
    let rows: Vec<ListItem<'_>> = files.iter().enumerate().skip(start)
        .take(usize::from(inner.height)).map(|(index, file)| {
            let name = file.file_name().unwrap_or(file.as_os_str()).to_string_lossy();
            let parent = file.parent().map_or_else(String::new, |path| path.display().to_string());
            let name_width = usize::from(inner.width).saturating_sub(5);
            let visual = composer.attachment_manager_visual_anchor.is_some_and(|anchor|
                (anchor.min(selected)..=anchor.max(selected)).contains(&index));
            let marked = composer.attachment_manager_marked.contains(&index);
            let line = Line::from(vec![
                browser_gutter(theme, visual, marked),
                Span::styled(if index == selected { "▸ " } else { "  " }, theme.accent()),
                Span::styled(truncate_display_width(&name, name_width.min(32)), theme.text()),
                Span::styled(format!("  {}", truncate_display_width(&parent, name_width.saturating_sub(32))), theme.muted()),
            ]);
            ListItem::new(line).style(browser_row_style(theme, app.visual_selection_style,
                visual, marked, index == selected, false))
        }).collect();
    frame.render_widget(List::new(rows), inner);
}

fn draw_compose_advanced_options(frame: &mut Frame<'_>, app: &App) {
    let Some(composer) = app.composer.as_ref() else { return; };
    let theme = app.theme();
    let area = centered_rect(frame.area(), 82, 32);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Advanced compose options ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2), Constraint::Length(1), Constraint::Min(1),
    ]).split(inner);
    frame.render_widget(Paragraph::new(vec![
        Line::styled("Custom email headers (optional)", theme.accent()),
        Line::styled("Use Name: value; Name: value. Standard fields are already handled.", theme.muted()),
    ]), chunks[0]);
    let mut spans = vec![Span::styled("  Headers: ", theme.accent())];
    spans.extend(editor_spans(&composer.headers, composer, true, theme));
    frame.render_widget(Paragraph::new(Line::from(spans)), chunks[1]);
    frame.render_widget(Paragraph::new(" Enter / Esc / Ctrl+O: return to compose")
        .style(theme.muted()), chunks[2]);
}

fn compose_mode_colors(mode: ComposeMode, theme: &Theme) -> (Color, Color) {
    match mode {
        ComposeMode::Normal => (theme.accent, theme.selected_fg),
        ComposeMode::Insert => (theme.unread, theme.selected_fg),
        ComposeMode::Visual => (theme.visual, theme.selected_fg),
    }
}

fn draw_compose_help(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let Some(composer) = app.composer.as_ref() else { return; };
    let area = centered_rect(frame.area(), 85, 73);
    frame.render_widget(Clear, area);
    let block = Block::default().title(" Compose keybindings ")
        .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2), Constraint::Min(1), Constraint::Length(1),
    ]).split(inner);
    draw_search_header(frame, chunks[0], &composer.help_query, app);
    let matches = composer.compose_help_matches();
    let max_start = matches.len().saturating_sub(usize::from(chunks[1].height));
    let start = composer.help_scroll.min(max_start);
    let shown: Vec<usize> = matches.into_iter().skip(start)
        .take(usize::from(chunks[1].height)).collect();
    let (category_width, key_width, description_width) = help_columns(chunks[1].width);
    let rows = shown.iter().map(|&index| {
        let entry = COMPOSE_HELP_ENTRIES[index];
        Row::new(vec![
            Cell::from(truncate_display_width(entry.category, usize::from(category_width))),
            Cell::from(truncate_display_width(entry.key, usize::from(key_width))),
            Cell::from(truncate_display_width(entry.description, usize::from(description_width))),
        ])
    });
    let table = Table::new(rows, [
        Constraint::Length(category_width), Constraint::Length(key_width),
        Constraint::Length(description_width),
    ]).style(theme.text()).column_spacing(1).row_highlight_style(theme.selected());
    let selected = shown.iter().position(|&index| index == composer.help_selected);
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, chunks[1], &mut state);
    frame.render_widget(Paragraph::new(" Type to filter   Backspace to edit   Esc to close ")
        .style(theme.muted()), chunks[2]);
}

/// Contacts are overlays over the *terminal* surface, not an opaque gray
/// popup. Match the message list and reader; reserve selected backgrounds for
/// the currently highlighted row only.
fn contact_surface_style(theme: &Theme) -> Style {
    theme.text()
}

/// Autocomplete is an overlay, not a new compose field: users keep their
/// normal Vim cursor, and Tab/Enter accept the highlighted address.
fn draw_contact_suggestions(frame: &mut Frame<'_>, app: &App) {
    let ids = app.contact_suggestions();
    if ids.is_empty() { return; }
    let Some(composer) = app.composer.as_ref() else { return; };
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
    if width < 12 || height < 3 { return; }
    let x = inner_x.saturating_add(9).min(frame.area().right().saturating_sub(width));
    // Recipient fields may now occupy multiple wrapped rows. Anchor the
    // suggestions below the active field rather than at a fixed line offset.
    let composer_width = compose.width.saturating_sub(2);
    let preview_theme = theme;
    let requested = [
        (ComposeField::To, "To", composer.to.as_str()),
        (ComposeField::Cc, "Cc", composer.cc.as_str()),
        (ComposeField::Bcc, "Bcc", composer.bcc.as_str()),
        (ComposeField::Subject, "Subject", composer.subject.as_str()),
    ].map(|(f, name, value)|
        compose_header_display(f, name, value, composer, preview_theme, usize::from(composer_width)).lines.len());
    let selected_names = compose_attachment_names(composer);
    let attachment_label = format!("  Attachments ({}):", composer.attachment_count());
    let summary = if selected_names.is_empty() { " none".to_owned() }
        else { format!(" {}", selected_names.join("  ·  ")) };
    let attachment_rows = clipped_wrap_text(&format!("{attachment_label}{summary}"),
        usize::from(composer_width).max(1), 3).len().max(1) as u16;
    let field_heights = compose_header_heights(requested, compose.height.saturating_sub(2), attachment_rows);
    let y_offset: u16 = field_heights[..=field].iter().copied().sum();
    let y = inner_y.saturating_add(y_offset).min(frame.area().bottom().saturating_sub(height));
    let area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().borders(Borders::ALL).border_type(theme.border_type)
        .border_style(theme.border()).style(contact_surface_style(theme)), area);
    for (position, id) in ids.iter().take(usize::from(height.saturating_sub(2))).enumerate() {
        let contact = &app.contacts.entries[*id];
        let content = format!("{}  {}", if contact.favorite { "★" } else { " " }, contact.display());
        let truncated = truncate_display_width(&content, usize::from(width.saturating_sub(4)));
        let style = if position == composer.contact_suggestion.min(ids.len() - 1) {
            theme.selected()
        } else { theme.text() };
        frame.render_widget(Paragraph::new(format!(" {truncated}"))
            .style(style), Rect::new(area.x + 1, area.y + 1 + position as u16, width - 2, 1));
    }
}

/// SEARCH uses the theme's visual/purple color, while VISUAL uses its
/// selected/blue color. Both badges occupy the same mode slot in the footer.
fn search_mode_style(theme: &Theme) -> Style {
    mode_style("SEARCH", theme)
}

/// The mode color is the same in the reader, message list, Contacts and Compose.
/// Normal = blue, Visual/Search = purple, Insert = green.
fn mode_style(label: &str, theme: &Theme) -> Style {
    let bg = match label {
        "VISUAL" | "SEARCH" => theme.visual,
        "INSERT" => theme.unread,
        _ => theme.accent,
    };
    Style::default().fg(theme.selected_fg).bg(bg).add_modifier(Modifier::BOLD)
}

fn compose_mode_style(mode: ComposeMode, theme: &Theme) -> Style {
    mode_style(mode.label(), theme)
}

/// Prefix for the shared bottom status row when Contacts is open. Keep the
/// account label and mode position identical to the mail browser.
fn contact_status_spans(theme: &Theme, account_name: Option<&str>,
    modal: &crate::contacts::ContactsModal) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        format!("[{}] ", account_name.unwrap_or("Contacts")), theme.accent())];
    spans.extend(contact_footer_mode(theme, modal));
    if modal.searching {
        spans.push(Span::styled(format!(" {} matches", modal.matches.len()), theme.muted()));
    }
    spans
}

/// Contact mode badges rendered in the shared bottom status bar. The query
/// itself lives in the dedicated search row above the contacts list.
fn contact_footer_mode(theme: &Theme, modal: &crate::contacts::ContactsModal) -> Vec<Span<'static>> {
    if modal.searching {
        vec![Span::styled(" SEARCH ", search_mode_style(theme))]
    } else if modal.visual_anchor.is_some() {
        vec![Span::styled(" VISUAL ", mode_style("VISUAL", theme))]
    } else {
        vec![Span::styled(" NORMAL ", mode_style("NORMAL", theme))]
    }
}

/// Contact filters and marks follow the mode at the left of the global status
/// bar. The contact total stays in the quiet footer inside Contacts itself.
fn contact_footer_details(theme: &Theme, modal: &crate::contacts::ContactsModal) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if !modal.marked.is_empty() {
        spans.push(Span::styled(format!(" {} marked ", modal.marked.len()), theme.accent()));
    }
    if !modal.searching && !modal.query.is_empty() {
        spans.push(Span::styled(format!(" filter:/{} ", modal.query), theme.accent()));
    }
    spans
}

/// Contacts get the same extra top search row as the message list, only while
/// actively editing a search. Normal browsing gets the full list height.
fn contacts_search_layout(inner: Rect, searching: bool) -> (Option<Rect>, Rect) {
    if !searching || inner.height <= 1 { return (None, inner); }
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    (Some(chunks[0]), chunks[1])
}

fn contacts_list_width(inner_width: u16) -> u16 {
    (inner_width.saturating_mul(47) / 100).max(12).min(inner_width)
}

/// Use the existing favorite/saved icon cell for visual and marked checks.
/// This keeps contact names aligned whether the row is selected or not.
fn contact_list_icon(favorite: bool, saved: bool, visual: bool, marked: bool) -> &'static str {
    if visual || marked { "✓ " }
    else if favorite { "★ " }
    else if saved { "● " }
    else { "  " }
}

fn draw_contacts(frame: &mut Frame<'_>, app: &mut App) {
    let theme = app.theme().clone();
    let selection_style = app.visual_selection_style;
    let Some(modal) = app.contacts_modal.as_mut() else { return; };
    let area = centered_rect(frame.area(), 116, 88);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().title(" Contacts ").borders(Borders::ALL).border_type(theme.border_type)
        .style(contact_surface_style(&theme)).border_style(theme.border()), area);
    let inner = Rect::new(area.x.saturating_add(2), area.y.saturating_add(1),
        area.width.saturating_sub(4), area.height.saturating_sub(2));
    if inner.width < 5 || inner.height < 4 { return; }
    // Reserve a quiet internal count row. It never uses the global status bar.
    let content_bounds = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
    let (search_row, content) = contacts_search_layout(content_bounds, modal.searching);
    if let Some(search_row) = search_row {
        draw_search_prompt(frame, search_row, &modal.query, &theme);
    }
    let left_width = contacts_list_width(content.width);
    let left = Rect::new(content.x, content.y, left_width, content.height);
    let right = Rect::new(left.right(), content.y,
        content.width.saturating_sub(left_width), content.height);
    frame.render_widget(Block::default().borders(Borders::RIGHT)
        .border_style(theme.border()), left);
    let list_width = left.width.saturating_sub(1);
    if left.height == 0 { return; }
    modal.view_height = usize::from(left.height);
    let item_count = modal.matches.len();
    modal.ensure_visible(item_count);
    let items = &modal.matches;
    let number_width = (items.len().max(1).to_string().len() + 1).max(3);
    for (visual, &id) in items.iter().enumerate().skip(modal.scroll).take(modal.view_height) {
        let contact = &app.contacts.entries[id];
        let active = visual == modal.selected;
        let is_visual = modal.in_visual_range(visual);
        let is_marked = modal.marked.contains(&contact.email.to_ascii_lowercase());
        let number = if active { (visual + 1).to_string() }
            else { visual.abs_diff(modal.selected).to_string() };
        let icon = contact_list_icon(contact.favorite, contact.saved, is_visual, is_marked);
        let label = if contact.name.is_empty() { &contact.email } else { &contact.name };
        let max_label = usize::from(list_width).saturating_sub(number_width + 4);
        let line = Line::from(vec![
            contact_gutter(&theme, is_visual, is_marked),
            Span::raw(format!("{number:>number_width$} ")),
            Span::styled(icon, selection_check_style(&theme,
                selection_style, is_visual, is_marked, active)),
            Span::raw(truncate_display_width(label, max_label)),
        ]);
        let row_style = browser_row_style(&theme, selection_style,
            is_visual, is_marked, active, false);
        frame.render_widget(Paragraph::new(line).style(row_style),
            Rect::new(left.x, left.y + (visual - modal.scroll) as u16, list_width, 1));
    }
    if items.is_empty() {
        frame.render_widget(Paragraph::new(if modal.query.is_empty() {
            " No contacts yet"
        } else { " No matching contacts" }).style(theme.muted()),
            Rect::new(left.x, left.y, list_width, 1));
    }
    if right.width > 5 {
        let x = right.x.saturating_add(2);
        let width = right.width.saturating_sub(3);
        if let Some(&id) = items.get(modal.selected) {
            let contact = &app.contacts.entries[id];
            let details = [
                ("Name", contact.name.as_str()),
                ("Email", contact.email.as_str()),
                ("Source", if contact.saved { "Saved locally" } else { "Learned from mail" }),
                ("Favorite", if contact.favorite { "Yes" } else { "No" }),
            ];
            for (pos, (label, value)) in details.iter().enumerate() {
                let y = right.y.saturating_add((pos * 3) as u16);
                if y + 1 >= right.bottom() { break; }
                frame.render_widget(Paragraph::new(*label).style(theme.accent()), Rect::new(x, y, width, 1));
                frame.render_widget(Paragraph::new(truncate_display_width(value, width as usize))
                    .style(theme.text()), Rect::new(x, y + 1, width, 1));
            }
        } else {
            frame.render_widget(Paragraph::new("No contact selected").style(theme.muted()),
                Rect::new(x, right.y, width, 1));
        }
    }
    let count = app.contacts.entries.len();
    let count_label = if modal.query.is_empty() { format!("{count} contacts") }
        else { format!("{} of {count} contacts", modal.matches.len()) };
    let count_rect = Rect::new(inner.x, inner.bottom().saturating_sub(1),
        inner.width, 1);
    frame.render_widget(Paragraph::new(count_label).alignment(Alignment::Right)
        .style(theme.muted()), count_rect);
    if modal.help {
        let matches = ranked_help_matches(CONTACT_HELP_ENTRIES, &modal.help_query);
        draw_help_palette(frame, &theme, " Contacts keybindings ", CONTACT_HELP_ENTRIES,
            &modal.help_query, &matches, modal.help_selected, modal.help_scroll);
        return;
    }
    if modal.confirm_delete {
        let ids = modal.action_ids(&app.contacts);
        let names = ids.iter().filter_map(|&id| app.contacts.entries.get(id))
            .map(|contact| contact.display()).collect::<Vec<_>>();
        draw_review_dialog(frame, &theme, "Delete contacts",
            "Delete these contacts from your local address book?", &names,
            modal.delete_review.position);
        return;
    }
    if let Some(edit) = &modal.edit {
        let edit_area = centered_rect(frame.area(), 74, 35);
        frame.render_widget(Clear, edit_area);
        frame.render_widget(Block::default().title(if edit.original_email.is_some() { " Edit contact " } else { " New contact " })
            .borders(Borders::ALL).border_type(theme.border_type).border_style(theme.border()).style(contact_surface_style(&theme)), edit_area);
        let width = edit_area.width.saturating_sub(4);
        let name_y = edit_area.y.saturating_add(2);
        let email_y = name_y.saturating_add(2);
        for (is_email, value, y, label) in [(false, edit.name.as_str(), name_y, "Name"),
            (true, edit.email.as_str(), email_y, "Email")] {
            if y >= edit_area.bottom().saturating_sub(1) { continue; }
            let label_text = format!("{label}: {value}");
            frame.render_widget(Paragraph::new(truncate_display_width(&label_text, width as usize))
                .style(if edit.active_email == is_email { theme.current_line() } else { theme.text() }),
                Rect::new(edit_area.x + 2, y, width, 1));
            if edit.active_email == is_email {
                let prefix = UnicodeWidthStr::width(format!("{label}: ").as_str());
                let before: String = value.chars().take(edit.cursor).collect();
                let offset = prefix + UnicodeWidthStr::width(before.as_str());
                if offset < width as usize {
                    frame.set_cursor_position((edit_area.x + 2 + offset as u16, y));
                }
            }
        }
        if edit_area.height > 8 {
            frame.render_widget(Paragraph::new("Tab switch fields · Enter next/save · Esc cancel")
                .style(theme.muted()), Rect::new(edit_area.x + 2,
                edit_area.bottom().saturating_sub(2), width, 1));
        }
    }
}

/// Layout of a single composer header. We wrap on grapheme boundaries
/// rather than relying on Paragraph word-wrap, so the visual caret position
/// cannot drift when a recipient contains spaces, wide characters or emoji.
struct ComposeHeaderDisplay {
    lines: Vec<Line<'static>>,
    cursor_row: usize,
    cursor_col: usize,
    active: bool,
}

fn compose_header_display(
    field: ComposeField, name: &str, value: &str, composer: &crate::app::Composer,
    theme: &Theme, width: usize,
) -> ComposeHeaderDisplay {
    let active = composer.field == field;
    let marker = if active { "▸ " } else { "  " };
    let prefix = truncate_display_width(&format!("{marker}{name}: "), width);
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let prefix_style = if active || !value.trim().is_empty() { theme.accent() } else { theme.muted() };
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
        if grapheme == "\n" || (width > 0 && column > continuation
            && column.saturating_add(glyph_width) > width) {
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
    if spans.len() > index { row.extend(spans[index..].iter().cloned()); }
    lines.push(Line::from(row));
    ComposeHeaderDisplay { lines, cursor_row, cursor_col, active }
}

/// Reserve space for the message body and distribute remaining header rows
/// fairly. When a header is taller than its assigned height, its active caret
/// stays in view via vertical scrolling inside that field.
fn compose_header_heights(requested: [usize; 4], height: u16, attachment_rows: u16) -> [u16; 4] {
    let reserved = attachment_rows.saturating_add(1 + 3); // body label and body; footer is global
    let budget = height.saturating_sub(reserved).max(4) as usize;
    let mut heights = [1u16; 4];
    while heights.iter().map(|&h| usize::from(h)).sum::<usize>() < budget {
        let mut increased = false;
        for (i, desired) in requested.iter().copied().enumerate() {
            if usize::from(heights[i]) < desired &&
                heights.iter().map(|&h| usize::from(h)).sum::<usize>() < budget {
                heights[i] += 1;
                increased = true;
            }
        }
        if !increased { break; }
    }
    heights
}

fn render_compose_header(frame: &mut Frame<'_>, area: Rect,
    display: ComposeHeaderDisplay, composer: &crate::app::Composer, theme: &Theme) {
    if area.width == 0 || area.height == 0 { return; }
    let scroll = if display.active {
        display.cursor_row.saturating_sub(usize::from(area.height).saturating_sub(1))
    } else { 0 };
    frame.render_widget(Paragraph::new(display.lines)
        .style(if display.active { editor_current_line_style(theme) } else { theme.text() })
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)), area);
    if display.active && composer.mode == ComposeMode::Insert
        && composer.file_picker.is_none() && !composer.attachment_manager_open
        && !composer.advanced_headers_open && !composer.help_visible
        && !composer.confirm_send && !composer.exit_prompt {
        let visual_row = display.cursor_row.saturating_sub(scroll);
        if visual_row < usize::from(area.height) && display.cursor_col < usize::from(area.width) {
            frame.set_cursor_position((area.x + display.cursor_col as u16,
                area.y + visual_row as u16));
        }
    }
}

fn draw_composer(frame: &mut Frame<'_>, app: &mut App) {
    let theme = app.theme().clone();
    let Some(composer) = app.composer.as_mut() else {
        return;
    };
    let area = centered_rect(frame.area(), 96, 82);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(if composer.sending { " Compose • sending… " } else { " Compose " })
        .borders(Borders::ALL).border_type(theme.border_type)
        .border_style(theme.border());
    let inside = block.inner(area);
    frame.render_widget(block, area);

    let selected_names = compose_attachment_names(composer);
    let attachments_label = format!("  Attachments ({}):", composer.attachment_count());
    let summary = if selected_names.is_empty() { " none".to_owned() }
        else { format!(" {}", selected_names.join("  ·  ")) };
    let width = usize::from(inside.width).max(1);
    let attachment_lines = clipped_wrap_text(&format!("{attachments_label}{summary}"), width, 3);
    let attachment_rows = attachment_lines.len().max(1) as u16;
    let header_specs = [
        (ComposeField::To, "To", composer.to.as_str()),
        (ComposeField::Cc, "Cc", composer.cc.as_str()),
        (ComposeField::Bcc, "Bcc", composer.bcc.as_str()),
        (ComposeField::Subject, "Subject", composer.subject.as_str()),
    ];
    let headers = header_specs.into_iter().map(|(field, name, value)|
        compose_header_display(field, name, value, composer, &theme, width))
        .collect::<Vec<_>>();
    let requested = std::array::from_fn(|i| headers[i].lines.len());
    let header_heights = compose_header_heights(requested, inside.height, attachment_rows);
    let fields = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_heights[0]), Constraint::Length(header_heights[1]),
            Constraint::Length(header_heights[2]), Constraint::Length(header_heights[3]),
            Constraint::Length(attachment_rows),
            Constraint::Length(1), Constraint::Min(3),
        ])
        .split(inside);

    for (i, header) in headers.into_iter().enumerate() {
        render_compose_header(frame, fields[i], header, composer, &theme);
    }
    let attachments_style = if selected_names.is_empty() { theme.muted() } else { theme.text() };
    let attachment_text = attachment_lines.into_iter().enumerate().map(|(i, line)| {
        if i == 0 && line.starts_with(&attachments_label) {
            Line::from(vec![
                Span::styled(attachments_label.clone(), if selected_names.is_empty() { theme.muted() } else { theme.accent() }),
                Span::styled(line[attachments_label.len()..].to_owned(), attachments_style),
            ])
        } else { Line::styled(line, attachments_style) }
    }).collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(attachment_text), fields[4]);
    let body_active = composer.field == ComposeField::Body;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if body_active { "▸ Body:" } else { "  Body:" },
            if body_active || !composer.body.trim().is_empty() { theme.accent() } else { theme.muted() },
        ))).style(if body_active { editor_current_line_style(&theme) } else { theme.text() }),
        fields[5],
    );

    // Compose, reply and forward use exactly the same physical-row model for
    // rendering, j/k, and Ctrl-j/k. Explicit wrap rows avoid treating a long
    // forwarded line as a single invisible logical line during navigation.
    composer.body_view_width = usize::from(fields[6].width.saturating_sub(4)).max(1);
    let viewport_height = usize::from(fields[6].height).max(1);
    let rows = composer.body_rows();
    let cursor_row = rows.iter().rposition(|row| row.start <= composer.cursor).unwrap_or(0);
    let active_line = rows[cursor_row].logical_line;
    let raw = editor_text(&composer.body, composer, body_active, &theme);
    let (mode_color, _) = compose_mode_colors(composer.mode, &theme);
    let mut display = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let current = body_active && index == cursor_row;
        let number = if row.continuation { "    ".to_owned() }
            else {
                let value = if row.logical_line == active_line { row.logical_line + 1 }
                    else { row.logical_line.abs_diff(active_line) };
                format!("{value:>3} ")
            };
        let number_style = if current {
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD)
        } else { theme.muted() };
        let mut spans = vec![Span::styled(number, number_style)];
        if let Some(line) = raw.lines.get(row.logical_line) {
            let offset = row.start.saturating_sub(row.line_start);
            let length = row.end.saturating_sub(row.start);
            spans.extend(line.spans.iter().skip(offset).take(length).cloned());
            // At the end of a newline-terminated or final line, editor_text
            // creates a separate block cursor cell in Normal/Visual mode.
            if current && composer.mode != ComposeMode::Insert
                && rows.get(index + 1).is_none_or(|next| next.logical_line != row.logical_line) {
                if let Some(cell) = line.spans.get(offset + length) {
                    spans.push(cell.clone());
                }
            }
        }
        display.push(Line::from(spans));
    }
    let mut scroll = composer.body_scroll;
    if body_active {
        if cursor_row < scroll { scroll = cursor_row; }
        if cursor_row >= scroll.saturating_add(viewport_height) {
            scroll = cursor_row + 1 - viewport_height;
        }
    }
    scroll = scroll.min(rows.len().saturating_sub(viewport_height));
    composer.body_scroll = scroll;
    if body_active && cursor_row >= scroll && cursor_row - scroll < viewport_height {
        let y = fields[6].y.saturating_add((cursor_row - scroll) as u16);
        frame.render_widget(Block::default().style(editor_current_line_style(&theme)),
            Rect::new(fields[6].x, y, fields[6].width, 1));
    }
    frame.render_widget(
        Paragraph::new(Text::from(display)).style(theme.text())
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)), fields[6],
    );
    if body_active && composer.mode == ComposeMode::Insert && fields[6].width > 4
        && composer.file_picker.is_none() && !composer.attachment_manager_open
        && !composer.advanced_headers_open && !composer.help_visible
        && !composer.confirm_send && !composer.exit_prompt
        && cursor_row >= scroll && cursor_row - scroll < viewport_height {
        let before = composer.body.chars().skip(rows[cursor_row].start)
            .take(composer.cursor.saturating_sub(rows[cursor_row].start))
            .collect::<String>();
        let column = UnicodeWidthStr::width(before.as_str());
        let x = fields[6].x.saturating_add(4)
            .saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
        let y = fields[6].y.saturating_add((cursor_row - scroll) as u16);
        if x < fields[6].right() { frame.set_cursor_position((x, y)); }
    }
    // Mode and status are rendered in draw_status(), the application-wide
    // bottom bar. Compose no longer creates a second competing footer.

}

fn editor_spans(
    value: &str,
    composer: &crate::app::Composer,
    active: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = composer.cursor.min(chars.len());
    let selection = active.then(|| composer.visual_range()).flatten();
    let mut spans = Vec::with_capacity(chars.len().saturating_add(1));
    let block = Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD);
    for (index, ch) in chars.iter().copied().enumerate() {
        let at_cursor = active && index == cursor;
        let selected = selection.is_some_and(|(start, end)| (start..end).contains(&index));
        spans.push(Span::styled(ch.to_string(),
            if at_cursor && composer.mode != ComposeMode::Insert { block }
            else if selected { theme.selected() }
            else if value.trim().is_empty() { theme.muted() } else { theme.text() }));
    }
    if active && cursor == chars.len() && composer.mode != ComposeMode::Insert {
        spans.push(Span::styled(" ", block));
    }
    spans
}

fn is_quoted_line(text: &str) -> bool {
    text.trim_start().starts_with('>')
}

fn quoted_line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut global = 0usize;
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content_len = content.chars().count();
        if is_quoted_line(content) {
            ranges.push((global, global.saturating_add(content_len)));
        }
        global = global.saturating_add(line.chars().count());
    }
    if !text.is_empty() && !text.ends_with('\n') && ranges.is_empty() && is_quoted_line(text) {
        ranges.push((0, text.chars().count()));
    }
    ranges
}

fn editor_text(
    value: &str,
    composer: &crate::app::Composer,
    active: bool,
    theme: &Theme,
) -> Text<'static> {
    let selection = active.then(|| composer.visual_range()).flatten();
    let cursor = composer.cursor.min(value.chars().count());
    let quote_ranges = quoted_line_ranges(value);
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut global = 0usize;
    let block = Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD);
    for ch in value.chars() {
        let at_cursor = active && global == cursor;
        if ch == '\n' {
            if at_cursor && composer.mode != ComposeMode::Insert {
                spans.push(Span::styled(" ", block));
            }
            lines.push(Line::from(std::mem::take(&mut spans)));
            global += 1;
            continue;
        }
        let selected = selection.is_some_and(|(start, end)| (start..end).contains(&global));
        let quoted = quote_ranges.iter().any(|&(start, end)| global >= start && global < end);
        spans.push(Span::styled(ch.to_string(),
            if at_cursor && composer.mode != ComposeMode::Insert { block }
            else if selected { theme.selected() }
            else if quoted { Style::default().fg(theme.muted) }
            else { theme.text() }));
        global += 1;
    }
    if active && global == cursor && composer.mode != ComposeMode::Insert {
        spans.push(Span::styled(" ", block));
    }
    lines.push(Line::from(spans));
    Text::from(lines)
}

fn centered_rect(area: Rect, max_width: u16, percent_height: u16) -> Rect {
    let width = area.width.min(max_width).max(1);
    let height = ((u32::from(area.height) * u32::from(percent_height) / 100) as u16)
        .max(3)
        .min(area.height.max(1));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn mailbox_row_text(name: &str, count: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let count_width = count.chars().count();
    if count.is_empty() {
        let name = truncate_chars(name, width);
        return format!("{name:<width$}");
    }
    if count_width >= width {
        return truncate_chars(count, width);
    }

    let name_width = width.saturating_sub(count_width + 1);
    let name = truncate_chars(name, name_width);
    format!("{name:<name_width$} {count:>count_width$}")
}

fn truncate_display_width(text: &str, width: usize) -> String {
    truncate_display_width_cow(text, width).into_owned()
}

/// Keep borrowed mail-list text when it fits rather than allocating every row.
fn truncate_display_width_cow(text: &str, width: usize) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if width == 0 { return Cow::Borrowed(""); }
    if UnicodeWidthStr::width(text) <= width { return Cow::Borrowed(text); }
    if width == 1 { return Cow::Borrowed("…"); }

    let budget = width.saturating_sub(1);
    let mut used = 0usize;
    let mut out = String::new();
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used.saturating_add(grapheme_width) > budget { break; }
        out.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    out.push('…');
    Cow::Owned(out)
}

fn truncate_chars(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut out = text.chars().take(width - 1).collect::<String>();
    out.push('…');
    out
}

#[cfg(test)]
fn subject_for_list(subject: &str) -> String {
    subject_for_list_cow(subject).into_owned()
}

fn subject_for_list_cow(subject: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    // Preserve the original bytes unless a text-capable emoji needs FE0E.
    // This avoids allocating for the overwhelming majority of mail subjects.
    let mut output: Option<String> = None;
    let mut copied_to = 0usize;
    for (offset, cluster) in UnicodeSegmentation::grapheme_indices(subject, true) {
        let mut chars = cluster.chars();
        if let Some(base) = chars.next() {
            let simple = chars.all(|ch| ch == '\u{fe0e}' || ch == '\u{fe0f}');
            if simple && supports_safe_text_presentation(base) {
                let out = output.get_or_insert_with(|| String::with_capacity(subject.len()));
                out.push_str(&subject[copied_to..offset]);
                out.push(base);
                out.push('\u{fe0e}');
                copied_to = offset + cluster.len();
            }
        }
    }
    match output {
        Some(mut out) => {
            out.push_str(&subject[copied_to..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(subject),
    }
}

/// Unicode characters with established text/emoji variants whose *text*
/// presentation can fit a normal terminal cell. Emoji-only pictographs stay
/// untouched because not all terminals/font stacks provide a text glyph.
fn supports_safe_text_presentation(ch: char) -> bool {
    matches!(ch, '☀' | '☁' | '☂' | '☃' | '☎' | '☘' | '☺' | '☹'
        | '✂' | '✈' | '✉' | '✌' | '❤' | '❄' | '✔' | '✖')
}

fn relative_number(index: usize, current: usize) -> String {
    if index == current {
        (index + 1).to_string()
    } else {
        index.abs_diff(current).to_string()
    }
}

fn relative_number_width(len: usize) -> u16 {
    let digits = len.max(1).ilog10() + 1;
    u16::try_from(digits + 1).unwrap_or(6).max(3)
}

#[cfg(test)]
mod tests {
    use super::{
        mailbox_row_text, pad_reader_line, reader_word_at, relative_number, rich_html_style,
        subject_for_list, truncate_display_width, word_match_ranges,
    };
    use crate::model::HtmlSpanStyle;
    use crate::theme::load_themes;
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn visual_ranges_are_purple_and_marks_remain_blue() {
        use crate::config::VisualSelectionStyle;
        let theme = load_themes().into_iter().find(|theme| theme.id == "tokyo-night").unwrap();
        assert_eq!(super::browser_row_style(&theme, VisualSelectionStyle::Gutter,
            true, false, true, false), theme.current_line());
        assert_eq!(super::browser_row_style(&theme, VisualSelectionStyle::Gutter,
            true, false, false, false), theme.text());
        assert_eq!(super::browser_row_style(&theme, VisualSelectionStyle::Row,
            true, false, false, false).bg, Some(theme.visual));
        assert_eq!(super::browser_row_style(&theme, VisualSelectionStyle::Row,
            false, true, true, false), theme.current_line());
        assert_eq!(super::browser_row_style(&theme, VisualSelectionStyle::Row,
            true, true, true, false), theme.current_line());
        assert_eq!(super::browser_row_style(&theme, VisualSelectionStyle::Row,
            false, true, true, true), theme.unread().bg(theme.popup_bg));
        let visual = super::browser_gutter(&theme, true, false);
        assert_eq!(visual.content.as_ref(), "▍");
        assert_eq!(visual.style.fg, Some(theme.visual));
        let marked = super::browser_gutter(&theme, false, true);
        assert_eq!(marked.style.fg, Some(theme.accent));
        let normal = super::browser_gutter(&theme, false, false);
        assert_eq!(normal.content.as_ref(), "▏");
        assert_eq!(normal.style.fg, Some(theme.border));
        assert_eq!(super::selection_check_style(&theme, VisualSelectionStyle::Gutter,
            true, false, false).fg, Some(theme.visual));
        assert_eq!(super::selection_check_style(&theme, VisualSelectionStyle::Row,
            true, false, true).fg, Some(theme.visual));
        assert_eq!(super::selection_check_style(&theme, VisualSelectionStyle::Row,
            false, true, true).fg, Some(theme.accent));
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
        let theme = load_themes().into_iter().find(|theme| theme.id == "tokyo-night").unwrap();
        assert_eq!(super::contact_gutter(&theme, false, false).content.as_ref(), " ");
        let visual = super::contact_gutter(&theme, true, false);
        assert_eq!(visual.content.as_ref(), "▍");
        assert_eq!(visual.style.fg, Some(theme.visual));
        let marked = super::contact_gutter(&theme, false, true);
        assert_eq!(marked.content.as_ref(), "▍");
        assert_eq!(marked.style.fg, Some(theme.accent));
    }

    #[test]
    fn editor_highlight_keeps_ordinary_text_unbolded() {
        use ratatui::style::Modifier;
        let theme = load_themes().into_iter().find(|t| t.id == "tokyo-night").unwrap();
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
        for entries in [crate::app::HELP_ENTRIES, crate::app::CONTACT_HELP_ENTRIES,
            crate::app::ATTACHMENT_HELP_ENTRIES, crate::app::PICKER_HELP_ENTRIES,
            crate::app::COMPOSE_HELP_ENTRIES] {
            for entry in entries {
                assert!(UnicodeWidthStr::width(entry.description) <= usize::from(max_description),
                    "key {} description too wide: {}", entry.key, entry.description);
                assert!(UnicodeWidthStr::width(entry.key) <= usize::from(shortcut),
                    "key label too wide: {}", entry.key);
                assert!(UnicodeWidthStr::width(entry.category) <= usize::from(category),
                    "category too wide: {}", entry.category);
            }
        }
        for screen_width in 3u16..80 {
            let (category, key, description) = super::help_columns(screen_width.saturating_sub(2));
            assert!(category + key + description + 2 <= screen_width,
                "columns overflow width {screen_width}");
        }
    }

    #[test]
    fn preview_resize_highlights_the_existing_divider_only_while_dragging() {
        let theme = load_themes().into_iter().find(|t| t.id == "tokyo-night").unwrap();
        assert_eq!(super::preview_split_border_style(&theme, false).fg, Some(theme.border));
        assert_eq!(super::preview_split_border_style(&theme, true).fg, Some(theme.accent));
    }

    #[test]
    fn mail_marker_is_thin_and_stays_right_of_sidebar_divider() {
        let theme = load_themes().into_iter().find(|theme| theme.id == "tokyo-night").unwrap();
        let visual = super::mail_gutter(&theme, true, false, true);
        assert_eq!(visual.content.as_ref(), "▍");
        assert_eq!(visual.style.fg, Some(theme.visual));
        assert_eq!(super::mail_gutter(&theme, false, false, true).content.as_ref(), " ");
        assert_eq!(super::mail_gutter(&theme, false, false, false).content.as_ref(), "▏");
    }

    #[test]
    fn compose_exit_uses_configured_square_or_rounded_corners() {
        use ratatui::{Terminal, backend::TestBackend, widgets::BorderType};
        for (border_type, expected) in [
            (BorderType::Plain, '┌'),
            (BorderType::Rounded, '╭'),
        ] {
            let mut theme = load_themes().into_iter().next().unwrap();
            theme.border_type = border_type;
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| super::draw_compose_exit_prompt(frame, &theme)).unwrap();
            let buffer = terminal.backend().buffer();
            let actual = (0u16..24).flat_map(|y| (0u16..80).map(move |x| (x, y)))
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
        let theme = load_themes().into_iter().find(|theme| theme.id == "tokyo-night").unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| super::draw_compose_exit_prompt(frame, &theme)).unwrap();
        let buffer = terminal.backend().buffer();
        let lines = (0u16..24).map(|y| (0u16..80).map(|x|
            buffer[(x, y)].symbol()).collect::<Vec<_>>().concat()).collect::<Vec<_>>();
        let rendered = lines.join("\n");
        assert!(rendered.contains("[Y]es"));
        assert!(rendered.contains("[N]o"));
        assert!(!rendered.contains("Esc"));
        assert!(!rendered.contains("Keep editing"));
        assert!(!rendered.contains("Discard"));
    }

    #[test]
    fn delete_review_names_align_for_single_and_double_digit_indices() {
        let theme = load_themes().into_iter().find(|theme| theme.id == "tokyo-night").unwrap();
        let one = super::review_item_line(1, 2, "First", 40, &theme);
        let ten = super::review_item_line(10, 2, "Tenth", 40, &theme);
        assert_eq!(one.spans.len(), 2); // no colored selection gutter
        assert_eq!(ten.spans.len(), 2);
        assert_eq!(one.spans[0].content.as_ref(), "  1  ");
        assert_eq!(ten.spans[0].content.as_ref(), " 10  ");
        assert_eq!(one.spans[1].content.as_ref(), "First");
        assert_eq!(ten.spans[1].content.as_ref(), "Tenth");
        assert_eq!(super::destructive_button_style(&theme).bg, Some(theme.error));
    }

    #[test]
    fn contacts_status_uses_account_and_same_mode_slot_as_mail() {
        let theme = load_themes().into_iter().find(|theme| theme.id == "tokyo-night").unwrap();
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
        assert!(!search.iter().any(|span| span.content.contains("/john")),
            "the editable query belongs above the contacts list");
        let details = super::contact_footer_details(&theme, &modal);
        assert!(details.is_empty(), "matches belong beside SEARCH, not at mid-footer");
        modal.searching = false;
        assert_eq!(super::contact_status_spans(&theme, Some("Gmail"), &modal).len(), 2);
        let filtered = super::contact_footer_details(&theme, &modal);
        assert!(filtered[0].content.contains("filter:/john"));
        modal.marked.insert("test@example.com".to_owned());
        let mut status = super::contact_status_spans(&theme, Some("Gmail"), &modal);
        status.extend(super::contact_footer_details(&theme, &modal));
        assert_eq!(status[0].content.as_ref(), "[Gmail] ");
        assert_eq!(status[1].content.as_ref(), " NORMAL ");
        assert_eq!(status[2].content.as_ref(), " 1 marked ",
            "marked count should follow the mode immediately at the left");
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
        let theme = load_themes().into_iter().find(|theme| theme.id == "tokyo-night").unwrap();
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
        let normal = plain_reader_line("- old text", 0, 10, 40, 9, 0, None, true, &theme,
            Some(PatchLineKind::Remove), None);
        let active = plain_reader_line("- old text", 0, 10, 40, 0, 0, None, true, &theme,
            Some(PatchLineKind::Remove), None);
        assert_eq!(normal.spans.last().and_then(|s| s.style.bg), diff.bg);
        assert_eq!(active.spans.last().and_then(|s| s.style.bg), theme.current_line().bg);
        assert!(!active.spans.last().is_some_and(|s|
            s.style.add_modifier.contains(ratatui::style::Modifier::BOLD)),
            "only the current relative line number should be bold");

        let added = plain_reader_line("+ new text", 0, 10, 40, 0, 0, None, true,
            &theme, Some(PatchLineKind::Add), None);
        assert_eq!(added.spans.last().and_then(|s| s.style.bg), theme.current_line().bg);
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
            .wrap(Wrap { trim: false }).line_count(20);
        let cursor_row = reader_visual_cursor_row(&lines, 1, 1, 20).unwrap();
        assert!(cursor_row >= preceding_rows);
        assert!(cursor_row > 2, "wrapped rows must contribute to the scroll position");
        assert_eq!(super::reader_active_diff_bg(&theme, theme.error), theme.popup_bg);
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
        let text = |show_details| super::reader_header_lines(&message, show_details, &theme)
            .into_iter()
            .map(|line| line.spans.into_iter().map(|span| span.content.into_owned()).collect::<String>())
            .collect::<Vec<_>>().join("\n");
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
        for address in ["one@example.org", "two@example.org", "cc@example.org", "bcc@example.org"] {
            assert!(!compact.contains(address), "leaked address: {address}");
            assert!(expanded.contains(address), "missing expanded address: {address}");
        }
        assert!(expanded.contains("From: Alex Sender <alex@example.org>"));
        assert!(expanded.contains("Message-ID: <message@example.org>"));
    }

    #[test]
    fn compact_recipients_support_quoted_commas_and_bare_addresses() {
        assert_eq!(super::compact_recipient_header(
            "\"Doe, Jane\" <jane@example.com>, bare@example.com, <only@example.com>"),
            "Doe, Jane, bare@example.com, <only@example.com>");
        assert_eq!(super::compact_recipient_header("Research Team <team@example.com>;"),
            "Research Team;");
    }

    #[test]
    fn missing_sender_name_keeps_address_even_when_compact() {
        assert_eq!(super::sender_header_value("", "nobody@example.org", false), "nobody@example.org");
        assert_eq!(super::sender_header_value("Alex", "alex@example.org", false), "Alex");
        assert_eq!(super::sender_header_value("Alex", "alex@example.org", true), "Alex <alex@example.org>");
    }

    #[test]
    fn message_attachment_subtitle_has_no_extra_indentation() {
        let names = vec!["config.example.toml".to_owned(), "LICENSE".to_owned()];
        assert_eq!(super::attachment_subtitle(&names), "config.example.toml · LICENSE");
        assert_eq!(super::attachment_subtitle(&[]), "Attachment");
    }

    #[test]
    fn composer_recipient_headers_wrap_without_truncating_or_losing_cursor() {
        use crate::app::{ComposeField, ComposeMode, Composer};
        let mut composer = Composer::new();
        composer.field = ComposeField::To;
        composer.mode = ComposeMode::Insert;
        composer.to = "Jane Doe <jane@example.org>, Björk <bjork@example.org>, Zoe <zoe@example.org>".into();
        composer.cursor = composer.to.chars().count();
        let theme = load_themes().into_iter().next().unwrap();
        let display = super::compose_header_display(ComposeField::To, "To", &composer.to, &composer, &theme, 24);
        assert!(display.lines.len() > 2);
        // Strip only layout indentation, never characters from the input.
        let mut reconstructed = String::new();
        for (row_number, line) in display.lines.iter().enumerate() {
            let text = line.spans.iter().map(|span| span.content.as_ref()).collect::<String>();
            if row_number == 0 { reconstructed.push_str(text.strip_prefix("▸ To: ").unwrap()); }
            else { reconstructed.push_str(text.strip_prefix("      ").unwrap()); }
        }
        assert_eq!(reconstructed, composer.to);
        assert!(display.cursor_row > 0);
        assert!(display.cursor_col < 24);
        for row in &display.lines {
            let text = row.spans.iter().map(|span| span.content.as_ref()).collect::<String>();
            assert!(UnicodeWidthStr::width(text.as_str()) <= 24, "overflow: {text:?}");
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
        let display = super::compose_header_display(ComposeField::Subject, "Subject", &composer.subject, &composer, &theme, 15);
        assert!(display.lines.len() >= 2);
        let rows = display.lines.iter().map(|line| line.spans.iter()
            .map(|span| span.content.as_ref()).collect::<String>()).collect::<Vec<_>>();
        assert!(rows.iter().any(|row| row.contains("👨\u{200d}💻")));
        assert!(rows.iter().all(|row| UnicodeWidthStr::width(row.as_str()) <= 15));
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
        use crate::app::{Composer, ComposeMode};
        let mut composer = Composer::new();
        composer.mode = ComposeMode::Insert;
        composer.cursor = 1;
        let theme = load_themes().into_iter().next().unwrap();
        let rendered = super::editor_spans("abc", &composer, true, &theme);
        assert_eq!(rendered.iter().map(|span| span.content.as_ref()).collect::<String>(), "abc");
        let rendered = super::editor_text("abc", &composer, true, &theme);
        assert_eq!(rendered.lines[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>(), "abc");
    }

    #[test]
    fn notification_indicator_has_unicode_fallback_and_nerd_opt_in() {
        let config: crate::config::Config = toml::from_str(
            "[ui]\nnew_mail_notifications = true\nnotification_icon_style = \"unicode\""
        ).unwrap();
        let app = crate::app::App::new(&config, vec![]);
        let theme = load_themes().into_iter().next().unwrap();
        assert!(super::notification_indicator(&app, &theme).0.contains('●'));
        let nerd_config: crate::config::Config = toml::from_str(
            "[ui]\nnew_mail_notifications = true\nnotification_icon_style = \"nerd\""
        ).unwrap();
        let nerd_app = crate::app::App::new(&nerd_config, vec![]);
        assert!(super::notification_indicator(&nerd_app, &theme).0.contains("󰂚"));
    }

    #[test]
    fn mailed_diff_only_colors_real_hunks_with_backgrounds() {
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
        assert!(kinds[..5].iter().all(Option::is_none));
        assert_eq!(kinds[5], Some(PatchLineKind::Meta));
        assert_eq!(kinds[9], Some(PatchLineKind::Hunk));
        assert_eq!(kinds[11], Some(PatchLineKind::Remove));
        assert_eq!(kinds[12], Some(PatchLineKind::Add));
        assert_eq!(kinds[13], None);
        let add = diff_line_style(PatchLineKind::Add, &theme);
        let remove = diff_line_style(PatchLineKind::Remove, &theme);
        assert_ne!(add.fg, remove.fg);
        assert!(add.bg.is_some());
        assert!(remove.bg.is_some());
        assert_ne!(add.bg, remove.bg);
    }

    #[test]
    fn adjacent_unified_headers_allow_non_git_patches() {
        use super::{PatchLineKind, classify_patch_lines};
        let lines = ["--- a/foo", "+++ b/foo", "@@ -1 +1 @@", "-before", "+after", "--", "-EIO"];
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
        fn old_viewport(heights: &[usize], offset: usize, current: usize, height: usize) -> (usize, usize) {
            let mut start = offset.min(heights.len() - 1);
            if current < start { start = current; }
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
            if end == start { end = (start + 1).min(heights.len()); }
            (start, end)
        }
        for mask in 0..256usize {
            let heights: Vec<_> = (0..8).map(|index| 1 + ((mask >> index) & 1)).collect();
            let visible: Vec<_> = (0..heights.len()).collect();
            for height in 1..=10 {
                for offset in 0..=heights.len() {
                    for current in 0..heights.len() {
                        assert_eq!(
                            super::message_list_viewport(&visible, offset, current, height, |index| heights[index]),
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
        assert_eq!(super::message_list_visual_range(&visible, Some(9), Some(1)), Some((1, 2)));
        assert_eq!(super::message_list_visual_range(&visible, Some(6), Some(3)), Some((0, 3)));
        assert_eq!(super::message_list_visual_range(&visible, Some(8), Some(1)), None);
        assert_eq!(super::message_list_visual_range(&visible, None, Some(1)), None);
    }

    #[test]
    fn mail_list_borrows_unchanged_subjects_and_truncation() {
        use std::borrow::Cow;
        assert!(matches!(super::subject_for_list_cow("Hello 👋"), Cow::Borrowed(_)));
        assert_eq!(super::subject_for_list_cow("❤️"), "❤\u{fe0e}");
        assert!(matches!(super::truncate_display_width_cow("Hello", 10), Cow::Borrowed(_)));
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
        assert_eq!(mailbox_row_text("[Gmail]/Trash", "5", 20), "[Gmail]/Trash      5");
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
        assert_eq!(subject_for_list("👋 Welcome to KLIPY!"), "👋 Welcome to KLIPY!");
        assert_eq!(subject_for_list("👨\u{200d}💻 Status"), "👨\u{200d}💻 Status");
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
            "Attachments (4): a.txt · image👨\u{200d}💻.png · very-long-filename.pdf · z.zip", 22, 2,
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| UnicodeWidthStr::width(row.as_str()) <= 22));
        assert!(rows.last().unwrap().ends_with('…'));
        assert!(!rows.iter().any(|row| row.ends_with('\u{200d}')));
    }

    #[test]
    fn compose_mode_badges_use_distinct_filled_colors() {
        use crate::app::ComposeMode;
        let theme = crate::theme::load_themes().into_iter().find(|t| t.id == "tokyo-night").unwrap();
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
            assert_eq!(super::compose_mode_style(crate::app::ComposeMode::Insert, &theme).bg, Some(theme.unread));
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
            0,
            0,
            None,
            true,
            &theme,
            &[(0, 4)],
        );
        assert_eq!(fragments.len(), 1);
        assert!(!fragments[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
        assert_eq!(fragments[0].style.bg, Some(super::reader_illuminate_bg(&theme)));
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
            0,
            0,
            None,
            true,
            &theme,
            &[(0, 4)],
        );
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].style.bg, Some(Color::White));
        assert_eq!(fragments[0].style.fg, Some(Color::Black));
        assert_eq!(fragments[1].style.bg, Some(super::reader_illuminate_bg(&theme)));
    }

    #[test]
    fn reader_cursor_remains_visible_on_blank_lines() {
        let theme = load_themes().into_iter().next().expect("theme");
        let mut spans = Vec::new();
        pad_reader_line(&mut spans, 0, 8, 2, 2, 0, None, true, &theme, None);

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
            filename: "notes.txt".into(), content_type: "text/plain".into(),
            data: vec![b'a'; 64 * 1024],
        };
        let preview = attachment_text_preview(&file).unwrap();
        assert!(preview.len() < file.data.len());
        assert!(preview.contains("truncated"));
        let binary = OutgoingAttachment {
            filename: "file.bin".into(), content_type: "application/octet-stream".into(),
            data: vec![0, 1, 2],
        };
        assert!(attachment_text_preview(&binary).is_none());
    }
}
