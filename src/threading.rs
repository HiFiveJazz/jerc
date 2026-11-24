//! Mailbox-scoped, conservative header threading. Native provider IDs win;
//! missing ancestors are represented by their RFC 5322 IDs, not fake rows.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use crate::model::{MessageHeaders, MessageId, MessageSummary};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalThreadView {
    Collapsed,
    Conversation(MessageId),
}

/// A row in the mailbox projection, never a synthetic message target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadRow {
    Plain,
    Collapsed(usize),
    Expanded(usize),
    Child { last: bool },
}

fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let a = root(parent, a);
    let b = root(parent, b);
    if a != b {
        parent[a.max(b)] = a.min(b);
    }
}

fn header_id(id: &str) -> Option<&str> {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>');
    (!id.is_empty() && id.len() <= 998 && !id.chars().any(char::is_whitespace)).then_some(id)
}

/// Canonical message indices, one group per conversation. Native IDs are
/// scoped by the caller to one account/mailbox. A conflicting header chain
/// must never merge two distinct server conversations.
fn groups(messages: &[MessageSummary]) -> Vec<Vec<usize>> {
    let mut parents: Vec<_> = (0..messages.len()).collect();
    let mut tokens = HashMap::new();
    for (i, msg) in messages.iter().enumerate() {
        for id in msg
            .message_id
            .iter()
            .chain(msg.in_reply_to.iter())
            .chain(msg.references.iter().rev().take(128))
            .filter_map(|id| header_id(id))
        {
            if let Some(previous) = tokens.insert(id, i) {
                union(&mut parents, i, previous);
            }
        }
    }
    // A standard IMAP THREAD root UID is a snapshot hint, not a stable
    // provider identity. It can join header components, but cannot override
    // a conflicting JMAP or Gmail thread ID.
    let mut hints = HashMap::new();
    for (i, msg) in messages.iter().enumerate() {
        if let Some(id) = msg
            .thread_id
            .as_deref()
            .filter(|id| id.starts_with("imap-ref:"))
            && let Some(previous) = hints.insert(id, i)
        {
            union(&mut parents, i, previous);
        }
    }
    let mut authority = HashMap::<usize, Option<&str>>::new();
    for (i, msg) in messages.iter().enumerate() {
        if let Some(id) = msg
            .thread_id
            .as_deref()
            .filter(|id| !id.is_empty() && !id.starts_with("imap-ref:"))
        {
            let component = root(&mut parents, i);
            authority
                .entry(component)
                .and_modify(|old| {
                    if *old != Some(id) {
                        *old = None;
                    }
                })
                .or_insert(Some(id));
        }
    }
    // Separate namespaces prevent an RFC header ID from masquerading as a
    // server ID. Ambiguous unassigned messages retain a local group.
    let mut grouped = HashMap::<(Option<&str>, usize), Vec<usize>>::new();
    for (i, msg) in messages.iter().enumerate() {
        let component = root(&mut parents, i);
        let native = msg
            .thread_id
            .as_deref()
            .filter(|id| !id.is_empty() && !id.starts_with("imap-ref:"))
            .or_else(|| authority.get(&component).copied().flatten());
        let key = if native.is_some() {
            (native, 0)
        } else {
            (None, component)
        };
        grouped.entry(key).or_default().push(i);
    }
    let mut groups: Vec<_> = grouped.into_values().collect();
    for group in &mut groups {
        group.sort_unstable_by(|a, b| {
            messages[*b]
                .timestamp
                .cmp(&messages[*a].timestamp)
                .then_with(|| messages[*b].id.cmp(&messages[*a].id))
        });
    }
    groups.sort_unstable_by(|a, b| {
        messages[b[0]]
            .timestamp
            .cmp(&messages[a[0]].timestamp)
            .then_with(|| messages[b[0]].id.cmp(&messages[a[0]].id))
    });
    groups
}

/// Reuse the graph during rendering and navigation. Weak pointers keep the
/// allocation identities stable without retaining message headers or bodies.
#[derive(Default)]
pub struct LocalThreadIndex {
    headers: Vec<Weak<MessageHeaders>>,
    groups: Vec<Vec<usize>>,
    member_counts: Vec<usize>,
    group_indices: Vec<usize>,
    representatives: Vec<usize>,
    collapsed: Vec<usize>,
    inline: Option<InlineProjection>,
}

struct InlineProjection {
    expanded: HashSet<MessageId>,
    extra: HashSet<MessageId>,
    rows: Arc<Vec<(usize, ThreadRow)>>,
    indices: Arc<Vec<usize>>,
}

