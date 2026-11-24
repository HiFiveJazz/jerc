//! POP3 backend (RFC 1939/2449/2595).
//!
//! POP3 is a single server-side maildrop. Read state is local to jerc, and
//! moves/filing/sent copies are intentionally unsupported. UIDL is required
//! so that a session reconnect cannot accidentally operate on a renumbered
//! message. Never send credentials over an unencrypted connection.
use std::collections::{HashMap, HashSet};
use std::fs;
#[cfg(test)]
use std::io;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use percent_encoding::percent_decode_str;
use rustls::{ClientConnection, StreamOwned, pki_types::ServerName};
use rustls_platform_verifier::BuilderVerifierExt;
use serde::{Deserialize, Serialize};
use url::Url;

use super::{MailBackend, imap::run_credential_command};
use crate::mail::{
    MAX_BODY_MESSAGE_BYTES, MAX_RAW_MESSAGE_BYTES, parse_body_compact, parse_summary,
};
use crate::model::{
    Capabilities, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId, MessagePage,
    MessageSummary,
};

const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LINE: usize = 64 * 1024;
const MAX_UIDL_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_MESSAGE_COUNT: usize = 200_000;
const MAX_EXPORT_MESSAGE_BYTES: usize = 1024 * 1024 * 1024;

trait Pop3Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Pop3Stream for T {}

#[derive(Clone, PartialEq, Eq)]
pub struct Pop3Source {
    host: String,
    port: u16,
    username: String,
    secret: Option<String>,
    starttls: bool,
}

impl std::fmt::Debug for Pop3Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pop3Source")
            .field("source", &self.redacted())
            .finish()
    }
}

impl Pop3Source {
    pub fn supports(source: &str) -> bool {
        source
            .split_once("://")
            .is_some_and(|(scheme, _)| matches!(scheme, "pop3s" | "pop3+starttls"))
    }

