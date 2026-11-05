use std::borrow::Cow;
use std::io::{BufRead, BufReader, Read};

use anyhow::{Context, Result, anyhow};
use mail_parser::{MessageParser, PartKind};

use crate::model::{HtmlLine, HtmlSpan, HtmlSpanStyle, MessageBody, MessageFlags, MessageId, MessageSummary, OutgoingAttachment};

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
            return Err(anyhow!("message header block exceeds {MAX_HEADER_BYTES} bytes"));
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
            $value.map(|value| value.mailboxes().filter_map(|mailbox| {
                let address = mailbox.address()?.trim();
                if address.is_empty() { return None; }
                let name = mailbox.name().unwrap_or_default().trim();
                Some(if name.is_empty() { address.to_owned() }
                    else { format!("{name} <{address}>") })
            }).collect::<Vec<_>>().join(", ")).unwrap_or_default()
        };
    }
    let to = render_addresses!(message.to());
    let cc = render_addresses!(message.cc());
    let bcc = render_addresses!(message.bcc());

    let subject = message.subject().unwrap_or("(no subject)").trim().to_owned();
    let (timestamp, date_label) = message.date().map_or((0, String::new()), |date| {
        let rendered = date.to_rfc3339();
        let label = rendered.get(5..10).unwrap_or(&rendered).to_owned();
        (date.to_timestamp(), label)
    });
    let thread = extract_thread_headers(raw_headers);

    Ok(MessageSummary {
        id,
        message_id: thread.message_id,
        in_reply_to: thread.in_reply_to,
        references: thread.references,
        from_name,
        from_address,
        to,
        cc,
        bcc,
        subject,
        timestamp,
        date_label,
        size,
        flags,
        has_attachment_hint: attachment_hint(raw_headers),
        attachment_names_hint: Vec::new(),
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
        return part.attachment_name().is_some_and(|name| !name.trim().is_empty())
            || part.content_disposition().is_some_and(|disposition| disposition.is_attachment());
    }
    true
}

/// Some calendar providers include identical invitation bytes twice: an unnamed
/// `text/calendar` alternative and a named `invite.ics` attachment. Show the
/// named file only, but preserve unique or explicitly attached calendar parts.
fn attachment_parts<'msg>(message: &'msg mail_parser::Message<'_>) -> Vec<mail_parser::MessagePart<'msg>> {
    let parts: Vec<_> = message.attachments()
        .filter(|part| visible_mime_attachment(part)).collect();
    parts.iter().filter(|part| {
        if !part.is_content_type("text", "calendar")
            || part.attachment_name().is_some_and(|name| !name.trim().is_empty())
            || part.content_disposition().is_some_and(|disposition| disposition.is_attachment())
        {
            return true;
        }
        !parts.iter().any(|named| {
            named.attachment_name().is_some_and(|name| name.to_ascii_lowercase().ends_with(".ics"))
                && named.decoded().as_ref() == part.decoded().as_ref()
        })
    }).copied().collect()
}

fn attachment_display_name(part: &mail_parser::MessagePart<'_>, index: usize) -> String {
    let raw_name = part.attachment_name().unwrap_or("");
    let name = raw_name.rsplit(|ch| ch == '/' || ch == '\\').next().unwrap_or("").trim();
    if name.is_empty() || name == "." || name == ".." {
        if part.is_content_type("text", "calendar") { "invite.ics".into() }
        else { format!("attachment-{}", index + 1) }
    } else { name.to_owned() }
}

/// Cached IMAP summary hints predate the calendar MIME deduplication fix.
/// Until the message body is fetched, hide a single generic placeholder when
/// a named calendar invite is also advertised. The decoded MIME parts remain
/// authoritative: distinct invitations are still available after opening.
pub fn visible_attachment_hints(hints: &[String]) -> Vec<String> {
    let has_ics = hints.iter().any(|name| name.to_ascii_lowercase().ends_with(".ics"));
    let generic_count = hints.iter().filter(|name| {
        name.strip_prefix("attachment-").is_some_and(|suffix|
            !suffix.is_empty() && suffix.bytes().all(|ch| ch.is_ascii_digit()))
    }).count();
    // Only the exact legacy two-chip case is unambiguous enough to
    // collapse without decoded MIME bytes. Keep larger/mixed lists intact.
    if !has_ics || generic_count != 1 || hints.len() != 2 { return hints.to_vec(); }
    hints.iter().filter(|name| !name.strip_prefix("attachment-").is_some_and(|suffix|
        !suffix.is_empty() && suffix.bytes().all(|ch| ch.is_ascii_digit())))
        .cloned().collect()
}

