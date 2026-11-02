use std::io::{BufRead, BufReader, Read};

use anyhow::{Context, Result, anyhow};
use mail_parser::{MessageParser, PartKind};

use crate::model::{HtmlLine, HtmlSpan, HtmlSpanStyle, MessageBody, MessageFlags, MessageId, MessageSummary};

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
        subject,
        timestamp,
        date_label,
        size,
        flags,
    })
}

pub fn parse_body(id: MessageId, raw: &[u8]) -> Result<MessageBody> {
    let parser = MessageParser::new();
    let message = parser
        .parse(raw)
        .ok_or_else(|| anyhow!("unable to parse RFC 5322 message"))?;

    // mail-parser's RFC 8621 body lists intentionally include synthesized
    // alternatives: an HTML part may stand in for missing text/plain, and a
    // text part may stand in for missing text/html. Inspect PartKind so the UI
    // only exposes `h` when the sender actually supplied both alternatives.
    let plain_text = message.text_body().find_map(|part| match part.kind() {
        PartKind::Text => part.text().map(|text| text.into_owned()),
        _ => None,
    });

    let rendered_html = message.html_body().find_map(|part| match part.kind() {
        PartKind::Html => part
            .text()
            .map(|html| render_html_for_terminal(html.as_ref())),
        _ => None,
    });
    let (html_text, html_lines, html_renderer) = rendered_html
        .map(|(text, lines, renderer)| (Some(text), Some(lines), Some(renderer)))
        .unwrap_or((None, None, None));

    Ok(MessageBody {
        id,
        plain_text,
        html_text,
        html_lines,
        html_renderer,
        attachment_count: message.attachments().count(),
    })
}

fn render_html_for_terminal(html: &str) -> (String, Vec<HtmlLine>, String) {
    // html2text's rich renderer is intentionally closer to a text browser than
    // its String-only API: tables keep their box drawing and semantic markup
    // such as links/strong/emphasis remains tagged. Ratatui maps those tags to
    // the active aerc-rs theme, which gives HTML mail the same useful visual
    // landmarks as a terminal browser without spawning an external process for
    // every message. Rendering happens once on the account worker, never in the
    // draw loop, so rich mail cannot make scrolling stutter.
    let rich = html2text::config::rich()
        .use_doc_css()
        .link_footnotes(false)
        .no_link_wrapping()
        .lines_from_read(html.as_bytes(), 100);

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

fn clean_rich_lines(lines: Vec<HtmlLine>) -> Vec<HtmlLine> {
    let mut cleaned = Vec::with_capacity(lines.len());
    let mut blank_run = 0usize;
    for mut line in lines {
        for span in &mut line.spans {
            span.text = clean_html_text(&span.text);
        }
        line.spans.retain(|span| !span.text.is_empty());
        while line
            .spans
            .last()
            .is_some_and(|span| span.text.chars().all(char::is_whitespace))
        {
            line.spans.pop();
        }
        let blank = line.spans.iter().all(|span| span.text.trim().is_empty());
        if blank {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        cleaned.push(line);
    }
    while cleaned
        .last()
        .is_some_and(|line| line.spans.iter().all(|span| span.text.trim().is_empty()))
    {
        cleaned.pop();
    }
    cleaned
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
    use super::{extract_thread_headers, parse_body, parse_summary, read_header_block};
    use crate::model::{MessageFlags, MessageId};
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
