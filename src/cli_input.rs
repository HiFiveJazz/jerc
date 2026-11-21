//! Short-lived local CLI inputs. Reuse the ordinary Maildir reader and composer
//! rather than introducing a second MIME renderer or a new persistent cache.
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use percent_encoding::percent_decode_str;
use tempfile::TempDir;

use jerc::runtime::StartupCompose;

const MAX_INPUT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SINGLE_MESSAGE: usize = jerc::mail::MAX_BODY_MESSAGE_BYTES;
const MAX_MBOX_MESSAGES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Message,
    Patch,
    Mbox,
}

pub fn input_kind(argument: &str) -> Result<(InputKind, PathBuf)> {
    if let Some(path) = argument.strip_prefix("mbox:") {
        ensure!(!path.is_empty(), "mbox: requires a file path");
        return Ok((InputKind::Mbox, PathBuf::from(path)));
    }
    let path = PathBuf::from(argument);
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    match extension.to_ascii_lowercase().as_str() {
        "eml" => Ok((InputKind::Message, path)),
        "patch" | "diff" => Ok((InputKind::Patch, path)),
        "mbox" => Ok((InputKind::Mbox, path)),
        _ => bail!("unsupported file type; expected .eml, .patch, .diff, .mbox, or mbox:FILE"),
    }
}

fn read_limited(reader: impl Read) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    reader.take(MAX_INPUT_BYTES + 1).read_to_end(&mut result)?;
    ensure!(result.len() as u64 <= MAX_INPUT_BYTES, "local input exceeds 128 MiB limit");
    Ok(result)
}

pub fn read_input(path: &Path) -> Result<Vec<u8>> {
    if path == Path::new("-") {
        // Avoid an indefinite block when launched without piped input.
        ensure!(!std::io::IsTerminal::is_terminal(&io::stdin()), "stdin is a terminal; pipe an email or patch into `jerc -`");
        read_limited(io::stdin().lock())
    } else {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        ensure!(file.metadata()?.len() <= MAX_INPUT_BYTES, "local input exceeds 128 MiB limit");
        read_limited(file)
    }
}

/// Distinguish a one-message git-format-patch stream from an mbox archive.
/// Both begin with an mbox envelope line, so relying on "From " alone is wrong.
pub fn detect_stdin_kind(bytes: &[u8]) -> InputKind {
    let first = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
    if first.starts_with(b"From ")
        && first.windows(b"Mon Sep 17 00:00:00 2001".len())
            .any(|part| part == b"Mon Sep 17 00:00:00 2001")
    {
        InputKind::Patch
    } else if first.starts_with(b"From ") {
        InputKind::Mbox
    } else if bytes.starts_with(b"diff --git ") || bytes.starts_with(b"--- ") {
        InputKind::Patch
    } else {
        InputKind::Message
    }
}

fn is_mbox_envelope(line: &[u8]) -> bool {
    line.starts_with(b"From ") && line.len() > b"From  \n".len()
}

fn strip_format_patch_envelope(raw: &[u8]) -> &[u8] {
    if is_mbox_envelope(raw.split_inclusive(|b| *b == b'\n').next().unwrap_or_default()) {
        raw.iter().position(|b| *b == b'\n').map_or(raw, |index| &raw[index + 1..])
    } else {
        raw
    }
}

fn header_body_split(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    raw.windows(4).position(|part| part == b"\r\n\r\n")
        .map(|idx| (&raw[..idx], &raw[idx+4..]))
        .or_else(|| raw.windows(2).position(|part| part == b"\n\n")
            .map(|idx| (&raw[..idx], &raw[idx+2..])))
}

fn is_rfc822_message(raw: &[u8]) -> bool {
    let Some((headers, _)) = header_body_split(raw) else { return false };
    let mut identified = false;
    for line in headers.split(|b| *b == b'\n') {
        if line.starts_with(b"From:") || line.starts_with(b"Subject:") || line.starts_with(b"Date:") {
            identified = true;
        }
        if !line.is_empty() && !line.starts_with(b" ") && !line.starts_with(b"\t") && !line.contains(&b':') {
            return false;
        }
    }
    identified
}

fn wrap_plain_patch(bytes: &[u8], title: &str) -> Vec<u8> {
    let mut output = format!(
        "From: Local file <local@localhost>\r\nSubject: [PATCH] {title}\r\nDate: {}\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\n",
        chrono::Utc::now().to_rfc2822()
    ).into_bytes();
    output.extend_from_slice(bytes);
    output
}

