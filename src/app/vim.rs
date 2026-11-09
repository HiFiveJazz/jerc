//! Shared, allocation-free cursor motions and identity-stable bulk selection.
//!
//! Compose and Contacts keep their own undo histories and field validation,
//! but agree on character positions, motions, and visual/marked action sets.

use std::collections::HashSet;
use std::hash::Hash;

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
    let belongs = |ch: char| {
        if whole { !ch.is_whitespace() } else { ch.is_alphanumeric() || ch == '_' }
    };
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
