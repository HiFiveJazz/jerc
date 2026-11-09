use std::fmt;

#[derive(
    serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct MessageId(pub String);

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
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

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum MailboxRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Archive,
    Junk,
    Other,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
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

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct MessageSummary {
    pub id: MessageId,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from_name: String,
    pub from_address: String,
    /// Recipients actually present in the message headers. Bcc is normally
    /// absent in recipient copies and must never be inferred from the envelope.
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub timestamp: i64,
    pub date_label: String,
    pub size: u64,
    pub flags: MessageFlags,
    /// Header-only, conservative hint available before loading the MIME body.
    pub has_attachment_hint: bool,
    /// MIME filenames from IMAP BODYSTRUCTURE, available before fetching body.
    pub attachment_names_hint: Vec<String>,
}

impl MessageSummary {
    #[must_use]
    pub fn unread(&self) -> bool {
        !self.flags.contains(MessageFlags::SEEN)
    }

    /// Prefer the definitive MIME-part count over any header-only hint.
    /// This also overrides stale, speculative hints in already-loaded lists.
    #[must_use]
    pub fn has_attachment(&self, body: Option<&MessageBody>) -> bool {
        body.map_or(self.has_attachment_hint, |body| body.attachment_count > 0)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HtmlImageCandidate {
    pub source: String,
    pub width_px: Option<u32>,
    pub height_px: Option<u32>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
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

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlSpan {
    pub text: String,
    pub style: HtmlSpanStyle,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlLine {
    pub spans: Vec<HtmlSpan>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct MessageBody {
    pub id: MessageId,
    /// The MIME text/plain representation when one exists.
    pub plain_text: Option<String>,
    /// A generated readable plain-text view for HTML-only messages. This keeps
    /// the rich/plain toggle useful even when the sender omitted text/plain.
    pub plain_fallback: Option<String>,
    /// Original HTML alternative. Keeping it lets the UI reflow the document
    /// to the actual reader width instead of freezing every message at the
    /// worker's fallback width.
    pub html_source: Option<String>,
    /// Plain-string projection of the rich HTML rendering. This is also used
    /// when quoting HTML-only mail in a reply.
    pub html_text: Option<String>,
    /// Rich terminal representation produced by the worker at a fallback width
    /// and reflowed by reader mode when the actual viewport width changes. Table
    /// borders plus semantic links/emphasis survive into Ratatui.
    pub html_lines: Option<Vec<HtmlLine>>,
    /// Human-readable renderer label shown in reader mode.
    pub html_renderer: Option<String>,
    /// Width used to produce html_text/html_lines. A zero value means no rich
    /// layout has been produced yet.
    pub html_render_width: usize,
    pub attachment_count: usize,
    /// MIME attachment names for Gmail-like preview chips under the subject.
    pub attachment_names: Vec<String>,
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
        if prefer_html
            && let Some(html) = self
                .html_text
                .as_deref()
                .filter(|text| !text.trim().is_empty())
        {
            return html;
        }
        if let Some(plain) = self
            .plain_text
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            return plain;
        }
        if let Some(plain) = self
            .plain_fallback
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            return plain;
        }
        if let Some(html) = self
            .html_text
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
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

    /// Return the rendered HTML link that actually occupies the reader cursor
    /// position. This intentionally follows the visible rich-text spans instead
    /// of a separate global link index, so `o` behaves like Vim's gf-style
    /// context action: move the cursor onto a link and open that exact target.
    #[must_use]
    pub fn html_link_at(&self, line_index: usize, column: usize) -> Option<&str> {
        let line = self.html_lines.as_ref()?.get(line_index)?;
        let mut start = 0usize;
        for span in &line.spans {
            let end = start.saturating_add(span.text.chars().count());
            if start <= column && column < end {
                return span.style.link.as_deref();
            }
            start = end;
        }
        None
    }

    #[must_use]
    pub fn html_images(&self) -> Vec<String> {
        self.html_image_candidates()
            .into_iter()
            .map(|image| image.source)
            .collect()
    }

    #[must_use]
    pub fn html_image_candidates(&self) -> Vec<HtmlImageCandidate> {
        // Prefer the original HTML source order over html2text image annotations.
        // html2text intentionally drops many <img> tags whose alt text is empty,
        // which made us skip real header/avatar images and then pick a footer badge
        // or social icon farther down the message instead. Browsers still render
        // those alt-less images, so use the source document as the authority.
        if let Some(html) = self.html_source.as_deref() {
            let images = html_source_image_candidates(html);
            if !images.is_empty() {
                return images;
            }
        }

        // Fallback for synthetic/test bodies where only rich annotations exist.
        let mut images = Vec::new();
        if let Some(lines) = &self.html_lines {
            for line in lines {
                for span in &line.spans {
                    let Some(source) = span.style.image.as_deref() else {
                        continue;
                    };
                    if !is_remote_image_source(source)
                        || looks_like_tracking_image(source)
                        || images
                            .iter()
                            .any(|existing: &HtmlImageCandidate| existing.source == source)
                    {
                        continue;
                    }
                    images.push(HtmlImageCandidate {
                        source: source.to_owned(),
                        width_px: None,
                        height_px: None,
                    });
                }
            }
        }
        images
    }

    #[must_use]
    pub fn has_plain_view(&self) -> bool {
        self.plain_text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
            || self
                .plain_fallback
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
    }

    #[must_use]
    pub fn plain_is_empty(&self) -> bool {
        !self.has_plain_view()
    }
}

fn html_source_image_candidates(html: &str) -> Vec<HtmlImageCandidate> {
    let mut images = Vec::new();
    let mut image_alts: Vec<Option<String>> = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = html[cursor..].find('<') {
        let start = cursor + relative;
        let Some(relative_end) = html[start..].find('>') else {
            break;
        };
        let end = start + relative_end;
        let tag = &html[start..=end];
        cursor = end.saturating_add(1);

        let trimmed = tag
            .trim_start_matches('<')
            .trim_start()
            .to_ascii_lowercase();
        if !trimmed.starts_with("img")
            || trimmed
                .chars()
                .nth(3)
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':'))
        {
            continue;
        }

        let Some(raw_source) = html_tag_attribute(tag, "src") else {
            continue;
        };
        let source = decode_html_attribute(&raw_source);
        if !is_remote_image_source(&source)
            || looks_like_tracking_image(&source)
            || images
                .iter()
                .any(|existing: &HtmlImageCandidate| existing.source == source)
        {
            continue;
        }

        let style = html_tag_attribute(tag, "style");
        if style.as_deref().is_some_and(html_image_style_is_hidden) {
            continue;
        }

        let attribute_width = html_tag_attribute(tag, "width")
            .as_deref()
            .and_then(parse_css_pixel_value);
        let attribute_height = html_tag_attribute(tag, "height")
            .as_deref()
            .and_then(parse_css_pixel_value);
        let width_px = style
            .as_deref()
            .and_then(|style| css_property_pixels(style, "width"))
            .or(attribute_width);
        let height_px = style
            .as_deref()
            .and_then(|style| css_property_pixels(style, "height"))
            .or(attribute_height);

        // Zero-sized/1px resources are almost always open trackers or layout
        // shims even when the URL itself does not contain a tracker keyword.
        if width_px.is_some_and(|width| width <= 1) || height_px.is_some_and(|height| height <= 1) {
            continue;
        }

        let alt = html_tag_attribute(tag, "alt")
            .map(|alt| decode_html_attribute(&alt).trim().to_owned())
            .filter(|alt| !alt.is_empty());
        images.push(HtmlImageCandidate {
            source,
            width_px,
            height_px,
        });
        image_alts.push(alt);
    }

    // Some marketing headers are assembled from several adjacent image slices
    // carrying the same descriptive alt text. Picking the first source-order
    // image can therefore select a decorative left/right fragment instead of
    // the actual central logo. Within a contiguous run of the same non-empty
    // alt text, move the largest declared image to the front while preserving
    // the order of everything else. Empty-alt runs deliberately stay untouched
    // because those are often meaningful avatar/hero sequences (for example
    // Ko-fi mail) where source order is the best signal we have.
    let mut run_start = 0usize;
    while run_start < images.len() {
        let Some(run_alt) = image_alts[run_start].as_deref() else {
            run_start += 1;
            continue;
        };
        let mut run_end = run_start + 1;
        while run_end < images.len()
            && image_alts[run_end]
                .as_deref()
                .is_some_and(|alt| alt.eq_ignore_ascii_case(run_alt))
        {
            run_end += 1;
        }
        if run_end - run_start > 1 {
            let best = (run_start..run_end)
                .max_by_key(|&index| image_candidate_declared_area(&images[index]))
                .unwrap_or(run_start);
            if best != run_start {
                images[run_start..=best].rotate_right(1);
                image_alts[run_start..=best].rotate_right(1);
            }
        }
        run_start = run_end;
    }

    images
}

fn image_candidate_declared_area(image: &HtmlImageCandidate) -> u64 {
    match (image.width_px, image.height_px) {
        (Some(width), Some(height)) => u64::from(width) * u64::from(height),
        (Some(width), None) => u64::from(width),
        (None, Some(height)) => u64::from(height),
        (None, None) => 0,
    }
}

fn is_remote_image_source(source: &str) -> bool {
    matches!(
        source.split_once(':').map(|(scheme, _)| scheme),
        Some("http" | "https")
    )
}

fn html_image_style_is_hidden(style: &str) -> bool {
    style.split(';').any(|declaration| {
        let Some((name, value)) = declaration.split_once(':') else {
            return false;
        };
        let name = name.trim();
        let value = value.trim().to_ascii_lowercase();
        (name.eq_ignore_ascii_case("display") && value.starts_with("none"))
            || (name.eq_ignore_ascii_case("visibility") && value.starts_with("hidden"))
    })
}

fn html_tag_attribute(tag: &str, wanted: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut i = 1usize;
    while i < bytes.len() {
        while i < bytes.len()
            && (bytes[i].is_ascii_whitespace() || matches!(bytes[i], b'<' | b'/' | b'>'))
        {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let name_start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'-' | b'_' | b':'))
        {
            i += 1;
        }
        if name_start == i {
            i += 1;
            continue;
        }
        let name = &tag[name_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let (value_start, value_end) = if matches!(bytes[i], b'"' | b'\'') {
            let quote = bytes[i];
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let end = i;
            i = i.saturating_add(1);
            (start, end)
        } else {
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                i += 1;
            }
            (start, i)
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Some(tag[value_start..value_end].to_owned());
        }
    }
    None
}

