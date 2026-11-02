use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use imap::types::{Flag, NameAttribute};
use percent_encoding::percent_decode_str;
use rustls_connector::{RustlsConnector, TlsStream};
use serde::Deserialize;
use url::Url;

use crate::backend::MailBackend;
use crate::mail::{parse_body, parse_summary};
use crate::model::{
    Capabilities, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId, MessageSummary,
};

type ImapSession = imap::Session<TlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const IO_TIMEOUT: Duration = Duration::from_secs(90);
const OAUTH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapAuth {
    Password,
    Login,
    OAuthBearer,
    XOAuth2,
}

#[derive(Clone)]
pub struct ImapSource {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    secret: Option<String>,
    pub auth: ImapAuth,
    token_endpoint: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    scope: Option<String>,
}

impl ImapSource {
    #[must_use]
    pub fn supports(source: &str) -> bool {
        source
            .split_once("://")
            .is_some_and(|(scheme, _)| matches!(scheme, "imaps" | "imaps+login" | "imaps+oauthbearer" | "imaps+xoauth2"))
    }

    pub fn parse(source: &str) -> Result<Self> {
        let url = Url::parse(source).context("parsing IMAP source URI")?;
        let auth = match url.scheme() {
            "imaps" => ImapAuth::Password,
            "imaps+login" => ImapAuth::Login,
            "imaps+oauthbearer" => ImapAuth::OAuthBearer,
            "imaps+xoauth2" => ImapAuth::XOAuth2,
            _ => bail!("unsupported IMAP source scheme"),
        };

        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| anyhow!("IMAP source has no hostname"))?
            .to_owned();
        let username = decode_component(url.username())?;
        if username.is_empty() {
            bail!("IMAP source has no username");
        }
        let secret = url.password().map(decode_component).transpose()?;

        let mut token_endpoint = None;
        let mut client_id = None;
        let mut client_secret = None;
        let mut scope = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "token_endpoint" => token_endpoint = Some(value.into_owned()),
                "client_id" => client_id = Some(value.into_owned()),
                "client_secret" => client_secret = Some(value.into_owned()),
                "scope" => scope = Some(value.into_owned()),
                _ => {}
            }
        }

        if matches!(auth, ImapAuth::OAuthBearer | ImapAuth::XOAuth2) {
            if let Some(endpoint) = &token_endpoint {
                let parsed = Url::parse(endpoint).context("parsing OAuth token endpoint")?;
                if parsed.scheme() != "https" {
                    bail!("OAuth token_endpoint must use https");
                }
            }
        }

        Ok(Self {
            scheme: url.scheme().to_owned(),
            host,
            port: url.port().unwrap_or(993),
            username,
            secret,
            auth,
            token_endpoint,
            client_id,
            client_secret,
            scope,
        })
    }

    #[must_use]
    pub fn redacted(&self) -> String {
        format!(
            "{}://{}@{}:{}",
            self.scheme, self.username, self.host, self.port
        )
    }

    fn credential(&self, credential_command: Option<&str>) -> Result<String> {
        if let Some(secret) = &self.secret {
            return Ok(secret.clone());
        }
        let command = credential_command.ok_or_else(|| {
            anyhow!(
                "IMAP source contains no credential; add a URI password/token or source_cred_cmd"
            )
        })?;
        run_credential_command(command)
    }

    fn access_token(&self, credential: &str) -> Result<String> {
        let Some(endpoint) = self.token_endpoint.as_deref() else {
            // aerc-compatible behavior: without token_endpoint, the configured
            // password/credential is already an access token.
            return Ok(credential.to_owned());
        };

        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", credential),
        ];
        if let Some(value) = self.client_id.as_deref() {
            form.push(("client_id", value));
        }
        if let Some(value) = self.client_secret.as_deref() {
            form.push(("client_secret", value));
        }
        if let Some(value) = self.scope.as_deref() {
            form.push(("scope", value));
        }

        let config = ureq::Agent::config_builder()
            .timeout_global(Some(OAUTH_TIMEOUT))
            .https_only(true)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let mut response = agent
            .post(endpoint)
            .send_form(form)
            .context("exchanging OAuth refresh token")?;
        let token: TokenResponse = response
            .body_mut()
            .read_json()
            .context("decoding OAuth token response")?;
        if token.access_token.is_empty() {
            bail!("OAuth token endpoint returned an empty access token");
        }
        Ok(token.access_token)
    }
}


