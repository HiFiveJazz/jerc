//! Typed outgoing MIME construction and reply address policy. This module does
//! no network I/O, so its wire-format tests can run without a mail account.
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use lettre::Message;
use lettre::message::{
    Attachment, Mailbox, Mailboxes, MultiPart, SinglePart,
    header::{ContentType, HeaderName, HeaderValue},
};
use mail_parser::MessageParser;

use crate::model::OutgoingMessage;

pub const FORWARD_SEPARATOR: &str = "---------- Forwarded message ---------";
static NEXT_OUTGOING_ID: AtomicU64 = AtomicU64::new(0);

/// Restrict interactive outgoing recipients to fully qualified public mail domains.
/// A syntactically valid but misspelled local part cannot be detected without
/// confirming with the recipient. Never silently rewrite an address.
pub fn validate_recipients(to: &str, cc: &str, bcc: &str) -> Result<()> {
    let mut count = 0;
    for (label, value) in [("To", to), ("Cc", cc), ("Bcc", bcc)] {
        if value.trim().is_empty() {
            continue;
        }
        let mailboxes: Mailboxes = value
            .parse()
            .with_context(|| format!("invalid {label} recipient list"))?;
        for mailbox in mailboxes {
            let email = mailbox.email.to_string();
            let (_, domain) = email
                .rsplit_once('@')
                .ok_or_else(|| anyhow!("invalid {label} address {email:?}"))?;
            let labels: Vec<_> = domain.split('.').collect();
            // A single-label host can be valid on private SMTP installations,
            // but accepting it by default lets typos such as @gmail reach Send.
            if labels.len() < 2
                || labels.iter().any(|part| {
                    part.is_empty()
                        || part.starts_with('-')
                        || part.ends_with('-')
                        || !part
                            .bytes()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
                })
                || labels.last().is_some_and(|tld| {
                    tld.len() < 2 || !tld.bytes().all(|c| c.is_ascii_alphabetic())
                })
            {
                bail!(
                    "{label} recipient {email:?} has an incomplete or invalid domain (e.g. use gmail.com, not gmail)"
                );
            }
            count += 1;
        }
    }
    if count == 0 {
        bail!("outgoing email has no To, Cc, or Bcc recipient");
    }
    Ok(())
}

fn sender_message_id_domain(from: &Mailbox) -> String {
    // Keep the sender's domain, not the machine's local hostname. A single
    // generated ID must be reused for both SMTP and the Sent folder copy.
    from.email.to_string().rsplit_once('@').map_or_else(
        || "localhost".to_owned(),
        |(_, domain)| domain.to_ascii_lowercase(),
    )
}

fn new_outgoing_message_id(domain: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_OUTGOING_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "<jerc-{timestamp:x}-{:x}-{sequence:x}@{domain}>",
        std::process::id()
    )
}

/// Inline forward: preserve MIME alternatives and the
/// original attachments, rather than sending the entire mail as forwarded.eml.
#[derive(Debug, Clone)]
pub struct InlineForwardParts {
    pub plain: String,
    pub html: Option<String>,
    pub attachments: Vec<crate::model::OutgoingAttachment>,
}

pub fn inline_forward_parts(raw: &[u8]) -> Result<InlineForwardParts> {
    use mail_parser::PartKind;
    let message = MessageParser::new()
        .parse(raw)
        .ok_or_else(|| anyhow!("cannot parse original message for inline forwarding"))?;
    let plain = message.text_body().find_map(|part| match part.kind() {
        PartKind::Text => part.text().map(|text| text.into_owned()),
        _ => None,
    });
    let html = message.html_body().find_map(|part| match part.kind() {
        PartKind::Html => part.text().map(|text| text.into_owned()),
        _ => None,
    });
    let plain = plain
        .or_else(|| html.as_deref().map(mail_parser::html_to_text))
        .unwrap_or_default();
    Ok(InlineForwardParts {
        plain,
        html,
        attachments: crate::mail::extract_attachments(raw)?,
    })
}