    pub fn parse(source: &str) -> Result<Self> {
        let url = Url::parse(source).context("parsing POP3 source")?;
        ensure!(
            Self::supports(source),
            "POP3 requires pop3s:// or pop3+starttls://"
        );
        // url::Url leaves a bare pop3s://host path empty because this is a
        // custom scheme. An explicit "/" is equivalent; other paths are not.
        ensure!(
            url.query().is_none()
                && url.fragment().is_none()
                && (url.path().is_empty() || url.path() == "/"),
            "POP3 source does not accept paths or query parameters"
        );
        let host = url
            .host_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("POP3 host is missing"))?
            .to_owned();
        let username = percent_decode_str(url.username())
            .decode_utf8()?
            .into_owned();
        ensure!(
            !username.is_empty() && !username.chars().any(|c| matches!(c, '\r' | '\n' | '\0')),
            "invalid POP3 username"
        );
        let secret = url
            .password()
            .map(|s| {
                percent_decode_str(s)
                    .decode_utf8()
                    .map(|decoded| decoded.into_owned())
            })
            .transpose()?;
        let starttls = url.scheme() == "pop3+starttls";
        Ok(Self {
            host,
            port: url.port().unwrap_or(if starttls { 110 } else { 995 }),
            username,
            secret,
            starttls,
        })
    }

    pub fn redacted(&self) -> String {
        format!(
            "{}://{}@{}:{}",
            if self.starttls {
                "pop3+starttls"
            } else {
                "pop3s"
            },
            self.username,
            self.host,
            self.port
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LocalSeen {
    version: u8,
    seen: HashSet<String>,
}

#[derive(Clone)]
struct Pop3Entry {
    number: u32,
    uidl: String,
    size: u64,
}

pub struct Pop3Backend {
    source: Pop3Source,
    credential_command: Option<String>,
    state_path: Option<PathBuf>,
    seen: HashSet<String>,
    entries: Vec<Pop3Entry>,
    stream: Option<Box<dyn Pop3Stream>>,
}

impl Pop3Backend {
    pub fn new(
        source: Pop3Source,
        credential_command: Option<String>,
        state_path: Option<PathBuf>,
    ) -> Self {
        let seen = state_path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<LocalSeen>(&bytes).ok())
            .filter(|state| state.version == 1)
            .map_or_else(HashSet::new, |state| state.seen);
        Self {
            source,
            credential_command,
            state_path,
            seen,
            entries: Vec::new(),
            stream: None,
        }
    }

    fn stream(&mut self) -> Result<&mut (dyn Pop3Stream + '_)> {
        match self.stream.as_mut() {
            Some(stream) => Ok(stream.as_mut()),
            None => bail!("POP3 session not connected"),
        }
    }

    fn ensure_connected(&mut self) -> Result<()> {
        if self.stream.is_none() {
            self.connect()?;
        }
        Ok(())
    }

    fn read_line(stream: &mut dyn Pop3Stream) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            stream
                .read_exact(&mut byte)
                .context("POP3 connection closed while reading response")?;
            line.push(byte[0]);
            ensure!(line.len() <= MAX_LINE, "oversized POP3 response line");
            if line.ends_with(b"\r\n") {
                line.truncate(line.len() - 2);
                return Ok(line);
            }
        }
    }

    fn expect_ok(stream: &mut dyn Pop3Stream) -> Result<Vec<u8>> {
        let line = Self::read_line(stream)?;
        // Server diagnostics can echo supplied credentials or identifiers.
        // Never include the raw line in UI errors or logs.
        ensure!(
            line == b"+OK" || line.starts_with(b"+OK "),
            "POP3 server rejected command"
        );
        Ok(line)
    }

    fn command(stream: &mut dyn Pop3Stream, command: &str) -> Result<Vec<u8>> {
        ensure!(
            !command.chars().any(|c| matches!(c, '\r' | '\n' | '\0')),
            "invalid POP3 command"
        );
        stream.write_all(command.as_bytes())?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
        Self::expect_ok(stream)
    }

    fn multiline_into(
        stream: &mut dyn Pop3Stream,
        limit: usize,
        writer: &mut dyn Write,
    ) -> Result<()> {
        let mut written = 0_usize;
        loop {
            let line = Self::read_line(stream)?;
            if line == b"." {
                return Ok(());
            }
            let line = if line.starts_with(b"..") {
                &line[1..]
            } else {
                &line[..]
            };
            ensure!(
                line.len().saturating_add(2) <= limit.saturating_sub(written),
                "POP3 multiline response exceeds configured limit"
            );
            writer.write_all(line)?;
            writer.write_all(b"\r\n")?;
            written += line.len() + 2;
        }
    }

    fn multiline(stream: &mut dyn Pop3Stream, limit: usize) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        Self::multiline_into(stream, limit, &mut data)?;
        Ok(data)
    }

    fn command_multi(&mut self, command: &str, limit: usize) -> Result<Vec<u8>> {
        let stream = self.stream()?;
        Self::command(stream, command)?;
        Self::multiline(stream, limit)
    }

    fn refresh_index(&mut self) -> Result<()> {
        self.ensure_connected()?;
        // Foreground flag writes and background STATUS use distinct sessions.
        // Reopen the small local seen-state file so unread counts stay correct.
        if let Some(path) = &self.state_path
            && let Ok(bytes) = fs::read(path)
            && let Ok(saved) = serde_json::from_slice::<LocalSeen>(&bytes)
            && saved.version == 1
        {
            self.seen = saved.seen;
        }
        let reply = self.command_multi("UIDL", MAX_UIDL_RESPONSE)?;
        let map = parse_uidls(&reply)?;
        ensure!(
            map.len() <= MAX_MESSAGE_COUNT,
            "POP3 mailbox exceeds supported message count"
        );
        let sizes = self.command_multi("LIST", MAX_UIDL_RESPONSE)?;
        let mut sizes_by_number = HashMap::new();
        for line in sizes.split(|b| *b == b'\n') {
            let Ok(text) = std::str::from_utf8(line) else {
                bail!("invalid POP3 LIST response")
            };
            let mut fields = text.split_ascii_whitespace();
            if let (Some(number), Some(size)) = (fields.next(), fields.next()) {
                let number: u32 = number.parse().context("invalid POP3 LIST message number")?;
                let size: u64 = size.parse().context("invalid POP3 LIST message size")?;
                ensure!(
                    sizes_by_number.insert(number, size).is_none(),
                    "duplicate POP3 LIST message number"
                );
            }
        }
        let mut entries = Vec::with_capacity(map.len());
        for (number, uidl) in map {
            let size = *sizes_by_number
                .get(&number)
                .ok_or_else(|| anyhow!("POP3 UIDL/LIST mismatch"))?;
            entries.push(Pop3Entry { number, uidl, size });
        }
        entries.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.number));
        self.entries = entries;
        Ok(())
    }

    fn resolve_number(&mut self, id: &MessageId) -> Result<u32> {
        self.ensure_connected()?;
        if self.entries.is_empty() {
            self.refresh_index()?;
        }
        let uidl =
            id.0.strip_prefix("pop3-uidl:")
                .ok_or_else(|| anyhow!("invalid POP3 message identity"))?;
        self.entries
            .iter()
            .find(|entry| entry.uidl == uidl)
            .map(|entry| entry.number)
            .ok_or_else(|| anyhow!("POP3 message no longer exists; refresh the mailbox"))
    }

    fn fetch_number(&mut self, number: u32, limit: usize) -> Result<Vec<u8>> {
        self.command_multi(&format!("RETR {number}"), limit)
    }

    fn summary(&mut self, entry: &Pop3Entry) -> Result<MessageSummary> {
        // TOP is optional; RETR is a bounded fallback for older POP3 servers.
        let headers = self
            .command_multi(&format!("TOP {} 0", entry.number), MAX_RAW_MESSAGE_BYTES)
            .or_else(|_| self.fetch_number(entry.number, MAX_RAW_MESSAGE_BYTES))?;
        let flags = {
            let mut flags = MessageFlags::empty();
            flags.set(MessageFlags::SEEN, self.seen.contains(&entry.uidl));
            flags
        };
        parse_summary(
            MessageId(format!("pop3-uidl:{}", entry.uidl)),
            &headers,
            entry.size,
            flags,
        )
    }

    fn persist_seen(&self) -> Result<()> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("invalid POP3 state path"))?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        crate::cache::write_json_buffered(
            &mut tmp,
            &LocalSeen {
                version: 1,
                seen: self.seen.clone(),
            },
        )?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    fn delete_uids(&mut self, ids: &[MessageId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.refresh_index()?;
        let mut numbers = Vec::with_capacity(ids.len());
        for id in ids {
            numbers.push(self.resolve_number(id)?);
        }
        let stream = self.stream()?;
        let transaction = (|| -> Result<()> {
            for number in numbers {
                Self::command(stream, &format!("DELE {number}"))?;
            }
            Self::command(stream, "QUIT")?; // Only QUIT commits POP3 DELE.
            Ok(())
        })();
        if transaction.is_err() {
            // Without QUIT the POP3 server must roll back deletion marks.
            // Drop the stream rather than accidentally committing on reconnect.
            self.stream = None;
            self.entries.clear();
            return transaction;
        }
        self.stream = None;
        self.entries.clear();
        transaction
    }
}

