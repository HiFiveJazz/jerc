//! Shared keyboard navigation for every searchable keybinding popup.
//! The owner keeps its own visibility flag, but query/selection semantics are
//! identical across the global, Compose, Contacts and picker help palettes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::navigation::filtered_move;
use super::{HelpEntry, ranked_help_matches};

#[derive(Debug, Clone, Copy)]
pub(crate) struct HelpKeys {
    /// Global and Contacts help use ? as an unconditional close shortcut.
    pub close_on_question: bool,
    /// The application-wide help cycles from last to first; local help stops.
    pub wrap: bool,
}

impl HelpKeys {
    pub const GLOBAL: Self = Self {
        close_on_question: true,
        wrap: true,
    };
    pub const MODAL: Self = Self {
        close_on_question: true,
        wrap: false,
    };
    pub const TEXT_MODAL: Self = Self {
        close_on_question: false,
        wrap: false,
    };
}

/// Returns true if the caller should close the help popup. Other changes are
/// made directly to its existing state to avoid introducing a parallel model.
pub(crate) fn handle_help_key(
    key: KeyEvent,
    entries: &[HelpEntry],
    query: &mut String,
    selected: &mut usize,
    scroll: &mut usize,
    options: HelpKeys,
) -> bool {
    if key.modifiers.is_empty()
        && (key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('?') && (options.close_on_question || query.is_empty())))
    {
        return true;
    }

    if key.modifiers.is_empty() && key.code == KeyCode::Backspace {
        query.pop();
        reset_help(entries, query, selected, scroll);
        return false;
    }

    let movement = if key.modifiers.is_empty() {
        match key.code {
            KeyCode::Up => Some(-1),
            KeyCode::Down => Some(1),
            KeyCode::Char('k') if query.is_empty() => Some(-1),
            KeyCode::Char('j') if query.is_empty() => Some(1),
            _ => None,
        }
    } else {
        None
    };
    if let Some(delta) = movement {
        let matches = ranked_help_matches(entries, query);
        if let Some((index, position)) = filtered_move(&matches, *selected, delta, options.wrap) {
            *selected = index;
            *scroll = position.saturating_sub(5);
        }
        return false;
    }
    if key.modifiers.is_empty() && matches!(key.code, KeyCode::Home | KeyCode::End) {
        let matches = ranked_help_matches(entries, query);
        if let Some(&index) = if key.code == KeyCode::Home {
            matches.first()
        } else {
            matches.last()
        } {
            *selected = index;
            *scroll = if key.code == KeyCode::Home {
                0
            } else {
                matches.len().saturating_sub(7)
            };
        }
        return false;
    }

    if let KeyCode::Char(ch) = key.code
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        query.push(ch);
        reset_help(entries, query, selected, scroll);
    }
    false
}

pub(crate) fn reset_help(
    entries: &[HelpEntry],
    query: &str,
    selected: &mut usize,
    scroll: &mut usize,
) {
    *selected = ranked_help_matches(entries, query)
        .first()
        .copied()
        .unwrap_or(0);
    *scroll = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    const ENTRIES: &[HelpEntry] = &[
        HelpEntry {
            category: "Action",
            key: "a",
            description: "archive",
        },
        HelpEntry {
            category: "Action",
            key: "d",
            description: "delete",
        },
        HelpEntry {
            category: "Action",
            key: "r",
            description: "reply",
        },
    ];
    fn press(code: KeyCode, query: &mut String, selected: &mut usize, scroll: &mut usize) -> bool {
        handle_help_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            ENTRIES,
            query,
            selected,
            scroll,
            HelpKeys::MODAL,
        )
    }

    #[test]
    fn navigation_is_bounded_and_search_resets_selection() {
        let (mut query, mut selected, mut scroll) = (String::new(), 0, 0);
        assert!(!press(
            KeyCode::Down,
            &mut query,
            &mut selected,
            &mut scroll
        ));
        assert_eq!(selected, 1);
        assert!(!press(KeyCode::End, &mut query, &mut selected, &mut scroll));
        assert_eq!(selected, 2);
        assert!(!press(
            KeyCode::Down,
            &mut query,
            &mut selected,
            &mut scroll
        ));
        assert_eq!(selected, 2);
        assert!(!press(
            KeyCode::Char('d'),
            &mut query,
            &mut selected,
            &mut scroll
        ));
        assert_eq!(query, "d");
        assert_eq!(selected, 1);
        assert!(!press(
            KeyCode::Backspace,
            &mut query,
            &mut selected,
            &mut scroll
        ));
        assert_eq!(selected, 0);
        assert!(press(KeyCode::Esc, &mut query, &mut selected, &mut scroll));
    }

    #[test]
    fn question_can_still_be_typed_in_text_mode() {
        let (mut query, mut selected, mut scroll) = (String::from("a"), 0, 0);
        assert!(!handle_help_key(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            ENTRIES,
            &mut query,
            &mut selected,
            &mut scroll,
            HelpKeys::TEXT_MODAL
        ));
        assert_eq!(query, "a?");
    }
}