/// Produce a Gmail-like attribution from the original RFC 5322 headers.
/// Bcc is excluded even if present in a Sent copy: forwarding it would reveal
/// private recipients to a new audience.
pub fn forward_attribution(raw: &[u8]) -> Result<String> {
    let summary = crate::mail::parse_summary(
        crate::model::MessageId("forward-attribution".into()),
        raw,
        raw.len() as u64,
        crate::model::MessageFlags::empty(),
    )?;
    let sender = if summary.from_name.is_empty() {
        summary.from_address
    } else {
        format!("{} <{}>", summary.from_name, summary.from_address)
    };
    let date = chrono::DateTime::<chrono::Utc>::from_timestamp(summary.timestamp, 0)
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%a, %b %-d, %Y at %-I:%M %p")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let mut header = format!(
        "{FORWARD_SEPARATOR}\nFrom: {sender}\nDate: {date}\nSubject: {}\n",
        summary.subject
    );
    if !summary.to.is_empty() {
        header.push_str(&format!("To: {}\n", summary.to));
    }
    if !summary.cc.is_empty() {
        header.push_str(&format!("Cc: {}\n", summary.cc));
    }
    Ok(header)
}

/// Insert forward attribution *inside* the original HTML document so that
/// CSS, inline layout, and image links are retained for recipients.
#[must_use]
pub fn forward_html(html: &str, attribution: &str, personal_note: &str) -> String {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let block = format!(
        "<div style=\"font-family:sans-serif\">{}<hr><div style=\"white-space:pre-wrap\">{}</div></div>",
        escape(personal_note),
        escape(attribution)
    );
    let lower = html.to_ascii_lowercase();
    if let Some(open) = lower.find("<body") {
        if let Some(end) = lower[open..].find('>') {
            let pos = open + end + 1;
            return format!("{}{}{}", &html[..pos], block, &html[pos..]);
        }
    }
    format!("<html><body>{block}{html}</body></html>")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyAudience {
    Sender,
    All,
    List,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRecipients {
    pub to: String,
    pub cc: String,
}

#[must_use]
pub fn reply_subject(subject: &str) -> String {
    let subject = subject.trim();
    if subject
        .get(..3)
        .is_some_and(|s| s.eq_ignore_ascii_case("re:"))
    {
        subject.to_owned()
    } else {
        format!("Re: {subject}")
    }
}

#[must_use]
pub fn forward_subject(subject: &str) -> String {
    let subject = subject.trim();
    if subject
        .get(..4)
        .is_some_and(|s| s.eq_ignore_ascii_case("fwd:"))
    {
        subject.to_owned()
    } else {
        format!("Fwd: {subject}")
    }
}

/// Route replies using actual RFC mailbox addresses, not string splitting.
/// Self addresses and duplicates are removed from Reply All. A list reply is
/// strict: a missing List-Post is an error, never a silent Reply All fallback.
pub fn reply_recipients(
    raw: &[u8],
    audience: ReplyAudience,
    own: &[String],
) -> Result<ReplyRecipients> {
    let message = MessageParser::new()
        .parse(raw)
        .ok_or_else(|| anyhow!("cannot parse original message for reply recipients"))?;
    let own: HashSet<String> = own.iter().map(|a| a.to_ascii_lowercase()).collect();
    let mut seen = HashSet::new();
    let mut to = Vec::new();
    let mut cc = Vec::new();

    if audience == ReplyAudience::List {
        let header = unfold_header(raw, "list-post")
            .ok_or_else(|| anyhow!("message has no List-Post header"))?;
        if header.trim().eq_ignore_ascii_case("no") {
            bail!("mailing list does not permit list replies");
        }
        let start = header
            .to_ascii_lowercase()
            .find("mailto:")
            .ok_or_else(|| anyhow!("List-Post does not contain a mailto URI"))?;
        let after = &header[start + "mailto:".len()..];
        let address = after
            .split(|c: char| matches!(c, '?' | '>' | ',' | ' '))
            .next()
            .unwrap_or("");
        let decoded = percent_encoding::percent_decode_str(address)
            .decode_utf8()
            .context("decoding List-Post address")?;
        let mailbox: Mailbox = decoded.parse().context("invalid List-Post address")?;
        let value = mailbox.email.to_string();
        if !seen.insert(value.to_ascii_lowercase()) {
            bail!("List-Post points to your own address");
        }
        return Ok(ReplyRecipients {
            to: value,
            cc: String::new(),
        });
    }

    // Reply-To takes precedence over From. Both can legally have many
    // mailboxes; preserve their order while discarding our own identities.
    let primary = message.reply_to().or_else(|| message.from());
    if let Some(addresses) = primary {
        for mailbox in addresses.mailboxes() {
            if let Some(address) = mailbox.address() {
                // Reply-Sender is allowed to address yourself. Reply-All
                // excludes local identities to avoid echoing the thread back
                // to the account that sent the original message.
                if audience != ReplyAudience::All || !own.contains(&address.to_ascii_lowercase()) {
                    push_unique(&mut to, &mut seen, address);
                }
            }
        }
    }
    if audience == ReplyAudience::All {
        for addresses in [message.to(), message.cc()].into_iter().flatten() {
            for mailbox in addresses.mailboxes() {
                if let Some(address) = mailbox.address() {
                    if own.contains(&address.to_ascii_lowercase()) {
                        continue;
                    }
                    if to.is_empty() {
                        push_unique(&mut to, &mut seen, address);
                    } else {
                        push_unique(&mut cc, &mut seen, address);
                    }
                }
            }
        }
    }
    if to.is_empty() {
        bail!("no valid non-self recipients for reply");
    }
    Ok(ReplyRecipients {
        to: to.join(", "),
        cc: cc.join(", "),
    })
}

fn push_unique(out: &mut Vec<String>, seen: &mut HashSet<String>, value: &str) {
    let key = value.trim().to_ascii_lowercase();
    if !key.is_empty() && seen.insert(key) {
        out.push(value.trim().to_owned());
    }
}

/// Read only the header block and unfold continuation lines. Used solely for
/// List-Post, which mail-parser does not expose as a typed convenience field.
fn unfold_header(raw: &[u8], target: &str) -> Option<String> {
    let mut found = None::<String>;
    for line in String::from_utf8_lossy(raw).lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(value) = found.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case(target) {
                found = Some(value.trim().to_owned());
            } else if found.is_some() {
                break;
            }
        }
    }
    found
}

