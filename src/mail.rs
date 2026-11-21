use std::borrow::Cow;
use std::io::{BufRead, BufReader, Read};

use anyhow::{Context, Result, anyhow};
use mail_parser::{MessageParser, PartKind};

use crate::model::{
    HtmlLine, HtmlSpan, HtmlSpanStyle, MessageBody, MessageFlags, MessageId, MessageSummary,
    OutgoingAttachment,
};

const MAX_HEADER_BYTES: usize = 512 * 1024;

#[derive(Debug, Default, Clone)]
pub struct ThreadHeaders {
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

pub fn read_header_block(reader: impl Read) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(reader);
    let mut out = Vec::with_capacity(4096);
    let mut line = Vec::with_capacity(256);

    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if out.len() + line.len() > MAX_HEADER_BYTES {
            return Err(anyhow!(
                "message header block exceeds {MAX_HEADER_BYTES} bytes"
            ));
        }
        let blank = line == b"\n" || line == b"\r\n";
        out.extend_from_slice(&line);
        if blank {
            break;
        }
    }

    if !out.ends_with(b"\n\n") && !out.ends_with(b"\r\n\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    Ok(out)
}

#[must_use]
pub fn extract_thread_headers(raw_headers: &[u8]) -> ThreadHeaders {
    let text = String::from_utf8_lossy(raw_headers);
    let mut unfolded = Vec::<String>::new();
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = unfolded.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
            }
        } else {
            unfolded.push(line.to_owned());
        }
    }

    let mut result = ThreadHeaders::default();
    for line in unfolded {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "message-id" => result.message_id = first_message_id(value),
            "in-reply-to" => result.in_reply_to = first_message_id(value),
            "references" => result.references = all_message_ids(value),
            _ => {}
        }
    }
    result
}

fn first_message_id(value: &str) -> Option<String> {
    all_message_ids(value).into_iter().next()
}

fn all_message_ids(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let id = rest[..end].trim();
        if !id.is_empty() {
            out.push(id.to_owned());
        }
        rest = &rest[end + 1..];
    }
    if out.is_empty() {
        out.extend(
            value
                .split_whitespace()
                .map(|v| v.trim_matches(|c| matches!(c, '<' | '>' | ',' | ';')))
                .filter(|v| v.contains('@') && !v.is_empty())
                .map(ToOwned::to_owned),
        );
    }
    out
}

