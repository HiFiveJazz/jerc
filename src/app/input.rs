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

/// Searchable action palette, showing gg/G explicitly while omitting other Vim motions.
/// Categories and the leading shortcut within each category sort alphabetically.
pub const HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry {
        category: "Accounts",
        key: "Alt-1 … Alt-9",
        description: "switch directly to an email account",
    },
    HelpEntry {
        category: "Accounts",
        key: "Alt-a",
        description: "add account with setup wizard",
    },
    HelpEntry {
        category: "Accounts",
        key: "Alt-d",
        description: "remove account (confirm)",
    },
    HelpEntry {
        category: "Accounts",
        key: "Alt-n / Alt-p",
        description: "next / previous email account",
    },
    HelpEntry {
        category: "General",
        key: "!",
        description: "show account configuration warnings",
    },
    HelpEntry {
        category: "General",
        key: "?",
        description: "search command palette",
    },
    HelpEntry {
        category: "General",
        key: "Alt-c",
        description: "open the native contacts manager",
    },
    HelpEntry {
        category: "General",
        key: "B",
        description: "toggle new-mail terminal bell",
    },
    HelpEntry {
        category: "General",
        key: "Ctrl-r",
        description: "refresh selected mailbox",
    },
    HelpEntry {
        category: "General",
        key: "n",
        description: "toggle desktop notifications",
    },
    HelpEntry {
        category: "General",
        key: "q / Ctrl-c",
        description: "quit jerc",
    },
    HelpEntry {
        category: "General",
        key: "T",
        description: "open theme picker",
    },
    HelpEntry {
        category: "Mail actions",
        key: "a",
        description: "archive selected message",
    },
    HelpEntry {
        category: "Mail actions",
        key: "A",
        description: "reply to all recipients",
    },
    HelpEntry {
        category: "Mail actions",
        key: "c",
        description: "compose new email",
    },
    HelpEntry {
        category: "Mail actions",
        key: "d / D",
        description: "delete with / without prompt",
    },
    HelpEntry {
        category: "Mail actions",
        key: "f",
        description: "forward message",
    },
    HelpEntry {
        category: "Mail actions",
        key: "L",
        description: "reply to mailing list",
    },
    HelpEntry {
        category: "Mail actions",
        key: "r / R",
        description: "reply / reply with quotation",
    },
    HelpEntry {
        category: "Mail actions",
        key: "s",
        description: "mark as spam / restore from Junk",
    },
    HelpEntry {
        category: "Mail actions",
        key: "Space",
        description: "toggle mark; Visual marks range",
    },
    HelpEntry {
        category: "Mail actions",
        key: "u",
        description: "mark read or unread",
    },
    HelpEntry {
        category: "Mail actions",
        key: "U",
        description: "restore archived message to INBOX",
    },
    HelpEntry {
        category: "Mail actions",
        key: "v / V",
        description: "select an inclusive message range",
    },
    HelpEntry {
        category: "Message",
        key: "C / Shift-C",
        description: "toggle quote / Git diff colors",
    },
    HelpEntry {
        category: "Message",
        key: "Ctrl-a",
        description: "open received attachments",
    },
    HelpEntry {
        category: "Message",
        key: "e / E",
        description: "export original RFC 5322 message",
    },
    HelpEntry {
        category: "Message",
        key: "Esc",
        description: "close the reader / exit selection",
    },
    HelpEntry {
        category: "Message",
        key: "i",
        description: "toggle remote images",
    },
    HelpEntry {
        category: "Message",
        key: "m",
        description: "show full email headers",
    },
    HelpEntry {
        category: "Message",
        key: "o",
        description: "open link under cursor",
    },
    HelpEntry {
        category: "Message",
        key: "O",
        description: "open original HTML in browser",
    },
    HelpEntry {
        category: "Message",
        key: "t",
        description: "toggle HTML / plain-text reader",
    },
    HelpEntry {
        category: "Message",
        key: "y",
        description: "copy selected text",
    },
    HelpEntry {
        category: "Navigation",
        key: "/",
        description: "filter current mailbox",
    },
    HelpEntry {
        category: "Navigation",
        key: "Ctrl-j / Ctrl-k",
        description: "smooth scroll the message list",
    },
    HelpEntry {
        category: "Navigation",
        key: "Enter",
        description: "open selected email",
    },
    HelpEntry {
        category: "Navigation",
        key: "gg / G",
        description: "first / last message or line",
    },
    HelpEntry {
        category: "Navigation",
        key: "J / K",
        description: "next / previous mailbox",
    },
    HelpEntry {
        category: "View",
        key: "Alt-h / Alt-l",
        description: "resize mailbox sidebar",
    },
    HelpEntry {
        category: "View",
        key: "Alt-Shift-h / Alt-Shift-l",
        description: "resize message preview",
    },
    HelpEntry {
        category: "View",
        key: "b",
        description: "show / hide mailbox sidebar",
    },
    HelpEntry {
        category: "View",
        key: "p",
        description: "show / hide split preview",
    },
];