impl std::fmt::Debug for ImapSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapSource")
            .field("source", &self.redacted())
            .field("auth", &self.auth)
            .field("has_uri_credential", &self.secret.is_some())
            .field("has_token_endpoint", &self.token_endpoint.is_some())
            .field("has_client_id", &self.client_id.is_some())
            .field("has_client_secret", &self.client_secret.is_some())
            .field("has_scope", &self.scope.is_some())
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

pub struct ImapBackend {
    source: ImapSource,
    credential_command: Option<String>,
    max_messages: usize,
    session: Option<ImapSession>,
    selected_mailbox: Option<String>,
}

impl std::fmt::Debug for ImapBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapBackend")
            .field("source", &self.source.redacted())
            .field("credential_command", &self.credential_command.as_ref().map(|_| "<configured>"))
            .field("max_messages", &self.max_messages)
            .field("connected", &self.session.is_some())
            .field("selected_mailbox", &self.selected_mailbox)
            .finish()
    }
}

impl ImapBackend {
    #[must_use]
    pub fn new(
        source: ImapSource,
        credential_command: Option<String>,
        max_messages: usize,
    ) -> Self {
        Self {
            source,
            credential_command,
            max_messages,
            session: None,
            selected_mailbox: None,
        }
    }

    fn session(&mut self) -> Result<&mut ImapSession> {
        self.session
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP account is not connected"))
    }

    fn select(&mut self, mailbox: &str) -> Result<imap::types::Mailbox> {
        if self.selected_mailbox.as_deref() == Some(mailbox) {
            // SELECT again intentionally refreshes EXISTS/UIDVALIDITY. This is
            // cheap compared with fetching headers and avoids stale sequence
            // ranges after another client changes the mailbox.
        }
        let selected = self
            .session()?
            .select(mailbox)
            .with_context(|| format!("selecting IMAP mailbox {mailbox:?}"))?;
        self.selected_mailbox = Some(mailbox.to_owned());
        Ok(selected)
    }

    fn uid_from_id(id: &MessageId) -> Result<u32> {
        id.0
            .strip_prefix("imap-uid:")
            .ok_or_else(|| anyhow!("message ID is not an IMAP UID"))?
            .parse::<u32>()
            .context("parsing IMAP UID")
    }

    fn ensure_selected(&mut self, mailbox: &str) -> Result<()> {
        if self.selected_mailbox.as_deref() != Some(mailbox) {
            let _ = self.select(mailbox)?;
        }
        Ok(())
    }
}