pub fn parse_summary(
    id: MessageId,
    raw_headers: &[u8],
    size: u64,
    flags: MessageFlags,
) -> Result<MessageSummary> {
    let parser = MessageParser::new();
    let message = parser
        .parse(raw_headers)
        .ok_or_else(|| anyhow!("unable to parse RFC 5322 headers"))?;

    let (from_name, from_address) = message
        .from()
        .and_then(|address| address.mailboxes().next())
        .map(|mailbox| {
            let name = mailbox.name().unwrap_or_default().trim().to_owned();
            let address = mailbox.address().unwrap_or_default().trim().to_owned();
            (name, address)
        })
        .unwrap_or_default();

    // Only render addresses actually serialized in this copy. In particular,
    // a recipient cannot recover who else was blind-copied via Bcc.
    macro_rules! render_addresses {
        ($value:expr) => {
            $value
                .map(|value| {
                    value
                        .mailboxes()
                        .filter_map(|mailbox| {
                            let address = mailbox.address()?.trim();
                            if address.is_empty() {
                                return None;
                            }
                            let name = mailbox.name().unwrap_or_default().trim();
                            Some(if name.is_empty() {
                                address.to_owned()
                            } else {
                                format!("{name} <{address}>")
                            })
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default()
        };
    }
    let to = render_addresses!(message.to());
    let cc = render_addresses!(message.cc());
    let bcc = render_addresses!(message.bcc());

    let subject = message
        .subject()
        .unwrap_or("(no subject)")
        .trim()
        .to_owned();
    let (timestamp, date_label) = message.date().map_or((0, String::new()), |date| {
        let rendered = date.to_rfc3339();
        let label = rendered.get(5..10).unwrap_or(&rendered).to_owned();
        (date.to_timestamp(), label)
    });
    let thread = extract_thread_headers(raw_headers);

    Ok(MessageSummary {
        headers: std::sync::Arc::new(crate::model::MessageHeaders {
            id,
            thread_id: None,
            message_id: thread.message_id,
            in_reply_to: thread.in_reply_to,
            references: thread.references.into(),
            from_name,
            from_address,
            to,
            cc,
            bcc,
            subject,
            timestamp,
            date_label: date_label.into(),
            size,
            has_attachment_hint: attachment_hint(raw_headers),
            attachment_names_hint: Vec::new().into(),
        }),
        flags,
    })
}

/// A header-only hint for an *explicitly declared* attachment.
///
/// `multipart/mixed` only describes a MIME container. It may hold nothing
/// except a `multipart/related` HTML body, as in some marketing messages.
/// The presence of a real file cannot be known without the MIME structure;
/// prefer the parsed body's attachment count once the body is available.
#[must_use]
pub fn attachment_hint(headers: &[u8]) -> bool {
    let text = String::from_utf8_lossy(headers).to_ascii_lowercase();
    text.lines()
        // Some callers provide a full .eml rather than only its RFC 5322
        // header block. Never mistake a nested MIME part's disposition for a
        // top-level header hint.
        .take_while(|line| !line.trim_end_matches('\r').is_empty())
        .any(|line| {
            let line = line.trim_start();
            line.starts_with("content-disposition:")
                && line["content-disposition:".len()..]
                    .trim_start()
                    .starts_with("attachment")
        })
}

/// Decoded MIME attachments in display order. Never infer a file path from a
/// sender-provided filename: names are sanitized at the UI/export boundary.
/// Some providers include internal metadata MIME parts (not user files) in
/// multipart/alternative. mail-parser classifies unknown text subtypes as
/// attachments, so hide unlabelled Gmail reaction JSON but preserve real files
/// explicitly marked attachment or supplied with a filename.
fn visible_mime_attachment(part: &mail_parser::MessagePart<'_>) -> bool {
    if part.is_content_type("text", "vnd.google.email-reaction+json") {
        return part
            .attachment_name()
            .is_some_and(|name| !name.trim().is_empty())
            || part
                .content_disposition()
                .is_some_and(|disposition| disposition.is_attachment());
    }
    true
}

/// Some calendar providers include identical invitation bytes twice: an unnamed
/// `text/calendar` alternative and a named `invite.ics` attachment. Show the
/// named file only, but preserve unique or explicitly attached calendar parts.
fn attachment_parts<'msg>(
    message: &'msg mail_parser::Message<'_>,
) -> Vec<mail_parser::MessagePart<'msg>> {
    let parts: Vec<_> = message
        .attachments()
        .filter(|part| visible_mime_attachment(part))
        .collect();
    parts
        .iter()
        .filter(|part| {
            if !part.is_content_type("text", "calendar")
                || part
                    .attachment_name()
                    .is_some_and(|name| !name.trim().is_empty())
                || part
                    .content_disposition()
                    .is_some_and(|disposition| disposition.is_attachment())
            {
                return true;
            }
            !parts.iter().any(|named| {
                named
                    .attachment_name()
                    .is_some_and(|name| name.to_ascii_lowercase().ends_with(".ics"))
                    && named.decoded().as_ref() == part.decoded().as_ref()
            })
        })
        .copied()
        .collect()
}

fn attachment_display_name(part: &mail_parser::MessagePart<'_>, index: usize) -> String {
    let raw_name = part.attachment_name().unwrap_or("");
    let name = raw_name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if name.is_empty() || name == "." || name == ".." {
        if part.is_content_type("text", "calendar") {
            "invite.ics".into()
        } else {
            format!("attachment-{}", index + 1)
        }
    } else {
        name.to_owned()
    }
}

/// Cached IMAP summary hints predate the calendar MIME deduplication fix.
/// Until the message body is fetched, hide a single generic placeholder when
/// a named calendar invite is also advertised. The decoded MIME parts remain
/// authoritative: distinct invitations are still available after opening.
pub fn visible_attachment_hints(hints: &[String]) -> Vec<String> {
    let has_ics = hints
        .iter()
        .any(|name| name.to_ascii_lowercase().ends_with(".ics"));
    let generic_count = hints
        .iter()
        .filter(|name| {
            name.strip_prefix("attachment-").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|ch| ch.is_ascii_digit())
            })
        })
        .count();
    // Only the exact legacy two-chip case is unambiguous enough to
    // collapse without decoded MIME bytes. Keep larger/mixed lists intact.
    if !has_ics || generic_count != 1 || hints.len() != 2 {
        return hints.to_vec();
    }
    hints
        .iter()
        .filter(|name| {
            !name.strip_prefix("attachment-").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|ch| ch.is_ascii_digit())
            })
        })
        .cloned()
        .collect()
}

// Raw message limits apply before mail-parser can decode MIME parts.
// Explicit attachments/forwarding may be larger than a reader preview.
pub const MAX_BODY_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RAW_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_RENDERED_HTML_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// Attachment names only: do not decode binary payloads for a preview.
pub(crate) fn attachment_names(raw: &[u8]) -> Result<Vec<String>> {
    if raw.len() > MAX_RAW_MESSAGE_BYTES {
        anyhow::bail!("message exceeds the 32 MiB MIME attachment limit");
    }
    let parsed = MessageParser::new()
        .parse(raw)
        .ok_or_else(|| anyhow!("unable to parse MIME attachments"))?;
    Ok(attachment_parts(&parsed)
        .into_iter()
        .enumerate()
        .map(|(index, part)| attachment_display_name(&part, index))
        .collect())
}

pub fn extract_attachments(raw: &[u8]) -> Result<Vec<OutgoingAttachment>> {
    if raw.len() > MAX_RAW_MESSAGE_BYTES {
        anyhow::bail!("message exceeds the 32 MiB MIME attachment limit");
    }

    let parsed = MessageParser::new()
        .parse(raw)
        .ok_or_else(|| anyhow!("unable to parse MIME attachments"))?;
    let mut results = Vec::new();
    for (index, part) in attachment_parts(&parsed).into_iter().enumerate() {
        let filename = attachment_display_name(&part, index);
        let data = part.decoded().into_owned();
        results.push(OutgoingAttachment {
            content_type: guessed_attachment_type(&filename, &data).to_owned(),
            filename,
            data,
        });
    }
    Ok(results)
}

fn guessed_attachment_type(name: &str, bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"%PDF-") {
        return "application/pdf";
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return "image/jpeg";
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif";
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]) {
        return "image/webp";
    }
    match name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("txt" | "patch" | "diff" | "log" | "md") => "text/plain",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("ics") => "text/calendar",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