#[must_use]
pub fn send_warnings(message: &OutgoingMessage) -> Vec<&'static str> {
    let mut result = Vec::new();
    if message.subject.trim().is_empty() {
        result.push("Subject is empty");
    }
    let mentions_attachment = message.body.to_ascii_lowercase().contains("attach");
    if mentions_attachment && message.attachments.is_empty() && message.forwarded_raw.is_none() {
        result.push("Body mentions an attachment, but none is attached");
    }
    result
}

/// Build the complete outgoing wire message without network I/O. `Message`'s
/// envelope retains Bcc while its formatted() output strips it by default.
/// No headers may be injected by arbitrary user text.
pub fn build_outgoing(message: &OutgoingMessage, default_from: &str) -> Result<Message> {
    let from_value = if message.from.trim().is_empty() {
        default_from
    } else {
        &message.from
    };
    let from: Mailbox = from_value.parse().context("invalid From mailbox")?;
    validate_recipients(&message.to, &message.cc, &message.bcc)?;
    let message_id = new_outgoing_message_id(&sender_message_id_domain(&from));
    let mut builder = Message::builder()
        .from(from)
        .message_id(Some(message_id))
        .subject(message.subject.clone())
        .user_agent(format!("jerc/{}", env!("CARGO_PKG_VERSION")));
    let mut recipients = 0usize;
    for (value, kind) in [(&message.to, 0), (&message.cc, 1), (&message.bcc, 2)] {
        if value.trim().is_empty() {
            continue;
        }
        let mailboxes: Mailboxes = value.parse().context("invalid recipient mailbox list")?;
        for mailbox in mailboxes {
            recipients += 1;
            builder = match kind {
                0 => builder.to(mailbox),
                1 => builder.cc(mailbox),
                _ => builder.bcc(mailbox),
            };
        }
    }
    if recipients == 0 {
        bail!("outgoing email has no To, Cc, or Bcc recipient");
    }

    if let Some(id) = message.in_reply_to.as_deref() {
        builder = builder.in_reply_to(format!("<{}>", normalized_message_id(id)?));
    }
    let mut refs = Vec::new();
    for id in &message.references {
        let normalized = normalized_message_id(id)?;
        if !refs.contains(&normalized) {
            refs.push(normalized);
        }
    }
    if !refs.is_empty() {
        builder = builder.references(
            refs.iter()
                .map(|id| format!("<{id}>"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    for (name, value) in &message.custom_headers {
        let lower = name.to_ascii_lowercase();
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || [
                "from",
                "to",
                "cc",
                "bcc",
                "subject",
                "date",
                "message-id",
                "in-reply-to",
                "references",
                "mime-version",
                "content-type",
                "content-transfer-encoding",
                "return-path",
            ]
            .contains(&lower.as_str())
        {
            bail!("reserved or invalid custom header {name:?}");
        }
        if value.contains('\r') || value.contains('\n') {
            bail!("newlines forbidden in custom header {name:?}");
        }
        let name =
            HeaderName::new_from_ascii(name.clone()).context("invalid custom header name")?;
        builder = builder.raw_header(HeaderValue::new(name, value.clone()));
    }

    if message.attachments.is_empty()
        && message.html_body.is_none()
        && message.forwarded_raw.is_none()
    {
        return builder
            .body(message.body.clone())
            .context("building plain outgoing message");
    }
    let mut body = if let Some(html) = message.html_body.as_ref() {
        let alternative = MultiPart::alternative()
            .singlepart(SinglePart::plain(message.body.clone()))
            .singlepart(SinglePart::html(html.clone()));
        MultiPart::mixed().multipart(alternative)
    } else {
        MultiPart::mixed().singlepart(SinglePart::plain(message.body.clone()))
    };
    for file in &message.attachments {
        if file.filename.is_empty()
            || (file.filename.contains('\r') || file.filename.contains('\n'))
        {
            bail!("invalid attachment filename");
        }
        let content_type = ContentType::parse(&file.content_type)
            .with_context(|| format!("invalid attachment MIME type {:?}", file.content_type))?;
        body = body.singlepart(
            Attachment::new(file.filename.clone()).body(file.data.clone(), content_type),
        );
    }
    if let Some(raw) = message.forwarded_raw.as_ref() {
        if raw.is_empty() {
            bail!("cannot forward an empty original message");
        }
        let content_type = ContentType::parse("message/rfc822")?;
        body = body.singlepart(
            Attachment::new("forwarded.eml".to_owned()).body(raw.clone(), content_type),
        );
    }
    builder
        .multipart(body)
        .context("building multipart outgoing message")
}

/// Construct an unsent RFC 5322 message for the user's Drafts folder.
/// Unlike SMTP mail, a draft can have incomplete/empty recipients. Lettre
/// requires an SMTP envelope recipient, so we use the sender ONLY in a
/// temporary builder and remove that placeholder To header before APPEND.
/// The original To/Cc/Bcc strings are stored exclusively in the saved draft.
/// The returned bytes must never be submitted to SMTP directly.
pub fn build_draft(message: &OutgoingMessage, default_from: &str) -> Result<Vec<u8>> {
    let sender = if message.from.trim().is_empty() {
        default_from
    } else {
        &message.from
    };
    if sender.trim().is_empty() {
        bail!(
            "no draft sender available; add `from = \"you@example.com\"` to this account in config.toml"
        );
    }
    let mailbox: Mailbox = sender
        .parse()
        .context("invalid draft sender (check this account's `from` address in config.toml)")?;
    let mut staged = message.clone();
    staged.to = mailbox.email.to_string();
    staged.cc.clear();
    staged.bcc.clear();
    let raw = build_outgoing(&staged, default_from)?.formatted();
    let separator = b"\r\n\r\n";
    let split = raw
        .windows(separator.len())
        .position(|part| part == separator)
        .ok_or_else(|| anyhow!("MIME message has no header/body boundary"))?;
    let header = std::str::from_utf8(&raw[..split]).context("invalid MIME header text")?;
    let mut output = Vec::with_capacity(raw.len() + 256);
    let mut skip_to = false;
    for line in header.split("\r\n") {
        if line.starts_with(' ') || line.starts_with('\t') {
            if skip_to {
                continue;
            }
        } else {
            skip_to = line
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("to:"));
            if skip_to {
                continue;
            }
        }
        output.extend_from_slice(line.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    for (name, value) in [
        ("To", &message.to),
        ("Cc", &message.cc),
        ("Bcc", &message.bcc),
    ] {
        if value.is_empty() {
            continue;
        }
        // A partially typed address may be saved, but not a header injection.
        if value.contains('\r') || value.contains('\n') || value.contains('\0') {
            bail!("invalid newline/control in draft {name} field");
        }
        output.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    output.extend_from_slice(b"X-Unsent: 1\r\n\r\n");
    output.extend_from_slice(&raw[split + separator.len()..]);
    Ok(output)
}

fn normalized_message_id(id: &str) -> Result<String> {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>');
    if id.is_empty()
        || !id.contains('@')
        || id
            .chars()
            .any(|ch| ch.is_whitespace() || ch == '<' || ch == '>')
    {
        bail!("invalid message-id in outgoing thread headers");
    }
    Ok(id.to_owned())
}

#[cfg(test)]
mod draft_tests {
    use super::build_draft;
    use crate::model::{OutgoingAttachment, OutgoingMessage};

    fn message() -> OutgoingMessage {
        OutgoingMessage {
            from: "sender@example.org".to_owned(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Unsent 🎉".to_owned(),
            body: "The unsent body".to_owned(),
            html_body: None,
            attachments: Vec::new(),
            custom_headers: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            forwarded_raw: None,
        }
    }

    #[test]
    fn missing_sender_has_an_actionable_error() {
        let mut message = message();
        message.from.clear();
        let error = build_draft(&message, "").unwrap_err();
        assert!(error.to_string().contains("config.toml"));
    }

    #[test]
    fn draft_with_no_recipients_does_not_leak_placeholder_to() {
        let raw = build_draft(&message(), "sender@example.org").unwrap();
        let headers = String::from_utf8_lossy(&raw);
        let headers = headers.split("\r\n\r\n").next().unwrap();
        assert!(headers.contains("X-Unsent: 1"));
        assert!(!headers.lines().any(|line| line.starts_with("To:")));
        assert!(headers.contains("Subject: "));
    }

    #[test]
    fn draft_keeps_partially_typed_to_and_private_bcc() {
        let mut draft = message();
        draft.to = "someone@gmail".to_owned();
        draft.cc = "other@example.org".to_owned();
        draft.bcc = "hidden@example.org".to_owned();
        let raw = build_draft(&draft, "sender@example.org").unwrap();
        let headers = String::from_utf8_lossy(&raw);
        let headers = headers.split("\r\n\r\n").next().unwrap();
        assert!(headers.contains("To: someone@gmail\r\n"));
        assert!(headers.contains("Cc: other@example.org\r\n"));
        assert!(headers.contains("Bcc: hidden@example.org\r\n"));
        assert!(!headers.contains("To: sender@example.org"));
    }

    #[test]
    fn draft_preserves_actual_mime_attachments() {
        let mut draft = message();
        draft.attachments.push(OutgoingAttachment {
            filename: "notes.txt".to_owned(),
            content_type: "text/plain".to_owned(),
            data: b"hello from attachment".to_vec(),
        });
        let raw = build_draft(&draft, "sender@example.org").unwrap();
        let attachments = crate::mail::extract_attachments(&raw).unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "notes.txt");
        assert_eq!(attachments[0].data, b"hello from attachment");
    }

    #[test]
    fn rejects_header_injection_in_unfinished_drafts() {
        let mut draft = message();
        draft.to = "someone@example.org\r\nBcc: attacker@example.org".to_owned();
        assert!(build_draft(&draft, "sender@example.org").is_err());
    }
}
