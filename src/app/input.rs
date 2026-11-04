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

/// Searchable action palette, intentionally excluding generic Vim motions.
/// Categories and the leading shortcut within each category sort alphabetically.
pub const HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry { category: "Accounts", key: "[ / ]", description: "previous / next account" },
    HelpEntry { category: "Accounts", key: "Alt-1 … Alt-9", description: "switch directly to an email account" },
    HelpEntry { category: "Accounts", key: "Alt-n / Alt-p", description: "next / previous email account" },
    HelpEntry { category: "Accounts", key: "Ctrl-a", description: "add an account with editable setup wizard" },
    HelpEntry { category: "General", key: "!", description: "show account configuration warnings" },
    HelpEntry { category: "General", key: "?", description: "search command palette" },
    HelpEntry { category: "General", key: "B", description: "toggle new-mail terminal bell" },
    HelpEntry { category: "General", key: "Ctrl-r", description: "refresh selected mailbox" },
    HelpEntry { category: "General", key: "N", description: "toggle desktop new-mail notifications (and sound)" },
    HelpEntry { category: "General", key: "q / Ctrl-c", description: "quit aerc-rs" },
    HelpEntry { category: "General", key: "t", description: "open theme picker" },
    HelpEntry { category: "Mail actions", key: "a", description: "archive selected message" },
    HelpEntry { category: "Mail actions", key: "A", description: "reply to all recipients" },
    HelpEntry { category: "Mail actions", key: "c", description: "compose new email" },
    HelpEntry { category: "Mail actions", key: "d / D", description: "delete (confirm) / delete immediately" },
    HelpEntry { category: "Mail actions", key: "f", description: "forward message" },
    HelpEntry { category: "Mail actions", key: "L", description: "reply to mailing list" },
    HelpEntry { category: "Mail actions", key: "r / R", description: "reply / reply with quotation" },
    HelpEntry { category: "Mail actions", key: "u", description: "mark read or unread" },
    HelpEntry { category: "Mail actions", key: "U", description: "restore archived message to INBOX" },
    HelpEntry { category: "Message", key: "e / E", description: "export original RFC 5322 message" },
    HelpEntry { category: "Message", key: "Esc", description: "close the reader / exit selection" },
    HelpEntry { category: "Message", key: "I", description: "toggle remote images for this session" },
    HelpEntry { category: "Message", key: "m", description: "show full recipients and Message-ID" },
    HelpEntry { category: "Message", key: "o", description: "open link under cursor" },
    HelpEntry { category: "Message", key: "O", description: "open original HTML in browser" },
    HelpEntry { category: "Message", key: "T", description: "toggle HTML / plain-text reader" },
    HelpEntry { category: "Message", key: "y", description: "copy selected text" },
    HelpEntry { category: "Message", key: "z", description: "open received attachments" },
    HelpEntry { category: "Navigation", key: "/", description: "filter current mailbox" },
    HelpEntry { category: "Navigation", key: "Ctrl-Shift-j / Ctrl-Shift-k", description: "smooth mailbox list scrolling" },
    HelpEntry { category: "Navigation", key: "Enter", description: "open selected email" },
    HelpEntry { category: "Navigation", key: "J / K", description: "next / previous mailbox" },
    HelpEntry { category: "View", key: "Alt-h / Alt-l", description: "resize mailbox sidebar" },
    HelpEntry { category: "View", key: "Alt-Shift-h / Alt-Shift-l", description: "resize message preview" },
    HelpEntry { category: "View", key: "b", description: "show / hide mailbox sidebar" },
    HelpEntry { category: "View", key: "p", description: "show / hide split preview" },
];