pub fn parse_body(id: MessageId, raw: &[u8]) -> Result<MessageBody> {
    if raw.len() > MAX_BODY_MESSAGE_BYTES {
        anyhow::bail!("message exceeds the 8 MiB reader limit; export the raw message instead");
    }
    let parser = MessageParser::new();
    let message = parser
        .parse(raw)
        .ok_or_else(|| anyhow!("unable to parse RFC 5322 message"))?;

    // mail-parser's RFC 8621 body lists intentionally include synthesized
    // alternatives: an HTML part may stand in for missing text/plain, and a
    // text part may stand in for missing text/html. Inspect PartKind so the UI
    // only exposes the rich/plain toggle when the sender actually supplied HTML.
    let plain_text = message.text_body().find_map(|part| match part.kind() {
        PartKind::Text => part.text().map(|text| text.into_owned()),
        _ => None,
    });

    let html_source = message.html_body().find_map(|part| match part.kind() {
        PartKind::Html => part.text().map(|html| html.into_owned()),
        _ => None,
    });
    let attachment_names = attachment_parts(&message)
        .into_iter()
        .enumerate()
        .map(|(index, part)| attachment_display_name(&part, index))
        .collect();
    render_message_body(id, plain_text, html_source, attachment_names)
}

pub(crate) fn render_message_body(
    id: MessageId,
    plain_text: Option<String>,
    html_source: Option<String>,
    attachment_names: Vec<String>,
) -> Result<MessageBody> {
    // HTML expansion (table flattening, rich layout, styled spans) can be much
    // larger than the RFC 5322 input. Reject oversized HTML before rendering.
    if html_source
        .as_ref()
        .is_some_and(|html| html.len() > MAX_RENDERED_HTML_SOURCE_BYTES)
    {
        anyhow::bail!("HTML body exceeds the 2 MiB rendering limit");
    }
    let plain_fallback = if plain_text
        .as_deref()
        .is_none_or(|text| text.trim().is_empty())
    {
        html_source
            .as_deref()
            .map(mail_parser::html_to_text)
            .map(|text| clean_reader_text(&text))
            .filter(|text| !text.trim().is_empty())
    } else {
        None
    };
    let rendered_html = html_source
        .as_deref()
        .map(|html| render_html_for_terminal(html, 100));
    let html_render_width = if rendered_html.is_some() { 100 } else { 0 };
    let (html_text, html_lines, html_renderer) = rendered_html
        .map(|(text, lines, renderer)| (Some(text), Some(lines), Some(renderer)))
        .unwrap_or((None, None, None));

    Ok(MessageBody {
        id,
        plain_text: plain_text.map(Into::into),
        plain_fallback: plain_fallback.map(Into::into),
        html_source: html_source.map(Into::into),
        html_text: html_text.map(Into::into),
        html_lines: html_lines.map(std::sync::Arc::new),
        html_renderer,
        html_render_width,
        attachment_count: attachment_names.len(),
        attachment_names,
    })
}

pub fn render_html_for_terminal(html: &str, width: usize) -> (String, Vec<HtmlLine>, String) {
    // html2text's rich renderer is intentionally closer to a text browser than
    // its String-only API: tables keep their box drawing and semantic markup
    // such as links/strong/emphasis remains tagged. Ratatui maps those tags to
    // the active jerc theme, which gives HTML mail the same useful visual
    // landmarks as a terminal browser without spawning an external process for
    // every message. The worker produces a fallback layout; reader mode can
    // reflow the preserved HTML when its actual viewport width changes.
    let flattened_html = flatten_presentation_tables_html(html);
    // Terminals cannot vertically offset glyphs, but Unicode superscripts
    // preserve explicitly marked ordinal suffixes such as 15<sup>th</sup>.
    let normalized_html = normalize_superscript_ordinals(&flattened_html);
    let rich = html2text::config::rich()
        .use_doc_css()
        .link_footnotes(false)
        .no_link_wrapping()
        .lines_from_read(normalized_html.as_bytes(), width.max(20));

    if let Ok(lines) = rich {
        let mut converted = Vec::with_capacity(lines.len());
        for line in lines {
            let spans = line
                .tagged_strings()
                .map(|tagged| HtmlSpan {
                    text: tagged.s.clone(),
                    style: rich_style(&tagged.tag),
                })
                .collect::<Vec<_>>();
            converted.push(HtmlLine { spans });
        }
        let converted = clean_rich_lines(converted);
        let converted = compact_prose_whitespace(converted);
        let converted = compact_generic_image_alt_lines(converted);
        let converted = compact_bare_list_markers(converted);
        let converted = clean_heading_markers(converted);
        let converted = simplify_layout_table_blocks(converted);
        let converted = compact_decorative_horizontal_rules(converted);
        let mut converted = compact_social_link_chrome(converted);
        // Do this after all layout/whitespace passes: those passes depend on
        // their original span boundaries. The reader keeps the resulting rich
        // lines, so merging equivalent adjacent spans reduces retained String
        // allocations without giving up the already-rendered HTML layout.
        coalesce_rich_spans(&mut converted);
        let text = rich_lines_to_text(&converted);
        if !text.trim().is_empty() {
            return (text, converted, "html2text-rich".to_owned());
        }
    }

    // Malformed HTML should still be readable. mail-parser's fallback is plain
    // text, so expose it as one unstyled span per line rather than failing the
    // whole message body.
    let text = clean_reader_text(&mail_parser::html_to_text(&normalized_html));
    let lines = text
        .lines()
        .map(|line| HtmlLine {
            spans: vec![HtmlSpan {
                text: line.to_owned(),
                style: HtmlSpanStyle::default(),
            }],
        })
        .collect();
    (text, lines, "mail-parser fallback".to_owned())
}

