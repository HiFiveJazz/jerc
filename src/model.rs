use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(pub String);

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageFlags(u16);

impl MessageFlags {
    pub const SEEN: u16 = 1 << 0;
    pub const ANSWERED: u16 = 1 << 1;
    pub const FLAGGED: u16 = 1 << 2;
    pub const DELETED: u16 = 1 << 3;
    pub const DRAFT: u16 = 1 << 4;
    pub const FORWARDED: u16 = 1 << 5;

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn contains(self, flag: u16) -> bool {
        self.0 & flag == flag
    }

    pub fn set(&mut self, flag: u16, enabled: bool) {
        if enabled {
            self.0 |= flag;
        } else {
            self.0 &= !flag;
        }
    }

    #[must_use]
    pub fn from_maildir_suffix(suffix: &str) -> Self {
        let mut flags = Self::empty();
        for ch in suffix.chars() {
            match ch {
                'S' => flags.set(Self::SEEN, true),
                'R' => flags.set(Self::ANSWERED, true),
                'F' => flags.set(Self::FLAGGED, true),
                'T' => flags.set(Self::DELETED, true),
                'D' => flags.set(Self::DRAFT, true),
                'P' => flags.set(Self::FORWARDED, true),
                _ => {}
            }
        }
        flags
    }

    #[must_use]
    pub fn to_maildir_suffix(self) -> String {
        let mut out = String::with_capacity(6);
        for (ch, flag) in [
            ('D', Self::DRAFT),
            ('F', Self::FLAGGED),
            ('P', Self::FORWARDED),
            ('R', Self::ANSWERED),
            ('S', Self::SEEN),
            ('T', Self::DELETED),
        ] {
            if self.contains(flag) {
                out.push(ch);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Archive,
    Junk,
    Other,
}

#[derive(Debug, Clone)]
pub struct Mailbox {
    pub name: String,
    pub role: MailboxRole,
    pub total: usize,
    pub unread: usize,
    /// IMAP mailbox counts are intentionally filled lazily so LIST can render
    /// immediately instead of waiting for one STATUS round trip per folder.
    pub counts_known: bool,
}

impl Mailbox {
    #[must_use]
    pub fn new(name: impl Into<String>, role: MailboxRole) -> Self {
        Self {
            name: name.into(),
            role,
            total: 0,
            unread: 0,
            counts_known: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MessageSummary {
    pub id: MessageId,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from_name: String,
    pub from_address: String,
    pub subject: String,
    pub timestamp: i64,
    pub date_label: String,
    pub size: u64,
    pub flags: MessageFlags,
}

impl MessageSummary {
    #[must_use]
    pub fn unread(&self) -> bool {
        !self.flags.contains(MessageFlags::SEEN)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlSpanStyle {
    pub foreground: Option<(u8, u8, u8)>,
    pub background: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub italic: bool,
    pub strikeout: bool,
    pub code: bool,
    pub link: Option<String>,
    pub image: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlSpan {
    pub text: String,
    pub style: HtmlSpanStyle,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlLine {
    pub spans: Vec<HtmlSpan>,
}

#[derive(Debug, Clone)]
pub struct MessageBody {
    pub id: MessageId,
    /// The MIME text/plain representation when one exists.
    pub plain_text: Option<String>,
    /// Plain-string projection of the rich HTML rendering. This is also used
    /// when quoting HTML-only mail in a reply.
    pub html_text: Option<String>,
    /// Rich terminal representation produced once on the worker thread. Table
    /// borders plus semantic links/emphasis survive into Ratatui, where they are
    /// coloured using the active terminal theme.
    pub html_lines: Option<Vec<HtmlLine>>,
    /// Human-readable renderer label shown in reader mode.
    pub html_renderer: Option<String>,
    pub attachment_count: usize,
}

#[derive(Debug, Clone)]
pub struct MessagePage {
    pub messages: Vec<MessageSummary>,
    /// Number of newer server-side entries skipped before this page.
    pub offset: usize,
    /// Offset to request for the next older page.
    pub next_offset: usize,
    pub has_more: bool,
}

impl MessageBody {
    #[must_use]
    pub fn display_text(&self, prefer_html: bool) -> &str {
        if prefer_html {
            if let Some(html) = self.html_text.as_deref().filter(|text| !text.trim().is_empty()) {
                return html;
            }
        }
        if let Some(plain) = self.plain_text.as_deref().filter(|text| !text.trim().is_empty()) {
            return plain;
        }
        if let Some(html) = self.html_text.as_deref().filter(|text| !text.trim().is_empty()) {
            return html;
        }
        "(message has no displayable text body)"
    }

    #[must_use]
    pub fn has_html_alternative(&self) -> bool {
        self.html_text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
    }

    #[must_use]
    pub fn html_links(&self) -> Vec<String> {
        let mut links = Vec::new();
        if let Some(lines) = &self.html_lines {
            for line in lines {
                for span in &line.spans {
                    if let Some(link) = &span.style.link
                        && !links.iter().any(|existing| existing == link)
                    {
                        links.push(link.clone());
                    }
                }
            }
        }
        links
    }

    #[must_use]
    pub fn plain_is_empty(&self) -> bool {
        self.plain_text
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub folders: bool,
    pub flags: bool,
    pub move_messages: bool,
    pub delete_messages: bool,
    pub server_search: bool,
    pub server_sort: bool,
    pub server_thread: bool,
}

impl Capabilities {
    pub const MAILDIR: Self = Self {
        folders: true,
        flags: true,
        move_messages: true,
        delete_messages: true,
        server_search: false,
        server_sort: false,
        server_thread: false,
    };

    pub const IMAP_BASIC: Self = Self {
        folders: true,
        flags: true,
        move_messages: true,
        delete_messages: true,
        server_search: true,
        server_sort: false,
        server_thread: false,
    };
}

#[cfg(test)]
mod tests {
    use super::{MessageBody, MessageFlags, MessageId};

    #[test]
    fn maildir_flags_round_trip() {
        let flags = MessageFlags::from_maildir_suffix("SRFP");
        assert!(flags.contains(MessageFlags::SEEN));
        assert!(flags.contains(MessageFlags::ANSWERED));
        assert!(flags.contains(MessageFlags::FLAGGED));
        assert!(flags.contains(MessageFlags::FORWARDED));
        assert_eq!(flags.to_maildir_suffix(), "FPRS");
    }

    #[test]
    fn message_ids_are_backend_opaque() {
        let a = MessageId("cur/123:2,S".into());
        let b = MessageId("cur/123:2,S".into());
        assert_eq!(a, b);
    }

    #[test]
    fn body_falls_back_to_html_when_plain_is_empty() {
        let body = MessageBody {
            id: MessageId("1".into()),
            plain_text: Some("   ".into()),
            html_text: Some("HTML version".into()),
            html_lines: None,
            html_renderer: Some("test".into()),
            attachment_count: 0,
        };
        assert_eq!(body.display_text(false), "HTML version");
        assert!(body.plain_is_empty());
    }
}
