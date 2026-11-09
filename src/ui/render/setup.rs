//! Setup rendering.

use super::*;

/// Two-step account setup: identity first, discovered and editable settings
/// second. Each selected row paints its entire rectangle, not just the text.
pub(super) fn draw_setup_wizard(frame: &mut Frame<'_>, app: &App) {
    use crate::setup::{
        CLIENT_ID, EMAIL, GOOGLE_SECRET, NAME, PROVIDER, SetupDisplayRow, SetupStep,
    };

    let Some(wizard) = app.setup.as_ref() else {
        return;
    };
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
    let stages = [
        "Add Email",
        "Review and Connect",
        "Browser Sign-In",
        "Confirmation",
    ];
    let active = wizard.progress_stage();
    let mut progress = Vec::new();
    for (i, stage) in stages.iter().enumerate() {
        if i != 0 {
            progress.push(Span::styled("  →  ", theme.muted()));
        }
        let style = if i == active {
            theme.accent()
        } else {
            theme.muted()
        };
        progress.push(Span::styled(*stage, style));
    }
    // A single border title avoids the two titles overlapping at smaller widths.
    let block = Block::default()
        .title(Line::from(progress).centered())
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.accent());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 7 || inner.width < 15 {
        return;
    }
    let content = inner;
    if wizard.saved {
        let description = if wizard.authenticated {
            "Your account is configured and signed in."
        } else {
            "Your account was saved. Sign in later to access mail."
        };
        let saved_regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(content);
        let lines = vec![
            Line::styled("✓ Account saved successfully", setup_success_style()),
            Line::raw(""),
            Line::styled(
                format!("{}  ·  {}", wizard.fields[NAME], wizard.fields[EMAIL]),
                theme.text(),
            ),
            Line::raw(""),
            Line::styled(description, theme.muted()),
        ];
        // The confirmation's success banner already conveys that the account
        // was saved. Avoid repeating an additional "Account saved" message.
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            saved_regions[0],
        );
        let button_area = Rect::new(
            saved_regions[1].x + saved_regions[1].width.saturating_sub(26) / 2,
            saved_regions[1].y,
            26.min(saved_regions[1].width),
            1,
        );
        frame.render_widget(
            Paragraph::new(" Open account ")
                .alignment(Alignment::Center)
                .style(setup_button_style(&theme)),
            button_area,
        );
        return;
    }
    if wizard.step == SetupStep::Authenticating {
        // This is a real stage for *both* Google and Microsoft, including
        // when sign-in is launched from the editable review page.
        let provider = if wizard.provider_hint() == "google" {
            "Google"
        } else {
            "Microsoft 365"
        };
        let lines = vec![
            Line::styled(format!("Sign in with {provider}"), theme.accent()),
            Line::raw(""),
            Line::styled(wizard.fields[EMAIL].clone(), theme.text()),
            Line::raw(""),
            Line::styled(
                format!("{}  Complete sign-in in your browser…", wizard.spinner()),
                theme.text(),
            ),
            Line::raw(""),
            Line::styled(
                "This page will update when authentication finishes.",
                theme.muted(),
            ),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), content);
        return;
    }

    let identity_step = wizard.step == SetupStep::Identity;
    let header_height = if identity_step { 1 } else { 3 };
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(content);

    if identity_step {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Enter your account name and email address.",
                theme.muted(),
            )),
            regions[0],
        );
    } else {
        let found = if wizard.source.is_some() {
            "Account settings found"
        } else {
            "Manual account settings"
        };
        let source = wizard
            .source
            .as_deref()
            .unwrap_or("Edit the settings below");
        let found_style = if wizard.source.is_some() {
            setup_success_style()
        } else {
            theme.accent()
        };
        // Render as a full-width success strip, not merely green text.
        frame.render_widget(
            Paragraph::new(format!(" ✓ {found} ")).style(found_style),
            Rect::new(regions[0].x, regions[0].y, regions[0].width, 1),
        );
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("{}  ·  {}", wizard.fields[NAME], wizard.fields[EMAIL]),
                theme.text(),
            )),
            Rect::new(regions[0].x, regions[0].y + 1, regions[0].width, 1),
        );
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("{}  ·  {}", wizard.fields[PROVIDER], source),
                theme.muted(),
            )),
            Rect::new(regions[0].x, regions[0].y + 2, regions[0].width, 1),
        );
    }

    let display_rows = wizard.display_rows();
    let list_height = usize::from(regions[1].height);
    let selected_pos = display_rows
        .iter()
        .position(|row| matches!(row, SetupDisplayRow::Field(index) if *index == wizard.selected))
        .unwrap_or(0);
    let first = selected_pos
        .saturating_sub(list_height.saturating_sub(1))
        .min(display_rows.len().saturating_sub(list_height));
    for (row, entry) in display_rows
        .iter()
        .skip(first)
        .take(list_height)
        .enumerate()
    {
        let row_area = Rect::new(regions[1].x, regions[1].y + row as u16, regions[1].width, 1);
        let width = usize::from(row_area.width);
        match entry {
            SetupDisplayRow::BoxTop(title) => {
                let (left, right) = match theme.border_type {
                    ratatui::widgets::BorderType::Rounded => ('╭', '╮'),
                    _ => ('┌', '┐'),
                };
                let start = format!("{left}─ {title} ");
                let line = format!(
                    "{}{}{right}",
                    start,
                    "─".repeat(width.saturating_sub(start.chars().count() + 1))
                );
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
                let style = if selected {
                    theme.current_line()
                } else {
                    theme.text()
                };
                // Paint full cell width, not just as many columns as the label.
                frame.render_widget(Block::default().style(style), row_area);
                let label = crate::setup::LABELS[*index];
                if identity_step && wizard.fields[*index].is_empty() {
                    // Examples are visual placeholders, never persisted values.
                    // Keep them dim even when the selection paints the row.
                    let example = if *index == NAME {
                        "e.g. Personal"
                    } else {
                        "name@example.com"
                    };
                    let prefix = format!(" {} {label:<20} ", if selected { "›" } else { " " });
                    let caret = if selected { "▏" } else { "" };
                    let available =
                        width.saturating_sub(prefix.chars().count() + caret.chars().count());
                    let placeholder = example.chars().take(available).collect::<String>();
                    let placeholder_style = if selected {
                        Style::default().fg(theme.muted).bg(theme.popup_bg)
                    } else {
                        Style::default().fg(theme.muted)
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(prefix, style),
                            Span::styled(caret, style),
                            Span::styled(placeholder, placeholder_style),
                        ])),
                        row_area,
                    );
                    continue;
                }
                let value = wizard.fields[*index].as_str();
                let value = if *index == CLIENT_ID
                    && value.is_empty()
                    && crate::oauth::embedded_client_id(wizard.provider_hint()).is_some()
                {
                    "(publisher-configured)"
                } else if *index == GOOGLE_SECRET
                    && value.is_empty()
                    && crate::oauth::embedded_google_client_secret().is_some()
                {
                    "(publisher-configured)"
                } else if value.is_empty() {
                    "(enter value)"
                } else {
                    value
                };
                let value = if *index == GOOGLE_SECRET && !wizard.fields[*index].is_empty() {
                    "(configured)".to_owned()
                } else {
                    value.to_owned()
                };
                let value = if *index == PROVIDER {
                    format!("◀ {value} ▶")
                } else if selected && !wizard.authenticating && !wizard.saved {
                    format!("{value}▏")
                } else {
                    value
                };
                let boxed = !identity_step && *index != PROVIDER;
                let maximum = width.saturating_sub(if boxed { 28 } else { 24 });
                let visible = value.chars().take(maximum).collect::<String>();
                let body = format!(
                    " {} {label:<20} {visible}",
                    if selected { "›" } else { " " }
                );
                let content_width = width.saturating_sub(if boxed { 2 } else { 0 });
                let mut text = body.chars().take(content_width).collect::<String>();
                text.push_str(&" ".repeat(content_width.saturating_sub(text.chars().count())));
                if boxed && width >= 2 {
                    // Keep vertical borders the same muted color as the top
                    // and bottom, even on a selected row. The row background
                    // is still painted across the entire width.
                    let border_style = if selected {
                        theme.border().bg(theme.popup_bg)
                    } else {
                        theme.border()
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled("│", border_style),
                            Span::styled(text, style),
                            Span::styled("│", border_style),
                        ])),
                        row_area,
                    );
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
        } else {
            wizard.message.clone()
        };
        let status_style = if wizard.error {
            theme.error()
        } else {
            theme.muted()
        };
        frame.render_widget(
            Paragraph::new(status).style(status_style),
            Rect::new(regions[2].x, regions[2].y, regions[2].width, 1),
        );
    }
    let action = if identity_step {
        "Continue"
    } else if wizard.authenticated {
        "Save and open account"
    } else if wizard.login_failed {
        "Retry sign-in"
    } else {
        "Continue to Browser Sign-In"
    };
    if !wizard.authenticating && !wizard.discovering {
        let button_width = (action.chars().count() as u16)
            .saturating_add(6)
            .min(regions[2].width);
        let button_area = Rect::new(
            regions[2].x + regions[2].width.saturating_sub(button_width) / 2,
            regions[2].y + 2,
            button_width,
            1,
        );
        frame.render_widget(
            Paragraph::new(format!("  {action}  "))
                .alignment(Alignment::Center)
                .style(setup_button_style(&theme)),
            button_area,
        );
    }
}

/// Consistent high-contrast success treatment without changing the theme's
/// accent, error, or currently selected row colors.
pub(super) fn setup_success_style() -> Style {
    Style::default()
        .fg(Color::Rgb(158, 236, 184))
        .bg(Color::Rgb(24, 69, 46))
        .add_modifier(Modifier::BOLD)
}

pub(super) fn setup_button_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.popup_bg)
        .bg(theme.accent)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn destructive_button_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.selected_fg)
        .bg(theme.error)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn draw_startup_warnings(frame: &mut Frame<'_>, app: &App) {
    let theme = app.theme().clone();
    let area = centered_rect(frame.area(), 85, 55);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Account configuration warnings ")
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.error());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let details = app
        .startup_warnings
        .iter()
        .enumerate()
        .map(|(i, issue)| format!("{}. {}\n", i + 1, issue))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(Paragraph::new(format!(
        "Other usable accounts remain active. Fix config.toml and restart to re-enable affected accounts or sending.\n\n{details}\nEsc / q / ! to close"
    )).style(theme.text()).wrap(Wrap { trim: false }), inner);
}