// Respect explicit HTML superscripts without rewriting ordinary text like
// "15th". The terminal cannot move glyphs vertically, so use narrow Unicode
// equivalents for the four English ordinal suffixes. Other <sup> content is
// left to the normal HTML renderer (e.g. mathematical expressions or footnotes).
// The common case stays allocation-free; an owned copy is created only if an
// ordinal superscript actually needs substituting.
fn normalize_superscript_ordinals(html: &str) -> Cow<'_, str> {
    if !html
        .as_bytes()
        .windows(4)
        .any(|window| window.eq_ignore_ascii_case(b"<sup"))
    {
        return Cow::Borrowed(html);
    }

    let mut rewritten = None::<String>;
    let mut scan = 0usize;
    let mut copy_from = 0usize;
    while let Some(relative) = html[scan..].find('<') {
        let start = scan + relative;
        let Some(tag_end) = find_html_tag_end(html, start) else {
            break;
        };
        if let Some((suffix, close_end)) = ordinal_superscript_at(html, start, tag_end) {
            let output = rewritten.get_or_insert_with(|| String::with_capacity(html.len()));
            output.push_str(&html[copy_from..start]);
            output.push_str(suffix);
            copy_from = close_end + 1;
            scan = copy_from;
        } else {
            scan = tag_end + 1;
        }
    }

    if let Some(mut output) = rewritten {
        output.push_str(&html[copy_from..]);
        Cow::Owned(output)
    } else {
        Cow::Borrowed(html)
    }
}

fn ordinal_superscript_at(
    html: &str,
    open_start: usize,
    open_end: usize,
) -> Option<(&'static str, usize)> {
    let (closing, name) = html_tag_name(&html[open_start..=open_end]);
    if closing || !name.is_some_and(|name| name.eq_ignore_ascii_case("sup")) {
        return None;
    }
    // Only handle simple text within a <sup> element. Nested tags and other
    // superscripts pass through unchanged, retaining their HTML semantics.
    let close_start = open_end + 1 + html[open_end + 1..].find('<')?;
    let close_end = find_html_tag_end(html, close_start)?;
    let (closing, name) = html_tag_name(&html[close_start..=close_end]);
    if !closing || !name.is_some_and(|name| name.eq_ignore_ascii_case("sup")) {
        return None;
    }
    let suffix = match &html[open_end + 1..close_start] {
        "st" => "ˢᵗ",
        "nd" => "ⁿᵈ",
        "rd" => "ʳᵈ",
        "th" => "ᵗʰ",
        _ => return None,
    };
    Some((suffix, close_end))
}

fn flatten_presentation_tables_html(html: &str) -> Cow<'_, str> {
    // Most messages have no role=presentation tables. Passing borrowed HTML
    // to html2text avoids allocating/copying the entire MIME part on every
    // render and reflow. The ASCII-insensitive precheck cannot miss a valid
    // presentation attribute, including one with mixed-case spelling.
    if !html
        .as_bytes()
        .windows(b"presentation".len())
        .any(|part| part.eq_ignore_ascii_case(b"presentation"))
    {
        return Cow::Borrowed(html);
    }

    // Marketing mail routinely uses deeply nested tables as a CSS layout
    // primitive. Linearize only presentation tables, leaving receipt/data
    // tables and all unmodified HTML intact.
    let mut out = String::with_capacity(html.len());
    let mut table_stack = Vec::<bool>::new();
    let mut presentation_depth = 0usize;
    let mut modified = false;
    let mut cursor = 0usize;

    while let Some(relative_start) = html[cursor..].find('<') {
        let start = cursor + relative_start;
        out.push_str(&html[cursor..start]);
        let Some(end) = find_html_tag_end(html, start) else {
            out.push_str(&html[start..]);
            cursor = html.len();
            break;
        };
        let tag = &html[start..=end];
        let (closing, name) = html_tag_name(tag);
        let is_table = name.is_some_and(|name| name.eq_ignore_ascii_case("table"));
        let is_table_section = name.is_some_and(|name| {
            ["tbody", "thead", "tfoot"]
                .iter()
                .any(|part| name.eq_ignore_ascii_case(part))
        });
        let is_cell_or_row = name.is_some_and(|name| {
            ["tr", "td", "th"]
                .iter()
                .any(|part| name.eq_ignore_ascii_case(part))
        });

        if is_table && !closing {
            let flatten = presentation_depth > 0 || tag_has_presentation_role(tag);
            table_stack.push(flatten);
            if flatten {
                modified = true;
                presentation_depth += 1;
                out.push_str("<div>");
            } else {
                out.push_str(tag);
            }
        } else if is_table && closing {
            if let Some(flatten) = table_stack.pop() {
                if flatten {
                    presentation_depth = presentation_depth.saturating_sub(1);
                    out.push_str("</div>");
                } else {
                    out.push_str(tag);
                }
            } else {
                out.push_str(tag);
            }
        } else if is_table_section && presentation_depth > 0 {
            // Drop the table-only grouping tags inside a presentation table.
        } else if is_cell_or_row && presentation_depth > 0 {
            out.push_str(if closing { "</div>" } else { "<div>" });
        } else {
            out.push_str(tag);
        }
        cursor = end + 1;
    }

    if cursor < html.len() {
        out.push_str(&html[cursor..]);
    }
    if modified {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(html)
    }
}

fn find_html_tag_end(html: &str, start: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut quote = None::<u8>;
    for (offset, &byte) in bytes.get(start + 1..)?.iter().enumerate() {
        match (quote, byte) {
            (Some(active), current) if active == current => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some(start + 1 + offset),
            _ => {}
        }
    }
    None
}

