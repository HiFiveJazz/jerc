use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Theme {
    pub id: String,
    pub name: String,
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
    pub fn text(&self) -> Style {
        Style::default().fg(self.foreground)
    }

    #[must_use]
    pub fn muted(&self) -> Style {
        Style::default().fg(self.muted).add_modifier(Modifier::DIM)
    }

    #[must_use]
    pub fn accent(&self) -> Style {
        Style::default().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn unread(&self) -> Style {
        Style::default().fg(self.unread).add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn selected(&self) -> Style {
        Style::default()
            .fg(self.selected_fg)
            .bg(self.selected_bg)
            .add_modifier(Modifier::BOLD)
    }

    /// Subtle current-row/current-line style shared by mailbox, message list,
    /// and the Vim reader cursor line. Visual selections keep using `selected()`.
    #[must_use]
    pub fn current_line(&self) -> Style {
        Style::default().fg(self.foreground).bg(self.popup_bg)
    }

    #[must_use]
    pub fn border(&self) -> Style {
        Style::default().fg(self.border)
    }

    #[must_use]
    pub fn error(&self) -> Style {
        Style::default().fg(self.error).add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn popup(&self) -> Style {
        Style::default().fg(self.foreground).bg(self.popup_bg)
    }

    #[must_use]
    pub fn status(&self) -> Style {
        Style::default().fg(self.foreground).bg(self.status_bg)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ThemeSpec {
    id: Option<String>,
    name: Option<String>,
    primary: Option<String>,
    success: Option<String>,
    error: Option<String>,
    warning: Option<String>,
    text: Option<String>,
    text_strong: Option<String>,
    text_muted: Option<String>,
    background: Option<String>,
    surface: Option<String>,
    border: Option<String>,
    selected_fg: Option<String>,
    selected_bg: Option<String>,
}

const BUILTIN_THEMES: &[(&str, &str)] = &[
    ("default", include_str!("../themes/default.json")),
    ("nord", include_str!("../themes/nord.json")),
    ("gruvbox", include_str!("../themes/gruvbox.json")),
    ("catppuccin", include_str!("../themes/catppuccin.json")),
    ("tokyo-night", include_str!("../themes/tokyo-night.json")),
    ("monochrome", include_str!("../themes/monochrome.json")),
];

#[must_use]
pub fn load_themes() -> Vec<Theme> {
    let mut themes = BUILTIN_THEMES
        .iter()
        .filter_map(|(fallback_id, json)| parse_theme(json, fallback_id).ok())
        .collect::<Vec<_>>();

    if let Ok(entries) = fs::read_dir(theme_dir()) {
        let mut paths = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("json")))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let Ok(json) = fs::read_to_string(&path) else {
                continue;
            };
            let fallback_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("custom");
            let Ok(theme) = parse_theme(&json, fallback_id) else {
                continue;
            };
            if let Some(index) = themes
                .iter()
                .position(|existing| existing.id.eq_ignore_ascii_case(&theme.id))
            {
                themes[index] = theme;
            } else {
                themes.push(theme);
            }
        }
    }

    if themes.is_empty() {
        themes.push(fallback_theme());
    }
    themes
}

fn parse_theme(json: &str, fallback_id: &str) -> Result<Theme> {
    let spec: ThemeSpec = serde_json::from_str(json).context("parsing theme JSON")?;
    let id = spec.id.unwrap_or_else(|| fallback_id.to_owned());
    let name = spec.name.unwrap_or_else(|| id.clone());
    let primary = parse_color(spec.primary.as_deref().unwrap_or("#7aa2f7"))?;
    let success = parse_color(spec.success.as_deref().unwrap_or("#9ece6a"))?;
    let error = parse_color(spec.error.as_deref().unwrap_or("#f7768e"))?;
    let _ = parse_color(spec.warning.as_deref().unwrap_or("#e0af68"))?;
    let foreground = parse_color(
        spec.text
            .as_deref()
            .or(spec.text_strong.as_deref())
            .unwrap_or("#c0caf5"),
    )?;
    let muted = parse_color(
        spec.text_muted
            .as_deref()
            .or(spec.border.as_deref())
            .unwrap_or("#565f89"),
    )?;
    let background = parse_color(spec.background.as_deref().unwrap_or("reset"))?;
    let surface = parse_color(spec.surface.as_deref().unwrap_or_else(|| {
        spec.background.as_deref().unwrap_or("reset")
    }))?;
    let border = parse_color(spec.border.as_deref().unwrap_or("#414868"))?;
    let selected_fg = parse_color(
        spec.selected_fg
            .as_deref()
            .or(spec.background.as_deref())
            .unwrap_or("#1a1b26"),
    )?;
    let selected_bg = parse_color(spec.selected_bg.as_deref().unwrap_or_else(|| {
        spec.primary.as_deref().unwrap_or("#7aa2f7")
    }))?;

    Ok(Theme {
        id,
        name,
        foreground,
        muted,
        accent: primary,
        unread: success,
        selected_fg,
        selected_bg,
        border,
        error,
        popup_bg: surface,
        status_bg: background,
    })
}

fn parse_color(value: &str) -> Result<Color> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("reset") || value.eq_ignore_ascii_case("none") {
        return Ok(Color::Reset);
    }
    let hex = value
        .strip_prefix('#')
        .ok_or_else(|| anyhow::anyhow!("theme color must be #RRGGBB or reset: {value}"))?;
    if hex.len() != 6 {
        return Err(anyhow::anyhow!("theme color must be six hex digits: {value}"));
    }
    let red = u8::from_str_radix(&hex[0..2], 16).context("parsing red theme component")?;
    let green = u8::from_str_radix(&hex[2..4], 16).context("parsing green theme component")?;
    let blue = u8::from_str_radix(&hex[4..6], 16).context("parsing blue theme component")?;
    Ok(Color::Rgb(red, green, blue))
}

fn fallback_theme() -> Theme {
    Theme {
        id: "default".to_owned(),
        name: "Default".to_owned(),
        foreground: Color::Reset,
        muted: Color::DarkGray,
        accent: Color::Cyan,
        unread: Color::White,
        selected_fg: Color::Black,
        selected_bg: Color::Cyan,
        border: Color::DarkGray,
        error: Color::LightRed,
        popup_bg: Color::Reset,
        status_bg: Color::Reset,
    }
}

#[must_use]
pub fn theme_index(themes: &[Theme], name: &str) -> usize {
    themes
        .iter()
        .position(|theme| {
            theme.id.eq_ignore_ascii_case(name.trim()) || theme.name.eq_ignore_ascii_case(name.trim())
        })
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

#[must_use]
pub fn theme_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("aerc-rs")
        .join("themes")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{load_themes, parse_theme, theme_index};

    #[test]
    fn built_in_theme_names_are_unique() {
        let themes = load_themes();
        let names: HashSet<_> = themes.iter().map(|theme| theme.id.as_str()).collect();
        assert_eq!(names.len(), themes.len());
    }

    #[test]
    fn theme_lookup_is_case_insensitive_and_safe() {
        let themes = load_themes();
        assert_eq!(themes[theme_index(&themes, "NORD")].id, "nord");
        assert_eq!(themes[theme_index(&themes, "does-not-exist")].id, "default");
    }

    #[test]
    fn lazygitrs_style_flat_json_is_supported() {
        let theme = parse_theme(
            r##"{"id":"mine","name":"Mine","primary":"#ff6600","success":"#00ff88","error":"#ff3333","warning":"#ffcc00","text_strong":"#ffffff","background":"#1a1a2e"}"##,
            "fallback",
        )
        .unwrap();
        assert_eq!(theme.id, "mine");
        assert_eq!(theme.name, "Mine");
    }
}
