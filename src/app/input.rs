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
        Self::Messages
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
    HelpEntry { category: "Browse", key: "j / k, ↑ / ↓", description: "move through visible messages" },
    HelpEntry { category: "Browse", key: "J / K", description: "next / previous mailbox" },
    HelpEntry { category: "Browse", key: "Ctrl-j / Ctrl-k", description: "smooth message scrolling" },
    HelpEntry { category: "Browse", key: "Ctrl-Shift-j / Ctrl-Shift-k", description: "smooth mailbox scrolling" },
    HelpEntry { category: "Browse", key: "g / G", description: "top / bottom" },
    HelpEntry { category: "Browse", key: "Ctrl-d / Ctrl-u", description: "page down / up" },
    HelpEntry { category: "Browse", key: "/", description: "filter/search current mailbox" },
    HelpEntry { category: "Browse", key: "b", description: "toggle mailbox sidebar" },
    HelpEntry { category: "Browse", key: "P", description: "toggle vertical message preview" },
    HelpEntry { category: "Browse", key: "Enter / l", description: "open selected message" },
    HelpEntry { category: "Browse", key: "v", description: "visual range selection" },
    HelpEntry { category: "Mail", key: "r / R", description: "reply without / with quoted text" },
    HelpEntry { category: "Mail", key: "f", description: "forward" },
    HelpEntry { category: "Mail", key: "c", description: "compose new message" },
    HelpEntry { category: "Mail", key: "d / D", description: "delete with confirmation / immediately" },
    HelpEntry { category: "Mail", key: "a / U", description: "archive / restore archived message to INBOX" },
    HelpEntry { category: "Mail", key: "u", description: "toggle read / unread" },
    HelpEntry { category: "Message", key: "Esc", description: "leave visual mode / close reader" },
    HelpEntry { category: "Message", key: "h/j/k/l", description: "move reader cursor" },
    HelpEntry { category: "Message", key: "w/b, 0/^/$, H/L", description: "word / line motions" },
    HelpEntry { category: "Message", key: "gg / G, { / }", description: "document / paragraph motions" },
    HelpEntry { category: "Message", key: "v / V, vi\" / vi' / vi() / vi[] / vi{}", description: "character / line visual selection and inner text objects" },
    HelpEntry { category: "Message", key: "T", description: "toggle rich HTML / plain fallback" },
    HelpEntry { category: "Message", key: "I", description: "toggle remote images for this session" },
    HelpEntry { category: "Message", key: "m", description: "show / hide Message-ID in reader" },
    HelpEntry { category: "Message", key: "z", description: "attachments picker: Enter open, p image preview" },
    HelpEntry { category: "Message", key: "o", description: "open link under reader cursor" },
    HelpEntry { category: "Message", key: "O", description: "open original HTML email in the browser" },
    HelpEntry { category: "Message", key: "y", description: "copy visual selection to clipboard" },
    HelpEntry { category: "Message", key: "e / E", description: "export raw RFC 5322 debug message" },
    HelpEntry { category: "General", key: "t", description: "open searchable theme picker" },
    HelpEntry { category: "General", key: "Ctrl-r", description: "refresh current mailbox" },
    HelpEntry { category: "General", key: "B", description: "toggle new mail bell (Inbox checked periodically)" },
    HelpEntry { category: "General", key: "[ / ]", description: "previous / next account" },
    HelpEntry { category: "Message", key: "A / L", description: "reply all / reply to mailing list (List-Post)" },
    HelpEntry { category: "Compose", key: "Ctrl-p / Ctrl-n", description: "postpone / resume local draft for current account" },
    HelpEntry { category: "Compose", key: "Tab / Shift-Tab", description: "To, Cc, Bcc, Subject, Attachments, Headers, Body" },
    HelpEntry { category: "General", key: "?", description: "search keybindings" },
    HelpEntry { category: "General", key: "q / Ctrl-c", description: "quit" },
];

/// Compose-specific, searchable modal shortcuts. Do not clutter the editor.
pub const COMPOSE_HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry { category: "Compose", key: "Tab / Shift-Tab", description: "next / previous header or body field" },
    HelpEntry { category: "Compose", key: "Enter (normal)", description: "review and confirm outgoing message" },
    HelpEntry { category: "Compose", key: "?", description: "search compose shortcuts" },
    HelpEntry { category: "Compose", key: "Ctrl-p", description: "postpone draft locally" },
    HelpEntry { category: "Compose", key: "Esc", description: "leave insert/visual mode or close composer" },
    HelpEntry { category: "Vim", key: "i a I A", description: "insert before/after cursor, line start/end" },
    HelpEntry { category: "Vim", key: "h j k l", description: "left, down, up, right" },
    HelpEntry { category: "Vim", key: "w W b B e E", description: "word and WORD navigation" },
    HelpEntry { category: "Vim", key: "0 ^ $ H L", description: "beginning / first nonblank / end of line" },
    HelpEntry { category: "Vim", key: "gg G", description: "first / last line in current field" },
    HelpEntry { category: "Vim", key: "{ }", description: "previous / next paragraph" },
    HelpEntry { category: "Vim", key: "v V", description: "character / whole-line visual selection" },
    HelpEntry { category: "Vim", key: "vi\" vi' vi( vi[ vi{", description: "select inside quoted text / paired brackets" },
    HelpEntry { category: "Vim", key: "f F", description: "find character forward / backward" },
    HelpEntry { category: "Vim", key: "o O", description: "open new body line below / above" },
    HelpEntry { category: "Vim", key: "x dd dw D", description: "delete character / line / word / to line end" },
    HelpEntry { category: "Vim", key: "u / Ctrl-r", description: "undo / redo editing" },
    HelpEntry { category: "Vim", key: "2j / 3w / …", description: "repeat a motion with a count" },
    HelpEntry { category: "Vim", key: "Ctrl-j / Ctrl-k", description: "move down/up five body lines" },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    Down,
    Up,
    MailboxDown,
    MailboxUp,
    SmoothMailboxDown,
    SmoothMailboxUp,
    SmoothDown,
    SmoothUp,
    PageDown,
    PageUp,
    Top,
    Bottom,
    Open,
    ToggleVisual,
    ToggleSeen,
    Delete,
    DeleteImmediate,
    Archive,
    Unarchive,
    Attachments,
    Search,
    ToggleSidebar,
    TogglePreviewSplit,
    Refresh,
    NextAccount,
    PreviousAccount,
    ToggleThemePicker,
    ExportRaw,
    ToggleHelp,
    ToggleHtml,
    ToggleImages,
    ToggleBell,
    OpenOriginal,
    Yank,
    Compose,
    Reply,
    ReplyQuoted,
    ReplyAll,
    ReplyList,
    Forward,
    Escape,
    Digit(u8),
    None,
}