fn decode_html_attribute(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&#x26;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn parse_css_pixel_value(value: &str) -> Option<u32> {
    let trimmed = value.trim().trim_end_matches("px").trim();
    if trimmed.is_empty() || trimmed.contains('%') || trimmed.eq_ignore_ascii_case("auto") {
        return None;
    }
    let value = trimmed.parse::<f64>().ok()?;
    (value.is_finite() && value > 0.0).then(|| value.round() as u32)
}

fn css_property_pixels(style: &str, property: &str) -> Option<u32> {
    style.split(';').find_map(|declaration| {
        let (name, value) = declaration.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(property)
            .then(|| parse_css_pixel_value(value))
            .flatten()
    })
}

fn looks_like_tracking_image(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    [
        "/open/",
        "opentracker",
        "tracking/open",
        "pixel",
        "beacon",
        "transparent.gif",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub html_body: Option<String>,
    pub attachments: Vec<OutgoingAttachment>,
    pub custom_headers: Vec<(String, String)>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Optional explicit RFC 5322 attachment-forwarding mode at the wire API
    /// level. Normal composer forwarding now reconstructs inline HTML/plain
    /// alternatives and file attachments, and sends this field as None.
    pub forwarded_raw: Option<Vec<u8>>,
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
    use super::{
        HtmlImageCandidate, HtmlLine, HtmlSpan, HtmlSpanStyle, MessageBody, MessageFlags, MessageId,
    };

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
    fn html_link_at_tracks_the_visible_span_under_the_cursor() {
        let body = MessageBody {
            id: MessageId("links".into()),
            plain_text: None,
            plain_fallback: None,
            html_source: Some("<a href=\"https://example.com/one\">first</a> second".into()),
            html_text: Some("first second".into()),
            html_lines: Some(vec![HtmlLine {
                spans: vec![
                    HtmlSpan {
                        text: "first".into(),
                        style: HtmlSpanStyle {
                            link: Some("https://example.com/one".into()),
                            ..HtmlSpanStyle::default()
                        },
                    },
                    HtmlSpan {
                        text: " second".into(),
                        style: HtmlSpanStyle::default(),
                    },
                ],
            }]),
            html_renderer: Some("test".into()),
            html_render_width: 80,
            attachment_count: 0,
            attachment_names: Vec::new(),
        };
        assert_eq!(body.html_link_at(0, 0), Some("https://example.com/one"));
        assert_eq!(body.html_link_at(0, 4), Some("https://example.com/one"));
        assert_eq!(body.html_link_at(0, 5), None);
        assert_eq!(body.html_link_at(1, 0), None);
    }

    #[test]
    fn html_images_skip_tracking_pixels_and_duplicate_sources() {
        let image_span = |source: &str| HtmlSpan {
            text: "image".into(),
            style: HtmlSpanStyle {
                image: Some(source.into()),
                ..HtmlSpanStyle::default()
            },
        };
        let body = MessageBody {
            id: MessageId("images".into()),
            plain_text: None,
            plain_fallback: None,
            html_source: Some("<img>".into()),
            html_text: Some("image".into()),
            html_lines: Some(vec![HtmlLine {
                spans: vec![
                    image_span("https://example.com/hero.png"),
                    image_span("https://example.com/hero.png"),
                    image_span("https://tracker.example/open/pixel.gif"),
                    image_span("cid:embedded-logo"),
                ],
            }]),
            html_renderer: Some("test".into()),
            html_render_width: 80,
            attachment_count: 0,
            attachment_names: Vec::new(),
        };
        assert_eq!(
            body.html_images(),
            vec!["https://example.com/hero.png".to_owned()]
        );
    }

    #[test]
    fn html_image_candidates_follow_source_order_even_when_alt_is_empty() {
        let body = MessageBody {
            id: MessageId("source-order-images".into()),
            plain_text: None,
            plain_fallback: None,
            html_source: Some(
                r#"<img src="https://example.com/avatar.png" width="45" height="45" alt="">
                    <p>message</p>
                    <img src="https://example.com/footer-badge.png" width="120" height="40" alt="App Store">"#
                    .to_owned(),
            ),
            html_text: Some("message App Store".into()),
            // Simulate html2text exposing only the footer image because the
            // real header/avatar had empty alt text.
            html_lines: Some(vec![HtmlLine {
                spans: vec![HtmlSpan {
                    text: "App Store".into(),
                    style: HtmlSpanStyle {
                        image: Some("https://example.com/footer-badge.png".into()),
                        ..HtmlSpanStyle::default()
                    },
                }],
            }]),
            html_renderer: Some("test".into()),
            html_render_width: 80,
            attachment_count: 0,
            attachment_names: Vec::new(),
        };

        assert_eq!(
            body.html_image_candidates(),
            vec![
                HtmlImageCandidate {
                    source: "https://example.com/avatar.png".into(),
                    width_px: Some(45),
                    height_px: Some(45),
                },
                HtmlImageCandidate {
                    source: "https://example.com/footer-badge.png".into(),
                    width_px: Some(120),
                    height_px: Some(40),
                },
            ]
        );
    }

    #[test]
    fn html_image_candidates_prefer_largest_slice_in_same_alt_header_group() {
        let body = MessageBody {
            id: MessageId("composite-header".into()),
            plain_text: None,
            plain_fallback: None,
            html_source: Some(
                r#"<img src="https://example.com/left.png" width="60" alt="Walmart Plus Home page">
                    <img src="https://example.com/logo.png" width="150" alt="Walmart Plus Home page">
                    <img src="https://example.com/right.png" width="60" alt="Walmart Plus Home page">
                    <img src="https://example.com/body.png" width="300" alt="Body image">"#
                    .to_owned(),
            ),
            html_text: Some("message".into()),
            html_lines: None,
            html_renderer: Some("test".into()),
            html_render_width: 80,
            attachment_count: 0,
            attachment_names: Vec::new(),
        };

        assert_eq!(
            body.html_images(),
            vec![
                "https://example.com/logo.png".to_owned(),
                "https://example.com/left.png".to_owned(),
                "https://example.com/right.png".to_owned(),
                "https://example.com/body.png".to_owned(),
            ]
        );
    }

    #[test]
    fn html_image_candidates_keep_empty_alt_source_order() {
        let body = MessageBody {
            id: MessageId("empty-alt-order".into()),
            plain_text: None,
            plain_fallback: None,
            html_source: Some(
                r#"<img src="https://example.com/avatar.png" width="45" height="45" alt="">
                    <img src="https://example.com/hero.png" width="500" height="300" alt="">"#
                    .to_owned(),
            ),
            html_text: Some("message".into()),
            html_lines: None,
            html_renderer: Some("test".into()),
            html_render_width: 80,
            attachment_count: 0,
            attachment_names: Vec::new(),
        };

        assert_eq!(
            body.html_images(),
            vec![
                "https://example.com/avatar.png".to_owned(),
                "https://example.com/hero.png".to_owned(),
            ]
        );
    }

    #[test]
    fn html_image_candidates_skip_hidden_and_tiny_source_images() {
        let body = MessageBody {
            id: MessageId("filtered-source-images".into()),
            plain_text: None,
            plain_fallback: None,
            html_source: Some(
                r#"<img src="https://example.com/hidden.png" style="display:none" width="90" height="60">
                    <img src="https://example.com/shim.png" width="1" height="1">
                    <img src="https://example.com/hero.png" width="90" height="60">"#
                    .to_owned(),
            ),
            html_text: Some("hero".into()),
            html_lines: None,
            html_renderer: Some("test".into()),
            html_render_width: 80,
            attachment_count: 0,
            attachment_names: Vec::new(),
        };

        assert_eq!(
            body.html_images(),
            vec!["https://example.com/hero.png".to_owned()]
        );
    }

    #[test]
    fn html_image_candidates_prefer_inline_css_pixel_size_hints() {
        let source = "https://example.com/logo@2x.png?a=1&b=2";
        let body = MessageBody {
            id: MessageId("sized-image".into()),
            plain_text: None,
            plain_fallback: None,
            html_source: Some(
                r#"<img src="https://example.com/logo@2x.png?a=1&amp;b=2" width="74" height="24" style="width: 148px; height: 48px">"#
                    .to_owned(),
            ),
            html_text: Some("logo".into()),
            html_lines: Some(vec![HtmlLine {
                spans: vec![HtmlSpan {
                    text: "logo".into(),
                    style: HtmlSpanStyle {
                        image: Some(source.into()),
                        ..HtmlSpanStyle::default()
                    },
                }],
            }]),
            html_renderer: Some("test".into()),
            html_render_width: 80,
            attachment_count: 0,
            attachment_names: Vec::new(),
        };

        assert_eq!(
            body.html_image_candidates(),
            vec![HtmlImageCandidate {
                source: source.into(),
                width_px: Some(148),
                height_px: Some(48),
            }]
        );
    }

    #[test]
    fn body_falls_back_to_html_when_plain_is_empty() {
        let body = MessageBody {
            id: MessageId("1".into()),
            plain_text: Some("   ".into()),
            plain_fallback: None,
            html_source: Some("<b>HTML version</b>".into()),
            html_text: Some("HTML version".into()),
            html_lines: None,
            html_renderer: Some("test".into()),
            html_render_width: 80,
            attachment_count: 0,
            attachment_names: Vec::new(),
        };
        assert_eq!(body.display_text(false), "HTML version");
        assert!(body.plain_is_empty());
    }
}