impl LocalThreadIndex {
    fn refresh(&mut self, messages: &[MessageSummary]) {
        if self.headers.len() != messages.len()
            || self
                .headers
                .iter()
                .zip(messages)
                .any(|(old, msg)| old.as_ptr() != Arc::as_ptr(&msg.headers))
        {
            self.inline = None;
            self.groups = groups(messages);
            self.headers = messages
                .iter()
                .map(|msg| Arc::downgrade(&msg.headers))
                .collect();
            self.member_counts = vec![1; messages.len()];
            self.representatives = vec![0; messages.len()];
            self.group_indices = vec![0; messages.len()];
            self.collapsed.clear();
            for (group_index, group) in self.groups.iter().enumerate() {
                // Preserve the mailbox's existing order when collapsing it.
                // The first row is its representative, even for UID ordering.
                let representative = *group.iter().min().expect("nonempty thread");
                self.collapsed.push(representative);
                for &index in group {
                    self.member_counts[index] = group.len();
                    self.group_indices[index] = group_index;
                    self.representatives[index] = representative;
                }
            }
            self.collapsed.sort_unstable();
        }
    }

    #[cfg(test)]
    pub fn member_counts(&mut self, messages: &[MessageSummary]) -> &[usize] {
        self.refresh(messages);
        &self.member_counts
    }

    pub fn representative(&mut self, messages: &[MessageSummary], index: usize) -> Option<usize> {
        self.refresh(messages);
        self.representatives.get(index).copied()
    }

    /// Expand by actual message IDs, not list positions or the current root.
    /// The same conversation stays expanded when a newer reply becomes its root.
    pub fn inline_rows(
        &mut self,
        messages: &[MessageSummary],
        expanded: &HashSet<MessageId>,
        extra: &HashSet<MessageId>,
    ) -> Arc<Vec<(usize, ThreadRow)>> {
        self.ensure_inline(messages, expanded, extra);
        Arc::clone(&self.inline.as_ref().expect("inline projection").rows)
    }

    pub fn inline_indices(
        &mut self,
        messages: &[MessageSummary],
        expanded: &HashSet<MessageId>,
        extra: &HashSet<MessageId>,
    ) -> Arc<Vec<usize>> {
        self.ensure_inline(messages, expanded, extra);
        Arc::clone(&self.inline.as_ref().expect("inline projection").indices)
    }

    fn ensure_inline(
        &mut self,
        messages: &[MessageSummary],
        expanded: &HashSet<MessageId>,
        extra: &HashSet<MessageId>,
    ) {
        self.refresh(messages);
        if self
            .inline
            .as_ref()
            .is_some_and(|cached| &cached.expanded == expanded && &cached.extra == extra)
        {
            return;
        }
        let mut rows = Vec::with_capacity(messages.len());
        for &representative in &self.collapsed {
            // Supplemental server results may be older than the normal window.
            // They must not create unrelated top-level mailbox rows.
            if extra.contains(&messages[representative].id) {
                continue;
            }
            let group = &self.groups[self.group_indices[representative]];
            let count = self.member_counts[representative];
            let open = group.iter().any(|&i| expanded.contains(&messages[i].id));
            let marker = if count <= 1 {
                ThreadRow::Plain
            } else if open {
                ThreadRow::Expanded(count)
            } else {
                ThreadRow::Collapsed(count)
            };
            rows.push((representative, marker));
            if open && count > 1 {
                let mut children = group
                    .iter()
                    .copied()
                    .filter(|&i| i != representative)
                    .peekable();
                while let Some(index) = children.next() {
                    rows.push((
                        index,
                        ThreadRow::Child {
                            last: children.peek().is_none(),
                        },
                    ));
                }
            }
        }
        let indices = Arc::new(rows.iter().map(|(index, _)| *index).collect());
        self.inline = Some(InlineProjection {
            expanded: expanded.clone(),
            extra: extra.clone(),
            rows: Arc::new(rows),
            indices,
        });
    }

    pub fn group_members(&mut self, messages: &[MessageSummary], index: usize) -> &[usize] {
        self.refresh(messages);
        self.group_indices
            .get(index)
            .map_or(&[], |&group| self.groups[group].as_slice())
    }

    pub fn visible_indices(
        &mut self,
        messages: &[MessageSummary],
        view: &LocalThreadView,
    ) -> Vec<usize> {
        self.refresh(messages);
        match view {
            LocalThreadView::Collapsed => self.collapsed.clone(),
            LocalThreadView::Conversation(id) => self
                .groups
                .iter()
                .find(|g| g.iter().any(|i| &messages[*i].id == id))
                .cloned()
                .unwrap_or_default(),
        }
    }
}

