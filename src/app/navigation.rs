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

/// Locate a filtered item's position, independent of its original list index.
/// Returning None on an empty result avoids indexing stale selections.
pub(crate) fn filtered_position(matches: &[usize], selected: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.iter().position(|&index| index == selected).unwrap_or(0))
}

/// Move within a list of original indices, optionally wrapping at the ends.
pub(crate) fn filtered_move(
    matches: &[usize], selected: usize, delta: isize, wrap: bool,
) -> Option<(usize, usize)> {
    let position = filtered_position(matches, selected)?;
    let next = if wrap {
        wrapped_index(position, matches.len(), delta)
    } else {
        bounded_index(position, matches.len(), delta)
    };
    Some((matches[next], next))
}

/// Animate one row of an already queued smooth scroll. The caller controls
/// whether its preview updates at *each* row or only at the destination.
/// The target is cleared on arrival or when the list becomes empty.
pub(crate) fn smooth_step(
    selected: usize, target: &mut Option<usize>, len: usize,
) -> (usize, bool) {
    if len == 0 {
        *target = None;
        return (0, false);
    }
    let current = selected.min(len - 1);
    let Some(destination) = *target else {
        return (current, false);
    };
    let destination = destination.min(len - 1);
    if destination == current {
        *target = None;
        return (current, false);
    }
    let next = bounded_index(current, len, if destination > current { 1 } else { -1 });
    *target = (next != destination).then_some(destination);
    (next, true)
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
    fn filtered_navigation_handles_empty_results_and_stale_indices() {
        let indices = [2, 7, 10];
        assert_eq!(filtered_move(&[], 123, 1, false), None);
        assert_eq!(filtered_move(&indices, 7, 1, false), Some((10, 2)));
        assert_eq!(filtered_move(&indices, 10, 1, false), Some((10, 2)));
        assert_eq!(filtered_move(&indices, 10, 1, true), Some((2, 0)));
        assert_eq!(filtered_move(&indices, 999, -1, false), Some((2, 0)));
    }

    #[test]
    fn smooth_scroll_steps_to_destination_and_handles_list_shrinking() {
        let mut target = Some(3);
        assert_eq!(smooth_step(0, &mut target, 5), (1, true));
        assert_eq!(target, Some(3));
        assert_eq!(smooth_step(1, &mut target, 5), (2, true));
        assert_eq!(smooth_step(2, &mut target, 5), (3, true));
        assert_eq!(target, None);
        let mut target = Some(999);
        assert_eq!(smooth_step(3, &mut target, 4), (3, false));
        assert_eq!(target, None);
        assert_eq!(smooth_step(0, &mut target, 0), (0, false));
    }

    #[test]
    fn row_reveal_preserves_scrolloff_on_short_and_wrapped_views() {
        assert_eq!(reveal_row(0, 5, 1, 20, 3), 5);
        assert_eq!(reveal_row(0, 19, 10, 10, 3), 10);
        assert_eq!(reveal_row(8, 4, 10, 20, 3), 1);
        assert_eq!(reveal_row(80, 99, 20, 80, 4), 80);
    }
}
