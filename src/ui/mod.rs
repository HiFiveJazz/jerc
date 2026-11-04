mod render;

pub use render::draw;


/// Mailbox width in terminal cells. Keep at least 32 cells for the message
/// list, even on small terminals. Used by rendering and mouse hit testing.
pub(crate) fn mailbox_column_width(total: u16, percent: u16) -> u16 {
    if total <= 34 { return total.saturating_sub(1); }
    ((u32::from(total) * u32::from(percent.clamp(12, 50)) / 100) as u16)
        .max(12)
        .min(total.saturating_sub(32))
}

/// The attachment popup, left file list, right preview and action footer.
/// A shared layout prevents mouse coordinates from drifting out of sync.
pub(crate) fn attachment_regions(viewport: ratatui::layout::Rect) -> (
    ratatui::layout::Rect, ratatui::layout::Rect,
    ratatui::layout::Rect, ratatui::layout::Rect,
) {
    use ratatui::layout::Rect;
    let width = viewport.width.saturating_sub(2).min(116).max(1);
    let height = (u32::from(viewport.height) * 86 / 100) as u16;
    let height = height.max(5).min(viewport.height.max(1));
    let popup = Rect::new(viewport.x + viewport.width.saturating_sub(width)/2,
        viewport.y + viewport.height.saturating_sub(height)/2, width, height);
    let inner = Rect::new(popup.x.saturating_add(1), popup.y.saturating_add(1),
        popup.width.saturating_sub(2), popup.height.saturating_sub(2));
    let content_height = inner.height.saturating_sub(2);
    let left_width = ((u32::from(inner.width) * 42 / 100) as u16)
        .max(16).min(inner.width.saturating_sub(1));
    let list = Rect::new(inner.x, inner.y, left_width, content_height);
    let preview = Rect::new(inner.x.saturating_add(left_width), inner.y,
        inner.width.saturating_sub(left_width), content_height);
    let footer = Rect::new(inner.x, inner.y.saturating_add(content_height), inner.width, inner.height.saturating_sub(content_height));
    (popup, list, preview, footer)
}

/// The received-attachment Save dialog deliberately shares the Yazi-style
/// three-pane geometry and row renderer used when attaching local files.
/// Keep these coordinates in one place so mouse clicks agree with drawing.
pub(crate) fn attachment_destination_regions(viewport: ratatui::layout::Rect) -> (
    ratatui::layout::Rect, ratatui::layout::Rect,
    ratatui::layout::Rect, ratatui::layout::Rect,
    ratatui::layout::Rect,
) {
    use ratatui::layout::Rect;
    let width = viewport.width.saturating_sub(4).min(110).max(1);
    let height = (u32::from(viewport.height) * 82 / 100) as u16;
    let height = height.max(7).min(viewport.height.max(1));
    let popup = Rect::new(viewport.x + viewport.width.saturating_sub(width) / 2,
        viewport.y + viewport.height.saturating_sub(height) / 2, width, height);
    let inner = Rect::new(popup.x.saturating_add(1), popup.y.saturating_add(1),
        popup.width.saturating_sub(2), popup.height.saturating_sub(2));
    let top = inner.y.saturating_add(1);
    let content_height = inner.height.saturating_sub(3);
    let parent_width = (u32::from(inner.width) * 24 / 100) as u16;
    let parent_width = parent_width.max(1).min(inner.width);
    let files_width = (u32::from(inner.width) * 34 / 100) as u16;
    let files_width = files_width.max(1).min(inner.width.saturating_sub(parent_width));
    let parent = Rect::new(inner.x, top, parent_width, content_height);
    let files = Rect::new(parent.right(), top, files_width, content_height);
    let preview = Rect::new(files.right(), top, inner.right().saturating_sub(files.right()), content_height);
    let footer = Rect::new(inner.x, top.saturating_add(content_height), inner.width,
        inner.bottom().saturating_sub(top.saturating_add(content_height)));
    (popup, parent, files, preview, footer)
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    #[test]
    fn sidebar_is_narrower_by_default_and_bounded() {
        assert_eq!(mailbox_column_width(120, 22), 26);
        assert_eq!(mailbox_column_width(120, 1), 14);
        assert_eq!(mailbox_column_width(120, 90), 60);
        assert!(mailbox_column_width(40, 50) <= 8);
    }
    #[test]
    fn destination_panels_are_contiguous_and_inside_popup() {
        let (popup, parent, files, preview, footer) = attachment_destination_regions(
            ratatui::layout::Rect::new(0, 0, 140, 44));
        assert_eq!(parent.right(), files.x);
        assert_eq!(files.right(), preview.x);
        assert_eq!(parent.bottom(), footer.y);
        assert!(footer.bottom() <= popup.bottom());
    }

    #[test]
    fn attachment_panels_are_inside_popup() {
        let (_, list, preview, footer) = attachment_regions(ratatui::layout::Rect::new(0, 0, 120, 40));
        assert_eq!(list.x + list.width, preview.x);
        assert_eq!(preview.y + preview.height, footer.y);
        assert!(footer.height >= 1);
    }
}
