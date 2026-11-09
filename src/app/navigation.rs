//! Shared bounded navigation and viewport scrolloff for list-based surfaces.
//! Pure helpers are used by the reader, mailbox list, contacts, and pickers.

pub(crate) fn bounded_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    current.saturating_add_signed(delta).min(len - 1)
}

/// Cycle through a list. Normalize the cursor before applying the delta so a
/// stale index after filtering cannot escape the range.
pub(crate) fn wrapped_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let current = current % len;
    let step = delta.unsigned_abs() % len;
    if delta < 0 {
        if step <= current { current - step } else { len - (step - current) }
    } else if current >= len - step {
        current - (len - step)
    } else {
        current + step
    }
}

/// Scroll a viewport just enough to keep its selection inside the requested
/// margin. Works in *display rows* for wrapped reader content.
pub(crate) fn reveal_row(
    current_offset: usize,
    index: usize,
    view_height: usize,
    max_scroll: usize,
    preferred_scrolloff: usize,
) -> usize {
    let height = view_height.max(1);
    let scrolloff = preferred_scrolloff.min(height.saturating_sub(1) / 2);
    let offset = current_offset.min(max_scroll);
    let upper_guard = offset.saturating_add(scrolloff);
    let lower_guard = offset.saturating_add(height.saturating_sub(scrolloff + 1));
    if index < upper_guard {
        index.saturating_sub(scrolloff).min(max_scroll)
    } else if index > lower_guard {
        index.saturating_add(scrolloff + 1).saturating_sub(height).min(max_scroll)
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_navigation_is_safe_for_empty_and_filtered_lists() {
        assert_eq!(bounded_index(0, 0, 1), 0);
        assert_eq!(bounded_index(0, 5, -1), 0);
        assert_eq!(bounded_index(3, 5, 10), 4);
        assert_eq!(wrapped_index(0, 3, -1), 2);
        assert_eq!(wrapped_index(2, 3, 1), 0);
        assert_eq!(wrapped_index(999, 3, 0), 0);
        assert_eq!(wrapped_index(0, 3, isize::MIN), 1);
    }

    #[test]
    fn row_reveal_preserves_scrolloff_on_short_and_wrapped_views() {
        assert_eq!(reveal_row(0, 5, 1, 20, 3), 5);
        assert_eq!(reveal_row(0, 19, 10, 10, 3), 10);
        assert_eq!(reveal_row(8, 4, 10, 20, 3), 1);
        assert_eq!(reveal_row(80, 99, 20, 80, 4), 80);
    }
}