impl MailBackend for ImapBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities::IMAP_BASIC
    }

    fn connect(&mut self) -> Result<()> {
        let credential = self
            .source
            .credential(self.credential_command.as_deref())?;

        let tcp = connect_tcp(&self.source.host, self.source.port)?;
        tcp.set_read_timeout(Some(IO_TIMEOUT))?;
        tcp.set_write_timeout(Some(IO_TIMEOUT))?;
        tcp.set_nodelay(true)?;

        let connector = RustlsConnector::new_with_platform_verifier()
            .context("building rustls connector")?;
        let tls = connector
            .connect(&self.source.host, tcp)
            .map_err(|err| anyhow!("IMAP TLS handshake failed: {err}"))?;
        let mut client = imap::Client::new(tls);
        client.read_greeting().context("reading IMAP greeting")?;

        let session = match self.source.auth {
            ImapAuth::Password | ImapAuth::Login => client
                .login(&self.source.username, &credential)
                .map_err(|(err, _)| anyhow!("IMAP authentication failed: {err}"))?,
            ImapAuth::OAuthBearer | ImapAuth::XOAuth2 => {
                let access_token = self.source.access_token(&credential)?;
                let (mechanism, response) = match self.source.auth {
                    ImapAuth::OAuthBearer => (
                        "OAUTHBEARER",
                        oauthbearer_response(
                            &self.source.username,
                            &self.source.host,
                            self.source.port,
                            &access_token,
                        ),
                    ),
                    ImapAuth::XOAuth2 => (
                        "XOAUTH2",
                        xoauth2_response(&self.source.username, &access_token),
                    ),
                    _ => unreachable!(),
                };
                let auth = OAuthAuthenticator::new(response);
                client
                    .authenticate(mechanism, &auth)
                    .map_err(|(err, _)| anyhow!("IMAP OAuth authentication failed: {err}"))?
            }
        };

        self.session = Some(session);
        self.selected_mailbox = None;
        Ok(())
    }

    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
        let names = self
            .session()?
            .list(None, Some("*"))
            .context("listing IMAP mailboxes")?;

        let mut selectable: Vec<(String, MailboxRole)> = names
            .iter()
            .filter(|name| !name.attributes().iter().any(|attr| matches!(attr, NameAttribute::NoSelect)))
            .map(|name| {
                let mailbox_name = name.name().to_owned();
                let role = role_from_attributes(name.attributes())
                    .unwrap_or_else(|| role_for_name(&mailbox_name));
                (mailbox_name, role)
            })
            .collect();
        selectable.sort_by(|a, b| mailbox_sort_key(&a.0, &a.1).cmp(&mailbox_sort_key(&b.0, &b.1)));

        // Deliberately do not issue STATUS once per folder here. On hosted IMAP
        // that turns one fast LIST into N sequential network round trips and
        // blocks first paint. The UI asks for mailbox_status() lazily after the
        // selected folder and its message list are usable.
        Ok(selectable
            .into_iter()
            .map(|(name, role)| Mailbox {
                name,
                role,
                total: 0,
                unread: 0,
                counts_known: false,
            })
            .collect())
    }

    fn mailbox_status(&mut self, name: &str) -> Result<(usize, usize)> {
        let status = self
            .session()?
            .status(name, "(MESSAGES UNSEEN)")
            .with_context(|| format!("reading status for {name:?}"))?;
        Ok((
            usize::try_from(status.exists).unwrap_or(usize::MAX),
            usize::try_from(status.unseen.unwrap_or(0)).unwrap_or(usize::MAX),
        ))
    }

    fn open_mailbox(&mut self, name: &str) -> Result<Vec<MessageSummary>> {
        let selected = self.select(name)?;
        if selected.exists == 0 {
            return Ok(Vec::new());
        }

        let window = if self.max_messages == 0 {
            u32::MAX
        } else {
            u32::try_from(self.max_messages).unwrap_or(u32::MAX)
        };
        let start = selected
            .exists
            .saturating_sub(window.saturating_sub(1))
            .max(1);
        let sequence = format!("{start}:*");
        let fetched = self
            .session()?
            .fetch(
                &sequence,
                "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES)])",
            )
            .with_context(|| format!("fetching headers from {name:?}"))?;

        let mut messages = Vec::with_capacity(fetched.len());
        for fetch in fetched.iter() {
            let Some(uid) = fetch.uid else {
                continue;
            };
            let Some(headers) = fetch.header() else {
                continue;
            };
            let id = MessageId(format!("imap-uid:{uid}"));
            let flags = flags_from_imap(fetch.flags());
            let size = u64::from(fetch.size.unwrap_or(0));
            if let Ok(summary) = parse_summary(id, headers, size, flags) {
                messages.push(summary);
            }
        }
        messages.sort_by(|a, b| {
            b.timestamp
                .cmp(&a.timestamp)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(messages)
    }

    fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody> {
        let raw = self.fetch_raw(mailbox, id)?;
        parse_body(id.clone(), &raw).context("parsing IMAP message body")
    }

    fn fetch_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        self.ensure_selected(mailbox)?;
        let uid = Self::uid_from_id(id)?;
        let fetched = self
            .session()?
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .with_context(|| format!("fetching IMAP message UID {uid}"))?;
        let fetch = fetched
            .iter()
            .find(|fetch| fetch.uid == Some(uid))
            .ok_or_else(|| anyhow!("IMAP server did not return requested UID {uid}"))?;
        let raw = fetch
            .body()
            .ok_or_else(|| anyhow!("IMAP server returned UID {uid} without a body"))?;
        Ok(raw.to_vec())
    }

    fn set_seen(&mut self, mailbox: &str, id: &MessageId, seen: bool) -> Result<()> {
        self.ensure_selected(mailbox)?;
        let uid = Self::uid_from_id(id)?;
        let query = if seen {
            "+FLAGS.SILENT (\\Seen)"
        } else {
            "-FLAGS.SILENT (\\Seen)"
        };
        self.session()?
            .uid_store(uid.to_string(), query)
            .with_context(|| format!("updating seen flag for IMAP UID {uid}"))?;
        Ok(())
    }

    fn delete(&mut self, mailbox: &str, id: &MessageId) -> Result<()> {
        self.ensure_selected(mailbox)?;
        let uid = Self::uid_from_id(id)?;
        self.session()?
            .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
            .with_context(|| format!("marking IMAP UID {uid} deleted"))?;
        self.session()?
            .uid_expunge(uid.to_string())
            .with_context(|| format!("expunging IMAP UID {uid}"))?;
        Ok(())
    }

    fn move_to(&mut self, source: &str, destination: &str, id: &MessageId) -> Result<()> {
        self.ensure_selected(source)?;
        let uid = Self::uid_from_id(id)?;
        if self
            .session()?
            .uid_mv(uid.to_string(), destination)
            .is_ok()
        {
            return Ok(());
        }

        // RFC 6851 MOVE is optional. Fall back to UID COPY + exact UID
        // deletion, avoiding a blanket EXPUNGE that could remove messages
        // another client had already marked \Deleted.
        self.session()?
            .uid_copy(uid.to_string(), destination)
            .with_context(|| format!("copying IMAP UID {uid} to {destination:?}"))?;
        self.session()?
            .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
            .with_context(|| format!("marking moved IMAP UID {uid} deleted"))?;
        self.session()?
            .uid_expunge(uid.to_string())
            .with_context(|| format!("expunging moved IMAP UID {uid}"))?;
        Ok(())
    }
}