fn html_tag_name(tag: &str) -> (bool, Option<&str>) {
    let inner = tag
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_start();
    if inner.starts_with('!') || inner.starts_with('?') {
        return (false, None);
    }
    let closing = inner.starts_with('/');
    let inner = inner.trim_start_matches('/').trim_start();
    let name_len = inner.bytes().take_while(u8::is_ascii_alphanumeric).count();
    (closing, (name_len > 0).then_some(&inner[..name_len]))
}

fn tag_has_presentation_role(tag: &str) -> bool {
    // Parsing role= with byte slices avoids a lowercase String allocation for
    // every layout table, while keeping attribute and value matching ASCII
    // case-insensitive and supporting unquoted or quoted values.
    let bytes = tag.as_bytes();
    let mut offset = 0usize;
    while let Some(relative) = bytes[offset..]
        .windows(4)
        .position(|candidate| candidate.eq_ignore_ascii_case(b"role"))
    {
        let start = offset + relative;
        let is_name_byte = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-');
        let before_ok = start == 0 || !is_name_byte(bytes[start - 1]);
        let mut cursor = start + 4;
        let after_ok = cursor >= bytes.len() || !is_name_byte(bytes[cursor]);
        if !before_ok || !after_ok {
            offset = cursor;
            continue;
        }
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            offset = cursor;
            continue;
        }
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        let quote = bytes
            .get(cursor)
            .copied()
            .filter(|byte| matches!(*byte, b'\'' | b'"'));
        if quote.is_some() {
            cursor += 1;
        }
        let value_start = cursor;
        while let Some(&byte) = bytes.get(cursor) {
            if quote.is_some_and(|quote| byte == quote)
                || (quote.is_none() && (byte.is_ascii_whitespace() || byte == b'>'))
            {
                break;
            }
            cursor += 1;
        }
        if bytes[value_start..cursor].eq_ignore_ascii_case(b"presentation") {
            return true;
        }
        offset = cursor.max(start + 4);
    }
    false
}

fn compact_prose_whitespace(mut lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    // HTML mail frequently uses combinations such as `&nbsp;  ` to tune a
    // graphical layout. Once NBSP has been converted to a normal space that
    // can leave distracting multi-space gaps in otherwise ordinary prose.
    // Do not touch box-drawn table rows or preformatted/code content: their
    // spacing is structural rather than decorative.
    for line in &mut lines {
        if line
            .spans
            .iter()
            .any(|span| span.style.code || span.text.chars().any(is_table_chrome_char))
        {
            continue;
        }

        let mut previous_space = false;
        for span in &mut line.spans {
            // Most rich-text spans already have normalized spacing. Scan for
            // actual changes before allocating another copy of their text.
            let mut last_space = previous_space;
            let mut needs_compaction = false;
            for ch in span.text.chars() {
                if matches!(ch, ' ' | '\t') {
                    needs_compaction |= last_space || ch == '\t';
                    last_space = true;
                } else {
                    last_space = false;
                }
            }
            if !needs_compaction {
                previous_space = last_space;
                continue;
            }
            let mut compacted = String::with_capacity(span.text.len());
            for ch in span.text.chars() {
                if matches!(ch, ' ' | '\t') {
                    if !previous_space {
                        compacted.push(' ');
                    }
                    previous_space = true;
                } else {
                    compacted.push(ch);
                    previous_space = false;
                }
            }
            span.text = compacted;
        }
    }
    lines
}

fn compact_generic_image_alt_lines(lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    // Useful image alt text is one of the best things a terminal renderer can
    // preserve (product names, event banners, brand names). Generic template
    // placeholders are different: labels such as "Dynamic Banner" and "item
    // image" add noise without conveying anything the user can act on. Drop a
    // line only when every visible span is an image annotation with a known
    // generic label, so descriptive alt text remains untouched.
    clean_rich_lines(
        lines
            .into_iter()
            .filter(|line| !line_is_generic_image_alt(line))
            .collect(),
    )
}

fn compact_bare_list_markers(lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    // Some newsletter HTML puts <br> elements between <li> elements. The rich
    // html2text renderer can turn those separators into marker-only rows (for
    // example a line containing just `*`) between otherwise correct bullets.
    // A single marker could be intentional; a repeated cluster is layout
    // fallout and can be removed safely.
    let bare_markers = lines
        .iter()
        .filter(|line| line_is_bare_list_marker(line))
        .count();
    if bare_markers < 2 {
        return lines;
    }

    clean_rich_lines(
        lines
            .into_iter()
            .filter(|line| !line_is_bare_list_marker(line))
            .collect(),
    )
}

fn compact_decorative_horizontal_rules(lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    // Layout-table-heavy marketing mail can turn every cell boundary into a
    // full-width horizontal rule. A real data table has corners/junctions and
    // vertical structure, while this chrome is usually just repeated runs of
    // horizontal glyphs. Leave occasional <hr>-style rules alone, but when a
    // message contains a cluster of them, strip only the pure horizontal rows.
    let rule_count = lines
        .iter()
        .filter(|line| line_is_pure_horizontal_rule(line))
        .count();
    if rule_count < 4 {
        return lines;
    }

    clean_rich_lines(
        lines
            .into_iter()
            .filter(|line| !line_is_pure_horizontal_rule(line))
            .collect(),
    )
}

