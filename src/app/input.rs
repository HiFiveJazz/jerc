use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Mailboxes,
    Messages,
    Preview,
}

impl Focus {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Mailboxes => Self::Messages,
            Self::Messages => Self::Preview,
            Self::Preview => Self::Mailboxes,
        }
    }

    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Mailboxes => Self::Preview,
            Self::Messages => Self::Mailboxes,
            Self::Preview => Self::Messages,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    Down,
    Up,
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
    None,
}

#[must_use]
pub fn command_for_key(key: KeyEvent) -> Command {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Command::Quit,
            KeyCode::Char('d') => Command::PageDown,
            KeyCode::Char('u') => Command::PageUp,
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
        _ => Command::None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, command_for_key};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
}
