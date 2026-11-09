//! Shared, allocation-free cursor motions and identity-stable bulk selection.
//!
//! Compose and Contacts share pending Vim commands and bounded history while
//! keeping independent field validation and editing operations.

use std::collections::{HashSet, VecDeque};

/// The Vim key sequences shared by text editors. Editing actions remain with
/// the owning surface (single-line Contacts vs. multiline Compose).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VimPrefix {
    Goto,
    Delete,
    Yank,
    Inner,
    FindForward,
    FindBackward,
}

/// Normal/Visual-mode input prefix state. A single pending command is possible
/// at a time; storing it as one enum prevents impossible combinations (e.g.
/// pending Delete and Yank simultaneously). Counts are bounded at input time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct VimSequence {
    command: Option<VimPrefix>,
    count: Option<usize>,
}

impl VimSequence {
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn start(&mut self, command: VimPrefix) {
        self.command = Some(command);
    }

    pub(crate) fn take_command(&mut self) -> Option<VimPrefix> {
        self.command.take()
    }

    pub(crate) fn has_operator(&self) -> bool {
        matches!(self.command, Some(VimPrefix::Delete | VimPrefix::Yank))
    }

    /// `0` without an existing count remains a normal Vim motion.
    pub(crate) fn push_digit(&mut self, digit: char, max: usize) -> bool {
        let Some(value) = digit.to_digit(10) else { return false };
        if value == 0 && self.count.is_none() { return false; }
        self.count = Some(
            self.count.unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(value as usize)
                .min(max),
        );
        true
    }

    pub(crate) fn set_count(&mut self, count: usize) {
        self.count = Some(count);
    }

    pub(crate) fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }

    pub(crate) fn discard_count(&mut self) {
        self.count = None;
    }
}

/// Bounded, reusable undo/redo state shared by Compose and Contacts.
/// Snapshots belong to each editor: the history doesn't know about fields,
/// selections, or how text should be restored.
#[derive(Debug, Clone)]
pub(crate) struct VimHistory<T> {
    undo: VecDeque<T>,
    redo: VecDeque<T>,
    limit: usize,
}

impl<T> Default for VimHistory<T> {
    fn default() -> Self { Self::new(200) }
}

impl<T> VimHistory<T> {
    pub(crate) fn new(limit: usize) -> Self {
        Self { undo: VecDeque::new(), redo: VecDeque::new(), limit: limit.max(1) }
    }

    pub(crate) fn record(&mut self, snapshot: T) {
        if self.undo.len() == self.limit { self.undo.pop_front(); }
        self.undo.push_back(snapshot);
        self.redo.clear();
    }

    pub(crate) fn undo(&mut self, current: T) -> Option<T> {
        let previous = self.undo.pop_back()?;
        if self.redo.len() == self.limit { self.redo.pop_front(); }
        self.redo.push_back(current);
        Some(previous)
    }

    pub(crate) fn redo(&mut self, current: T) -> Option<T> {
        let next = self.redo.pop_back()?;
        if self.undo.len() == self.limit { self.undo.pop_front(); }
        self.undo.push_back(current);
        Some(next)
    }
}
use std::hash::Hash;

/// One unnamed Vim register is shared by Compose, Contacts, and the reader.
/// It is never serialized with drafts or synchronized to mail providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YankRegister {
    pub(crate) text: String,
    pub(crate) linewise: bool,
}

pub(crate) fn byte_offset(text: &str, cursor: usize) -> usize {
    text.char_indices().nth(cursor).map_or(text.len(), |(at, _)| at)
}

/// Return a character-indexed selection without splitting UTF-8 codepoints.
/// A missing or empty selection must not overwrite the yank register.
pub(crate) fn text_range(text: &str, start: usize, end: usize) -> Option<&str> {
    let len = text.chars().count();
    if start >= end || start >= len {
        return None;
    }
    Some(&text[byte_offset(text, start)..byte_offset(text, end.min(len))])
}

pub(crate) fn horizontal(cursor: usize, len: usize, delta: isize) -> usize {
    cursor.saturating_add_signed(delta).min(len)
}

/// Vim word motion without allocating a temporary Vec<char> per keystroke.
pub(crate) fn word_start(text: &str, cursor: usize, backward: bool, whole: bool) -> usize {
    let belongs = |ch: char| is_word_part(ch, whole);
    let len = text.chars().count();
    let mut at = cursor.min(len);
    if backward {
        let mut previous = text.chars().rev().skip(len - at).peekable();
        while previous.peek().is_some_and(|ch| !belongs(*ch)) {
            at -= 1;
            previous.next();
        }
        while previous.peek().is_some_and(|ch| belongs(*ch)) {
            at -= 1;
            previous.next();
        }
    } else {
        let mut upcoming = text.chars().skip(at).peekable();
        while upcoming.peek().is_some_and(|ch| belongs(*ch)) {
            at += 1;
            upcoming.next();
        }
        while upcoming.peek().is_some_and(|ch| !belongs(*ch)) {
            at += 1;
            upcoming.next();
        }
    }
    at
}

