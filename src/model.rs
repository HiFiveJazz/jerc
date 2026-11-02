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
}

impl Mailbox {
    #[must_use]
    pub fn new(name: impl Into<String>, role: MailboxRole) -> Self {
        Self {
            name: name.into(),
            role,
            total: 0,
            unread: 0,
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

#[derive(Debug, Clone)]
pub struct MessageBody {
    pub id: MessageId,
    pub text: String,
    pub attachment_count: usize,
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
}

#[cfg(test)]
mod tests {
    use super::{MessageFlags, MessageId};

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
}