/// Contacts-specific shortcuts shown in the same searchable palette as mail.
/// Keep all discoverability here instead of showing permanent shortcut hints
/// across the contacts list.
pub const CONTACT_HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry {
        category: "Actions",
        key: "a",
        description: "add a contact",
    },
    HelpEntry {
        category: "Actions",
        key: "d",
        description: "delete selection (confirm)",
    },
    HelpEntry {
        category: "Actions",
        key: "e",
        description: "edit current contact",
    },
    HelpEntry {
        category: "Actions",
        key: "Enter",
        description: "edit or insert into compose",
    },
    HelpEntry {
        category: "Actions",
        key: "p",
        description: "toggle favorite for selection",
    },
    HelpEntry {
        category: "Navigation",
        key: "Ctrl-j / Ctrl-k",
        description: "smooth scroll 22 rows",
    },
    HelpEntry {
        category: "Navigation",
        key: "gg / G",
        description: "first / last contact",
    },
    HelpEntry {
        category: "Navigation",
        key: "H / M / L",
        description: "top / middle / bottom visible row",
    },
    HelpEntry {
        category: "Navigation",
        key: "j / k",
        description: "move down / up (counts allowed)",
    },
    HelpEntry {
        category: "Navigation",
        key: "PageUp / PageDown",
        description: "page through contacts",
    },
    HelpEntry {
        category: "Search",
        key: "/",
        description: "fuzzy search contacts",
    },
    HelpEntry {
        category: "Search",
        key: "Enter / Esc",
        description: "finish contact search",
    },
    HelpEntry {
        category: "Selection",
        key: "Space",
        description: "toggle mark; Visual marks range",
    },
    HelpEntry {
        category: "Selection",
        key: "v / V",
        description: "select visual contact rows",
    },
    HelpEntry {
        category: "Window",
        key: "?",
        description: "open searchable keybindings palette",
    },
    HelpEntry {
        category: "Window",
        key: "Esc / q",
        description: "clear selection / close contacts",
    },
];

/// Fuzzy ranking shared by the main and contacts keybinding palettes. Preserve
/// the original category/shortcut order when no query is typed.
#[must_use]
pub fn ranked_help_matches(entries: &[HelpEntry], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut ranked = entries
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| {
            let score = [entry.key, entry.description, entry.category]
                .iter()
                .filter_map(|field| help_field_score(field, &needle))
                .max()?;
            Some((i, score))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(i, _)| i).collect()
}

fn help_field_score(field: &str, needle: &str) -> Option<i32> {
    let lower = field.to_lowercase();
    if lower == needle {
        return Some(500);
    }
    if lower.starts_with(needle) {
        return Some(350);
    }
    if lower.contains(needle) {
        return Some(250);
    }
    let mut chars = lower.chars().enumerate();
    let mut previous = None;
    let mut score = 0;
    for wanted in needle.chars() {
        let (position, _) = chars.find(|(_, c)| *c == wanted)?;
        score += if previous.is_some_and(|prev| position == prev + 1) {
            15
        } else {
            5
        };
        previous = Some(position);
    }
    Some(score)
}