fn is_word_part(ch: char, whole: bool) -> bool {
    if whole { !ch.is_whitespace() } else { is_word_char(ch) }
}

/// Move to the final character of the next Vim word, without allocating a
/// character vector on each keystroke. This preserves the composer's existing
/// behavior when the cursor starts on punctuation or at the end of a field.
pub(crate) fn word_end(text: &str, cursor: usize, whole: bool) -> usize {
    let len = text.chars().count();
    if len == 0 { return 0; }
    let mut index = cursor.min(len - 1);
    let mut chars = text.chars().skip(index).peekable();
    if chars.peek().is_some_and(|ch| is_word_part(*ch, whole)) && index < len - 1 {
        chars.next();
        index += 1;
    }
    while index < len - 1 && chars.peek().is_some_and(|ch| !is_word_part(*ch, whole)) {
        chars.next();
        index += 1;
    }
    chars.next(); // the current character; peek now examines the next one
    while index < len - 1 && chars.peek().is_some_and(|ch| is_word_part(*ch, whole)) {
        chars.next();
        index += 1;
    }
    index
}

/// Word characters have the same meaning in the editor, reader motions,
/// and reader highlighting. Keep this predicate in one place.
pub(crate) fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn delimiter_pair(delimiter: char) -> Option<(char, char)> {
    match delimiter {
        '"' => Some(('"', '"')),
        '\'' => Some(('\'', '\'')),
        '`' => Some(('`', '`')),
        '(' | ')' => Some(('(', ')')),
        '[' | ']' => Some(('[', ']')),
        '{' | '}' => Some(('{', '}')),
        '<' | '>' => Some(('<', '>')),
        _ => None,
    }
}

pub(crate) fn inner_text_object_range(text: &str, column: usize, delimiter: char) -> Option<(usize, usize)> {
    let (open, close) = delimiter_pair(delimiter)?;
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return None;
    }
    let cursor = column.min(chars.len().saturating_sub(1));

    let (opening, closing) = if open == close {
        let positions = chars
            .iter()
            .enumerate()
            .filter_map(|(index, ch)| (*ch == open).then_some(index))
            .collect::<Vec<_>>();
        positions
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|pair| {
                let start = pair[0];
                let end = pair[1];
                (start <= cursor && cursor <= end).then_some((start, end))
            })
            .min_by_key(|(start, end)| end.saturating_sub(*start))?
    } else {
        let mut stack = Vec::new();
        let mut enclosing = Vec::new();
        for (index, ch) in chars.iter().copied().enumerate() {
            if ch == open {
                stack.push(index);
            } else if ch == close
                && let Some(start) = stack.pop()
                && start <= cursor
                && cursor <= index
            {
                enclosing.push((start, index));
            }
        }
        enclosing
            .into_iter()
            .min_by_key(|(start, end)| end.saturating_sub(*start))?
    };

    if closing <= opening.saturating_add(1) {
        return None;
    }
    Some((opening + 1, closing - 1))
}

/// An inclusive Vim visual selection, expressed as an exclusive Rust range.
pub(crate) fn visual_chars(anchor: usize, cursor: usize, len: usize) -> Option<(usize, usize)> {
    (len > 0).then(|| {
        let a = anchor.min(len - 1);
        let b = cursor.min(len - 1);
        (a.min(b), a.max(b) + 1)
    })
}

/// Visible range plus persistent marks; hidden marked identities survive filters.
/// Falling back to the current row happens only when neither selection is active.
pub(crate) fn selected<T: Eq + Hash + Clone>(
    visible: &[T],
    all: &[T],
    cursor: usize,
    anchor: Option<usize>,
    marked: &HashSet<T>,
) -> Vec<T> {
    let mut chosen = Vec::new();
    let mut seen = HashSet::new();
    if let Some(anchor) = anchor.filter(|_| !visible.is_empty()) {
        let end = visible.len() - 1;
        let from = anchor.min(end).min(cursor.min(end));
        let to = anchor.min(end).max(cursor.min(end));
        for item in &visible[from..=to] {
            if seen.insert(item.clone()) { chosen.push(item.clone()); }
        }
    }
    for item in all {
        if marked.contains(item) && seen.insert(item.clone()) {
            chosen.push(item.clone());
        }
    }
    if chosen.is_empty() && let Some(item) = visible.get(cursor) {
        chosen.push(item.clone());
    }
    chosen
}