impl MailBackend for Pop3Backend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            folders: false,
            flags: true,
            move_messages: false,
            delete_messages: true,
            server_search: false,
            server_sort: false,
            server_thread: false,
        }
    }

    fn connect(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let credential = self
            .source
            .secret
            .clone()
            .map(Ok)
            .or_else(|| {
                self.credential_command
                    .as_deref()
                    .map(run_credential_command)
            })
            .unwrap_or_else(|| {
                Err(anyhow!(
                    "POP3 credential missing: configure source_cred_cmd"
                ))
            })?;
        ensure!(
            !credential.chars().any(|c| matches!(c, '\r' | '\n' | '\0')),
            "invalid POP3 credential"
        );
        let addr = format!(
            "{}:{}",
            if self.source.host.contains(':') {
                format!("[{}]", self.source.host)
            } else {
                self.source.host.clone()
            },
            self.source.port
        );
        let mut addresses = addr.to_socket_addrs().context("resolving POP3 host")?;
        let mut socket = addresses
            .find_map(|address| TcpStream::connect_timeout(&address, IO_TIMEOUT).ok())
            .ok_or_else(|| anyhow!("unable to connect to POP3 server"))?;
        socket.set_read_timeout(Some(IO_TIMEOUT))?;
        socket.set_write_timeout(Some(IO_TIMEOUT))?;
        if self.source.starttls {
            Self::expect_ok(&mut socket)?;
            // Reject unsupported STLS before initializing platform TLS roots.
            // No credentials are sent on the plaintext connection.
            Self::command(&mut socket, "STLS")?;
        }
        let server_name =
            ServerName::try_from(self.source.host.clone()).context("invalid POP3 TLS hostname")?;
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_platform_verifier()?
        .with_no_client_auth();
        let tls_config = Arc::new(config);
        let mut stream: Box<dyn Pop3Stream> = if self.source.starttls {
            // Do not send USER/PASS until the TLS handshake has completed.
            Box::new(StreamOwned::new(
                ClientConnection::new(tls_config, server_name)?,
                socket,
            ))
        } else {
            let mut tls: Box<dyn Pop3Stream> = Box::new(StreamOwned::new(
                ClientConnection::new(tls_config, server_name)?,
                socket,
            ));
            Self::expect_ok(tls.as_mut())?;
            tls
        };
        // Credentials are deliberately not placed in any error string.
        Self::command(stream.as_mut(), &format!("USER {}", self.source.username))
            .context("POP3 username rejected")?;
        Self::command(stream.as_mut(), &format!("PASS {credential}"))
            .context("POP3 authentication failed")?;
        self.stream = Some(stream);
        self.entries.clear();
        Ok(())
    }

    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
        let (total, unread) = self.mailbox_status("INBOX")?;
        Ok(vec![Mailbox {
            name: "INBOX".to_owned(),
            role: MailboxRole::Inbox,
            total,
            unread,
            counts_known: true,
        }])
    }

    fn mailbox_status(&mut self, name: &str) -> Result<(usize, usize)> {
        ensure!(
            name.eq_ignore_ascii_case("INBOX"),
            "POP3 supports only INBOX"
        );
        self.refresh_index()?;
        let unread = self
            .entries
            .iter()
            .filter(|entry| !self.seen.contains(&entry.uidl))
            .count();
        Ok((self.entries.len(), unread))
    }

    fn inbox_status(
        &mut self,
        name: &str,
        previous: Option<&super::InboxCheckpoint>,
    ) -> Result<((usize, usize), super::InboxCheck)> {
        let counts = self.mailbox_status(name)?;
        let check = if self.entries.len() <= super::notifications::MAX_TRACKED_IDS {
            super::InboxCheck::ids(
                previous,
                self.entries
                    .iter()
                    .map(|entry| {
                        (
                            entry.uidl.clone(),
                            MessageId(format!("pop3-uidl:{}", entry.uidl)),
                        )
                    })
                    .collect(),
            )
        } else {
            super::InboxCheck::default()
        };
        Ok((counts, check))
    }

    fn notification_raw(&mut self, _mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        let number = self.resolve_number(id)?;
        self.fetch_number(number, MAX_BODY_MESSAGE_BYTES)
    }

    fn open_mailbox(&mut self, name: &str, offset: usize, limit: usize) -> Result<MessagePage> {
        ensure!(
            name.eq_ignore_ascii_case("INBOX"),
            "POP3 supports only INBOX"
        );
        self.refresh_index()?;
        let end = if limit == 0 {
            self.entries.len()
        } else {
            offset.saturating_add(limit).min(self.entries.len())
        };
        let mut messages = Vec::new();
        for entry in self
            .entries
            .iter()
            .skip(offset)
            .take(end.saturating_sub(offset))
            .cloned()
            .collect::<Vec<_>>()
        {
            messages.push(self.summary(&entry)?);
        }
        Ok(MessagePage {
            messages,
            offset,
            next_offset: end,
            has_more: end < self.entries.len(),
        })
    }

    fn fetch_body(&mut self, _mailbox: &str, id: &MessageId) -> Result<MessageBody> {
        let number = self.resolve_number(id)?;
        let raw = self.fetch_number(number, MAX_BODY_MESSAGE_BYTES)?;
        parse_body_compact(id.clone(), &raw)
    }

    fn fetch_raw(&mut self, _mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        let number = self.resolve_number(id)?;
        self.fetch_number(number, MAX_RAW_MESSAGE_BYTES)
    }

    fn export_raw_into(
        &mut self,
        _mailbox: &str,
        id: &MessageId,
        writer: &mut dyn Write,
    ) -> Result<()> {
        let number = self.resolve_number(id)?;
        let stream = self.stream()?;
        Self::command(stream, &format!("RETR {number}"))?;
        Self::multiline_into(stream, MAX_EXPORT_MESSAGE_BYTES, writer)
    }

    fn set_seen(&mut self, mailbox: &str, id: &MessageId, seen: bool) -> Result<()> {
        ensure!(
            mailbox.eq_ignore_ascii_case("INBOX"),
            "POP3 supports only INBOX"
        );
        let _ = self.resolve_number(id)?;
        let uidl =
            id.0.strip_prefix("pop3-uidl:")
                .ok_or_else(|| anyhow!("invalid POP3 message identity"))?;
        if self.seen.contains(uidl) == seen {
            return Ok(());
        }
        let previous = self.seen.clone();
        if seen {
            self.seen.insert(uidl.to_owned());
        } else {
            self.seen.remove(uidl);
        }
        if let Err(error) = self.persist_seen() {
            self.seen = previous;
            return Err(error);
        }
        Ok(())
    }

    fn delete(&mut self, mailbox: &str, id: &MessageId) -> Result<()> {
        ensure!(
            mailbox.eq_ignore_ascii_case("INBOX"),
            "POP3 supports only INBOX"
        );
        self.delete_uids(std::slice::from_ref(id))
    }

    fn delete_many(&mut self, mailbox: &str, ids: &[MessageId]) -> Result<()> {
        ensure!(
            mailbox.eq_ignore_ascii_case("INBOX"),
            "POP3 supports only INBOX"
        );
        self.delete_uids(ids)
    }

    fn move_to(&mut self, _source: &str, _destination: &str, _id: &MessageId) -> Result<()> {
        bail!("POP3 cannot move messages or manage server folders")
    }
}