fn message_for_reader(kind: InputKind, raw: &[u8], title: &str) -> Vec<u8> {
    if kind == InputKind::Patch {
        let raw = strip_format_patch_envelope(raw);
        if is_rfc822_message(raw) { raw.to_vec() } else { wrap_plain_patch(raw, title) }
    } else { raw.to_vec() }
}

/// Import into an automatically deleted, private temporary Maildir, never the
/// user's real mailbox or ~/.cache/jerc. The ordinary reader handles the MIME.
pub fn empty_maildir() -> Result<TempDir> {
    let temp = tempfile::Builder::new().prefix("jerc-cli-").tempdir()?;
    for folder in ["cur", "new", "tmp"] {
        fs::create_dir(temp.path().join(folder))?;
    }
    Ok(temp)
}

pub fn prepare_reader(kind: InputKind, bytes: &[u8], title: &str) -> Result<TempDir> {
    let temp = empty_maildir()?;
    let messages = if kind == InputKind::Mbox {
        split_mbox(bytes)?
    } else {
        vec![message_for_reader(kind, bytes, title)]
    };
    ensure!(!messages.is_empty(), "no messages found in input");
    for (index, message) in messages.iter().enumerate() {
        ensure!(message.len() <= MAX_SINGLE_MESSAGE, "message {} exceeds the 8 MiB reader limit", index + 1);
        let file = temp.path().join("cur").join(format!("{index:08}:2,S"));
        let mut handle = File::create(&file)?;
        handle.write_all(message)?;
        handle.flush()?;
    }
    Ok(temp)
}

/// mboxrd: an envelope line starts each message; >From escaping applies only
/// to message body lines. Messages stay in their original RFC 5322 form.
fn split_mbox(raw: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut messages = Vec::new();
    let mut current = Vec::new();
    let mut in_message = false;
    let mut in_body = false;
    for line in raw.split_inclusive(|b| *b == b'\n') {
        if is_mbox_envelope(line) && (!in_message || in_body) {
            if in_message && !current.is_empty() {
                messages.push(std::mem::take(&mut current));
                ensure!(messages.len() <= MAX_MBOX_MESSAGES, "mbox contains more than 10,000 messages");
            }
            in_message = true;
            in_body = false;
            continue;
        }
        ensure!(in_message, "not an mbox archive: missing From envelope");
        if in_body && line.iter().position(|b| *b != b'>')
            .is_some_and(|index| index > 0 && line[index..].starts_with(b"From "))
        {
            current.extend_from_slice(&line[1..]);
        } else {
            current.extend_from_slice(line);
        }
        if line == b"\n" || line == b"\r\n" { in_body = true; }
        ensure!(current.len() <= MAX_SINGLE_MESSAGE, "mbox message exceeds the 8 MiB reader limit");
    }
    if in_message && !current.is_empty() { messages.push(current); }
    ensure!(messages.len() <= MAX_MBOX_MESSAGES, "mbox contains more than 10,000 messages");
    Ok(messages)
}

fn safe_header(value: &str, name: &str) -> Result<String> {
    ensure!(!value.chars().any(|ch| matches!(ch, '\r' | '\n' | '\0')),
        "{name} contains an unsafe line break");
    Ok(value.to_owned())
}

pub fn parse_mailto(argument: &str) -> Result<StartupCompose> {
    let source = argument.strip_prefix("mailto:").context("expected mailto: URI")?;
    let (path, query) = source.split_once('?').unwrap_or((source, ""));
    let to = percent_decode_str(path).decode_utf8().context("invalid recipient encoding")?;
    let mut compose = StartupCompose { to: safe_header(&to, "recipient")?, ..Default::default() };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.to_ascii_lowercase().as_str() {
            "to" => { let new = safe_header(&value, "recipient")?; if !compose.to.is_empty() { compose.to.push_str(", "); } compose.to.push_str(&new); }
            "cc" => { let new = safe_header(&value, "cc")?; if !compose.cc.is_empty() { compose.cc.push_str(", "); } compose.cc.push_str(&new); }
            "bcc" => { let new = safe_header(&value, "bcc")?; if !compose.bcc.is_empty() { compose.bcc.push_str(", "); } compose.bcc.push_str(&new); }
            "subject" => compose.subject = safe_header(&value, "subject")?,
            "body" => compose.body = value.into_owned(),
            _ => {} // Unknown RFC 6068 extensions must not change the draft.
        }
    }
    Ok(compose)
}

