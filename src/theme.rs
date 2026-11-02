use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub foreground: Color,
    pub muted: Color,
    pub accent: Color,
    pub unread: Color,
    pub selected_fg: Color,
    pub selected_bg: Color,
    pub border: Color,
    pub error: Color,
    pub popup_bg: Color,
    pub status_bg: Color,
}

impl Theme {
    #[must_use]
    pub fn text(self) -> Style {
        Style::default().fg(self.foreground)
    }

    #[must_use]
    pub fn muted(self) -> Style {
        Style::default().fg(self.muted).add_modifier(Modifier::DIM)
    }

    #[must_use]
    pub fn accent(self) -> Style {
        Style::default().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn unread(self) -> Style {
        Style::default().fg(self.unread).add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn selected(self) -> Style {
        Style::default()
            .fg(self.selected_fg)
            .bg(self.selected_bg)
            .add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn border(self) -> Style {
        Style::default().fg(self.border)
    }

    #[must_use]
    pub fn error(self) -> Style {
        Style::default().fg(self.error).add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn popup(self) -> Style {
        Style::default().fg(self.foreground).bg(self.popup_bg)
    }

    #[must_use]
    pub fn status(self) -> Style {
        Style::default().fg(self.foreground).bg(self.status_bg)
    }
}

pub const THEMES: [Theme; 6] = [
    Theme {
        name: "default",
        foreground: Color::Reset,
        muted: Color::DarkGray,
        accent: Color::Cyan,
        unread: Color::White,
        selected_fg: Color::Black,
        selected_bg: Color::Cyan,
        border: Color::DarkGray,
        error: Color::LightRed,
        popup_bg: Color::Black,
        status_bg: Color::Reset,
    },
    Theme {
        name: "nord",
        foreground: Color::Rgb(216, 222, 233),
        muted: Color::Rgb(76, 86, 106),
        accent: Color::Rgb(136, 192, 208),
        unread: Color::Rgb(235, 203, 139),
        selected_fg: Color::Rgb(46, 52, 64),
        selected_bg: Color::Rgb(136, 192, 208),
        border: Color::Rgb(76, 86, 106),
        error: Color::Rgb(191, 97, 106),
        popup_bg: Color::Rgb(46, 52, 64),
        status_bg: Color::Rgb(59, 66, 82),
    },
    Theme {
        name: "gruvbox",
        foreground: Color::Rgb(235, 219, 178),
        muted: Color::Rgb(146, 131, 116),
        accent: Color::Rgb(250, 189, 47),
        unread: Color::Rgb(184, 187, 38),
        selected_fg: Color::Rgb(40, 40, 40),
        selected_bg: Color::Rgb(250, 189, 47),
        border: Color::Rgb(102, 92, 84),
        error: Color::Rgb(251, 73, 52),
        popup_bg: Color::Rgb(40, 40, 40),
        status_bg: Color::Rgb(60, 56, 54),
    },
    Theme {
        name: "catppuccin",
        foreground: Color::Rgb(205, 214, 244),
        muted: Color::Rgb(108, 112, 134),
        accent: Color::Rgb(137, 180, 250),
        unread: Color::Rgb(166, 227, 161),
        selected_fg: Color::Rgb(30, 30, 46),
        selected_bg: Color::Rgb(137, 180, 250),
        border: Color::Rgb(88, 91, 112),
        error: Color::Rgb(243, 139, 168),
        popup_bg: Color::Rgb(30, 30, 46),
        status_bg: Color::Rgb(49, 50, 68),
    },
    Theme {
        name: "tokyo-night",
        foreground: Color::Rgb(192, 202, 245),
        muted: Color::Rgb(86, 95, 137),
        accent: Color::Rgb(125, 207, 255),
        unread: Color::Rgb(158, 206, 106),
        selected_fg: Color::Rgb(26, 27, 38),
        selected_bg: Color::Rgb(125, 207, 255),
        border: Color::Rgb(65, 72, 104),
        error: Color::Rgb(247, 118, 142),
        popup_bg: Color::Rgb(26, 27, 38),
        status_bg: Color::Rgb(36, 40, 59),
    },
    Theme {
        name: "monochrome",
        foreground: Color::White,
        muted: Color::DarkGray,
        accent: Color::White,
        unread: Color::White,
        selected_fg: Color::Black,
        selected_bg: Color::White,
        border: Color::DarkGray,
        error: Color::White,
        popup_bg: Color::Black,
        status_bg: Color::Reset,
    },
];

#[must_use]
pub fn theme_index(name: &str) -> usize {
    THEMES
        .iter()
        .position(|theme| theme.name.eq_ignore_ascii_case(name.trim()))
        .unwrap_or(0)
}

#[must_use]
pub fn persisted_theme() -> Option<String> {
    fs::read_to_string(theme_state_path())
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn persist_theme(name: &str) -> Result<()> {
    let path = theme_state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating theme state directory {}", parent.display()))?;
    }
    fs::write(&path, format!("{name}\n"))
        .with_context(|| format!("writing theme state {}", path.display()))
}

#[must_use]
pub fn theme_state_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("aerc-rs")
        .join("theme")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{THEMES, theme_index};

    #[test]
    fn built_in_theme_names_are_unique() {
        let names: HashSet<_> = THEMES.iter().map(|theme| theme.name).collect();
        assert_eq!(names.len(), THEMES.len());
    }

    #[test]
    fn theme_lookup_is_case_insensitive_and_safe() {
        assert_eq!(THEMES[theme_index("NORD")].name, "nord");
        assert_eq!(THEMES[theme_index("does-not-exist")].name, "default");
    }
}
