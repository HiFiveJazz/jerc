//! RFC 8620 EventSource parsing, bounds, and state-token coalescing.
//! No network connections are created by this module.
use anyhow::{Result, ensure};
use serde_json::Value;
use std::io::{self, BufRead};

/// RFC 8620 section 7.3: three required URI template variables. The
/// expansion is deterministic, with no server-supplied data or secrets.
pub(super) fn event_source_endpoint(template: &str) -> Result<String> {
    ensure!(
        ["{types}", "{closeafter}", "{ping}"]
            .iter()
            .all(|key| template.contains(key)),
        "JMAP eventSourceUrl does not include required template variables"
    );
    let url = template
        .replace("{types}", "Email,Mailbox")
        .replace("{closeafter}", "no")
        .replace("{ping}", "30");
    ensure!(
        !url.contains('{') && !url.contains('}'),
        "unsupported JMAP event-source URI template"
    );
    Ok(url)
}

/// Do not allow a malicious or broken server to grow an unlimited line buffer.
pub(super) fn read_bounded_sse_line(
    reader: &mut impl BufRead,
    line: &mut String,
) -> io::Result<usize> {
    const MAX_LINE: usize = 64 * 1024;
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let end = buf
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buf.len(), |i| i + 1);
        if bytes.len().saturating_add(end) > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JMAP SSE line too long",
            ));
        }
        let newline = buf.get(end - 1) == Some(&b'\n');
        bytes.extend_from_slice(&buf[..end]);
        reader.consume(end);
        if newline {
            break;
        }
    }
    let count = bytes.len();
    *line = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "JMAP SSE is not UTF-8"))?;
    Ok(count)
}

#[derive(Default)]
pub(super) struct SseFrame {
    pub(super) event: String,
    pub(super) data: String,
    pub(super) id: Option<String>,
}

impl SseFrame {
    /// Returns true when the blank line terminates the current SSE event.
    pub(super) fn push_line(&mut self, line: &str) -> bool {
        if line.is_empty() {
            return true;
        }
        if line.starts_with(':') {
            return false;
        }
        let (key, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match key {
            "event" if value.len() < 80 => self.event = value.to_owned(),
            "data" if self.data.len() + value.len() < 128 * 1024 => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            "id" if value.len() <= 512 && !value.contains('\0') => {
                self.id = Some(value.to_owned());
            }
            _ => {}
        }
        false
    }

    fn mail_state(&self, account_id: &str) -> Option<Value> {
        if self.event != "state" {
            return None;
        }
        let value = serde_json::from_str::<Value>(&self.data).ok()?;
        if value["@type"] != "StateChange" {
            return None;
        }
        let changes = &value["changed"][account_id];
        (changes["Email"].is_string() || changes["Mailbox"].is_string()).then(|| changes.clone())
    }

    #[cfg(test)]
    pub(super) fn is_mail_state(&self, account_id: &str) -> bool {
        self.mail_state(account_id).is_some()
    }
}

#[derive(Default)]
pub(super) struct PushStates {
    email: Option<String>,
    mailbox: Option<String>,
}

impl PushStates {
    pub(super) fn accept(&mut self, frame: &SseFrame, account_id: &str) -> bool {
        let Some(changes) = frame.mail_state(account_id) else {
            return false;
        };
        let mut changed = false;
        for (key, previous) in [("Email", &mut self.email), ("Mailbox", &mut self.mailbox)] {
            if let Some(state) = changes[key].as_str()
                && previous.as_deref() != Some(state)
            {
                *previous = Some(state.to_owned());
                changed = true;
            }
        }
        changed
    }
}