/// Only enrich identities of known canonical rows. Results never change
/// canonical ordering, flags, pagination or body data.
pub fn merge_native_ids(
    canonical: &mut [MessageSummary],
    ids: &HashMap<MessageId, String>,
) -> bool {
    let mut changed = false;
    for message in canonical {
        if let Some(id) = ids.get(&message.id)
            && message.thread_id.as_ref() != Some(id)
        {
            message.thread_id = Some(id.clone());
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
fn visible_indices(messages: &[MessageSummary], view: &LocalThreadView) -> Vec<usize> {
    LocalThreadIndex::default().visible_indices(messages, view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessageFlags;

    fn msg(id: &str, mid: &str, refs: &str, native: Option<&str>, time: i64) -> MessageSummary {
        let headers =
            format!("Subject: Same subject\r\nMessage-ID: <{mid}>\r\nReferences: {refs}\r\n\r\n");
        let mut m = crate::mail::parse_summary(
            MessageId(id.into()),
            headers.as_bytes(),
            0,
            MessageFlags::empty(),
        )
        .unwrap();
        m.thread_id = native.map(str::to_owned);
        m.timestamp = time;
        m
    }

    #[test]
    fn inline_projection_reuses_rows_and_invalidates_on_expansion_and_header_changes() {
        let mut messages = vec![msg("a", "a", "", None, 1), msg("b", "b", "<a>", None, 2)];
        let mut index = LocalThreadIndex::default();
        let mut expanded = HashSet::new();
        let extra = HashSet::new();
        let first = index.inline_rows(&messages, &expanded, &extra);
        let indices = index.inline_indices(&messages, &expanded, &extra);
        assert!(Arc::ptr_eq(
            &first,
            &index.inline_rows(&messages, &expanded, &extra)
        ));
        assert!(Arc::ptr_eq(
            &indices,
            &index.inline_indices(&messages, &expanded, &extra)
        ));
        assert_eq!(first.len(), 1);
        expanded.insert(messages[0].id.clone());
        let opened = index.inline_rows(&messages, &expanded, &extra);
        assert_eq!(opened.len(), 2);
        assert!(!Arc::ptr_eq(&first, &opened));
        messages.push(msg("c", "c", "", None, 3));
        let changed = index.inline_rows(&messages, &expanded, &extra);
        assert_eq!(changed.len(), 3);
        assert!(!Arc::ptr_eq(&opened, &changed));
    }

    #[test]
    fn missing_ancestor_and_cycles_are_safe_and_subject_is_not_identity() {
        let messages = vec![
            msg("a", "a", "<missing> <b>", None, 1),
            msg("b", "b", "<missing> <a>", None, 2),
            msg("c", "c", "", None, 3),
        ];
        assert_eq!(
            visible_indices(&messages, &LocalThreadView::Collapsed),
            vec![0, 2]
        );
        assert_eq!(
            visible_indices(
                &messages,
                &LocalThreadView::Conversation(MessageId("a".into()))
            ),
            vec![1, 0]
        );
    }

    #[test]
    fn native_id_wins_and_conflicts_do_not_merge() {
        let messages = vec![
            msg("a", "a", "", Some("one"), 1),
            msg("b", "b", "<a>", Some("two"), 2),
            msg("c", "c", "<a>", None, 3),
            msg("d", "d", "", Some("one"), 4),
        ];
        assert_eq!(
            visible_indices(&messages, &LocalThreadView::Collapsed),
            vec![0, 1, 2]
        );
        assert_eq!(
            visible_indices(
                &messages,
                &LocalThreadView::Conversation(MessageId("a".into()))
            ),
            vec![3, 0]
        );
    }

    #[test]
    fn cached_counts_and_representatives_follow_native_membership_changes() {
        let mut messages = vec![msg("a", "a", "", None, 2), msg("b", "b", "", None, 1)];
        let mut index = LocalThreadIndex::default();
        assert_eq!(index.member_counts(&messages), &[1, 1]);
        messages[0].thread_id = Some("one".into());
        messages[1].thread_id = Some("one".into());
        assert_eq!(index.member_counts(&messages), &[2, 2]);
        assert_eq!(index.representative(&messages, 1), Some(0));
        assert_eq!(
            index.visible_indices(&messages, &LocalThreadView::Collapsed),
            vec![0]
        );
        messages.remove(0);
        assert_eq!(index.member_counts(&messages), &[1]);
    }

    #[test]
    fn local_reply_joins_one_known_native_conversation() {
        let messages = vec![
            msg("a", "a", "", Some("one"), 1),
            msg("b", "b", "<a>", None, 2),
            msg("c", "c", "", Some("one"), 3),
        ];
        assert_eq!(
            visible_indices(&messages, &LocalThreadView::Collapsed),
            vec![0]
        );
        assert_eq!(
            visible_indices(
                &messages,
                &LocalThreadView::Conversation(MessageId("b".into()))
            ),
            vec![2, 1, 0]
        );
    }
}