#[must_use]
pub fn command_for_key(key: KeyEvent) -> Command {
    if key.modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT) {
        return match key.code {
            KeyCode::Char('j') | KeyCode::Char('J') => Command::SmoothMailboxDown,
            KeyCode::Char('k') | KeyCode::Char('K') => Command::SmoothMailboxUp,
            _ => Command::None,
        };
    }

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
        KeyCode::Char('J') => Command::MailboxDown,
        KeyCode::Char('K') => Command::MailboxUp,
        KeyCode::Char('g') | KeyCode::Home => Command::Top,
        KeyCode::Char('G') | KeyCode::End => Command::Bottom,
        KeyCode::Char(' ') | KeyCode::PageDown => Command::PageDown,
        KeyCode::PageUp => Command::PageUp,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Command::Open,
        KeyCode::Char('v') => Command::ToggleVisual,
        KeyCode::Char('u') => Command::ToggleSeen,
        KeyCode::Char('d') => Command::Delete,
        KeyCode::Char('D') => Command::DeleteImmediate,
        KeyCode::Char('a') => Command::Archive,
        KeyCode::Char('U') => Command::Unarchive,
        KeyCode::Char('z') => Command::Attachments,
        KeyCode::Char('/') => Command::Search,
        KeyCode::Char('b') => Command::ToggleSidebar,
        KeyCode::Char('P') => Command::TogglePreviewSplit,
        KeyCode::Char('r') => Command::Reply,
        KeyCode::Char('R') => Command::ReplyQuoted,
        KeyCode::Char('A') => Command::ReplyAll,
        KeyCode::Char('L') => Command::ReplyList,
        KeyCode::Char('f') => Command::Forward,
        KeyCode::Char(']') => Command::NextAccount,
        KeyCode::Char('[') => Command::PreviousAccount,
        KeyCode::Char('t') => Command::ToggleThemePicker,
        KeyCode::Char('e') | KeyCode::Char('E') => Command::ExportRaw,
        KeyCode::Char('?') => Command::ToggleHelp,
        KeyCode::Char('T') => Command::ToggleHtml,
        KeyCode::Char('I') => Command::ToggleImages,
        KeyCode::Char('B') => Command::ToggleBell,
        KeyCode::Char('O') => Command::OpenOriginal,
        KeyCode::Char('y') => Command::Yank,
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
    fn browser_focus_stays_on_messages() {
        assert_eq!(Focus::Mailboxes.next(false), Focus::Messages);
        assert_eq!(Focus::Messages.next(false), Focus::Messages);
        assert_eq!(Focus::Preview.next(false), Focus::Messages);
        assert_eq!(Focus::Messages.next(true), Focus::Preview);
    }

    #[test]
    fn bell_toggles_with_uppercase_b() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT)),
            Command::ToggleBell
        );
    }

    #[test]
    fn lowercase_jk_move_messages() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            Command::Down
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
            Command::Up
        );
    }

    #[test]
    fn uppercase_jk_move_mailboxes() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT)),
            Command::MailboxDown
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT)),
            Command::MailboxUp
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
    fn ctrl_shift_jk_scroll_mailboxes() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::CONTROL | KeyModifiers::SHIFT)),
            Command::SmoothMailboxDown
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::CONTROL | KeyModifiers::SHIFT)),
            Command::SmoothMailboxUp
        );
    }

    #[test]
    fn visual_mode_has_a_direct_key() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
            Command::ToggleVisual
        );
    }

    #[test]
    fn reader_yank_has_a_direct_key() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Command::Yank
        );
    }

    #[test]
    fn reader_debug_export_and_html_toggle_have_non_motion_keys() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            Command::ExportRaw
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT)),
            Command::ToggleHtml
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT)),
            Command::ToggleImages
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT)),
            Command::OpenOriginal
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
            command_for_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT)),
            Command::ReplyQuoted
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT)),
            Command::DeleteImmediate
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