fn get_header(headers: &str, name: &str) -> Option<String> {
    let mut value: Option<String> = None;
    let mut collecting = false;
    for line in headers.lines() {
        if (line.starts_with(' ') || line.starts_with('\t')) && collecting {
            if let Some(value) = value.as_mut() { value.push(' '); value.push_str(line.trim()); }
            continue;
        }
        collecting = false;
        if let Some((key, rest)) = line.split_once(':')
            && key.eq_ignore_ascii_case(name)
        {
            value = Some(rest.trim().to_owned());
            collecting = true;
        }
    }
    value
}

pub fn patch_draft(bytes: &[u8], name: &str) -> Result<StartupCompose> {
    let content = std::str::from_utf8(strip_format_patch_envelope(bytes))
        .context("patch draft must be UTF-8 text")?;
    let mut draft = StartupCompose { subject: format!("[PATCH] {name}"), ..Default::default() };
    if is_rfc822_message(content.as_bytes()) {
        let (headers, body) = content.split_once("\r\n\r\n")
            .or_else(|| content.split_once("\n\n"))
            .context("invalid patch headers")?;
        draft.to = safe_header(&get_header(headers, "To").unwrap_or_default(), "to")?;
        draft.cc = safe_header(&get_header(headers, "Cc").unwrap_or_default(), "cc")?;
        draft.subject = safe_header(&get_header(headers, "Subject").unwrap_or(draft.subject), "subject")?;
        draft.body = body.to_owned();
    } else {
        draft.body = content.to_owned();
    }
    Ok(draft)
}

pub fn validate_attachments(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    paths.iter().map(|path| {
        let absolute = fs::canonicalize(path).with_context(|| format!("attachment {}", path.display()))?;
        ensure!(absolute.is_file(), "attachment {} is not a file", path.display());
        Ok(absolute)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mailto_headers_and_body_are_decoded_without_header_injection() {
        let compose = parse_mailto("mailto:a%2Bb@example.org?subject=Patch+review&cc=b%40example.org&body=Hi%0Aall").unwrap();
        assert_eq!(compose.to, "a+b@example.org");
        assert_eq!(compose.cc, "b@example.org");
        assert_eq!(compose.subject, "Patch review");
        assert_eq!(compose.body, "Hi\nall");
        assert!(parse_mailto("mailto:a@example.org?subject=Hello%0ABcc%3Ame@example.com").is_err());
    }
    #[test]
    fn git_format_patch_becomes_unsent_draft_without_envelope() {
        let raw = b"From abcdef Mon Sep 17 00:00:00 2001\nFrom: Dev <dev@example.org>\nTo: list@example.org\nSubject: [PATCH] test\n\nMessage\n\ndiff --git a/a b/a\n";
        let draft = patch_draft(raw, "test.patch").unwrap();
        assert_eq!(draft.to, "list@example.org");
        assert_eq!(draft.subject, "[PATCH] test");
        assert!(draft.body.contains("diff --git a/a b/a"));
    }
    #[test]
    fn mboxrd_restores_escaped_from_body_lines() {
        let raw = b"From a Fri Jan 01 00:00:00 2021\nFrom: a@example.org\nSubject: A\n\n>From someone\nFrom b Fri Jan 02 00:00:00 2021\nFrom: b@example.org\nSubject: B\n\nsecond\n";
        let mails = split_mbox(raw).unwrap();
        assert_eq!(mails.len(),2);
        assert!(mails[0].windows(12).any(|w| w == b"From someone"));
    }
    #[test]
    fn stdin_detects_git_format_patch_without_misclassifying_mbox() {
        assert_eq!(detect_stdin_kind(b"From deadbeef Mon Sep 17 00:00:00 2001\nSubject: [PATCH] fix\n\ntext\n"), InputKind::Patch);
        assert_eq!(detect_stdin_kind(b"From alice Fri Nov 20 10:00:00 2026\nSubject: Hi\n\ntext\n"), InputKind::Mbox);
        assert_eq!(detect_stdin_kind(b"diff --git a/x b/x\n"), InputKind::Patch);
        assert_eq!(detect_stdin_kind(b"Subject: hi\n\nhello\n"), InputKind::Message);
    }
    #[test]
    fn preview_is_temp_and_does_not_touch_user_mailboxes() {
        let temp = prepare_reader(InputKind::Patch, b"diff --git a/a b/b\n", "a.patch").unwrap();
        assert_eq!(fs::read_dir(temp.path().join("cur")).unwrap().count(),1);
        assert_eq!(fs::read_dir(temp.path().join("new")).unwrap().count(),0);
    }
}