fn clean_heading_markers(mut lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    for line in &mut lines {
        // Most rows are not headings; don't allocate a concatenated copy just
        // to discover that their first visible character is not '#'.
        if !line
            .spans
            .iter()
            .flat_map(|span| span.text.chars())
            .find(|ch| !ch.is_whitespace())
            .is_some_and(|ch| ch == '#')
        {
            continue;
        }
        let text = html_line_text(line);
        let trimmed = text.trim_start();
        let marker_len = trimmed.chars().take_while(|ch| *ch == '#').count();
        if !(1..=6).contains(&marker_len)
            || !trimmed
                .chars()
                .nth(marker_len)
                .is_some_and(char::is_whitespace)
        {
            continue;
        }

        let leading = text.chars().count().saturating_sub(trimmed.chars().count());
        remove_visible_prefix(line, leading + marker_len + 1);
        for span in &mut line.spans {
            if !span.text.trim().is_empty() {
                span.style.bold = true;
            }
        }
    }
    clean_rich_lines(lines)
}

fn remove_visible_prefix(line: &mut HtmlLine, mut count: usize) {
    for span in &mut line.spans {
        if count == 0 {
            break;
        }
        let span_len = span.text.chars().count();
        if span_len <= count {
            span.text.clear();
            count -= span_len;
        } else {
            span.text = span.text.chars().skip(count).collect();
            count = 0;
        }
    }
    line.spans.retain(|span| !span.text.is_empty());
}

fn html_line_is_blank(line: &HtmlLine) -> bool {
    line.spans.iter().all(|span| span.text.trim().is_empty())
}

fn simplify_layout_table_blocks(lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    // Layout blocks used to be analyzed as slices and then cloned *again*
    // into the output, including every styled span and URL in ordinary mail.
    // Keep ownership of each line from the original html2text rendering.
    let mut output = Vec::with_capacity(lines.len());
    let mut remaining = lines.into_iter().peekable();
    while let Some(line) = remaining.next() {
        if html_line_is_blank(&line) || remaining.peek().is_none_or(html_line_is_blank) {
            // Single-line prose can never be a layout-table block.
            output.push(line);
            continue;
        }
        let mut block = vec![line];
        while remaining
            .peek()
            .is_some_and(|next| !html_line_is_blank(next))
        {
            if let Some(next) = remaining.next() {
                block.push(next);
            }
        }
        let separator_rows = block
            .iter()
            .filter(|line| line_is_separator_or_blank(line) && !html_line_is_blank(line))
            .count();
        let empty_cell_rows = block
            .iter()
            .filter(|line| {
                let text = html_line_text(line);
                line_has_table_chrome(&text)
                    && !text.trim().is_empty()
                    && text
                        .chars()
                        .filter(|ch| !ch.is_whitespace() && !is_table_chrome_char(*ch))
                        .count()
                        == 0
                    && text.chars().any(|ch| matches!(ch, '│' | '║'))
            })
            .count();
        let nested_rows = block
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .flat_map(|span| span.text.chars())
                    .filter(|ch| matches!(ch, '┬' | '┴' | '┼' | '│'))
                    .count()
                    >= 5
            })
            .count();

        if block.len() >= 5 && separator_rows >= 3 && (empty_cell_rows >= 2 || nested_rows >= 2) {
            for line in &block {
                let flattened = flatten_layout_line(line);
                if !html_line_is_blank(&flattened) {
                    output.push(flattened);
                }
            }
            output.push(HtmlLine::default());
        } else {
            output.extend(block);
        }
    }
    clean_rich_lines(output)
}

fn flatten_layout_line(line: &HtmlLine) -> HtmlLine {
    if line_is_separator_or_blank(line) {
        return HtmlLine::default();
    }
    let mut spans = line.spans.clone();
    for span in &mut spans {
        let mut out = String::with_capacity(span.text.len());
        let mut previous_space = false;
        for ch in span.text.chars() {
            if is_table_chrome_char(ch) {
                if !previous_space {
                    out.push(' ');
                    previous_space = true;
                }
            } else {
                out.push(ch);
                previous_space = ch.is_whitespace();
            }
        }
        span.text = out;
    }
    let mut flattened = HtmlLine { spans };
    trim_rich_line(&mut flattened);
    flattened
}

fn trim_rich_line(line: &mut HtmlLine) {
    while line
        .spans
        .first()
        .is_some_and(|span| span.text.trim().is_empty())
    {
        line.spans.remove(0);
    }
    while line
        .spans
        .last()
        .is_some_and(|span| span.text.trim().is_empty())
    {
        line.spans.pop();
    }
    if let Some(first) = line.spans.first_mut()
        && first.text.len() != first.text.trim_start().len()
    {
        first.text = first.text.trim_start().to_owned();
    }
    if let Some(last) = line.spans.last_mut()
        && last.text.len() != last.text.trim_end().len()
    {
        last.text = last.text.trim_end().to_owned();
    }
}

fn rich_style(annotations: &[html2text::render::RichAnnotation]) -> HtmlSpanStyle {
    use html2text::render::RichAnnotation;

    let mut style = HtmlSpanStyle::default();
    for annotation in annotations {
        match annotation {
            RichAnnotation::Link(url) => style.link = Some(url.clone()),
            RichAnnotation::Image(src) => style.image = Some(src.clone()),
            RichAnnotation::Emphasis => style.italic = true,
            RichAnnotation::Strong => style.bold = true,
            RichAnnotation::Strikeout => style.strikeout = true,
            RichAnnotation::Code | RichAnnotation::Preformat(_) => style.code = true,
            RichAnnotation::Colour(colour) => {
                style.foreground = Some((colour.r, colour.g, colour.b));
            }
            RichAnnotation::BgColour(colour) => {
                style.background = Some((colour.r, colour.g, colour.b));
            }
            RichAnnotation::Default => {}
            _ => {}
        }
    }
    style
}