/// Searchable help for the attachment picker; intentionally not duplicated
/// in the global compose keymap or at the bottom of the picker.
pub const PICKER_HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry { category: "Browse", key: ".", description: "toggle hidden files" },
    HelpEntry { category: "Browse", key: "/", description: "find folders or files in this directory" },
    HelpEntry { category: "Browse", key: "Enter (search)", description: "accept a search match" },
    HelpEntry { category: "Browse", key: "~ / d", description: "home / Downloads directory" },
    HelpEntry { category: "General", key: "?", description: "search picker keybindings" },
    HelpEntry { category: "General", key: "Esc / q", description: "exit visual selection / close picker" },
    HelpEntry { category: "Select", key: "Enter", description: "attach selection / open a directory" },
    HelpEntry { category: "Select", key: "Space", description: "toggle file; in visual mode detach an all-attached range" },
    HelpEntry { category: "Select", key: "v / V", description: "start or exit visual range selection" },
    HelpEntry { category: "Select", key: "x", description: "detach highlighted file or visual selection" },
];

/// Compose-specific, searchable modal shortcuts. Do not clutter the editor.
pub const COMPOSE_HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry { category: "Compose", key: "?", description: "search compose shortcuts" },
    HelpEntry { category: "Compose", key: "Ctrl-a", description: "open attachment file picker" },
    HelpEntry { category: "Compose", key: "Ctrl-d", description: "manage attached files" },
    HelpEntry { category: "Compose", key: "Ctrl-o", description: "open advanced options for custom email headers" },
    HelpEntry { category: "Compose", key: "Ctrl-p", description: "save locally and in the account Drafts folder" },
    HelpEntry { category: "Compose", key: "Enter (normal)", description: "preview outgoing email before sending" },
    HelpEntry { category: "Compose", key: "Esc / q (normal)", description: "ask to save an unsent email in Drafts before closing" },
    HelpEntry { category: "Compose", key: "Tab / Shift-Tab", description: "next / previous recipient, subject or body field" },
    HelpEntry { category: "Send preview", key: "Enter / Esc", description: "send email / return to editor" },
    HelpEntry { category: "Send preview", key: "T / O", description: "toggle HTML/plain, open browser preview" },
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
    SelectAccount(usize),
    ToggleThemePicker,
    ExportRaw,
    ToggleHelp,
    ToggleHtml,
    ToggleImages,
    ToggleBell,
    ToggleNotifications,
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
    // Alt-based shortcuts are portable in most modern terminals and avoid
    // Ctrl-Tab ambiguity. Alt-1 through Alt-9 select a specific account.
    if key.modifiers.contains(KeyModifiers::ALT) {
        return match key.code {
            KeyCode::Char('n') => Command::NextAccount,
            KeyCode::Char('p') => Command::PreviousAccount,
            KeyCode::Char(ch @ '1'..='9') => Command::SelectAccount((ch as u8 - b'1') as usize),
            _ => Command::None,
        };
    }
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
        KeyCode::Char('p') => Command::TogglePreviewSplit,
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
        KeyCode::Char('N') => Command::ToggleNotifications,
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
    fn account_shortcuts_are_exposed() {
        assert_eq!(command_for_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT)), Command::NextAccount);
        assert_eq!(command_for_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT)), Command::PreviousAccount);
        assert_eq!(command_for_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT)), Command::SelectAccount(0));
        assert_eq!(command_for_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT)), Command::SelectAccount(2));
        assert_eq!(command_for_key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::ALT)), Command::SelectAccount(8));
        assert_eq!(command_for_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)), Command::NextAccount);
    }

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
    fn uppercase_n_toggles_desktop_notifications() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)),
            Command::ToggleNotifications
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
    fn lowercase_p_toggles_unlabeled_split_preview() {
        assert_eq!(command_for_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            Command::TogglePreviewSplit);
        assert_eq!(command_for_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT)),
            Command::None);
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
    fn palette_categories_and_first_shortcuts_are_sorted_and_omit_vim_motions() {
        let keys = HELP_ENTRIES.iter()
            .map(|entry| (entry.category.to_ascii_lowercase(), entry.key.to_ascii_lowercase()))
            .collect::<Vec<_>>();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        assert!(!HELP_ENTRIES.iter().any(|entry| entry.category == "Vim"));
        assert!(!HELP_ENTRIES.iter().any(|entry| entry.key.contains("vi")));
    }

    #[test]
    fn digits_feed_vim_count_prefix() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
            Command::Digit(2)
        );
    }
}
