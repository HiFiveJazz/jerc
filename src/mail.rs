use std::io::{BufRead, BufReader, Read};

use anyhow::{Context, Result, anyhow};
use mail_parser::{MessageParser, PartKind};

use crate::model::{MessageBody, MessageFlags, MessageId, MessageSummary};

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

    let html_text = message.html_body().find_map(|part| match part.kind() {
        PartKind::Html => part.text().map(|html| render_html_for_terminal(html.as_ref())),
        _ => None,
    });

    Ok(MessageBody {
        id,
        plain_text,
        html_text,
        attachment_count: message.attachments().count(),
    })
}

#[derive(Clone, Debug)]
struct MailReaderDecorator;

impl html2text::render::TextDecorator for MailReaderDecorator {
    type Annotation = ();

    fn decorate_link_start(&mut self, _url: &str) -> (String, Self::Annotation) {
        (String::new(), ())
    }

    fn decorate_link_end(&mut self) -> String {
        String::new()
    }

    fn decorate_em_start(&self) -> (String, Self::Annotation) {
        (String::new(), ())
    }

    fn decorate_em_end(&self) -> String {
        String::new()
    }

    fn decorate_strong_start(&self) -> (String, Self::Annotation) {
        (String::new(), ())
    }

    fn decorate_strong_end(&self) -> String {
        String::new()
    }

    fn decorate_strikeout_start(&self) -> (String, Self::Annotation) {
        (String::new(), ())
    }

    fn decorate_strikeout_end(&self) -> String {
        String::new()
    }

    fn decorate_code_start(&self) -> (String, Self::Annotation) {
        (String::new(), ())
    }

    fn decorate_code_end(&self) -> String {
        String::new()
    }

    fn decorate_preformat_first(&self) -> Self::Annotation {}

    fn decorate_preformat_cont(&self) -> Self::Annotation {}

    fn decorate_image(
        &mut self,
        _src: &str,
        _title: &str,
    ) -> (String, Self::Annotation) {
        // Layout-heavy email frequently uses logos, avatars and tracking pixels.
        // Suppress them in reader mode instead of turning alt text into noisy rows.
        (String::new(), ())
    }

    fn header_prefix(&self, _level: usize) -> String {
        String::new()
    }

    fn quote_prefix(&self) -> String {
        "> ".to_owned()
    }

    fn unordered_item_prefix(&self) -> String {
        "• ".to_owned()
    }

    fn ordered_item_prefix(&self, index: i64) -> String {
        format!("{index}. ")
    }

    fn decorate_superscript_start(&self) -> (String, Self::Annotation) {
        (String::new(), ())
    }

    fn decorate_superscript_end(&self) -> String {
        String::new()
    }

    fn make_subblock_decorator(&self) -> Self {
        Self
    }
}

fn render_html_for_terminal(html: &str) -> String {
    // Email HTML is usually a pile of layout tables. The default html2text
    // configuration tries to preserve those tables and emits numbered link
    // footnotes, which makes notification mail dramatically noisier than the
    // message a human sees. Reader mode intentionally flattens table cells,
    // suppresses URL footnote dumps, and leaves only the visible content.
    let rendered = html2text::config::with_decorator(MailReaderDecorator)
        .raw_mode(true)
        .no_table_borders()
        .link_footnotes(false)
        .string_from_read(html.as_bytes(), 100)
        .ok()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| mail_parser::html_to_text(html));
    clean_reader_text(&rendered)
}

fn clean_reader_text(text: &str) -> String {
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
    }

    #[test]
    fn html_only_mail_uses_terminal_html_renderer() {
        let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><h1>Hello</h1><p>World</p></body></html>";
        let body = parse_body(MessageId("html".into()), raw).unwrap();
        assert!(body.plain_text.is_none());
        assert!(body.html_text.as_deref().is_some_and(|text| text.contains("Hello")));
    }

    #[test]
    fn html_reader_mode_does_not_dump_tracking_links_or_table_borders() {
        let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<table><tr><td><a href=\"https://tracking.example/very/long/url\">View in Figma</a></td></tr><tr><td>Comment text</td></tr></table>";
        let body = parse_body(MessageId("html-reader".into()), raw).unwrap();
        let rendered = body.html_text.unwrap();
        assert!(rendered.contains("View in Figma"));
        assert!(rendered.contains("Comment text"));
        assert!(!rendered.contains("[View in Figma]"));
        assert!(!rendered.contains("[1]:"));
        assert!(!rendered.contains("tracking.example"));
        assert!(!rendered.contains('│'));
        assert!(!rendered.contains('─'));
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