fn clean_rich_lines(mut lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    // This cleanup is applied after several layout passes. Retain in place
    // instead of allocating another Vec and copying all the line headers.
    let mut blank_run = 0usize;
    lines.retain_mut(|line| {
        for span in &mut line.spans {
            // Preserve already-normalized String buffers across cleanup passes.
            if span.text.chars().any(needs_html_text_cleanup) {
                span.text = clean_html_text(&span.text);
            }
        }
        line.spans.retain(|span| !span.text.is_empty());
        while line
            .spans
            .last()
            .is_some_and(|span| span.text.trim().is_empty())
        {
            line.spans.pop();
        }
        let blank = html_line_is_blank(line);
        if blank {
            blank_run += 1;
            blank_run == 1
        } else {
            blank_run = 0;
            true
        }
    });
    while lines.last().is_some_and(html_line_is_blank) {
        lines.pop();
    }
    lines
}

fn compact_social_link_chrome(lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    let lines = compact_inline_social_rows(lines);
    let social = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            if !line.spans.iter().any(|span| span.style.link.is_some()) {
                return None;
            }
            let label = html_line_text(line);
            let label = label.trim();
            if !is_social_link_label(label) {
                return None;
            }
            let style = line
                .spans
                .iter()
                .find(|span| span.style.link.is_some())
                .map(|span| span.style.clone())
                .unwrap_or_default();
            Some((index, label.to_owned(), style))
        })
        .collect::<Vec<_>>();

    // A lone "Website" or "X" can be meaningful content. Only compact when the
    // message contains an obvious social-footer cluster.
    if social.len() < 3 {
        return lines;
    }

    let mut remove = vec![false; lines.len()];
    for (index, _, _) in &social {
        remove[*index] = true;
        if *index > 0 && line_is_separator_or_blank(&lines[*index - 1]) {
            remove[*index - 1] = true;
        }
        if *index + 1 < lines.len() && line_is_separator_or_blank(&lines[*index + 1]) {
            remove[*index + 1] = true;
        }
    }

    let insert_at = social[0].0;
    let social_links = social
        .iter()
        .map(|(_, label, style)| (label.clone(), style.clone()))
        .collect::<Vec<_>>();
    let compact_line = social_link_line(&social_links);

    let mut output = Vec::with_capacity(lines.len());
    let mut compact_line = Some(compact_line);
    for (index, line) in lines.into_iter().enumerate() {
        if index == insert_at
            && let Some(compact) = compact_line.take()
        {
            output.push(compact);
        }
        if !remove[index] {
            output.push(line);
        }
    }
    clean_rich_lines(output)
}

fn compact_inline_social_rows(lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    let mut replacements = Vec::<(usize, HtmlLine)>::new();
    let mut remove = vec![false; lines.len()];

    for (index, line) in lines.iter().enumerate() {
        let Some(social) = social_links_from_chrome_row(line) else {
            continue;
        };
        replacements.push((index, social_link_line(&social)));
        remove[index] = true;
        if index > 0 && line_is_separator_or_blank(&lines[index - 1]) {
            remove[index - 1] = true;
        }
        if index + 1 < lines.len() && line_is_separator_or_blank(&lines[index + 1]) {
            remove[index + 1] = true;
        }
    }

    if replacements.is_empty() {
        return lines;
    }

    let mut output = Vec::with_capacity(lines.len());
    let mut replacements = replacements.into_iter().peekable();
    for (index, line) in lines.into_iter().enumerate() {
        if replacements.peek().is_some_and(|(at, _)| *at == index)
            && let Some((_, replacement)) = replacements.next()
        {
            output.push(replacement);
        }
        if !remove[index] {
            output.push(line);
        }
    }
    clean_rich_lines(output)
}

fn social_links_from_chrome_row(line: &HtmlLine) -> Option<Vec<(String, HtmlSpanStyle)>> {
    let mut social = Vec::new();
    for span in &line.spans {
        let label = span.text.trim();
        if label.is_empty() || label.chars().all(is_table_chrome_char) {
            continue;
        }
        if span.style.link.is_some() && is_social_link_label(label) {
            social.push((label.to_owned(), span.style.clone()));
            continue;
        }
        return None;
    }
    (social.len() >= 3).then_some(social)
}

fn social_link_line(social: &[(String, HtmlSpanStyle)]) -> HtmlLine {
    let mut spans = vec![HtmlSpan {
        text: "[links: ".to_owned(),
        style: HtmlSpanStyle::default(),
    }];
    for (position, (label, style)) in social.iter().enumerate() {
        if position > 0 {
            spans.push(HtmlSpan {
                text: " · ".to_owned(),
                style: HtmlSpanStyle::default(),
            });
        }
        spans.push(HtmlSpan {
            text: label.clone(),
            style: style.clone(),
        });
    }
    spans.push(HtmlSpan {
        text: "]".to_owned(),
        style: HtmlSpanStyle::default(),
    });
    HtmlLine { spans }
}

fn html_line_text(line: &HtmlLine) -> String {
    line.spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>()
}