/// Shortcuts specific to the received-attachment viewer. Navigation uses
/// normal Vim motions; it does not need a permanent on-screen legend.
pub const ATTACHMENT_HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry {
        category: "Attachments",
        key: "?",
        description: "search attachment shortcuts",
    },
    HelpEntry {
        category: "Attachments",
        key: "o / Enter",
        description: "open selected attachment externally",
    },
    HelpEntry {
        category: "Attachments",
        key: "s",
        description: "save selected attachments",
    },
    HelpEntry {
        category: "Attachments",
        key: "Space",
        description: "mark or unmark an attachment",
    },
    HelpEntry {
        category: "Attachments",
        key: "a",
        description: "mark or unmark all attachments",
    },
    HelpEntry {
        category: "Attachments",
        key: "v / V",
        description: "select a visual range of attachments",
    },
    HelpEntry {
        category: "Attachments",
        key: "p",
        description: "refresh attachment preview",
    },
    HelpEntry {
        category: "Attachments",
        key: "Esc / q",
        description: "clear selection / close viewer",
    },
];

/// Searchable help for the attachment picker; intentionally not duplicated
/// in the global compose keymap or at the bottom of the picker.
pub const PICKER_HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry {
        category: "Browse",
        key: ".",
        description: "toggle hidden files",
    },
    HelpEntry {
        category: "Browse",
        key: "/",
        description: "search files and folders",
    },
    HelpEntry {
        category: "Browse",
        key: "Enter (search)",
        description: "accept a search match",
    },
    HelpEntry {
        category: "Browse",
        key: "~ / d",
        description: "home / Downloads directory",
    },
    HelpEntry {
        category: "General",
        key: "?",
        description: "search picker keybindings",
    },
    HelpEntry {
        category: "General",
        key: "Esc / q",
        description: "exit visual selection / close picker",
    },
    HelpEntry {
        category: "Select",
        key: "Enter",
        description: "attach selection / open a directory",
    },
    HelpEntry {
        category: "Select",
        key: "Space",
        description: "toggle attachment / select range",
    },
    HelpEntry {
        category: "Select",
        key: "v / V",
        description: "start or exit visual range selection",
    },
    HelpEntry {
        category: "Select",
        key: "x",
        description: "review selected attachments",
    },
];

/// Searchable help for choosing where to save received attachments.
/// Kept separate from the Compose file picker because the actions differ.
pub const DESTINATION_HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry {
        category: "Actions",
        key: "s",
        description: "save selected attachments here",
    },
    HelpEntry {
        category: "Actions",
        key: "n",
        description: "create a new folder",
    },
    HelpEntry {
        category: "Browse",
        key: "Enter / l",
        description: "open selected folder",
    },
    HelpEntry {
        category: "Browse",
        key: "h / Backspace",
        description: "go to parent folder",
    },
    HelpEntry {
        category: "Browse",
        key: "~",
        description: "go to home directory",
    },
    HelpEntry {
        category: "Browse",
        key: ".",
        description: "toggle hidden files",
    },
    HelpEntry {
        category: "Navigation",
        key: "j / k",
        description: "move down / up",
    },
    HelpEntry {
        category: "Navigation",
        key: "gg / G",
        description: "first / last folder",
    },
    HelpEntry {
        category: "Navigation",
        key: "Ctrl-j / Ctrl-k",
        description: "smooth scroll 22 rows",
    },
    HelpEntry {
        category: "Window",
        key: "?",
        description: "search destination keybindings",
    },
    HelpEntry {
        category: "Window",
        key: "Esc / q",
        description: "cancel save destination",
    },
];