pub fn extract_attachments(raw: &[u8]) -> Result<Vec<OutgoingAttachment>> {
    let parsed = MessageParser::new().parse(raw)
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
    if bytes.starts_with(b"%PDF-") { return "application/pdf"; }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return "image/png"; }
    if bytes.starts_with(b"\xff\xd8\xff") { return "image/jpeg"; }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") { return "image/gif"; }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]) { return "image/webp"; }
    match name.rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase()).as_deref() {
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

    let attachment_names: Vec<String> = attachment_parts(&message).into_iter()
        .enumerate().map(|(index, part)| attachment_display_name(&part, index)).collect();
    Ok(MessageBody {
        id,
        plain_text,
        plain_fallback,
        html_source,
        html_text,
        html_lines,
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
    // the active aerc-rs theme, which gives HTML mail the same useful visual
    // landmarks as a terminal browser without spawning an external process for
    // every message. The worker produces a fallback layout; reader mode can
    // reflow the preserved HTML when its actual viewport width changes.
    let normalized_html = flatten_presentation_tables_html(html);
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
        let converted = compact_social_link_chrome(converted);
        let text = rich_lines_to_text(&converted);
        if !text.trim().is_empty() {
            return (text, converted, "html2text-rich".to_owned());
        }
    }

    // Malformed HTML should still be readable. mail-parser's fallback is plain
    // text, so expose it as one unstyled span per line rather than failing the
    // whole message body.
    let text = clean_reader_text(&mail_parser::html_to_text(html));
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

fn flatten_presentation_tables_html(html: &str) -> Cow<'_, str> {
    // Most messages have no role=presentation tables. Passing borrowed HTML
    // to html2text avoids allocating/copying the entire MIME part on every
    // render and reflow. The ASCII-insensitive precheck cannot miss a valid
    // presentation attribute, including one with mixed-case spelling.
    if !html.as_bytes().windows(b"presentation".len())
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
            ["tbody", "thead", "tfoot"].iter().any(|part| name.eq_ignore_ascii_case(part))
        });
        let is_cell_or_row = name.is_some_and(|name| {
            ["tr", "td", "th"].iter().any(|part| name.eq_ignore_ascii_case(part))
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
    if modified { Cow::Owned(out) } else { Cow::Borrowed(html) }
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
    let inner = tag.trim_start_matches('<').trim_end_matches('>').trim_start();
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
    while let Some(relative) = bytes[offset..].windows(4)
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
        let quote = bytes.get(cursor).copied().filter(|byte| matches!(*byte, b'\'' | b'"'));
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
        if line.spans.iter().any(|span| span.style.code
            || span.text.chars().any(is_table_chrome_char))
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
        if !line.spans.iter().flat_map(|span| span.text.chars())
            .find(|ch| !ch.is_whitespace()).is_some_and(|ch| ch == '#')
        {
            continue;
        }
        let text = html_line_text(line);
        let trimmed = text.trim_start();
        let marker_len = trimmed.chars().take_while(|ch| *ch == '#').count();
        if !(1..=6).contains(&marker_len)
            || !trimmed.chars().nth(marker_len).is_some_and(char::is_whitespace)
        {
            continue;
        }

        let leading = text
            .chars()
            .count()
            .saturating_sub(trimmed.chars().count());
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
        if html_line_is_blank(&line) || remaining.peek().is_none_or(|next| html_line_is_blank(next)) {
            // Single-line prose can never be a layout-table block.
            output.push(line);
            continue;
        }
        let mut block = vec![line];
        while remaining.peek().is_some_and(|next| !html_line_is_blank(next)) {
            if let Some(next) = remaining.next() {
                block.push(next);
            }
        }
        let separator_rows = block.iter()
            .filter(|line| line_is_separator_or_blank(line) && !html_line_is_blank(line))
            .count();
        let empty_cell_rows = block.iter()
            .filter(|line| {
                let text = html_line_text(line);
                line_has_table_chrome(&text)
                    && !text.trim().is_empty()
                    && text.chars().filter(|ch| !ch.is_whitespace() && !is_table_chrome_char(*ch)).count() == 0
                    && text.chars().any(|ch| matches!(ch, '│' | '║'))
            })
            .count();
        let nested_rows = block.iter()
            .filter(|line| line.spans.iter()
                .flat_map(|span| span.text.chars())
                .filter(|ch| matches!(ch, '┬' | '┴' | '┼' | '│'))
                .count() >= 5)
            .count();

        if block.len() >= 5
            && separator_rows >= 3
            && (empty_cell_rows >= 2 || nested_rows >= 2)
        {
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
        while line.spans.last().is_some_and(|span| span.text.trim().is_empty()) {
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
            if !is_social_link_label(label) { return None; }
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
        if index == insert_at {
            if let Some(compact) = compact_line.take() {
                output.push(compact);
            }
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
        if replacements.peek().is_some_and(|(at, _)| *at == index) {
            if let Some((_, replacement)) = replacements.next() {
                output.push(replacement);
            }
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
                '-' | '─' | '━' | '│' | '┌' | '┐' | '└' | '┘' | '├' | '┤'
                    | '┬' | '┴' | '┼' | '═' | '║' | '╔' | '╗' | '╚' | '╝'
                    | '╠' | '╣' | '╦' | '╩' | '╬'
            )
        })
}

fn line_has_table_chrome(text: &str) -> bool {
    text.chars().any(is_table_chrome_char)
}

fn is_table_chrome_char(ch: char) -> bool {
    matches!(
        ch,
        '─' | '━' | '│' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴'
            | '┼' | '═' | '║' | '╔' | '╗' | '╚' | '╝' | '╠' | '╣' | '╦'
            | '╩' | '╬' | '╌' | '╍'
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
    matches!(ch, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}' | '\u{00a0}')
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

pub fn read_all(mut reader: impl Read) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out).context("reading message")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        compact_bare_list_markers, compact_decorative_horizontal_rules,
        compact_generic_image_alt_lines, compact_prose_whitespace, compact_social_link_chrome,
        extract_thread_headers, parse_body, parse_summary, read_header_block,
        render_html_for_terminal, simplify_layout_table_blocks,
    };
    use crate::model::{HtmlLine, HtmlSpan, HtmlSpanStyle, MessageFlags, MessageId};
    use std::io::Cursor;

    const MAIL: &[u8] = b"From: Example Person <person@example.com>\r\n\
Subject: [PATCH v2 1/2] usb: fix thing\r\n\
Date: Thu, 22 Oct 2026 01:17:54 +0200\r\n\
Message-ID: <patch-1@example.com>\r\n\
In-Reply-To: <cover@example.com>\r\n\
References: <root@example.com>\r\n\t<cover@example.com>\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hello from the body.\r\n";

    #[test]
    fn old_cached_calendar_hints_are_collapsed_for_list_and_reader() {
        let hints = vec!["attachment-1".into(), "invite.ics".into()];
        assert_eq!(super::visible_attachment_hints(&hints), vec!["invite.ics"]);
        assert_eq!(hints.len(), 2, "do not mutate cached source data");
        assert_eq!(super::visible_attachment_hints(&["attachment-1".into()]),
            vec!["attachment-1"]);
        assert_eq!(super::visible_attachment_hints(&[
            "attachment-1".into(), "invite.ics".into(), "report.pdf".into(),
        ]).len(), 3, "never guess about larger multipart messages");
    }

    #[test]
    fn duplicate_calendar_part_shows_only_named_ics_attachment() {
        // Matches the structure of Google Calendar invitations: a calendar
        // alternate alongside the exact same bytes named invite.ics.
        let raw = b"From: sender@example.org\r\nSubject: Event\r\n\
MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n\
--outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n\
--inner\r\nContent-Type: text/plain\r\n\r\nInvitation\r\n\
--inner--\r\n\
--outer\r\nContent-Type: text/calendar\r\n\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n\
--outer\r\nContent-Type: application/ics; name=invite.ics\r\n\
Content-Disposition: attachment; filename=invite.ics\r\n\r\n\
BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n\
--outer--\r\n";
        let attachments = super::extract_attachments(raw).unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "invite.ics");
        let body = parse_body(MessageId("calendar".into()), raw).unwrap();
        assert_eq!(body.attachment_names, vec!["invite.ics"]);
    }

    #[test]
    fn standalone_calendar_part_still_appears_as_invite_ics() {
        let raw = b"From: sender@example.org\r\nSubject: Event\r\n\
MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n\
--outer\r\nContent-Type: text/plain\r\n\r\nBody\r\n\
--outer\r\nContent-Type: text/calendar\r\n\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n\
--outer--\r\n";
        let files = super::extract_attachments(raw).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "invite.ics");
    }

    #[test]
    fn header_reader_stops_before_body() {
        let headers = read_header_block(Cursor::new(MAIL)).unwrap();
        let text = String::from_utf8(headers).unwrap();
        assert!(text.contains("Subject:"));
        assert!(!text.contains("Hello from the body"));
    }

    #[test]
    fn extracts_threading_metadata() {
        let headers = read_header_block(Cursor::new(MAIL)).unwrap();
        let t = extract_thread_headers(&headers);
        assert_eq!(t.message_id.as_deref(), Some("patch-1@example.com"));
        assert_eq!(t.in_reply_to.as_deref(), Some("cover@example.com"));
        assert_eq!(t.references, ["root@example.com", "cover@example.com"]);
    }

    #[test]
    fn plain_only_mail_does_not_invent_an_html_toggle() {
        let body = parse_body(MessageId("plain".into()), MAIL).unwrap();
        assert!(body.plain_text.is_some());
        assert!(body.html_text.is_none());
        assert!(body.html_lines.is_none());
    }

    #[test]
    fn html_only_mail_uses_terminal_html_renderer() {
        let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><h1>Hello</h1><p>World</p></body></html>";
        let body = parse_body(MessageId("html".into()), raw).unwrap();
        assert!(body.plain_text.is_none());
        assert!(body.html_text.as_deref().is_some_and(|text| text.contains("Hello")));
        assert!(body.html_lines.as_ref().is_some_and(|lines| !lines.is_empty()));
    }

    #[test]
    fn html_reader_mode_preserves_table_structure_and_links() {
        let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<table><tr><td><a href=\"https://tracking.example/very/long/url\">View in Figma</a></td><td><strong>Open</strong></td></tr><tr><td>Comment text</td><td>More</td></tr></table>";
        let body = parse_body(MessageId("html-reader".into()), raw).unwrap();
        let rendered = body.html_text.as_deref().unwrap();
        assert!(rendered.contains("View in Figma"));
        assert!(rendered.contains("Comment text"));
        assert!(rendered.chars().any(|ch| matches!(ch, '│' | '─' | '┌' | '┐')));
        assert_eq!(
            body.html_links(),
            vec!["https://tracking.example/very/long/url".to_owned()]
        );
        assert_eq!(body.html_renderer.as_deref(), Some("html2text-rich"));
    }


    #[test]
    fn html_reader_preserves_supported_css_colours() {
        let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p style=\"color:#ff0000;background-color:#0000ff\"><a href=\"https://example.com\">Colour link</a></p>";
        let body = parse_body(MessageId("html-colour".into()), raw).unwrap();
        let styles = body
            .html_lines
            .as_ref()
            .into_iter()
            .flatten()
            .flat_map(|line| line.spans.iter())
            .map(|span| &span.style)
            .collect::<Vec<_>>();
        assert!(styles.iter().any(|style| style.foreground == Some((255, 0, 0))));
        assert!(styles.iter().any(|style| style.background == Some((0, 0, 255))));
        assert!(styles.iter().any(|style| style.link.as_deref() == Some("https://example.com")));
    }

    #[test]
    fn html_reader_strips_zero_width_preheader_padding() {
        let raw = "From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>\u{200c}\u{200c}Visible</p>";
        let body = parse_body(MessageId("html-zero-width".into()), raw.as_bytes()).unwrap();
        assert_eq!(body.html_text.as_deref().map(str::trim), Some("Visible"));
    }

    #[test]
    fn html_reader_mode_collapses_excess_blank_lines() {
        let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>One</p><br><br><br><p>Two</p>";
        let body = parse_body(MessageId("html-space".into()), raw).unwrap();
        let rendered = body.html_text.unwrap();
        assert!(!rendered.contains("\n\n\n"));
    }

    #[test]
    fn html_reader_removes_repeated_layout_table_rules_but_keeps_real_table_chrome() {
        let line = |text: &str| HtmlLine {
            spans: vec![HtmlSpan {
                text: text.to_owned(),
                style: HtmlSpanStyle::default(),
            }],
        };
        let compacted = compact_decorative_horizontal_rules(vec![
            line("────────────────"),
            line("Hello"),
            line("────────────────"),
            line("World"),
            line("────────────────"),
            line("┌────┬────┐"),
            line("│ A  │ B  │"),
            line("────────────────"),
        ]);
        let text = compacted
            .iter()
            .map(super::html_line_text)
            .collect::<Vec<_>>();
        assert!(!text.iter().any(|line| line == "────────────────"));
        assert!(text.iter().any(|line| line == "┌────┬────┐"));
        assert!(text.iter().any(|line| line == "│ A  │ B  │"));
    }

    #[test]
    fn social_footer_links_compact_without_losing_targets() {
        let social = ["Facebook", "Instagram", "YouTube"]
            .into_iter()
            .map(|label| HtmlLine {
                spans: vec![HtmlSpan {
                    text: label.to_owned(),
                    style: HtmlSpanStyle {
                        link: Some(format!("https://example.com/{label}")),
                        ..HtmlSpanStyle::default()
                    },
                }],
            })
            .collect::<Vec<_>>();
        let compacted = compact_social_link_chrome(social);
        assert_eq!(compacted.len(), 1);
        let text = compacted[0]
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert_eq!(text, "[links: Facebook · Instagram · YouTube]");
        assert_eq!(
            compacted[0]
                .spans
                .iter()
                .filter(|span| span.style.link.is_some())
                .count(),
            3
        );
    }

    #[test]
    fn horizontal_social_footer_table_compacts_without_losing_targets() {
        let separator = |text: &str| HtmlSpan {
            text: text.to_owned(),
            style: HtmlSpanStyle::default(),
        };
        let social = |label: &str| HtmlSpan {
            text: label.to_owned(),
            style: HtmlSpanStyle {
                link: Some(format!("https://example.com/{label}")),
                image: Some(format!("https://example.com/{label}.png")),
                ..HtmlSpanStyle::default()
            },
        };
        let lines = vec![
            HtmlLine {
                spans: vec![separator("────────┬───────┬────────")],
            },
            HtmlLine {
                spans: vec![
                    social("Facebook"),
                    separator(" │ "),
                    social("X"),
                    separator(" │ "),
                    social("Instagram"),
                ],
            },
            HtmlLine {
                spans: vec![separator("────────┴───────┴────────")],
            },
        ];

        let compacted = compact_social_link_chrome(lines);
        assert_eq!(compacted.len(), 1);
        assert_eq!(
            compacted[0]
                .spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
            "[links: Facebook · X · Instagram]"
        );
        assert_eq!(
            compacted[0]
                .spans
                .iter()
                .filter(|span| span.style.link.is_some())
                .count(),
            3
        );
    }

    #[test]
    fn generic_image_alt_placeholders_are_removed_but_descriptive_images_survive() {
        let image_line = |label: &str| HtmlLine {
            spans: vec![HtmlSpan {
                text: label.to_owned(),
                style: HtmlSpanStyle {
                    image: Some("https://example.com/image.png".to_owned()),
                    ..HtmlSpanStyle::default()
                },
            }],
        };
        let compacted = compact_generic_image_alt_lines(vec![
            image_line("Dynamic Banner"),
            image_line("item image"),
            image_line("June Bluefin Tuna Cutting Ceremony"),
        ]);
        assert_eq!(compacted.len(), 1);
        assert_eq!(
            compacted[0]
                .spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
            "June Bluefin Tuna Cutting Ceremony"
        );
    }

    #[test]
    fn repeated_bare_list_markers_are_removed() {
        let line = |text: &str| HtmlLine {
            spans: vec![HtmlSpan {
                text: text.to_owned(),
                style: HtmlSpanStyle::default(),
            }],
        };
        let compacted = compact_bare_list_markers(vec![
            line("* First item"),
            line("*"),
            line("* Second item"),
            line("*"),
            line("* Third item"),
        ]);
        let text = compacted
            .iter()
            .map(super::html_line_text)
            .collect::<Vec<_>>();
        assert_eq!(text, ["* First item", "* Second item", "* Third item"]);
    }

    #[test]
    fn prose_spaces_compact_without_changing_table_alignment() {
        let line = |text: &str| HtmlLine {
            spans: vec![HtmlSpan {
                text: text.to_owned(),
                style: HtmlSpanStyle::default(),
            }],
        };
        let table = "│ A    │ B  │";
        let compacted = compact_prose_whitespace(vec![
            line("Please don't miss the   first cut!"),
            line(table),
        ]);
        assert_eq!(super::html_line_text(&compacted[0]), "Please don't miss the first cut!");
        assert_eq!(super::html_line_text(&compacted[1]), table);
    }

    #[test]
    fn html_headings_keep_emphasis_without_literal_markdown_markers() {
        let (text, lines, _) = render_html_for_terminal(
            "<h1>Your Services Agreement made clearer</h1><p>Hello</p>",
            80,
        );
        assert!(text.contains("Your Services Agreement made clearer"));
        assert!(!text.contains("# Your Services Agreement"));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.text.contains("Your Services Agreement") && span.style.bold
        }));
    }


    #[test]
    fn explicit_presentation_tables_are_linearized_before_rich_rendering() {
        let html = r#"
            <table role = 'presentation'><tr>
              <td><strong>Logo</strong></td><td>Order #580<br>$10.38</td>
            </tr></table>
            <table><tr><td>Ticket</td><td>$85.00</td></tr></table>
        "#;
        let (text, _, _) = render_html_for_terminal(html, 80);
        assert!(text.contains("Logo"));
        assert!(text.contains("Order #580"));
        // The role=presentation table is layout, not data, so it should not
        // contribute box-drawing separators. The unmarked receipt table still
        // should.
        assert_eq!(text.matches('┬').count(), 1);
        assert!(text.contains("Ticket"));
    }

    #[test]
    fn presentation_table_blocks_flatten_without_touching_small_data_tables() {
        let line = |text: &str| HtmlLine {
            spans: vec![HtmlSpan {
                text: text.to_owned(),
                style: HtmlSpanStyle::default(),
            }],
        };
        let flattened = simplify_layout_table_blocks(vec![
            line("─┬────────────────┬─"),
            line(" │Share Your Thoughts│ "),
            line(" │────────────────│ "),
            line(" │                │ "),
            line(" │────────────────│ "),
            line(" │Take Survey     │ "),
            line("─┴────────────────┴─"),
        ]);
        let text = flattened.iter().map(super::html_line_text).collect::<Vec<_>>();
        assert!(text.iter().any(|line| line.contains("Share Your Thoughts")));
        assert!(text.iter().any(|line| line.contains("Take Survey")));
        assert!(!text.iter().any(|line| line.contains('┬') || line.contains('┴')));

        let data = simplify_layout_table_blocks(vec![
            line("──────┬────────"),
            line("Ticket│$85.00"),
            line("──────┴────────"),
        ]);
        assert!(data.iter().any(|line| super::html_line_text(line).contains('┬')));
    }

    #[test]
    fn html_preprocessor_borrows_unchanged_markup_and_handles_mixed_case_roles() {
        let ordinary = "<table><tr><td>Receipt</td></tr></table>";
        let unchanged = super::flatten_presentation_tables_html(ordinary);
        assert!(matches!(&unchanged, std::borrow::Cow::Borrowed(_)));
        assert_eq!(unchanged, ordinary);

        // A word in the body must not trigger rewriting unrelated data tables.
        let ordinary = "<p>Presentation details</p><table><tr><td>$85</td></tr></table>";
        assert!(matches!(
            super::flatten_presentation_tables_html(ordinary),
            std::borrow::Cow::Borrowed(_)
        ));

        let marked = "<TABLE ROLE = 'PreSentation'><TR><TD>Logo</TD></TR></TABLE>\
                      <table><tr><td>Receipt</td></tr></table>";
        let normalized = super::flatten_presentation_tables_html(marked);
        assert!(matches!(&normalized, std::borrow::Cow::Owned(_)));
        assert!(normalized.starts_with("<div><div><div>Logo</div></div></div>"));
        assert!(normalized.contains("<table><tr><td>Receipt</td></tr></table>"));
    }

    #[test]
    fn html_cleanup_preserves_styled_links_and_collapses_whitespace_across_spans() {
        let url = "https://example.com/article";
        let linked = HtmlSpanStyle {
            link: Some(url.into()),
            bold: true,
            ..HtmlSpanStyle::default()
        };
        let lines = vec![HtmlLine { spans: vec![
            HtmlSpan { text: "Read  ".into(), style: linked.clone() },
            HtmlSpan { text: "\tmore\u{200b}".into(), style: linked.clone() },
        ] }];
        let clean = compact_prose_whitespace(super::clean_rich_lines(lines));
        assert_eq!(super::html_line_text(&clean[0]), "Read more");
        assert!(clean[0].spans.iter().all(|span| span.style == linked));
        // Multiple cleanup passes should be idempotent, including blank rows.
        assert_eq!(super::clean_rich_lines(clean.clone()), clean);
    }

    #[test]
    fn ordinary_layout_blocks_keep_their_styled_spans() {
        let first = HtmlLine { spans: vec![HtmlSpan {
            text: "Hello ".into(),
            style: HtmlSpanStyle { italic: true, ..HtmlSpanStyle::default() },
        }, HtmlSpan {
            text: "world".into(),
            style: HtmlSpanStyle { link: Some("https://example.org".into()), ..HtmlSpanStyle::default() },
        }] };
        let second = HtmlLine { spans: vec![HtmlSpan {
            text: "Next paragraph".into(), style: HtmlSpanStyle::default(),
        }] };
        let actual = simplify_layout_table_blocks(vec![
            first.clone(), HtmlLine::default(), second.clone(),
        ]);
        assert_eq!(actual, vec![first, HtmlLine::default(), second]);
    }

    #[test]
    fn html_only_mail_gets_a_plain_toggle_fallback() {
        let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<h1>Hello</h1><p>Fallback body</p>";
        let body = parse_body(MessageId("html-fallback".into()), raw).unwrap();
        assert!(body.has_html_alternative());
        assert!(body.has_plain_view());
        assert!(body.display_text(false).contains("Fallback body"));
        assert!(body.display_text(true).contains("Fallback body"));
    }

    #[test]
    fn parses_summary_and_body() {
        let headers = read_header_block(Cursor::new(MAIL)).unwrap();
        let summary = parse_summary(
            MessageId("fixture".into()),
            &headers,
            MAIL.len() as u64,
            MessageFlags::empty(),
        )
        .unwrap();
        assert_eq!(summary.subject, "[PATCH v2 1/2] usb: fix thing");
        assert_eq!(summary.from_address, "person@example.com");
        assert!(summary.unread());

        let body = parse_body(MessageId("fixture".into()), MAIL).unwrap();
        assert!(body.display_text(false).contains("Hello from the body"));
    }

    #[test]
    fn renders_html_only_mail_without_external_filters() {
        let raw = b"From: Example <e@example.com>\r\n\
Subject: HTML\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<table><tr><td>Hello</td><td>World</td></tr></table>";
        let body = parse_body(MessageId("html".into()), raw).unwrap();
        assert!(body.html_text.as_deref().is_some_and(|text| text.contains("Hello")));
        assert!(body.display_text(false).contains("Hello"));
    }

}
