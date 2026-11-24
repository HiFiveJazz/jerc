//! Session-only checkpoints for detecting Inbox membership additions.
use crate::model::MessageId;
use std::collections::HashSet;
use std::sync::Arc;

// Never retain an unbounded per-message history for local/POP maildrops.
pub(super) const MAX_TRACKED_IDS: usize = 4096;
pub(super) const MAX_ARRIVAL_RANGE: u32 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboxCheckpoint {
    Imap {
        validity: u32,
        next: u32,
    },
    Jmap {
        account: String,
        mailbox: String,
        state: String,
    },
    Ids(Arc<HashSet<String>>),
}

#[derive(Debug, Clone, Default)]
pub struct InboxCheck {
    pub checkpoint: Option<InboxCheckpoint>,
    pub count: usize,
    pub candidate: Option<MessageId>,
}

impl InboxCheck {
    pub(super) fn ids(
        previous: Option<&InboxCheckpoint>,
        entries: Vec<(String, MessageId)>,
    ) -> Self {
        if entries.len() > MAX_TRACKED_IDS {
            return Self::default();
        }
        let mut ids = HashSet::with_capacity(entries.len());
        let mut count = 0;
        let mut candidate = None;
        for (key, id) in entries {
            let is_new = matches!(previous, Some(InboxCheckpoint::Ids(old)) if !old.contains(&key));
            if !ids.insert(key) {
                continue;
            }
            if is_new {
                count += 1;
                candidate.get_or_insert(id);
            }
        }
        Self {
            checkpoint: Some(InboxCheckpoint::Ids(Arc::new(ids))),
            count,
            candidate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_detect_replacement_and_ignore_removals_and_renames() {
        let make = |entries: &[(&str, &str)]| {
            entries
                .iter()
                .map(|(key, id)| (key.to_string(), MessageId(id.to_string())))
                .collect()
        };
        let first = InboxCheck::ids(None, make(&[("a", "new/a"), ("b", "new/b")]));
        assert_eq!(first.count, 0);
        let next = InboxCheck::ids(
            first.checkpoint.as_ref(),
            make(&[("b", "cur/b:2,S"), ("c", "new/c")]),
        );
        assert_eq!(next.count, 1);
        assert_eq!(next.candidate, Some(MessageId("new/c".into())));
        assert_eq!(
            InboxCheck::ids(next.checkpoint.as_ref(), make(&[("b", "cur/b:2,S")])).count,
            0
        );
        assert!(
            InboxCheck::ids(
                None,
                (0..=MAX_TRACKED_IDS)
                    .map(|i| (i.to_string(), MessageId(i.to_string())))
                    .collect()
            )
            .checkpoint
            .is_none()
        );
    }
}
