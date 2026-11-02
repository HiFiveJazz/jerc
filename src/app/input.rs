use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Mailboxes,
    Messages,
    Preview,
}

impl Focus {
    #[must_use]
    pub const fn next(self, message_open: bool) -> Self {
        if message_open {
            return Self::Preview;
        }
        match self {
            Self::Mailboxes => Self::Messages,
            Self::Messages | Self::Preview => Self::Mailboxes,
        }
    }

    #[must_use]
    pub const fn previous(self, message_open: bool) -> Self {
        self.next(message_open)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpEntry {
    pub category: &'static str,
    pub key: &'static str,
    pub description: &'static str,
}

/// Keep this list as the single source of truth for the searchable `?` popup.
/// Count prefixes are intentionally not listed: `20j` is a Vim motion composed
/// from a count plus `j`, not a separate keybinding.
pub const HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry { category: "Browse", key: "j / k, ↑ / ↓", description: "move in the focused list" },
    HelpEntry { category: "Browse", key: "J / K", description: "move the message list regardless of focus" },
    HelpEntry { category: "Browse", key: "Ctrl-j / Ctrl-k", description: "smooth 22-row scroll" },
    HelpEntry { category: "Browse", key: "g / G", description: "top / bottom" },
    HelpEntry { category: "Browse", key: "Ctrl-d / Ctrl-u", description: "page down / up" },
    HelpEntry { category: "Browse", key: "Tab / Shift-Tab", description: "switch mailbox/message focus" },
    HelpEntry { category: "Browse", key: "Enter / l", description: "open mailbox or message" },
    HelpEntry { category: "Mail", key: "r / R", description: "reply" },
    HelpEntry { category: "Mail", key: "f", description: "forward" },
    HelpEntry { category: "Mail", key: "c", description: "compose new message" },
    HelpEntry { category: "Mail", key: "d", description: "move to Trash; delete permanently from Trash" },
    HelpEntry { category: "Mail", key: "a", description: "archive selected message" },
    HelpEntry { category: "Mail", key: "u", description: "toggle read / unread" },
    HelpEntry { category: "Message", key: "Esc", description: "return to message list" },
    HelpEntry { category: "Message", key: "h", description: "toggle plain / rendered HTML" },
    HelpEntry { category: "Message", key: "e", description: "export raw RFC 5322 message" },
    HelpEntry { category: "General", key: "t", description: "open searchable theme picker" },
    HelpEntry { category: "General", key: "Ctrl-r", description: "refresh current account" },
    HelpEntry { category: "General", key: "[ / ]", description: "previous / next account" },
    HelpEntry { category: "General", key: "?", description: "search keybindings" },
    HelpEntry { category: "General", key: "q / Ctrl-c", description: "quit" },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    Down,
    Up,
    MessageDown,
    MessageUp,
    SmoothDown,
    SmoothUp,
    PageDown,
    PageUp,
    Top,
    Bottom,
    FocusNext,
    FocusPrevious,
    Open,
    ToggleSeen,
    Delete,
    Archive,
    Refresh,
    NextAccount,
    PreviousAccount,
    ToggleThemePicker,
    ExportRaw,
    ToggleHelp,
    ToggleHtml,
    Compose,
    Reply,
    Forward,
    Escape,
    Digit(u8),
    None,
}

#[must_use]
pub fn command_for_key(key: KeyEvent) -> Command {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Command::Quit,
            KeyCode::Char('d') => Command::PageDown,
            KeyCode::Char('u') => Command::PageUp,
            KeyCode::Char('j') => Command::SmoothDown,
            KeyCode::Char('k') => Command::SmoothUp,
            KeyCode::Char('r') => Command::Refresh,
            KeyCode::PageDown => Command::NextAccount,
            KeyCode::PageUp => Command::PreviousAccount,
            _ => Command::None,
        };
    }

    match key.code {
        KeyCode::Char('q') => Command::Quit,
        KeyCode::Char('j') | KeyCode::Down => Command::Down,
        KeyCode::Char('k') | KeyCode::Up => Command::Up,
        KeyCode::Char('J') => Command::MessageDown,
        KeyCode::Char('K') => Command::MessageUp,
        KeyCode::Char('g') | KeyCode::Home => Command::Top,
        KeyCode::Char('G') | KeyCode::End => Command::Bottom,
        KeyCode::Char(' ') | KeyCode::PageDown => Command::PageDown,
        KeyCode::PageUp => Command::PageUp,
        KeyCode::Tab => Command::FocusNext,
        KeyCode::BackTab => Command::FocusPrevious,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Command::Open,
        KeyCode::Char('u') => Command::ToggleSeen,
        KeyCode::Char('d') => Command::Delete,
        KeyCode::Char('a') => Command::Archive,
        KeyCode::Char('r') | KeyCode::Char('R') => Command::Reply,
        KeyCode::Char('f') => Command::Forward,
        KeyCode::Char(']') => Command::NextAccount,
        KeyCode::Char('[') => Command::PreviousAccount,
        KeyCode::Char('t') => Command::ToggleThemePicker,
        KeyCode::Char('e') => Command::ExportRaw,
        KeyCode::Char('?') => Command::ToggleHelp,
        KeyCode::Char('h') => Command::ToggleHtml,
        KeyCode::Char('c') => Command::Compose,
        KeyCode::Esc => Command::Escape,
        KeyCode::Char(ch) if ch.is_ascii_digit() => {
            Command::Digit(ch.to_digit(10).unwrap_or_default() as u8)
        }
        _ => Command::None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Focus, HELP_ENTRIES, command_for_key};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn browser_focus_never_enters_message_reader() {
        assert_eq!(Focus::Mailboxes.next(false), Focus::Messages);
        assert_eq!(Focus::Messages.next(false), Focus::Mailboxes);
        assert_eq!(Focus::Preview.next(false), Focus::Mailboxes);
        assert_eq!(Focus::Messages.next(true), Focus::Preview);
    }

    #[test]
    fn vim_navigation_is_first_class() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            Command::Down
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Command::Bottom
        );
    }

    #[test]
    fn uppercase_jk_always_target_message_list() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT)),
            Command::MessageDown
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT)),
            Command::MessageUp
        );
    }

    #[test]
    fn ctrl_jk_are_reserved_for_neoscroll() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Command::SmoothDown
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            Command::SmoothUp
        );
    }

    #[test]
    fn mail_actions_have_direct_keys() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            Command::Delete
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Command::Reply
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)),
            Command::Forward
        );
    }

    #[test]
    fn refresh_moved_off_reply_key() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Command::Refresh
        );
    }

    #[test]
    fn help_does_not_advertise_count_combinations_as_keybindings() {
        assert!(HELP_ENTRIES.iter().all(|entry| !entry.key.contains("20j")));
    }

    #[test]
    fn digits_feed_vim_count_prefix() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
            Command::Digit(2)
        );
    }
}
