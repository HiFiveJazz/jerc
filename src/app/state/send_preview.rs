//! Transient outgoing review document, navigation and layout state.

use super::*;

/// Display-only representation of an outgoing message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SendPreviewMode {
    #[default]
    Plain,
    Html,
}

impl SendPreviewMode {
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::Plain => Self::Html,
            Self::Html => Self::Plain,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SendPreviewDocument {
    pub(crate) html_source: Option<String>,
    pub(crate) forwarded_attachments: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SendPreviewLayout {
    pub(crate) text: String,
    pub(crate) headers: Vec<ratatui::text::Line<'static>>,
    pub(crate) rows: Vec<ratatui::text::Line<'static>>,
    pub(crate) width: u16,
    pub(crate) gutter: usize,
    pub(crate) height: usize,
    pub(crate) top: usize,
}

impl Default for SendPreviewLayout {
    fn default() -> Self {
        Self {
            text: String::new(),
            headers: Vec::new(),
            rows: Vec::new(),
            width: 0,
            gutter: 4,
            height: 0,
            top: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SendPreview {
    pub(crate) mode: SendPreviewMode,
    pub(crate) document: SendPreviewDocument,
    pub(crate) layout: SendPreviewLayout,
    pub(crate) navigation: ReaderNavigation,
    pub(crate) count: Option<usize>,
}

impl SendPreview {
    fn reset_navigation(&mut self) {
        self.navigation = ReaderNavigation::default();
        self.count = None;
        self.layout = SendPreviewLayout::default();
    }

    pub(super) fn clear_pending_motion(&mut self) {
        self.count = None;
        self.navigation.pending_g = false;
    }

    pub(super) fn invalidate_layout(&mut self) {
        self.navigation.clear_visual();
        self.layout.width = 0;
    }

    pub(super) fn toggle_mode(&mut self) {
        self.mode = self.mode.next();
        self.invalidate_layout();
        self.navigation
            .jump(self.navigation.row, self.layout.rows.len());
        self.clear_pending_motion();
    }

    pub(crate) fn advance(&mut self) {
        self.navigation
            .advance(&self.layout.text, self.layout.rows.len());
    }

    pub(crate) fn update_viewport(&mut self, height: usize) {
        self.layout.height = height;
        self.navigation
            .clamp_document(&self.layout.text, self.layout.rows.len());
        self.layout.top = reader_scroll_for_visual_cursor(
            self.layout.top,
            self.navigation.row,
            height,
            self.layout.rows.len().saturating_sub(height),
        );
    }

    fn handle_motion(&mut self, key: KeyEvent) {
        use crate::app::vim::read_motion;
        let rows = self.layout.rows.len();
        if let Some(motion) = read_motion(
            key,
            &mut self.count,
            &mut self.navigation.pending_g,
            rows,
            self.layout.height,
            SMOOTH_SCROLL_ROWS,
            MAX_COUNT_PREFIX,
        ) {
            self.navigation.execute(motion, &self.layout.text, rows);
        }
    }
}

impl Composer {
    pub(super) fn prepare_send_preview(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.original_loading,
            "original message is still loading; cannot preview an incomplete reply/forward"
        );
        crate::outgoing::validate_recipients(&self.to, &self.cc, &self.bcc)?;
        // Only parse/reflow rich mail and attachment metadata upon opening the
        // preview, never while the terminal draws successive frames.
        self.send_preview.document.forwarded_attachments = self.draft_attachment_names.clone();
        self.send_preview.document.forwarded_attachments.extend(
            self.forwarded_raw
                .as_deref()
                .and_then(|raw| crate::mail::attachment_names(raw).ok())
                .unwrap_or_default(),
        );
        let original_html = self.outgoing_html_preview()?;
        // Plain uses the semantic text reader. HTML is a separate view of
        // the actual HTML alternative (or a safe display-only projection).
        self.send_preview.mode = if original_html.is_some() {
            SendPreviewMode::Html
        } else {
            SendPreviewMode::Plain
        };
        self.send_preview.document.html_source = original_html;
        self.reset_preview_navigation();
        self.help_visible = false;
        self.confirm_send = true;
        Ok(())
    }

    /// Project a plain draft into the rich reader's native semantic spans.
    /// Unlike an HTML round trip this preserves blank lines, quote prefixes,
    /// tabs (as display cells), patch context and arbitrarily long lines.
    pub(crate) fn plain_review_lines(&self) -> Vec<HtmlLine> {
        use crate::model::{HtmlSpan, HtmlSpanStyle};

        self.outgoing_plain_body()
            .split('\n')
            .map(|line| {
                let line = line
                    .trim_end_matches('\r')
                    .replace('\t', COMPOSE_TAB_SPACES);
                let mut spans: Vec<HtmlSpan> = Vec::new();
                let mut plain = String::new();
                for token in line.split_inclusive(char::is_whitespace) {
                    let word = token.trim_end_matches(char::is_whitespace);
                    if word.starts_with("https://") || word.starts_with("http://") {
                        if !plain.is_empty() {
                            spans.push(HtmlSpan {
                                text: std::mem::take(&mut plain),
                                style: HtmlSpanStyle::default(),
                            });
                        }
                        spans.push(HtmlSpan {
                            text: word.to_owned(),
                            style: HtmlSpanStyle {
                                link: Some(word.to_owned()),
                                ..HtmlSpanStyle::default()
                            },
                        });
                        plain.push_str(&token[word.len()..]);
                    } else {
                        plain.push_str(token);
                    }
                }
                if !plain.is_empty() {
                    spans.push(HtmlSpan {
                        text: plain,
                        style: HtmlSpanStyle::default(),
                    });
                }
                HtmlLine { spans }
            })
            .collect()
    }

    /// A safe, browser-only HTML projection of the exact outgoing plain body.
    /// Never use this as a MIME alternative when sending mail.
    pub(crate) fn plain_review_html(&self) -> String {
        fn escape(text: &str) -> String {
            text.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        }
        let body = self.outgoing_plain_body().replace('\t', COMPOSE_TAB_SPACES);
        let mut html = String::from(
            "<html><meta charset=\"utf-8\"><body><pre style=\"white-space:pre-wrap\">",
        );
        // Keep whitespace and literal patch markup. Give explicit web URLs
        // the same semantic link treatment as actual HTML mail.
        for token in body.split_inclusive(char::is_whitespace) {
            let word = token.trim_end_matches(char::is_whitespace);
            let escaped = escape(word);
            if word.starts_with("https://") || word.starts_with("http://") {
                html.push_str("<a href=\"");
                html.push_str(&escaped);
                html.push_str("\">");
                html.push_str(&escaped);
                html.push_str("</a>");
            } else {
                html.push_str(&escaped);
            }
            html.push_str(&token[word.len()..]);
        }
        html.push_str("</pre></body></html>");
        html
    }

    /// The original HTML is a separate reading representation of a quoted
    /// reply, never a replacement for its wire body. If the user edits the
    /// quotation itself, show the edited plain body instead of stale HTML.
    pub(crate) fn reply_html_review_parts(&self) -> Option<(&str, &str, &str, &str)> {
        let original = self.reply_html_quote.as_ref()?;
        let (before, after) = self.body.split_once(original.quoted_body.as_str())?;
        // Require the original attribution to start on its own line.
        if !before.is_empty() && !before.ends_with('\n') {
            return None;
        }
        let (attribution, _) = original.quoted_body.split_once('\n')?;
        Some((before, attribution, original.source.as_ref(), after))
    }

    /// The actual HTML alternative used for inline forwards, if present.
    pub fn outgoing_html_preview(&self) -> anyhow::Result<Option<String>> {
        let Some(raw) = self.forwarded_raw.as_deref() else {
            return Ok(None);
        };
        let parts = crate::outgoing::inline_forward_body(raw)?;
        Ok(parts.html.map(|html| {
            let personal_note = self.body.split(FORWARD_SEPARATOR).next().unwrap_or("");
            let original_headers = self
                .body
                .split(FORWARD_SEPARATOR)
                .nth(1)
                .and_then(|tail| tail.split("\n\n").next())
                .unwrap_or("");
            let attribution = format!("{FORWARD_SEPARATOR}{original_headers}");
            forward_html(&html, &attribution, personal_note)
        }))
    }

    pub(crate) fn reset_preview_navigation(&mut self) {
        self.send_preview.reset_navigation();
    }

    /// Drop all transient representations when leaving the review.
    pub(crate) fn close_send_preview(&mut self) {
        self.confirm_send = false;
        self.send_preview = SendPreview::default();
    }

    pub(crate) fn clear_preview_visual(&mut self) {
        self.send_preview.navigation.clear_visual();
    }

    pub(super) fn toggle_preview_visual(&mut self, linewise: bool) {
        let preview = &mut self.send_preview;
        preview
            .navigation
            .toggle_visual(linewise, preview.layout.rows.len());
        preview.count = None;
    }

    #[must_use]
    pub fn preview_visual_range(&self) -> Option<ReaderSelection> {
        self.send_preview.navigation.visual_range()
    }

    pub(crate) fn update_preview_viewport(&mut self, height: usize) {
        self.send_preview.update_viewport(height);
    }

    pub(super) fn handle_preview_motion(&mut self, key: KeyEvent) {
        self.send_preview.handle_motion(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outgoing_review_state_is_ephemeral_and_draft_shape_is_unchanged() {
        let mut composer = Composer::new();
        composer.body = "draft content".into();
        composer.send_preview.mode = SendPreviewMode::Html;
        composer.send_preview.document.html_source = Some("<p>preview only</p>".into());
        composer.send_preview.navigation.row = 3;
        composer.send_preview.layout.text = "preview only".into();
        let draft = serde_json::to_value(&composer).unwrap();
        assert!(draft.get("send_preview").is_none());
        assert!(draft.get("preview_html_source").is_none());
        let recovered: Composer = serde_json::from_value(draft).unwrap();
        assert_eq!(recovered.body, composer.body);
        assert_eq!(recovered.send_preview.mode, SendPreviewMode::Plain);
        assert_eq!(
            recovered.send_preview.navigation,
            ReaderNavigation::default()
        );
        assert!(recovered.send_preview.document.html_source.is_none());
        assert!(recovered.send_preview.layout.rows.is_empty());
    }
}