fn decode_component(raw: &str) -> Result<String> {
    percent_decode_str(raw)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .context("decoding percent-encoded IMAP source field")
}

fn connect_tcp(host: &str, port: u16) -> Result<TcpStream> {
    let addresses: Vec<_> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}:{port}"))?
        .collect();
    if addresses.is_empty() {
        bail!("hostname resolved to no addresses");
    }

    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow!("hostname resolved to no usable addresses")))
        .with_context(|| format!("connecting to {host}:{port}"))
}

fn run_credential_command(command: &str) -> Result<String> {
    #[cfg(unix)]
    let output = Command::new("sh").arg("-c").arg(command).output();
    #[cfg(windows)]
    let output = Command::new("cmd").arg("/C").arg(command).output();
    #[cfg(not(any(unix, windows)))]
    let output = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "credential commands are unsupported on this platform",
    ));

    let output = output.context("running source_cred_cmd")?;
    if !output.status.success() {
        bail!("source_cred_cmd exited unsuccessfully");
    }
    let secret = String::from_utf8(output.stdout)
        .context("source_cred_cmd returned non-UTF-8 output")?
        .trim_end_matches(|ch| ch == '\r' || ch == '\n')
        .to_owned();
    if secret.is_empty() {
        bail!("source_cred_cmd returned an empty credential");
    }
    Ok(secret)
}