/// Compose-specific, searchable modal shortcuts. Do not clutter the editor.
pub const COMPOSE_HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry {
        category: "Compose",
        key: "C / Shift-C",
        description: "toggle quote / diff colors",
    },
    HelpEntry {
        category: "Compose",
        key: "?",
        description: "search compose shortcuts",
    },
    HelpEntry {
        category: "Compose",
        key: "Alt-c",
        description: "open contacts for recipients",
    },
    HelpEntry {
        category: "Compose",
        key: "Ctrl-j / Ctrl-k",
        description: "next / previous contact suggestion",
    },
    HelpEntry {
        category: "Compose",
        key: "Ctrl-n / Ctrl-p",
        description: "alternate suggestion navigation",
    },
    HelpEntry {
        category: "Compose",
        key: "Tab / Enter (suggestion)",
        description: "accept suggested recipient",
    },
    HelpEntry {
        category: "Compose",
        key: "Ctrl-a",
        description: "open attachment file picker",
    },
    HelpEntry {
        category: "Compose",
        key: "Ctrl-d",
        description: "manage attachments",
    },
    HelpEntry {
        category: "Compose",
        key: "Ctrl-o",
        description: "edit advanced message headers",
    },
    HelpEntry {
        category: "Compose",
        key: "Ctrl-p",
        description: "save to Drafts folder",
    },
    HelpEntry {
        category: "Compose",
        key: "Enter (normal)",
        description: "preview email before sending",
    },
    HelpEntry {
        category: "Compose",
        key: "yy / Y / y$ (normal)",
        description: "yank line or to end of line",
    },
    HelpEntry {
        category: "Compose",
        key: "v / V then y (visual)",
        description: "yank selected text or lines",
    },
    HelpEntry {
        category: "Compose",
        key: "p / P (normal)",
        description: "paste after or before cursor / line",
    },
    HelpEntry {
        category: "Compose",
        key: "p (visual)",
        description: "replace selected text with register",
    },
    HelpEntry {
        category: "Compose",
        key: "Esc / q (normal)",
        description: "save or discard draft on exit",
    },
    HelpEntry {
        category: "Compose",
        key: "Tab / Shift-Tab",
        description: "next / previous compose field",
    },
    HelpEntry {
        category: "Send preview",
        key: "Enter / Esc",
        description: "send email / return to editor",
    },
    HelpEntry {
        category: "Send preview",
        key: "t / O",
        description: "HTML/plain or browser preview",
    },
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
    ToggleMark,
    ToggleSeen,
    Delete,
    DeleteImmediate,
    Archive,
    Unarchive,
    ToggleSpam,
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
    ToggleQuotedDiff,
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
    if key
        .modifiers
        .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
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
            KeyCode::Char('a') => Command::Attachments,
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
        KeyCode::Char(' ') => Command::ToggleMark,
        KeyCode::PageDown => Command::PageDown,
        KeyCode::PageUp => Command::PageUp,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Command::Open,
        KeyCode::Char('v' | 'V') => Command::ToggleVisual,
        KeyCode::Char('u') => Command::ToggleSeen,
        KeyCode::Char('d') => Command::Delete,
        KeyCode::Char('D') => Command::DeleteImmediate,
        KeyCode::Char('a') => Command::Archive,
        KeyCode::Char('U') => Command::Unarchive,
        KeyCode::Char('s') => Command::ToggleSpam,
        KeyCode::Char('/') => Command::Search,
        KeyCode::Char('b') => Command::ToggleSidebar,
        KeyCode::Char('p') => Command::TogglePreviewSplit,
        KeyCode::Char('r') => Command::Reply,
        KeyCode::Char('R') => Command::ReplyQuoted,
        KeyCode::Char('A') => Command::ReplyAll,
        KeyCode::Char('L') => Command::ReplyList,
        KeyCode::Char('f') => Command::Forward,
        KeyCode::Char('T') => Command::ToggleThemePicker,
        KeyCode::Char('e') | KeyCode::Char('E') => Command::ExportRaw,
        KeyCode::Char('?') => Command::ToggleHelp,
        KeyCode::Char('t') => Command::ToggleHtml,
        KeyCode::Char('C') => Command::ToggleQuotedDiff,
        KeyCode::Char('i') => Command::ToggleImages,
        KeyCode::Char('B') => Command::ToggleBell,
        KeyCode::Char('n') => Command::ToggleNotifications,
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
    use super::{Command, Focus, HELP_ENTRIES, command_for_key, ranked_help_matches};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn contact_keybindings_have_fuzzy_search_and_stable_default_order() {
        use super::{CONTACT_HELP_ENTRIES, ranked_help_matches};
        let all = ranked_help_matches(CONTACT_HELP_ENTRIES, "");
        assert_eq!(all, (0..CONTACT_HELP_ENTRIES.len()).collect::<Vec<_>>());
        let fuzzy = ranked_help_matches(CONTACT_HELP_ENTRIES, "fvrt");
        assert!(
            fuzzy
                .iter()
                .any(|&i| CONTACT_HELP_ENTRIES[i].description.contains("favorite"))
        );
        let key_match = ranked_help_matches(CONTACT_HELP_ENTRIES, "space");
        assert_eq!(CONTACT_HELP_ENTRIES[key_match[0]].key, "Space");
        assert!(ranked_help_matches(CONTACT_HELP_ENTRIES, "noMatchingShortcutXYZ").is_empty());
    }

    #[test]
    fn space_marks_mail_without_becoming_page_down() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            Command::ToggleMark
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            Command::PageDown
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT)),
            Command::ToggleVisual
        );
    }

    #[test]
    fn account_shortcuts_are_exposed() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT)),
            Command::NextAccount
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT)),
            Command::PreviousAccount
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT)),
            Command::SelectAccount(0)
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT)),
            Command::SelectAccount(2)
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::ALT)),
            Command::SelectAccount(8)
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
            Command::None
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)),
            Command::None
        );
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
    fn lowercase_n_toggles_desktop_notifications() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            Command::ToggleNotifications
        );
    }

    #[test]
    fn uppercase_n_does_not_toggle_notifications() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)),
            Command::None
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
            command_for_key(KeyEvent::new(
                KeyCode::Char('J'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Command::SmoothMailboxDown
        );
        assert_eq!(
            command_for_key(KeyEvent::new(
                KeyCode::Char('K'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Command::SmoothMailboxUp
        );
    }

    #[test]
    fn lowercase_p_toggles_unlabeled_split_preview() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            Command::TogglePreviewSplit
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT)),
            Command::None
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
    fn help_documents_the_real_smooth_message_scroll_keys() {
        let entry = HELP_ENTRIES
            .iter()
            .find(|entry| {
                entry.category == "Navigation"
                    && entry.description == "smooth scroll the message list"
            })
            .expect("help should document smooth message scrolling");
        assert_eq!(entry.key, "Ctrl-j / Ctrl-k");
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
    fn gg_and_g_are_searchable_in_main_help() {
        assert!(
            HELP_ENTRIES
                .iter()
                .any(|entry| entry.key == "gg / G" && entry.category == "Navigation")
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Command::Bottom
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)),
            Command::Top
        );
    }

    #[test]
    fn quoted_diff_mode_has_a_reader_key() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT)),
            Command::ToggleQuotedDiff
        );
    }

    #[test]
    fn quoted_diff_toggle_is_discoverable_in_searchable_help() {
        let entry = HELP_ENTRIES
            .iter()
            .find(|entry| entry.category == "Message" && entry.key == "C / Shift-C")
            .expect("reader quote/diff toggle should appear in ? help");
        assert!(entry.description.contains("diff"));
        let matches = ranked_help_matches(HELP_ENTRIES, "Shift-C");
        assert!(matches.iter().any(|&index| HELP_ENTRIES[index] == *entry));
        let matches = ranked_help_matches(HELP_ENTRIES, "quote");
        assert!(matches.iter().any(|&index| HELP_ENTRIES[index] == *entry));
    }

    #[test]
    fn reader_debug_export_and_html_toggle_have_non_motion_keys() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            Command::ExportRaw
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            Command::ToggleHtml
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            Command::ToggleImages
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT)),
            Command::None
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
        let keys = HELP_ENTRIES
            .iter()
            .map(|entry| {
                (
                    entry.category.to_ascii_lowercase(),
                    entry.key.to_ascii_lowercase(),
                )
            })
            .collect::<Vec<_>>();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        assert!(!HELP_ENTRIES.iter().any(|entry| entry.category == "Vim"));
        assert!(!HELP_ENTRIES.iter().any(|entry| entry.key.contains("vi")));
    }

    #[test]
    fn spam_attachment_and_theme_shortcuts_do_not_conflict() {
        let key = |ch, modifiers| command_for_key(KeyEvent::new(KeyCode::Char(ch), modifiers));
        assert_eq!(key('s', KeyModifiers::NONE), Command::ToggleSpam);
        assert_eq!(key('a', KeyModifiers::CONTROL), Command::Attachments);
        assert_eq!(key('z', KeyModifiers::NONE), Command::None);
        assert_eq!(key('t', KeyModifiers::NONE), Command::ToggleHtml);
        assert_eq!(key('T', KeyModifiers::SHIFT), Command::ToggleThemePicker);
    }

    #[test]
    fn digits_feed_vim_count_prefix() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
            Command::Digit(2)
        );
    }
}