/// One semantic toggle for visual ranges in mail, contacts, and attachments.
pub(crate) fn toggle_group<T: Eq + Hash + Clone>(marked: &mut HashSet<T>, items: &[T]) {
    if items.is_empty() { return; }
    let all_marked = items.iter().all(|item| marked.contains(item));
    for item in items {
        if all_marked { marked.remove(item); } else { marked.insert(item.clone()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_motions_and_visual_ranges_use_character_offsets() {
        assert_eq!(byte_offset("a😊é", 2), 5);
        assert_eq!(text_range("a😊é", 1, 3), Some("😊é"));
        assert_eq!(text_range("a😊é", 1, 99), Some("😊é"));
        assert_eq!(text_range("a😊é", 3, 4), None);
        assert_eq!(text_range("a😊é", 1, 1), None);
        assert_eq!(horizontal(0, 3, -1), 0);
        assert_eq!(horizontal(1, 3, 100), 3);
        assert_eq!(visual_chars(3, 1, 4), Some((1, 4)));
        assert_eq!(visual_chars(0, 0, 0), None);
        assert_eq!(word_start("first second", 0, false, true), 6);
        assert_eq!(word_start("first second", 8, true, true), 6);
        assert_eq!(word_end("first second", 0, false), 4);
        assert_eq!(word_end("first second", 4, false), 11); // advances to next word
        assert_eq!(word_end("first second", 5, false), 11);
        assert_eq!(word_end("éé 😊abc", 0, false), 1);
        assert_eq!(word_end("", 0, true), 0);
    }

    #[test]
    fn nested_text_objects_work_in_reader_and_composer() {
        assert_eq!(inner_text_object_range(r#"say "hello" now"#, 6, '"'), Some((5, 9)));
        assert_eq!(inner_text_object_range("call(foo[bar])", 9, ']'), Some((9, 11)));
        assert_eq!(inner_text_object_range("call(foo[bar])", 9, ')'), Some((5, 12)));
        assert_eq!(inner_text_object_range("{empty}", 0, '('), None);
        assert!(is_word_char('é'));
        assert!(!is_word_char('-'));
    }

    #[test]
    fn marked_and_visual_ranges_combine_without_duplicates_or_losing_hidden_marks() {
        let visible = [1, 3, 5];
        let all = [1, 2, 3, 4, 5];
        let marks = HashSet::from([2, 3]);
        assert_eq!(selected(&visible, &all, 2, Some(1), &marks), vec![3, 5, 2]);
        assert_eq!(selected(&visible, &all, 0, None, &HashSet::new()), vec![1]);
        assert_eq!(selected(&visible, &all, 9, None, &HashSet::new()), Vec::<i32>::new());
    }

    #[test]
    fn marking_a_whole_range_twice_toggles_it_as_a_group() {
        let mut marks = HashSet::from([2]);
        toggle_group(&mut marks, &[2, 3]);
        assert_eq!(marks, HashSet::from([2, 3]));
        toggle_group(&mut marks, &[2, 3]);
        assert!(marks.is_empty());
    }
}

#[cfg(test)]
mod editing_tests {
    use super::*;

    #[test]
    fn pending_sequences_do_not_overlap_and_zero_without_count_is_a_motion() {
        let mut sequence = VimSequence::default();
        assert!(!sequence.push_digit('0', 99));
        assert!(sequence.push_digit('2', 99));
        assert!(sequence.push_digit('0', 99));
        assert_eq!(sequence.take_count(), 20);
        assert!(sequence.push_digit('9', 99));
        assert!(sequence.push_digit('9', 99));
        assert_eq!(sequence.take_count(), 99);
        sequence.start(VimPrefix::Delete);
        assert!(sequence.has_operator());
        sequence.start(VimPrefix::Yank);
        assert_eq!(sequence.take_command(), Some(VimPrefix::Yank));
        assert_eq!(sequence.take_command(), None);
        assert!(!sequence.has_operator());
        sequence.start(VimPrefix::FindForward);
        sequence.clear();
        assert_eq!(sequence.take_command(), None);
        assert_eq!(sequence.take_count(), 1);
    }

    #[test]
    fn bounded_history_supports_undo_redo_and_new_edits_invalidate_redo() {
        let mut history = VimHistory::new(2);
        history.record("initial");
        history.record("second");
        history.record("third");
        assert_eq!(history.undo("current"), Some("third"));
        assert_eq!(history.undo("third"), Some("second"));
        assert_eq!(history.undo("second"), None); // oldest entry evicted
        assert_eq!(history.redo("second"), Some("third"));
        history.record("fourth");
        assert_eq!(history.redo("next"), None);
        assert_eq!(history.undo("next"), Some("fourth"));
    }
}