fn xoauth2_response(username: &str, access_token: &str) -> Vec<u8> {
    format!("user={username}\x01auth=Bearer {access_token}\x01\x01").into_bytes()
}

fn oauthbearer_response(username: &str, host: &str, port: u16, access_token: &str) -> Vec<u8> {
    let authzid = sasl_name(username);
    format!(
        "n,a={authzid},\x01host={host}\x01port={port}\x01auth=Bearer {access_token}\x01\x01"
    )
    .into_bytes()
}

fn sasl_name(value: &str) -> String {
    value.replace('=', "=3D").replace(',', "=2C")
}

struct OAuthAuthenticator {
    response: Vec<u8>,
    sent: AtomicBool,
}

impl OAuthAuthenticator {
    fn new(response: Vec<u8>) -> Self {
        Self {
            response,
            sent: AtomicBool::new(false),
        }
    }
}

impl imap::Authenticator for OAuthAuthenticator {
    type Response = Vec<u8>;

    fn process(&self, _challenge: &[u8]) -> Self::Response {
        if self.sent.swap(true, Ordering::Relaxed) {
            // OAuth SASL requires an empty response after an error challenge.
            Vec::new()
        } else {
            self.response.clone()
        }
    }
}

fn flags_from_imap(flags: &[Flag<'_>]) -> MessageFlags {
    let mut out = MessageFlags::empty();
    for flag in flags {
        match flag {
            Flag::Seen => out.set(MessageFlags::SEEN, true),
            Flag::Answered => out.set(MessageFlags::ANSWERED, true),
            Flag::Flagged => out.set(MessageFlags::FLAGGED, true),
            Flag::Deleted => out.set(MessageFlags::DELETED, true),
            Flag::Draft => out.set(MessageFlags::DRAFT, true),
            Flag::Custom(name) if name.eq_ignore_ascii_case("$Forwarded") => {
                out.set(MessageFlags::FORWARDED, true);
            }
            _ => {}
        }
    }
    out
}

fn role_from_attributes(attributes: &[NameAttribute<'_>]) -> Option<MailboxRole> {
    for attribute in attributes {
        let NameAttribute::Custom(value) = attribute else {
            continue;
        };
        match value.as_ref().to_ascii_lowercase().as_str() {
            "\\sent" => return Some(MailboxRole::Sent),
            "\\drafts" => return Some(MailboxRole::Drafts),
            "\\trash" => return Some(MailboxRole::Trash),
            "\\archive" | "\\all" => return Some(MailboxRole::Archive),
            "\\junk" => return Some(MailboxRole::Junk),
            _ => {}
        }
    }
    None
}

fn role_for_name(name: &str) -> MailboxRole {
    let lower = name.to_ascii_lowercase();
    if lower == "inbox" {
        MailboxRole::Inbox
    } else if lower.contains("sent") {
        MailboxRole::Sent
    } else if lower.contains("draft") {
        MailboxRole::Drafts
    } else if lower.contains("trash") || lower.contains("deleted") {
        MailboxRole::Trash
    } else if lower.contains("archive") || lower.contains("all mail") {
        MailboxRole::Archive
    } else if lower.contains("junk") || lower.contains("spam") {
        MailboxRole::Junk
    } else {
        MailboxRole::Other
    }
}

fn mailbox_sort_key(name: &str, role: &MailboxRole) -> (u8, String) {
    let weight = match role {
        MailboxRole::Inbox => 0,
        MailboxRole::Sent => 1,
        MailboxRole::Drafts => 2,
        MailboxRole::Archive => 3,
        MailboxRole::Trash => 4,
        MailboxRole::Junk => 5,
        MailboxRole::Other => 6,
    };
    (weight, name.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{
        ImapAuth, ImapSource, OAuthAuthenticator, flags_from_imap, oauthbearer_response,
        role_from_attributes, sasl_name, xoauth2_response,
    };
    use imap::{Authenticator, types::{Flag, NameAttribute}};

    #[test]
    fn parses_aerc_style_password_source() {
        let source = ImapSource::parse(
            "imaps://person%40example.com:abcd%20efgh@imap.example.com:993",
        )
        .unwrap();
        assert_eq!(source.username, "person@example.com");
        assert_eq!(source.host, "imap.example.com");
        assert_eq!(source.port, 993);
        assert_eq!(source.auth, ImapAuth::Password);
        assert_eq!(source.secret.as_deref(), Some("abcd efgh"));
    }

    #[test]
    fn parses_aerc_style_oauth_refresh_source() {
        let source = ImapSource::parse(
            "imaps+oauthbearer://person%40example.edu:refresh-token@imap.example.com:993?client_id=client&client_secret=secret&token_endpoint=https%3A%2F%2Foauth.example.com%2Ftoken",
        )
        .unwrap();
        assert_eq!(source.auth, ImapAuth::OAuthBearer);
        assert_eq!(source.secret.as_deref(), Some("refresh-token"));
        assert_eq!(
            source.token_endpoint.as_deref(),
            Some("https://oauth.example.com/token")
        );
    }


    #[test]
    fn uri_credential_takes_precedence_over_external_command() {
        let source = ImapSource::parse(
            "imaps://person%40example.com:uri-secret@imap.example.com:993",
        )
        .unwrap();
        assert_eq!(
            source.credential(Some("this command must never run")).unwrap(),
            "uri-secret"
        );
    }

    #[test]
    fn debug_output_never_contains_secret_material() {
        let source = ImapSource::parse(
            "imaps+oauthbearer://person%40example.com:refresh-secret@imap.example.com:993?client_id=id&client_secret=client-secret&token_endpoint=https%3A%2F%2Foauth.example.com%2Ftoken",
        )
        .unwrap();
        let debug = format!("{source:?}");
        assert!(!debug.contains("refresh-secret"));
        assert!(!debug.contains("client-secret"));
        assert!(!debug.contains("oauth.example.com"));
    }

    #[test]
    fn seen_flag_maps_to_local_read_state() {
        let flags = flags_from_imap(&[Flag::Seen, Flag::Answered]);
        assert!(flags.contains(crate::model::MessageFlags::SEEN));
        assert!(flags.contains(crate::model::MessageFlags::ANSWERED));
    }

    #[test]
    fn special_use_attributes_map_to_roles() {
        assert_eq!(
            role_from_attributes(&[NameAttribute::Custom("\\Sent".into())]),
            Some(crate::model::MailboxRole::Sent)
        );
        assert_eq!(
            role_from_attributes(&[NameAttribute::Custom("\\All".into())]),
            Some(crate::model::MailboxRole::Archive)
        );
    }

    #[test]
    fn oauthbearer_escapes_sasl_authzid() {
        assert_eq!(sasl_name("a,b=c@example.com"), "a=2Cb=3Dc@example.com");
    }

    #[test]
    fn xoauth2_payload_matches_google_shape() {
        assert_eq!(
            xoauth2_response("person@example.com", "token"),
            b"user=person@example.com\x01auth=Bearer token\x01\x01"
        );
    }

    #[test]
    fn oauthbearer_payload_contains_rfc7628_fields() {
        assert_eq!(
            oauthbearer_response("person@example.com", "imap.example.com", 993, "token"),
            b"n,a=person@example.com,\x01host=imap.example.com\x01port=993\x01auth=Bearer token\x01\x01"
        );
    }

    #[test]
    fn oauth_authenticator_only_sends_secret_once() {
        let auth = OAuthAuthenticator::new(b"secret-bearing-payload".to_vec());
        assert_eq!(auth.process(&[]), b"secret-bearing-payload");
        assert!(auth.process(b"error-json").is_empty());
        assert!(auth.sent.load(Ordering::Relaxed));
    }
}