fn parse_uidls(bytes: &[u8]) -> Result<Vec<(u32, String)>> {
    let mut entries = Vec::new();
    let mut numbers = HashSet::new();
    let mut ids = HashSet::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let value = std::str::from_utf8(line).context("non-ASCII POP3 UIDL")?;
        let mut fields = value.split_ascii_whitespace();
        let number: u32 = fields
            .next()
            .ok_or_else(|| anyhow!("missing POP3 message number"))?
            .parse()?;
        let uidl = fields.next().ok_or_else(|| anyhow!("missing POP3 UIDL"))?;
        ensure!(
            number != 0 && fields.next().is_none() && uidl.len() <= 1024 && !uidl.is_empty(),
            "invalid POP3 UIDL record"
        );
        ensure!(
            numbers.insert(number) && ids.insert(uidl.to_owned()),
            "duplicate POP3 UIDL identity"
        );
        entries.push((number, uidl.to_owned()));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_plaintext_and_redacts_password() {
        assert!(Pop3Source::parse("pop3://u:secret@example.net").is_err());
        let source = Pop3Source::parse("pop3s://u:secret@example.net").unwrap();
        assert_eq!(source.port, 995);
        assert!(!source.redacted().contains("secret"));
        assert!(
            Pop3Source::parse("pop3+starttls://u@example.net")
                .unwrap()
                .starttls
        );
        assert_eq!(
            Pop3Source::parse("pop3s://u@example.net/").unwrap().port,
            995
        );
        assert!(Pop3Source::parse("pop3s://u@example.net/INBOX").is_err());
        assert!(Pop3Source::parse("pop3s://u@example.net/?token=secret").is_err());
        assert!(Pop3Source::parse("pop3s://u@example.net/#fragment").is_err());
    }
    #[test]
    fn rejects_duplicate_or_corrupt_uidls() {
        assert!(parse_uidls(b"1 first\r\n2 first\r\n").is_err());
        assert!(parse_uidls(b"1 one\r\n1 two\r\n").is_err());
        assert!(parse_uidls(b"0 zero\r\n").is_err());
        assert_eq!(parse_uidls(b"1 stable\r\n2 other\r\n").unwrap().len(), 2);
    }
    #[test]
    fn dot_stuffed_multiline_is_restored_and_bounded() {
        struct Fake {
            input: io::Cursor<Vec<u8>>,
        }
        impl Read for Fake {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.input.read(buf)
            }
        }
        impl Write for Fake {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut stream = Fake {
            input: io::Cursor::new(b"one\r\n..two\r\n.\r\n".to_vec()),
        };
        assert_eq!(
            Pop3Backend::multiline(&mut stream, 32).unwrap(),
            b"one\r\n.two\r\n"
        );
        let mut small = Fake {
            input: io::Cursor::new(b"big\r\n.\r\n".to_vec()),
        };
        assert!(Pop3Backend::multiline(&mut small, 3).is_err());
    }
    #[test]
    fn failed_stls_never_sends_credentials_over_plaintext() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream.write_all(b"+OK ready\r\n").unwrap();
            let mut line = Vec::new();
            let mut byte = [0_u8; 1];
            while !line.ends_with(b"\r\n") && line.len() < 128 {
                stream.read_exact(&mut byte).unwrap();
                line.push(byte[0]);
            }
            stream.write_all(b"-ERR no STLS\r\n").unwrap();
            assert_eq!(line, b"STLS\r\n");
        });
        let source = Pop3Source::parse(&format!(
            "pop3+starttls://someone:verysecret@127.0.0.1:{port}"
        ))
        .unwrap();
        let mut backend = Pop3Backend::new(source, None, None);
        assert!(backend.connect().is_err());
        peer.join().unwrap();
    }

    #[test]
    fn local_seen_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("seen.json");
        let source = Pop3Source::parse("pop3s://person@example.net").unwrap();
        let mut backend = Pop3Backend::new(source.clone(), None, Some(file.clone()));
        backend.seen.insert("stable-uidl".into());
        backend.persist_seen().unwrap();
        let reopened = Pop3Backend::new(source, None, Some(file));
        assert!(reopened.seen.contains("stable-uidl"));
    }
}