fn line_is_separator_or_blank(line: &HtmlLine) -> bool {
    let text = html_line_text(line);
    let trimmed = text.trim();
    trimmed.is_empty()
        || trimmed.chars().all(|ch| {
            matches!(
                ch,
                '-' | '─'
                    | '━'
                    | '│'
                    | '┌'
                    | '┐'
                    | '└'
                    | '┘'
                    | '├'
                    | '┤'
                    | '┬'
                    | '┴'
                    | '┼'
                    | '═'
                    | '║'
                    | '╔'
                    | '╗'
                    | '╚'
                    | '╝'
                    | '╠'
                    | '╣'
                    | '╦'
                    | '╩'
                    | '╬'
            )
        })
}

fn line_has_table_chrome(text: &str) -> bool {
    text.chars().any(is_table_chrome_char)
}

fn is_table_chrome_char(ch: char) -> bool {
    matches!(
        ch,
        '─' | '━'
            | '│'
            | '┌'
            | '┐'
            | '└'
            | '┘'
            | '├'
            | '┤'
            | '┬'
            | '┴'
            | '┼'
            | '═'
            | '║'
            | '╔'
            | '╗'
            | '╚'
            | '╝'
            | '╠'
            | '╣'
            | '╦'
            | '╩'
            | '╬'
            | '╌'
            | '╍'
    )
}

fn line_is_generic_image_alt(line: &HtmlLine) -> bool {
    let mut saw_visible_image = false;
    for span in &line.spans {
        let label = span.text.trim();
        if label.is_empty() {
            continue;
        }
        if span.style.image.is_none() || !is_generic_image_alt(label) {
            return false;
        }
        saw_visible_image = true;
    }
    saw_visible_image
}

fn is_generic_image_alt(label: &str) -> bool {
    let label = label
        .trim()
        .trim_matches(|ch| matches!(ch, '[' | ']' | '(' | ')'))
        .trim()
        .to_ascii_lowercase();
    matches!(
        label.as_str(),
        "image"
            | "item image"
            | "dynamic banner"
            | "banner image"
            | "spacer"
            | "spacer image"
            | "tracking pixel"
            | "pixel"
            | "blank image"
    )
}

fn line_is_bare_list_marker(line: &HtmlLine) -> bool {
    matches!(html_line_text(line).trim(), "*" | "•" | "◦" | "▪" | "‣")
}

fn line_is_pure_horizontal_rule(line: &HtmlLine) -> bool {
    let text = html_line_text(line);
    let trimmed = text.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '-' | '─' | '━' | '═' | '╌' | '╍'))
}

fn is_social_link_label(label: &str) -> bool {
    matches!(
        label.trim().to_ascii_lowercase().as_str(),
        "facebook"
            | "x"
            | "twitter"
            | "instagram"
            | "youtube"
            | "tiktok"
            | "linkedin"
            | "pinterest"
            | "threads"
            | "discord"
            | "website"
    )
}

/// Merge text with exactly the same appearance and link semantics in place.
/// Never merge image annotations: two adjacent images with the same source
/// must remain two distinct image positions in the rendered line. Box-drawing
/// text is also kept separate: the reader colors it based on its contents,
/// even if its stored HtmlSpanStyle is the same as adjacent prose.
fn coalesce_rich_spans(lines: &mut [HtmlLine]) {
    for line in lines {
        let mut previous = 0;
        let has_box_drawing = |text: &str| {
            text.chars()
                .any(|ch| ('\u{2500}'..='\u{257f}').contains(&ch))
        };
        let mut previous_has_box = line
            .spans
            .first()
            .is_some_and(|span| has_box_drawing(&span.text));
        for current in 1..line.spans.len() {
            let current_has_box = has_box_drawing(&line.spans[current].text);
            let merge = line.spans[previous].style == line.spans[current].style
                && line.spans[previous].style.image.is_none()
                && !previous_has_box
                && !current_has_box;
            if merge {
                // No second Vec and no cloned style/link strings. String
                // growth is amortized and the discarded spans are dropped.
                let text = std::mem::take(&mut line.spans[current].text);
                line.spans[previous].text.push_str(&text);
            } else {
                previous += 1;
                line.spans.swap(previous, current);
                previous_has_box = current_has_box;
            }
        }
        if !line.spans.is_empty() {
            line.spans.truncate(previous + 1);
            // Truncation drops span strings, but leaves the original span
            // vector's capacity allocated. Reclaim it only when the reduction
            // is substantial, avoiding churn for small rich-text lines.
            if line.spans.capacity() >= 16 && line.spans.len() <= line.spans.capacity() / 2 {
                line.spans.shrink_to_fit();
            }
        }
    }
}

fn rich_lines_to_text(lines: &[HtmlLine]) -> String {
    let mut out = String::new();
    for (line_index, line) in lines.iter().enumerate() {
        if line_index > 0 {
            out.push('\n');
        }
        for span in &line.spans {
            out.push_str(&span.text);
        }
    }
    out
}

fn needs_html_text_cleanup(ch: char) -> bool {
    matches!(
        ch,
        '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}' | '\u{00a0}'
    )
}

fn clean_html_text(text: &str) -> String {
    text.chars()
        .filter_map(|ch| match ch {
            // Marketing/design mail often contains long preheader runs of
            // zero-width characters. They are useful in graphical clients but
            // turn into giant blank/indented regions in a terminal reader.
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}' => None,
            '\u{00a0}' => Some(' '),
            _ => Some(ch),
        })
        .collect()
}

fn clean_reader_text(text: &str) -> String {
    let text = clean_html_text(text);
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        let blank = line.trim().is_empty();
        if blank {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }

    out.trim().to_owned()
}

pub fn read_all_limited(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .context("reading message")?;
    if out.len() > limit {
        anyhow::bail!(
            "message exceeds the {} MiB in-memory limit",
            limit / (1024 * 1024)
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
