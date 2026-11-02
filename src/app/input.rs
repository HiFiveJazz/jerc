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
pub enum Command {
    Quit,
    Down,
    Up,
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
    Refresh,
    NextAccount,
    PreviousAccount,
    ToggleThemePicker,
    ExportRaw,
    ToggleHelp,
    ToggleHtml,
    Compose,
    Reply,
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
            KeyCode::PageDown => Command::NextAccount,
            KeyCode::PageUp => Command::PreviousAccount,
            _ => Command::None,
        };
    }

    match key.code {
        KeyCode::Char('q') => Command::Quit,
        KeyCode::Char('j') | KeyCode::Down => Command::Down,
        KeyCode::Char('k') | KeyCode::Up => Command::Up,
        KeyCode::Char('g') | KeyCode::Home => Command::Top,
        KeyCode::Char('G') | KeyCode::End => Command::Bottom,
        KeyCode::Char(' ') | KeyCode::PageDown => Command::PageDown,
        KeyCode::PageUp => Command::PageUp,
        KeyCode::Tab => Command::FocusNext,
        KeyCode::BackTab => Command::FocusPrevious,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Command::Open,
        KeyCode::Char('u') => Command::ToggleSeen,
        KeyCode::Char('r') => Command::Refresh,
        KeyCode::Char(']') => Command::NextAccount,
        KeyCode::Char('[') => Command::PreviousAccount,
        KeyCode::Char('t') => Command::ToggleThemePicker,
        KeyCode::Char('e') => Command::ExportRaw,
        KeyCode::Char('?') => Command::ToggleHelp,
        KeyCode::Char('h') => Command::ToggleHtml,
        KeyCode::Char('c') => Command::Compose,
        KeyCode::Char('R') => Command::Reply,
        KeyCode::Esc => Command::Escape,
        KeyCode::Char(ch) if ch.is_ascii_digit() => {
            Command::Digit(ch.to_digit(10).unwrap_or_default() as u8)
        }
        _ => Command::None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Focus, command_for_key};
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
    fn theme_picker_and_raw_export_have_direct_keys() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            Command::ToggleThemePicker
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            Command::ExportRaw
        );
    }

    #[test]
    fn digits_feed_vim_count_prefix() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
            Command::Digit(2)
        );
    }
}
