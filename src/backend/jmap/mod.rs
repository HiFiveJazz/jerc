//! JMAP Mail backend (RFC 8620/8621): discovery, mailbox listing, queries,
//! MIME blob retrieval, message flags, moves, deletion and Email/import.
//!
//! JMAP uses account/mailbox-scoped cache identities, never IMAP UIDs.
//! SSE push uses the dedicated account watch worker. Native EmailSubmission
//! reuses the MIME builder, upload endpoint and shared authenticated session.
mod body;
mod push;
mod session;
mod submission;
mod views;

use self::push::{PushStates, SseFrame, event_source_endpoint, read_bounded_sse_line};
use self::session::parse_session;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::DateTime;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use super::{BackendError, ErrorKind, MailBackend, MailboxSyncHint, imap::run_credential_command};
use crate::config::AccountConfig;
use crate::mail::{MAX_BODY_MESSAGE_BYTES, MAX_RAW_MESSAGE_BYTES, parse_body};
use crate::model::{
    CacheIdentity, Capabilities, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageHeaders,
    MessageId, MessagePage, MessageSummary,
};

const JMAP_CORE: &str = "urn:ietf:params:jmap:core";
const JMAP_MAIL: &str = "urn:ietf:params:jmap:mail";
const JMAP_SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const HTTP_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_JSON_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_EXPORT_MESSAGE_BYTES: usize = 1024 * 1024 * 1024;
const MAX_PAGE: usize = 100;
const DEFAULT_GET_BATCH: usize = 50;
const MAX_SET_BATCH: usize = 100;
const MAX_QUERY_SNAPSHOTS: usize = 16;
const MAX_CHANGES: usize = 512;
const MAILBOX_PROPERTIES: &[&str] = &[
    "id",
    "name",
    "parentId",
    "role",
    "totalEmails",
    "unreadEmails",
];
// RFC 8621: Message-ID, In-Reply-To and References are structured msg-id
// fields. `:asText` is forbidden for them and causes `invalidArguments` on
// conforming servers. The convenience properties return String[]|null.
const EMAIL_SUMMARY_PROPERTIES: &[&str] = &[
    "id",
    "blobId",
    "threadId",
    "size",
    "receivedAt",
    "subject",
    "from",
    "to",
    "cc",
    "bcc",
    "keywords",
    "hasAttachment",
    "messageId",
    "inReplyTo",
    "references",
];

#[derive(Clone, PartialEq, Eq)]
pub struct JmapSource {
    session_url: Url,
    origin: Url,
    username: String,
    secret: Option<String>,
    bearer: bool,
}

impl std::fmt::Debug for JmapSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JmapSource")
            .field("source", &self.redacted())
            .finish()
    }
}

impl JmapSource {
    pub fn supports(source: &str) -> bool {
        source
            .split_once("://")
            .is_some_and(|(s, _)| matches!(s, "jmaps" | "jmaps+bearer"))
    }

    pub fn parse(source: &str) -> Result<Self> {
        ensure!(
            Self::supports(source),
            "unsupported JMAP source; use jmaps:// or jmaps+bearer://"
        );
        let url = Url::parse(source).context("parsing JMAP source")?;
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "JMAP source does not accept query parameters"
        );
        let host = url
            .host_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("JMAP host is missing"))?;
        let username = percent_decode_str(url.username())
            .decode_utf8()?
            .into_owned();
        ensure!(
            !username.is_empty() && !username.chars().any(|c| c.is_control() || c == ':'),
            "invalid JMAP username"
        );
        let secret = url
            .password()
            .map(|s| {
                percent_decode_str(s)
                    .decode_utf8()
                    .map(|decoded| decoded.into_owned())
            })
            .transpose()?;
        let mut origin = Url::parse(&format!(
            "https://{}/",
            if host.contains(':') {
                format!("[{host}]")
            } else {
                host.to_owned()
            }
        ))?;
        // Preserve port while translating jmaps: into an HTTPS origin.
        origin
            .set_port(url.port())
            .map_err(|()| anyhow!("invalid JMAP port"))?;
        let mut session_url = origin.clone();
        // Unlike https:// URLs, custom schemes may parse a bare host with an
        // empty path. Both an empty path and "/" mean default discovery.
        session_url.set_path(if url.path().is_empty() || url.path() == "/" {
            "/.well-known/jmap"
        } else {
            url.path()
        });
        Ok(Self {
            session_url,
            origin,
            username,
            secret,
            bearer: url.scheme() == "jmaps+bearer",
        })
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub fn redacted(&self) -> String {
        format!(
            "{}://{}@{}{}",
            if self.bearer { "jmaps+bearer" } else { "jmaps" },
            self.username,
            self.origin.host_str().unwrap_or("<host>"),
            self.origin
                .port()
                .map_or_else(String::new, |port| format!(":{port}"))
        )
    }
}

#[derive(Clone)]
struct JmapSession {
    account_id: String,
    api_url: String,
    download_url: String,
    upload_url: String,
    event_source_url: Option<String>,
    submission: bool,
    max_get: usize,
    max_set: usize,
    max_calls: usize,
    max_request: usize,
    max_upload: usize,
    max_concurrent_requests: usize,
    max_concurrent_uploads: usize,
    state: Option<String>,
}

#[derive(Clone)]
struct QuerySnapshot {
    state: String,
    ids: Vec<String>,
    total: usize,
    limit: usize,
}

#[derive(Clone)]
struct JmapMailbox {
    id: String,
    name: String,
    parent: Option<String>,
    role: MailboxRole,
    total: usize,
    unread: usize,
}

// Shared only by workers belonging to the same configured account. No global
// cache and no lock is held across HTTP, credential commands, or MIME parsing.
#[derive(Default)]
struct Shared {
    session: Option<JmapSession>,
    credential: Option<(Instant, String)>,
    mailbox_names: HashMap<String, String>,
    blobs: HashMap<(String, String), String>,
    blob_order: VecDeque<(String, String)>,
    // Account-wide immutable message metadata shared by all worker sessions.
    // Message flags remain per visible mailbox row.
    objects: HashMap<String, MessageSummary>,
    object_order: VecDeque<String>,
    attachments: HashMap<String, Vec<super::RemoteAttachment>>,
    attachment_order: VecDeque<String>,
    identities: Option<Value>,
}

impl Shared {
    fn remember_email(&mut self, email: &Value) {
        const MAX_OBJECTS: usize = 256;
        let Some(id) = email["id"].as_str().filter(|id| !id.is_empty()) else {
            return;
        };
        // Body/get and blob lookups have fewer properties; never overwrite a
        // complete summary with an artificial '(no subject)' partial object.
        if !email["subject"].is_string() || !email["receivedAt"].is_string() {
            return;
        }
        let Ok(summary) = summary_from_email(email) else {
            return;
        };
        if self.objects.contains_key(id) {
            self.object_order.retain(|entry| entry != id);
        } else if self.objects.len() >= MAX_OBJECTS
            && let Some(old) = self.object_order.pop_front()
        {
            self.objects.remove(&old);
        }
        self.object_order.push_back(id.to_owned());
        self.objects.insert(id.to_owned(), summary);
    }

    fn remember_attachments(&mut self, id: &str, entries: Vec<super::RemoteAttachment>) {
        const MAX_ATTACHMENTS: usize = 128;
        if self.attachments.contains_key(id) {
            self.attachment_order.retain(|entry| entry != id);
        } else if self.attachments.len() >= MAX_ATTACHMENTS
            && let Some(old) = self.attachment_order.pop_front()
        {
            self.attachments.remove(&old);
        }
        self.attachment_order.push_back(id.to_owned());
        self.attachments.insert(id.to_owned(), entries);
    }

    fn remember_blob(&mut self, account: &str, id: &str, blob: &str) {
        const MAX_BLOBS: usize = 1024;
        let key = (account.to_owned(), id.to_owned());
        if let Some(existing) = self.blobs.get_mut(&key) {
            *existing = blob.to_owned();
            return;
        }
        if self.blobs.len() >= MAX_BLOBS
            && let Some(old) = self.blob_order.pop_front()
        {
            self.blobs.remove(&old);
        }
        self.blob_order.push_back(key.clone());
        self.blobs.insert(key, blob.to_owned());
    }
}

/// Limit concurrent API calls/uploads across the existing account workers.
/// No metadata or credential lock is held while waiting or performing I/O.
#[derive(Default)]
struct RequestGate {
    active: Mutex<usize>,
    ready: Condvar,
}

impl RequestGate {
    fn enter(&self, limit: usize) -> Result<RequestPermit<'_>> {
        let active = self
            .active
            .lock()
            .map_err(|_| anyhow!("JMAP request gate poisoned"))?;
        let mut active = self
            .ready
            .wait_while(active, |active| *active >= limit)
            .map_err(|_| anyhow!("JMAP request gate poisoned"))?;
        *active += 1;
        Ok(RequestPermit(self))
    }
}

struct RequestPermit<'a>(&'a RequestGate);
impl Drop for RequestPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.0.active.lock() {
            *active -= 1;
            self.0.ready.notify_all();
        }
    }
}

pub struct JmapBackend {
    source: JmapSource,
    credential_command: Option<String>,
    oauth: Option<crate::oauth::OAuthSettings>,
    native_submission: Option<bool>, // Some(true): native required; Some(false): SMTP fallback
    agent: ureq::Agent,
    session: Option<JmapSession>,
    mailboxes: HashMap<String, JmapMailbox>,
    checkpoint: Option<CacheIdentity>,
    mailbox_state: Option<String>,
    mailbox_names: HashMap<String, String>,
    queries: HashMap<String, QuerySnapshot>,
    shared: Arc<Mutex<Shared>>,
    api_gate: Arc<RequestGate>,
    upload_gate: Arc<RequestGate>,
}

impl JmapBackend {
    pub fn new(
        source: JmapSource,
        credential_command: Option<String>,
        account: &AccountConfig,
    ) -> Result<Self> {
        let oauth = crate::oauth::OAuthSettings::for_account(account)?;
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(HTTP_TIMEOUT))
                .https_only(true)
                .max_redirects(0)
                .http_status_as_error(false)
                .build(),
        );
        let native_submission = match account.outgoing.as_deref() {
            Some("jmap") => Some(true),
            Some(value)
                if value.starts_with("smtp://")
                    || value.starts_with("smtps://")
                    || value.starts_with("smtp+")
                    || value.starts_with("smtps+") =>
            {
                Some(false)
            }
            _ => None,
        };
        Ok(Self {
            source,
            credential_command,
            oauth,
            native_submission,
            agent,
            session: None,
            mailboxes: HashMap::new(),
            checkpoint: None,
            mailbox_state: None,
            mailbox_names: HashMap::new(),
            queries: HashMap::new(),
            shared: Arc::new(Mutex::new(Shared::default())),
            api_gate: Arc::new(RequestGate::default()),
            upload_gate: Arc::new(RequestGate::default()),
        })
    }

    /// Independent worker state, shared HTTP pool and small account metadata.
    pub(crate) fn fork(&self) -> Self {
        Self {
            source: self.source.clone(),
            credential_command: self.credential_command.clone(),
            oauth: self.oauth.clone(),
            native_submission: self.native_submission,
            agent: self.agent.clone(),
            session: None,
            mailboxes: HashMap::new(),
            checkpoint: None,
            mailbox_state: None,
            mailbox_names: HashMap::new(),
            queries: HashMap::new(),
            shared: Arc::clone(&self.shared),
            api_gate: Arc::clone(&self.api_gate),
            upload_gate: Arc::clone(&self.upload_gate),
        }
    }

    /// Wizard probe: credentials are passed in memory and never serialized.
    pub(crate) fn set_probe_password(&mut self, password: &str) {
        self.source.secret = Some(password.to_owned());
    }

    fn auth_header(&self) -> Result<String> {
        if let Some(oauth) = &self.oauth {
            return Ok(format!("Bearer {}", oauth.access_token()?));
        }
        let credential = if let Some(secret) = &self.source.secret {
            secret.clone()
        } else {
            let cached = self.shared.lock().ok().and_then(|shared| {
                shared
                    .credential
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
                    .map(|(_, credential)| credential.clone())
            });
            if let Some(credential) = cached {
                credential
            } else {
                let command = self
                    .credential_command
                    .as_deref()
                    .ok_or_else(|| anyhow!("JMAP credential missing: configure source_cred_cmd"))?;
                let credential = run_credential_command(command)?;
                if let Ok(mut shared) = self.shared.lock() {
                    shared.credential = Some((Instant::now(), credential.clone()));
                }
                credential
            }
        };
        ensure!(
            !credential.chars().any(|c| matches!(c, '\r' | '\n' | '\0')),
            "invalid JMAP credential"
        );
        if self.source.bearer {
            Ok(format!("Bearer {credential}"))
        } else {
            Ok(format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{credential}", self.source.username))
            ))
        }
    }

    fn invalidate_rejected_credential(&self, status: u16) {
        if status == 401
            && let Ok(mut shared) = self.shared.lock()
        {
            shared.credential = None;
        }
    }

    fn approved_url(&self, target: &str) -> Result<()> {
        let url = Url::parse(target).context("invalid JMAP endpoint URL")?;
        ensure!(
            url.scheme() == "https"
                && url.host_str() == self.source.origin.host_str()
                && url.port_or_known_default() == self.source.origin.port_or_known_default()
                && url.username().is_empty()
                && url.password().is_none(),
            "JMAP server supplied an untrusted endpoint origin; credentials were not sent"
        );
        Ok(())
    }

    fn get_json(&self, url: &str) -> Result<Value> {
        // RFC 8620 allows /.well-known/jmap to redirect. Never forward the
        // Authorization header to another origin, even over HTTPS.
        let mut endpoint = url.to_owned();
        for _ in 0..=3 {
            self.approved_url(&endpoint)?;
            let authorization = self.auth_header()?;
            let mut response = self
                .agent
                .get(&endpoint)
                .header("Authorization", authorization.as_str())
                .call()
                .map_err(|error| BackendError::transport("JMAP HTTPS request", &error))?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|header| header.to_str().ok())
                    .ok_or_else(|| anyhow!("JMAP redirect has no valid Location"))?;
                let next = Url::parse(&endpoint)?.join(location)?;
                self.approved_url(next.as_str())?;
                endpoint = next.to_string();
                continue;
            }
            self.invalidate_rejected_credential(response.status().as_u16());
            if !response.status().is_success() {
                return Err(BackendError::http("JMAP server", response.status().as_u16()).into());
            }
            let bytes = response
                .body_mut()
                .with_config()
                .limit(MAX_JSON_RESPONSE as u64)
                .read_to_vec()
                .map_err(|error| BackendError::transport("reading JMAP response", &error))?;
            return serde_json::from_slice(&bytes).map_err(|_| {
                BackendError::new(ErrorKind::InvalidResponse, "invalid JMAP JSON response").into()
            });
        }
        bail!("JMAP session discovery redirected too many times")
    }

    fn post_json(&self, url: &str, data: &[u8]) -> Result<Value> {
        self.approved_url(url)?;
        let authorization = self.auth_header()?;
        let mut response = self
            .agent
            .post(url)
            .header("Authorization", authorization.as_str())
            .header("Content-Type", "application/json")
            .send(data)
            .map_err(|error| BackendError::transport("JMAP HTTPS API request", &error))?;
        self.invalidate_rejected_credential(response.status().as_u16());
        if !response.status().is_success() {
            return Err(BackendError::http("JMAP API", response.status().as_u16()).into());
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_JSON_RESPONSE as u64)
            .read_to_vec()
            .map_err(|error| BackendError::transport("reading JMAP API response", &error))?;
        serde_json::from_slice(&bytes).map_err(|_| {
            BackendError::new(ErrorKind::InvalidResponse, "invalid JMAP API response").into()
        })
    }

    fn session(&self) -> Result<&JmapSession> {
        self.session
            .as_ref()
            .ok_or_else(|| anyhow!("JMAP session is not connected"))
    }

    fn call(&mut self, name: &str, mut params: Value) -> Result<Value> {
        if self.session.is_none() {
            self.connect()?;
        }
        let session = self.session()?;
        params["accountId"] = Value::String(session.account_id.clone());
        let request = method_request(name, params);
        let reply = self.send_request(&request)?;
        let response = extract_method_response(reply, name)?;
        validate_account(&response, &session.account_id)?;
        Ok(response)
    }

    fn send_request(&self, request: &Value) -> Result<Value> {
        let session = self.session()?;
        ensure!(
            request["methodCalls"]
                .as_array()
                .is_some_and(|calls| calls.len() <= session.max_calls),
            "JMAP request exceeds server method-call limit"
        );
        let bytes = serde_json::to_vec(request)?;
        ensure!(
            bytes.len() <= session.max_request,
            "JMAP request exceeds server size limit"
        );
        let _permit = self.api_gate.enter(session.max_concurrent_requests)?;
        let reply = self.post_json(&session.api_url, &bytes)?;
        if let Some(state) = reply["sessionState"].as_str()
            && session.state.as_deref() != Some(state)
            && let Ok(mut shared) = self.shared.lock()
            && shared
                .session
                .as_ref()
                .is_some_and(|current| current.state == session.state)
        {
            // Finish this operation on its current session. Rediscover at the
            // next operation boundary, not halfway through a mutation/read.
            shared.session = None;
        }
        Ok(reply)
    }

    fn load_mailboxes(&mut self) -> Result<()> {
        self.connect()?;
        let max_get = self.session()?.max_get;
        let delta = if let Some(state) = self.mailbox_state.clone() {
            let response = self.call(
                "Mailbox/changes",
                json!({"sinceState": state, "maxChanges": MAX_CHANGES}),
            );
            match response {
                Ok(changes) => apply_mailbox_changes(&self.mailboxes, &state, &changes),
                Err(error) if changes_unavailable(&error) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let (mailboxes, state) = if let Some((mut mailboxes, state, ids, counts_only)) = delta {
            let mut coherent = true;
            for chunk in ids.chunks(max_get) {
                let properties = if counts_only {
                    &["id", "totalEmails", "unreadEmails"][..]
                } else {
                    MAILBOX_PROPERTIES
                };
                let response = self.call(
                    "Mailbox/get",
                    json!({"ids": chunk, "properties": properties}),
                )?;
                if counts_only {
                    let rows: GetResponse<MailboxCountsWire> =
                        decode_response(response, "Mailbox/get counts")?;
                    if rows.state.as_deref() != Some(state.as_str())
                        || !rows.not_found.is_empty()
                        || rows.list.len() != chunk.len()
                    {
                        coherent = false;
                        break;
                    }
                    for row in rows.list {
                        let Some(old) = mailboxes.get_mut(&row.id) else {
                            coherent = false;
                            break;
                        };
                        if row.unread_emails > row.total_emails {
                            coherent = false;
                            break;
                        }
                        old.total = row.total_emails;
                        old.unread = row.unread_emails;
                    }
                } else {
                    let (rows, read_state) = parse_mailboxes(response)?;
                    if read_state.as_deref() != Some(state.as_str())
                        || rows.len() != chunk.len()
                        || !chunk.iter().all(|id| rows.contains_key(id))
                    {
                        coherent = false;
                        break;
                    }
                    mailboxes.extend(rows);
                }
                if !coherent {
                    break;
                }
            }
            if coherent {
                (mailboxes, Some(state))
            } else {
                self.full_mailboxes()?
            }
        } else {
            self.full_mailboxes()?
        };
        // Resolve paths only once, even for a counts-only change.
        let mut names = HashMap::new();
        for entry in mailboxes.values() {
            let name = if entry.role == MailboxRole::Inbox {
                "INBOX".to_owned()
            } else {
                mailbox_path(&mailboxes, &entry.id)?
            };
            ensure!(
                names.insert(name, entry.id.clone()).is_none(),
                "duplicate JMAP folder paths"
            );
        }
        self.mailboxes = mailboxes;
        self.mailbox_state = state;
        self.shared
            .lock()
            .map_err(|_| anyhow!("JMAP cache lock poisoned"))?
            .mailbox_names = names.clone();
        self.mailbox_names = names;
        Ok(())
    }

    fn full_mailboxes(&mut self) -> Result<(HashMap<String, JmapMailbox>, Option<String>)> {
        let response = self.call(
            "Mailbox/get",
            json!({"ids": null, "properties": MAILBOX_PROPERTIES}),
        )?;
        parse_mailboxes(response)
    }

    fn named_mailboxes(&self) -> Result<Vec<Mailbox>> {
        self.mailbox_names
            .iter()
            .map(|(name, id)| {
                let entry = self
                    .mailboxes
                    .get(id)
                    .ok_or_else(|| anyhow!("JMAP mailbox index is inconsistent"))?;
                Ok(Mailbox {
                    name: name.clone(),
                    role: entry.role.clone(),
                    total: entry.total,
                    unread: entry.unread,
                    counts_known: true,
                })
            })
            .collect()
    }

    fn mailbox_id(&mut self, mailbox: &str) -> Result<String> {
        self.connect()?;
        let cached = self
            .shared
            .lock()
            .ok()
            .and_then(|shared| shared.mailbox_names.get(mailbox).cloned());
        if let Some(id) = cached {
            return Ok(id);
        }
        self.load_mailboxes()?;
        self.shared
            .lock()
            .ok()
            .and_then(|shared| shared.mailbox_names.get(mailbox).cloned())
            .ok_or_else(|| anyhow!("JMAP mailbox not found: {mailbox}"))
    }

    fn share_account_headers(&self, messages: &mut [MessageSummary]) {
        // Different mailbox windows can point to the exact same account Email.
        // Share immutable header allocations, but keep flags per visible row.
        let Ok(shared) = self.shared.lock() else {
            return;
        };
        for message in messages {
            if let Some(cached) = shared.objects.get(&message.id.0)
                && cached.headers == message.headers
            {
                message.headers = Arc::clone(&cached.headers);
            }
        }
    }

    fn remember_response_blobs(&self, ids: &Value, response: &Value) {
        let Some(ids) = ids.as_array() else { return };
        let Ok(session) = self.session() else { return };
        let account = &session.account_id;
        if let Some(list) = response["list"].as_array()
            && let Ok(mut shared) = self.shared.lock()
        {
            for email in list {
                if let (Some(id), Some(blob)) = (email["id"].as_str(), email["blobId"].as_str())
                    && ids.iter().any(|value| value.as_str() == Some(id))
                    && !blob.is_empty()
                {
                    shared.remember_blob(account, id, blob);
                }
                shared.remember_email(email);
            }
        }
    }

    fn call_with_blobs(&mut self, method: &str, args: Value) -> Result<Value> {
        let ids = if method == "Email/get" {
            args["ids"].clone()
        } else {
            Value::Null
        };
        let response = self.call(method, args)?;
        self.remember_response_blobs(&ids, &response);
        Ok(response)
    }

    fn query_and_get(&self, mut query: Value) -> Result<Option<(Value, Option<Value>)>> {
        let session = self.session()?;
        if session.max_calls < 2
            || query["limit"].as_u64().unwrap_or(u64::MAX) > session.max_get as u64
        {
            return Ok(None);
        }
        query["accountId"] = json!(session.account_id);
        let request = query_get_request(query, &session.account_id);
        if serde_json::to_vec(&request)?.len() > session.max_request {
            return Ok(None);
        }
        let reply = self.send_request(&request)?;
        let (query, details) = extract_query_get(reply)?;
        validate_account(&query, &session.account_id)?;
        if let Some(details) = &details {
            validate_account(details, &session.account_id)?;
        }
        Ok(Some((query, details)))
    }

    fn query_page(
        &mut self,
        mailbox: &str,
        args: Value,
        batch: bool,
    ) -> Result<(Value, Option<Value>)> {
        let limit = args["limit"].as_u64().unwrap_or(MAX_PAGE as u64) as usize;
        let first = args["position"].as_u64() == Some(0);
        if first
            && let Some(old) = self
                .queries
                .get(mailbox)
                .filter(|old| old.limit == limit)
                .cloned()
        {
            let response = self.call(
                "Email/queryChanges",
                json!({
                    "filter": args["filter"], "sort": args["sort"], "sinceQueryState": old.state,
                    "maxChanges": MAX_CHANGES, "calculateTotal": true,
                }),
            );
            match response {
                Ok(response) => {
                    if let Some(next) = apply_query_changes(&old, &response) {
                        let query = json!({"ids": next.ids, "total": next.total,
                        "queryState": next.state, "canCalculateChanges": true});
                        self.queries.insert(mailbox.to_owned(), next);
                        return Ok((query, None));
                    }
                }
                Err(error) if changes_unavailable(&error) => {}
                Err(error) => return Err(error),
            }
            self.queries.remove(mailbox);
        }
        let (query, details) = if batch {
            match self.query_and_get(args.clone())? {
                Some(pair) => pair,
                None => (self.call("Email/query", args)?, None),
            }
        } else {
            (self.call("Email/query", args)?, None)
        };
        if first {
            self.queries.remove(mailbox);
            if let Some(snapshot) = query_snapshot(&query, limit) {
                if self.queries.len() >= MAX_QUERY_SNAPSHOTS {
                    self.queries.clear();
                }
                self.queries.insert(mailbox.to_owned(), snapshot);
            }
        }
        Ok((query, details))
    }

    fn message_blob(&mut self, id: &MessageId) -> Result<String> {
        if self.session.is_none() {
            self.connect()?;
        }
        let account = self.session()?.account_id.clone();
        if let Some(blob) = self
            .shared
            .lock()
            .ok()
            .and_then(|shared| shared.blobs.get(&(account, id.0.clone())).cloned())
        {
            return Ok(blob);
        }
        let response = self.call_with_blobs(
            "Email/get",
            json!({
                "ids": [&id.0], "properties": ["id", "blobId"]
            }),
        )?;
        let entry = response["list"]
            .as_array()
            .and_then(|list| {
                list.iter()
                    .find(|entry| entry["id"].as_str() == Some(id.0.as_str()))
            })
            .ok_or_else(|| anyhow!("JMAP message no longer exists; refresh the folder"))?;
        value_string(entry, "blobId")
    }

    fn blob_url(&self, blob_id: &str) -> Result<String> {
        let session = self.session()?;
        expand_url(
            &session.download_url,
            &session.account_id,
            blob_id,
            "message.eml",
        )
    }

    fn raw_response(&mut self, id: &MessageId) -> Result<ureq::http::Response<ureq::Body>> {
        let blob_id = self.message_blob(id)?;
        self.download_blob(&blob_id)
    }

    fn download_blob(&self, blob_id: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let download = self.blob_url(blob_id)?;
        self.approved_url(&download)?;
        let authorization = self.auth_header()?;
        let response = self
            .agent
            .get(&download)
            .header("Authorization", authorization.as_str())
            .call()
            .map_err(|error| BackendError::transport("JMAP blob download", &error))?;
        self.invalidate_rejected_credential(response.status().as_u16());
        if !response.status().is_success() {
            return Err(
                BackendError::http("JMAP blob download", response.status().as_u16()).into(),
            );
        }
        Ok(response)
    }

    fn set_emails(&mut self, updates: Value, destroy: Vec<String>) -> Result<MutationReport> {
        self.set_emails_at_state(updates, destroy, None)
    }

    // Use a preflight state when a destructive operation depends on mailbox
    // membership. If another client modifies any Email in the meantime, the
    // server rejects the write instead of letting us destroy an updated Email.
    fn set_emails_at_state(
        &mut self,
        updates: Value,
        destroy: Vec<String>,
        state: Option<&str>,
    ) -> Result<MutationReport> {
        let expected_updates: HashSet<String> = updates
            .as_object()
            .ok_or_else(|| anyhow!("invalid JMAP update request"))?
            .keys()
            .cloned()
            .collect();
        let expected_destroy: HashSet<String> = destroy.iter().cloned().collect();
        ensure!(
            expected_destroy.len() == destroy.len(),
            "duplicate JMAP destroy IDs"
        );
        let mut arguments = json!({"update": updates, "destroy": destroy});
        if let Some(state) = state {
            arguments["ifInState"] = json!(state);
        }
        let response = self.call("Email/set", arguments);
        // Whether successful, rejected, or only partly successful, these
        // summaries may now be stale. Invalidate metadata but retain immutable
        // body blobs for moves and edits (their IDs remain account-scoped).
        if let Ok(mut shared) = self.shared.lock() {
            for id in expected_updates.iter().chain(expected_destroy.iter()) {
                shared.objects.remove(id);
            }
            shared
                .object_order
                .retain(|id| !expected_updates.contains(id) && !expected_destroy.contains(id));
        }
        self.queries.clear();
        self.mailbox_state = None;
        validate_set_result(response?, &expected_updates, &expected_destroy)
    }

    // Read the complete membership set before a destructive action. JMAP
    // Emails are account-scoped and can belong to multiple mailboxes; an
    // unconditional destroy must not silently erase their other memberships.
    fn memberships_for(
        &mut self,
        ids: &[MessageId],
    ) -> Result<(String, HashMap<String, HashSet<String>>)> {
        let strings = ids.iter().map(|id| id.0.clone()).collect::<Vec<_>>();
        let response = self.call(
            "Email/get",
            json!({"ids": strings, "properties": ["id", "mailboxIds"]}),
        )?;
        let response: GetResponse<Value> = decode_response(response, "Email/get memberships")?;
        ensure!(
            response.not_found.is_empty() && response.list.len() == ids.len(),
            "JMAP message membership changed; refresh before deleting"
        );
        let state = response
            .state
            .filter(|state| !state.is_empty())
            .ok_or_else(|| anyhow!("JMAP Email/get returned no state for delete preflight"))?;
        let mut memberships = HashMap::new();
        let requested = ids.iter().map(|id| id.0.as_str()).collect::<HashSet<_>>();
        for email in response.list {
            let id = value_string(&email, "id")?;
            ensure!(
                requested.contains(id.as_str()),
                "JMAP membership response returned an unsolicited message"
            );
            let mailbox_ids = email["mailboxIds"]
                .as_object()
                .ok_or_else(|| anyhow!("JMAP Email lacks mailboxIds"))?;
            ensure!(
                mailbox_ids
                    .values()
                    .all(|flag| flag.as_bool() == Some(true)),
                "JMAP mailboxIds contains invalid membership flags"
            );
            let ids = mailbox_ids.keys().cloned().collect::<HashSet<_>>();
            ensure!(
                !ids.is_empty() && memberships.insert(id, ids).is_none(),
                "JMAP returned missing or duplicate memberships"
            );
        }
        ensure!(
            memberships.len() == requested.len(),
            "JMAP omitted message memberships"
        );
        Ok((state, memberships))
    }

    fn upload_message(&self, raw: &[u8]) -> Result<String> {
        ensure!(
            raw.len() <= MAX_RAW_MESSAGE_BYTES,
            "JMAP message exceeds the attachment upload limit"
        );
        let session = self.session()?;
        ensure!(
            raw.len() <= session.max_upload,
            "JMAP message exceeds server upload limit"
        );
        let url = expand_url(&session.upload_url, &session.account_id, "", "")?;
        self.approved_url(&url)?;
        let authorization = self.auth_header()?;
        let permit = self.upload_gate.enter(session.max_concurrent_uploads)?;
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", authorization.as_str())
            .header("Content-Type", "application/octet-stream")
            .send(raw)
            .map_err(|error| BackendError::transport("JMAP upload", &error))?;
        self.invalidate_rejected_credential(response.status().as_u16());
        if !response.status().is_success() {
            return Err(BackendError::http("JMAP upload", response.status().as_u16()).into());
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_JSON_RESPONSE as u64)
            .read_to_vec()
            .map_err(|error| BackendError::transport("reading JMAP upload response", &error))?;
        let data: Value = serde_json::from_slice(&bytes).map_err(|_| {
            BackendError::new(ErrorKind::InvalidResponse, "invalid JMAP upload response")
        })?;
        drop(response);
        drop(permit);
        validate_account(&data, &self.session()?.account_id)?;
        let blob_id = value_string(&data, "blobId")?;
        ensure!(!blob_id.is_empty(), "JMAP upload returned empty blob ID");
        Ok(blob_id)
    }

    fn import_message(&mut self, mailbox_id: &str, raw: &[u8], keywords: Value) -> Result<String> {
        let blob_id = self.upload_message(raw)?;
        let result = self.call(
            "Email/import",
            json!({"emails": {"m1": {
                "blobId": blob_id, "mailboxIds": {(mailbox_id): true}, "keywords": keywords
            }}}),
        )?;
        imported_email_id(result)
    }

    fn append(&mut self, mailbox: &str, raw: &[u8], keywords: Value) -> Result<()> {
        let mailbox_id = self.mailbox_id(mailbox)?;
        self.import_message(&mailbox_id, raw, keywords).map(|_| ())
    }
}

impl MailBackend for JmapBackend {
    fn submit_message(
        &mut self,
        message: &crate::model::OutgoingMessage,
        sent_mailbox: Option<&str>,
        copy_to: Option<&[String]>,
    ) -> Result<Option<super::NativeSendReceipt>> {
        self.submit_native(message, sent_mailbox, copy_to)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            folders: true,
            flags: true,
            move_messages: true,
            delete_messages: true,
            server_search: true,
            server_sort: true,
            server_thread: true,
        }
    }

    fn connect(&mut self) -> Result<()> {
        let cached = self
            .shared
            .lock()
            .ok()
            .and_then(|shared| shared.session.clone());
        let parsed = if let Some(session) = cached {
            session
        } else {
            let session = self.get_json(self.source.session_url.as_str())?;
            let parsed = parse_session(&session, &self.source)?;
            if let Ok(mut shared) = self.shared.lock() {
                shared.mailbox_names.clear();
                shared.objects.clear();
                shared.object_order.clear();
                shared.attachments.clear();
                shared.attachment_order.clear();
                shared.identities = None;
                shared.session = Some(parsed.clone());
            }
            parsed
        };
        if self
            .session
            .as_ref()
            .is_none_or(|old| old.account_id != parsed.account_id || old.state != parsed.state)
        {
            self.mailboxes.clear();
            self.mailbox_names.clear();
            self.checkpoint = None;
            self.mailbox_state = None;
            self.queries.clear();
        }
        self.session = Some(parsed);
        Ok(())
    }

    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
        self.load_mailboxes()?;
        let mut out = self.named_mailboxes()?;
        out.sort_by_key(|mailbox| {
            (
                match mailbox.role {
                    MailboxRole::Inbox => 0,
                    _ => 1,
                },
                mailbox.name.to_lowercase(),
            )
        });
        Ok(out)
    }

    fn watch_is_account_wide(&self) -> bool {
        true
    }

    fn watch_mailbox(
        &mut self,
        _name: &str,
        cancel: &AtomicBool,
        changed: &mut dyn FnMut(),
    ) -> Result<()> {
        let mut last_event_id = None::<String>;
        let mut last_endpoint = None::<(String, String)>;
        let mut last_states = PushStates::default();
        let mut reconnected = false;
        while !cancel.load(Ordering::Acquire) {
            self.connect()?;
            let session = self.session()?;
            let Some(template) = &session.event_source_url else {
                return Ok(());
            };
            let account_id = session.account_id.clone();
            let url = event_source_endpoint(template)?;
            self.approved_url(&url)?;
            let endpoint = (account_id.clone(), url.clone());
            if last_endpoint.as_ref() != Some(&endpoint) {
                last_event_id = None;
                last_states = PushStates::default();
                last_endpoint = Some(endpoint);
            }
            let authorization = self.auth_header()?;
            let mut request = self
                .agent
                .get(&url)
                .header("Authorization", authorization.as_str())
                .header("Accept", "text/event-stream");
            if let Some(id) = &last_event_id {
                request = request.header("Last-Event-ID", id.as_str());
            }
            let mut response = request
                .call()
                .map_err(|error| BackendError::transport("JMAP event stream connection", &error))?;
            self.invalidate_rejected_credential(response.status().as_u16());
            if !response.status().is_success() {
                return Err(
                    BackendError::http("JMAP event stream", response.status().as_u16()).into(),
                );
            }
            ensure!(
                response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value
                        .to_ascii_lowercase()
                        .starts_with("text/event-stream")),
                "JMAP endpoint did not return an event stream"
            );
            // A disconnected stream can miss events (SSE replay is not
            // guaranteed). Reconcile once after reconnecting successfully.
            if reconnected {
                changed();
            }
            reconnected = true;
            let mut reader = BufReader::new(response.body_mut().as_reader());
            let mut frame = SseFrame::default();
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Ok(());
                }
                let mut line = String::new();
                let count = match read_bounded_sse_line(&mut reader, &mut line) {
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                        return Err(error).context("reading JMAP SSE");
                    }
                    Err(_) => break, // reconnect on idle timeout or dropped stream
                };
                if count == 0 {
                    // Only blank-line-terminated events are dispatched.
                    break;
                }
                if frame.push_line(line.trim_end_matches(['\r', '\n'])) {
                    if let Some(id) = frame.id.take() {
                        last_event_id = Some(id);
                    }
                    if last_states.accept(&frame, &account_id) {
                        changed();
                    }
                    frame = SseFrame::default();
                    // Keep the account's stream open across state changes.
                    // Last-Event-ID is still sent on timeout or disconnect.
                }
            }
            // Do not spin against servers/proxies closing idle SSE streams.
            for _ in 0..10 {
                if cancel.load(Ordering::Acquire) {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(())
    }

    fn mailbox_status(&mut self, name: &str) -> Result<(usize, usize)> {
        let id = self.mailbox_id(name)?;
        let response = self.call(
            "Mailbox/get",
            json!({"ids": [id], "properties": ["id", "totalEmails", "unreadEmails"]}),
        )?;
        let response: GetResponse<MailboxCountsWire> =
            decode_response(response, "Mailbox/get counts")?;
        ensure!(
            response.not_found.is_empty() && response.list.len() == 1,
            "JMAP mailbox no longer exists or returned ambiguous counts"
        );
        let mailbox = &response.list[0];
        ensure!(
            mailbox.id == id && mailbox.unread_emails <= mailbox.total_emails,
            "JMAP mailbox count response does not match the requested mailbox"
        );
        Ok((mailbox.total_emails, mailbox.unread_emails))
    }

    fn mailbox_cache_identity(&self, name: &str) -> Option<CacheIdentity> {
        let checkpoint = self.checkpoint.as_ref()?;
        let CacheIdentity::Jmap {
            account_id,
            mailbox_id,
            ..
        } = checkpoint
        else {
            return None;
        };
        let shared = self.shared.lock().ok()?;
        if self.session.as_ref()?.account_id.as_str() != account_id.as_str()
            || shared.mailbox_names.get(name)? != mailbox_id
        {
            return None;
        }
        Some(checkpoint.clone())
    }

    fn open_mailbox(&mut self, name: &str, offset: usize, limit: usize) -> Result<MessagePage> {
        self.open_mailbox_with_hint(name, offset, limit, None)
    }

    fn open_mailbox_with_hint(
        &mut self,
        name: &str,
        offset: usize,
        limit: usize,
        hint: Option<MailboxSyncHint>,
    ) -> Result<MessagePage> {
        self.checkpoint = None;
        let mailbox_id = self.mailbox_id(name)?;
        let session = self.session()?;
        let account_id = session.account_id.clone();
        let max_get = session.max_get;
        let can_reuse = offset == 0 && hint.as_ref().is_some_and(|hint| {
            matches!(&hint.identity, CacheIdentity::Jmap { account_id: a, mailbox_id: m, email_state: Some(_), .. }
                if a == &account_id && m == &mailbox_id) && !hint.messages.is_empty()
        });
        // The last first-page query state is persisted with the mailbox
        // snapshot. Reuse it after restart only when the cached window has
        // exactly the same size as the requested window. Otherwise a fresh
        // query is required; stale/expired server states fall back safely.
        let window = if limit == 0 {
            MAX_PAGE
        } else {
            limit.min(MAX_PAGE)
        };
        if offset == 0
            && !self.queries.contains_key(&mailbox_id)
            && let Some(hint) = hint.as_ref()
            && let CacheIdentity::Jmap {
                query_state: Some(query_state),
                ..
            } = &hint.identity
            && hint.messages.len() == window
            && can_reuse
        {
            self.queries.insert(
                mailbox_id.clone(),
                QuerySnapshot {
                    state: query_state.clone(),
                    ids: hint
                        .messages
                        .iter()
                        .map(|message| message.id.0.clone())
                        .collect(),
                    total: window.saturating_add(1),
                    limit: window,
                },
            );
        }
        let mut prefetched = None;
        let result = load_cached_email_page(
            &account_id,
            &mailbox_id,
            offset,
            limit,
            max_get,
            hint.as_ref(),
            |method, args| {
                if method == "Email/query" {
                    let (query, details) = self.query_page(&mailbox_id, args, !can_reuse)?;
                    if let Some(details) = details {
                        self.remember_response_blobs(&query["ids"], &details);
                        prefetched = Some((query["ids"].clone(), details));
                    }
                    return Ok(query);
                }
                if method == "Email/get"
                    && let Some((ids, details)) = prefetched.take()
                    && ids == args["ids"]
                {
                    return Ok(details);
                }
                self.call_with_blobs(method, args)
            },
        );
        let (mut page, state) = match result {
            Ok(result) => result,
            Err(error) => {
                self.queries.remove(&mailbox_id);
                return Err(error);
            }
        };
        self.share_account_headers(&mut page.messages);
        let query_state = (offset == 0 && state.is_some())
            .then(|| {
                self.queries
                    .get(&mailbox_id)
                    .map(|query| query.state.clone())
            })
            .flatten();
        self.checkpoint = Some(CacheIdentity::Jmap {
            account_id,
            mailbox_id,
            email_state: state,
            query_state,
        });
        Ok(page)
    }

    fn query_mailbox(
        &mut self,
        name: &str,
        view: &super::MailboxView,
        offset: usize,
        limit: usize,
    ) -> Result<MessagePage> {
        self.native_query(name, view, offset, limit)
    }

    fn list_native_attachments(
        &mut self,
        mailbox: &str,
        id: &MessageId,
    ) -> Result<Option<Vec<super::RemoteAttachment>>> {
        let mailbox_id = self.mailbox_id(mailbox)?;
        self.native_attachments(id, mailbox_id)
    }

    fn download_native_attachment_into(
        &mut self,
        mailbox: &str,
        attachment: &super::RemoteAttachment,
        writer: &mut dyn Write,
    ) -> Result<()> {
        let mailbox_id = self.mailbox_id(mailbox)?;
        let identity = CacheIdentity::Jmap {
            account_id: self.session()?.account_id.clone(),
            mailbox_id,
            email_state: None,
            query_state: None,
        };
        ensure!(
            identity.same_generation(&attachment.identity),
            "JMAP attachment belongs to a different account or mailbox generation"
        );
        ensure!(
            attachment.size <= MAX_EXPORT_MESSAGE_BYTES,
            "attachment exceeds the 1 GiB export limit"
        );
        let mut response = self.download_blob(&attachment.blob_id)?;
        // Bound the stream to the advertised decoded byte count plus one.
        let mut bounded = response
            .body_mut()
            .as_reader()
            .take(attachment.size as u64 + 1);
        let written = io::copy(&mut bounded, writer)?;
        ensure!(
            written == attachment.size as u64,
            "JMAP attachment size mismatch"
        );
        Ok(())
    }

    fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody> {
        let _ = self.mailbox_id(mailbox)?;
        if let Some(body) = self.native_body(id)? {
            return Ok(body);
        }
        let mut response = self.raw_response(id)?;
        let raw = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_MESSAGE_BYTES as u64)
            .read_to_vec()
            .map_err(|error| BackendError::transport("reading JMAP message body", &error))?;
        parse_body(id.clone(), &raw)
    }

    fn fetch_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        let _ = self.mailbox_id(mailbox)?;
        let mut response = self.raw_response(id)?;
        Ok(response
            .body_mut()
            .with_config()
            .limit(MAX_RAW_MESSAGE_BYTES as u64)
            .read_to_vec()
            .map_err(|error| BackendError::transport("reading JMAP message blob", &error))?)
    }

    fn export_raw_into(
        &mut self,
        mailbox: &str,
        id: &MessageId,
        writer: &mut dyn Write,
    ) -> Result<()> {
        let _ = self.mailbox_id(mailbox)?;
        let mut response = self.raw_response(id)?;
        // Avoid unbounded disk use when a server returns an oversized blob.
        let mut bounded = response
            .body_mut()
            .as_reader()
            .take((MAX_EXPORT_MESSAGE_BYTES as u64) + 1);
        let written = io::copy(&mut bounded, writer)?;
        ensure!(
            written <= MAX_EXPORT_MESSAGE_BYTES as u64,
            "JMAP raw export exceeds size limit"
        );
        Ok(())
    }

    fn append_message(&mut self, mailbox: &str, raw: &[u8], seen: bool) -> Result<()> {
        self.append(
            mailbox,
            raw,
            if seen {
                json!({"$seen": true})
            } else {
                json!({})
            },
        )
    }

    fn append_draft(&mut self, mailbox: &str, raw: &[u8]) -> Result<()> {
        self.append(mailbox, raw, json!({"$seen": true, "$draft": true}))
    }

    fn set_seen(&mut self, mailbox: &str, id: &MessageId, seen: bool) -> Result<()> {
        let _ = self.mailbox_id(mailbox)?;
        let mut updates = serde_json::Map::new();
        updates.insert(
            id.0.clone(),
            json!({"keywords/$seen": if seen { Value::Bool(true) } else { Value::Null }}),
        );
        self.set_emails(Value::Object(updates), Vec::new())?
            .finish()
    }

    fn delete(&mut self, mailbox: &str, id: &MessageId) -> Result<()> {
        self.delete_many(mailbox, std::slice::from_ref(id))
    }

    fn delete_many(&mut self, mailbox: &str, ids: &[MessageId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let source_id = self.mailbox_id(mailbox)?;
        let ids = unique_message_ids(ids)?;
        let max_set = self.session()?.max_set;
        run_mutation_batches(&ids, max_set, |chunk| {
            let (state, memberships) = self.memberships_for(chunk)?;
            let (updates, destroy) = deletion_plan(&source_id, chunk, &memberships)?;
            self.set_emails_at_state(Value::Object(updates), destroy, Some(&state))
        })
    }

    fn move_to(&mut self, source: &str, destination: &str, id: &MessageId) -> Result<()> {
        self.move_many(source, destination, std::slice::from_ref(id))
    }

    fn move_many(&mut self, source: &str, destination: &str, ids: &[MessageId]) -> Result<()> {
        if source == destination || ids.is_empty() {
            return Ok(());
        }
        let src = self.mailbox_id(source)?;
        let account = self.session()?.account_id.clone();
        let dest = self.mailbox_id(destination)?;
        if src == dest {
            return Ok(());
        }
        ensure!(
            self.session()?.account_id == account,
            "JMAP account changed during move; refresh first"
        );
        // JMAP JSON PatchObject key segments are JSON-pointer escaped.
        let add = format!("mailboxIds/{}", pointer_segment(&dest));
        let remove = format!("mailboxIds/{}", pointer_segment(&src));
        let ids = unique_message_ids(ids)?;
        let max_set = self.session()?.max_set;
        run_mutation_batches(&ids, max_set, |chunk| {
            let mut updates = serde_json::Map::new();
            for id in chunk {
                let mut patch = serde_json::Map::new();
                patch.insert(add.clone(), Value::Bool(true));
                patch.insert(remove.clone(), Value::Null);
                updates.insert(id.0.clone(), Value::Object(patch));
            }
            self.set_emails(Value::Object(updates), Vec::new())
        })
    }
}

/// Keep the wire format in one place, shared by all JMAP methods and tests.
fn decode_response<T: serde::de::DeserializeOwned>(value: Value, label: &str) -> Result<T> {
    let invalid = || {
        BackendError::new(
            ErrorKind::InvalidResponse,
            format!("invalid JMAP {label} response"),
        )
    };
    if !value.is_object() {
        return Err(invalid().into());
    }
    serde_json::from_value(value).map_err(|_| invalid().into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum MethodErrorType {
    UnknownMethod,
    InvalidArguments,
    AccountNotFound,
    AccountNotSupportedByMethod,
    RequestTooLarge,
    CannotCalculateChanges,
    ServerFail,
    Forbidden,
    RateLimit,
    StateMismatch,
    TooManyChanges,
    InvalidResultReference,
    #[serde(other)]
    Unknown,
}
impl MethodErrorType {
    fn label(self) -> &'static str {
        match self {
            Self::UnknownMethod => "unknownMethod",
            Self::InvalidArguments => "invalidArguments",
            Self::AccountNotFound => "accountNotFound",
            Self::AccountNotSupportedByMethod => "accountNotSupportedByMethod",
            Self::RequestTooLarge => "requestTooLarge",
            Self::CannotCalculateChanges => "cannotCalculateChanges",
            Self::ServerFail => "serverFail",
            Self::Forbidden => "forbidden",
            Self::RateLimit => "rateLimit",
            Self::StateMismatch => "stateMismatch",
            Self::TooManyChanges => "tooManyChanges",
            Self::InvalidResultReference => "invalidResultReference",
            Self::Unknown => "server error",
        }
    }
    fn kind(self) -> ErrorKind {
        match self {
            Self::Forbidden => ErrorKind::PermissionDenied,
            Self::RateLimit => ErrorKind::RateLimited,
            Self::StateMismatch | Self::CannotCalculateChanges => ErrorKind::StaleState,
            _ => ErrorKind::Other,
        }
    }
}

#[derive(Deserialize)]
struct MethodFailure {
    #[serde(rename = "type")]
    kind: MethodErrorType,
}

fn method_failure(method: &str, kind: MethodErrorType) -> anyhow::Error {
    // Both contexts are typed. The outer diagnostic deliberately excludes
    // arbitrary server-controlled descriptions and error properties.
    anyhow::Error::new(kind).context(BackendError::new(
        kind.kind(),
        format!("JMAP {method} request failed ({})", kind.label()),
    ))
}

impl std::fmt::Display for MethodErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::error::Error for MethodErrorType {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MethodResponseEnvelope {
    method_responses: Vec<(String, Value, String)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetResponse<T> {
    list: Vec<T>,
    #[serde(default)]
    not_found: Vec<String>,
    state: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailboxWire {
    id: String,
    name: String,
    parent_id: Option<String>,
    role: Option<String>,
    total_emails: usize,
    unread_emails: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailboxCountsWire {
    id: String,
    total_emails: usize,
    unread_emails: usize,
}

fn method_request(name: &str, params: Value) -> Value {
    let using = if name.starts_with("Identity/") || name.starts_with("EmailSubmission/") {
        vec![JMAP_CORE, JMAP_MAIL, JMAP_SUBMISSION]
    } else {
        vec![JMAP_CORE, JMAP_MAIL]
    };
    json!({"using": using, "methodCalls": [[name, params, "m1"]]})
}

fn query_get_request(query: Value, account: &str) -> Value {
    json!({"using": [JMAP_CORE, JMAP_MAIL], "methodCalls": [
        ["Email/query", query, "q"],
        ["Email/get", {"accountId": account, "#ids": {"resultOf": "q", "name": "Email/query", "path": "/ids"},
            "properties": EMAIL_SUMMARY_PROPERTIES}, "g"],
    ]})
}

fn extract_query_get(reply: Value) -> Result<(Value, Option<Value>)> {
    let envelope: MethodResponseEnvelope = decode_response(reply, "methodResponses")?;
    ensure!(
        envelope.method_responses.len() == 2,
        "JMAP returned an unexpected number of method responses"
    );
    let mut responses = envelope.method_responses.into_iter();
    let query = extract_correlated(responses.next().unwrap(), "Email/query", "q")?;
    let details = match extract_correlated(responses.next().unwrap(), "Email/get", "g") {
        Ok(details) => Some(details),
        Err(error)
            if changes_unavailable(&error)
                || error.downcast_ref::<MethodErrorType>()
                    == Some(&MethodErrorType::InvalidResultReference) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    Ok((query, details))
}

fn validate_account(response: &Value, account: &str) -> Result<()> {
    if response["accountId"].as_str() != Some(account) {
        return Err(BackendError::new(
            ErrorKind::InvalidResponse,
            "JMAP response account does not match request",
        )
        .into());
    }
    Ok(())
}

fn id_array(value: &Value) -> Option<Vec<String>> {
    let ids = value
        .as_array()?
        .iter()
        .map(|v| v.as_str().filter(|id| !id.is_empty()).map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    (ids.iter().collect::<HashSet<_>>().len() == ids.len()).then_some(ids)
}

fn query_snapshot(query: &Value, limit: usize) -> Option<QuerySnapshot> {
    if !query["canCalculateChanges"].as_bool()? {
        return None;
    }
    let ids = id_array(&query["ids"])?;
    let total = usize::try_from(query["total"].as_u64()?).ok()?;
    if ids.len() != total.min(limit) {
        return None;
    }
    Some(QuerySnapshot {
        state: query["queryState"].as_str()?.to_owned(),
        ids,
        total,
        limit,
    })
}

fn apply_query_changes(old: &QuerySnapshot, response: &Value) -> Option<QuerySnapshot> {
    if response["oldQueryState"].as_str()? != old.state {
        return None;
    }
    let total = usize::try_from(response["total"].as_u64()?).ok()?;
    let removed = id_array(&response["removed"])?;
    let added = response["added"].as_array()?;
    if removed.len().saturating_add(added.len()) > MAX_CHANGES {
        return None;
    }
    let mut ids = old.ids.clone();
    ids.retain(|id| !removed.contains(id));
    let mut previous = None;
    let mut seen = HashSet::new();
    for item in added {
        let index = usize::try_from(item["index"].as_u64()?).ok()?;
        let id = item["id"].as_str()?.to_owned();
        if id.is_empty()
            || index >= total
            || previous.is_some_and(|p| index <= p)
            || !seen.insert(id.clone())
            || ids.contains(&id)
        {
            return None;
        }
        previous = Some(index);
        // Unknown tail entries cannot be shifted into our known prefix. If
        // there is a gap in the requested window, re-query instead.
        if index <= ids.len() && index < old.limit {
            ids.insert(index, id);
        }
    }
    ids.truncate(old.limit);
    if ids.len() != total.min(old.limit) {
        return None;
    }
    Some(QuerySnapshot {
        state: response["newQueryState"].as_str()?.to_owned(),
        ids,
        total,
        limit: old.limit,
    })
}

fn parse_mailboxes(response: Value) -> Result<(HashMap<String, JmapMailbox>, Option<String>)> {
    let response: GetResponse<MailboxWire> = decode_response(response, "Mailbox/get")?;
    let mut mailboxes = HashMap::new();
    for item in response.list {
        ensure!(
            !item.id.is_empty() && item.unread_emails <= item.total_emails,
            "invalid JMAP mailbox ID or counts"
        );
        let entry = JmapMailbox {
            id: item.id.clone(),
            name: item.name,
            parent: item.parent_id,
            role: role(item.role.as_deref()),
            total: item.total_emails,
            unread: item.unread_emails,
        };
        ensure!(
            mailboxes.insert(item.id, entry).is_none(),
            "duplicate JMAP mailbox ID"
        );
    }
    Ok((mailboxes, response.state))
}

type MailboxDelta = (HashMap<String, JmapMailbox>, String, Vec<String>, bool);

fn apply_mailbox_changes(
    old: &HashMap<String, JmapMailbox>,
    state: &str,
    response: &Value,
) -> Option<MailboxDelta> {
    if response["oldState"].as_str()? != state || response["hasMoreChanges"].as_bool()? {
        return None;
    }
    let new_state = response["newState"].as_str()?.to_owned();
    let mut ids = id_array(&response["created"])?;
    ids.extend(id_array(&response["updated"])?);
    let destroyed = id_array(&response["destroyed"])?;
    if ids.len().saturating_add(destroyed.len()) > MAX_CHANGES
        || ids.iter().chain(&destroyed).collect::<HashSet<_>>().len() != ids.len() + destroyed.len()
    {
        return None;
    }
    let counts_only = response["updatedProperties"]
        .as_array()
        .is_some_and(|properties| {
            properties
                .iter()
                .all(|property| matches!(property.as_str(), Some("totalEmails" | "unreadEmails")))
                && !properties.is_empty()
        })
        && response["created"].as_array().is_some_and(Vec::is_empty)
        && ids.iter().all(|id| old.contains_key(id));
    let mut mailboxes = old.clone();
    for id in destroyed {
        mailboxes.remove(&id);
    }
    Some((mailboxes, new_state, ids, counts_only))
}

fn email_get_arguments(ids: &[String]) -> Value {
    json!({"ids": ids, "properties": EMAIL_SUMMARY_PROPERTIES})
}

fn email_query_arguments(mailbox_id: &str, offset: usize, limit: usize) -> Value {
    json!({
        "filter": {"inMailbox": mailbox_id},
        "sort": [{"property": "receivedAt", "isAscending": false}],
        "position": offset,
        "limit": limit,
        "calculateTotal": true,
    })
}

/// Fetch requested IDs in protocol-sized batches, not assuming that the list
/// response order matches the query order. Missing IDs are permitted only if
/// explicitly named in `notFound`: a message can disappear between query/get.
fn request_email_details(
    ids: &[String],
    max_get: usize,
    mut call: impl FnMut(&str, Value) -> Result<Value>,
) -> Result<HashMap<String, Value>> {
    ensure!(max_get > 0, "invalid JMAP Email/get batch limit");
    ensure!(
        ids.iter().collect::<HashSet<_>>().len() == ids.len(),
        "duplicate JMAP query IDs"
    );
    let mut results = HashMap::new();
    for chunk in ids.chunks(max_get) {
        let requested: HashSet<&str> = chunk.iter().map(String::as_str).collect();
        ensure!(requested.len() == chunk.len(), "duplicate JMAP query IDs");
        let response: GetResponse<Value> =
            decode_response(call("Email/get", email_get_arguments(chunk))?, "Email/get")?;
        let mut accounted = HashSet::new();
        for email in response.list {
            let id = value_string(&email, "id")?;
            ensure!(
                requested.contains(id.as_str()),
                "JMAP Email/get returned an unrequested ID"
            );
            ensure!(
                accounted.insert(id.clone()),
                "JMAP Email/get returned duplicate IDs"
            );
            results.insert(id, email);
        }
        for id in response.not_found {
            ensure!(
                requested.contains(id.as_str()),
                "JMAP notFound contained an unrequested ID"
            );
            ensure!(
                accounted.insert(id),
                "JMAP Email/get counted the same ID twice"
            );
        }
        ensure!(
            accounted.len() == requested.len(),
            "JMAP Email/get omitted a requested message without notFound"
        );
    }
    Ok(results)
}

/// A mailbox query is followed by Email/get only when it returns IDs.
/// Keep the pagination cursor based on *queried* IDs, not surviving Email/get
/// responses, so deletions between the two calls cannot replay pages.
#[cfg(test)]
fn load_email_page(
    mailbox_id: &str,
    offset: usize,
    limit: usize,
    max_get: usize,
    call: impl FnMut(&str, Value) -> Result<Value>,
) -> Result<MessagePage> {
    load_cached_email_page("test", mailbox_id, offset, limit, max_get, None, call)
        .map(|(page, _)| page)
}

/// Query the bounded current window every time. Reuse summaries only if a
/// complete Email/changes response proves they were not modified. The state
/// is an account-wide opaque token and is not part of body-cache identity.
fn load_cached_email_page(
    account_id: &str,
    mailbox_id: &str,
    offset: usize,
    limit: usize,
    max_get: usize,
    hint: Option<&MailboxSyncHint>,
    mut call: impl FnMut(&str, Value) -> Result<Value>,
) -> Result<(MessagePage, Option<String>)> {
    let limit = if limit == 0 {
        MAX_PAGE
    } else {
        limit.min(MAX_PAGE)
    };
    let response = call(
        "Email/query",
        email_query_arguments(mailbox_id, offset, limit),
    )?;
    let ids = response["ids"]
        .as_array()
        .ok_or_else(|| anyhow!("JMAP Email/query returned no ids"))?
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("invalid JMAP message ID"))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        ids.len() <= limit,
        "JMAP Email/query exceeded requested page limit"
    );
    ensure!(
        ids.iter().collect::<HashSet<_>>().len() == ids.len(),
        "duplicate JMAP query IDs"
    );
    let mut reused = HashMap::new();
    let mut expected_state = None;
    if offset == 0
        && !ids.is_empty()
        && let Some(hint) = hint
        && let CacheIdentity::Jmap {
            account_id: old_account,
            mailbox_id: old_mailbox,
            email_state: Some(old_state),
            ..
        } = &hint.identity
        && old_account == account_id
        && old_mailbox == mailbox_id
        && !hint.messages.is_empty()
    {
        // Expired state / unsupported changes: a full read is safe. Do not
        // retry ambiguous writes; this path contains only read-only methods.
        let changes = match call(
            "Email/changes",
            json!({"sinceState": old_state, "maxChanges": 512}),
        ) {
            Ok(changes) => Some(changes),
            Err(error) if changes_unavailable(&error) => None,
            Err(error) => return Err(error),
        };
        if let Some(changes) = changes.as_ref()
            && let Some((state, changed)) = complete_email_changes(changes, old_state)
        {
            expected_state = Some(state);
            for message in &hint.messages {
                if message.thread_id.is_some()
                    && ids.contains(&message.id.0)
                    && !changed.contains(message.id.0.as_str())
                {
                    reused.insert(message.id.0.clone(), message.clone());
                }
            }
        }
    }
    let missing = ids
        .iter()
        .filter(|id| !reused.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    let (mut details, mut state) = get_details_at_state(&missing, max_get, &mut call)?;
    if let Some(expected) = expected_state
        && !reused.is_empty()
    {
        if missing.is_empty() || state.as_deref() == Some(expected.as_str()) {
            state = Some(expected);
        } else {
            // A mutation raced the changes/get sequence. Never stamp reused
            // rows with a newer state they were not checked against.
            reused.clear();
            (details, state) = get_details_at_state(&ids, max_get, &mut call)?;
        }
    }
    let mut messages = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(message) = reused.remove(id) {
            messages.push(message);
        } else if let Some(email) = details.remove(id) {
            messages.push(summary_from_email(&email)?);
        }
    }
    // A missing row during a query/get race makes the window incomplete.
    // The UI only combines page checkpoints if both state tokens match.
    if messages.len() != ids.len() {
        state = None;
    }
    let next_offset = offset.saturating_add(ids.len());
    let has_more = response["total"]
        .as_u64()
        .map_or(ids.len() == limit, |total| (next_offset as u64) < total);
    Ok((
        MessagePage {
            messages,
            offset,
            next_offset,
            has_more,
        },
        state,
    ))
}

fn complete_email_changes<'a>(
    response: &'a Value,
    since: &str,
) -> Option<(String, HashSet<&'a str>)> {
    if response["oldState"].as_str()? != since || response["hasMoreChanges"].as_bool()? {
        return None;
    }
    let state = response["newState"].as_str()?.to_owned();
    let mut changed = HashSet::new();
    for key in ["created", "updated", "destroyed"] {
        for id in response[key].as_array()? {
            changed.insert(id.as_str()?);
        }
    }
    Some((state, changed))
}

fn get_details_at_state(
    ids: &[String],
    max_get: usize,
    mut call: impl FnMut(&str, Value) -> Result<Value>,
) -> Result<(HashMap<String, Value>, Option<String>)> {
    let mut state = None::<String>;
    let mut coherent = true;
    let details = request_email_details(ids, max_get, |method, args| {
        let response = call(method, args)?;
        if let Some(next) = response["state"].as_str() {
            if state.as_deref().is_some_and(|previous| previous != next) {
                coherent = false;
            }
            state = Some(next.to_owned());
        } else {
            coherent = false;
        }
        Ok(response)
    })?;
    Ok((details, if coherent { state } else { None }))
}

/// RFC 8620 SetResponse permits null or absent maps. Reject wrong types,
/// unsolicited IDs, duplicates, and IDs claimed as both success and failure.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetResponse {
    updated: Option<HashMap<String, Option<serde_json::Map<String, Value>>>>,
    destroyed: Option<Vec<String>>,
    not_updated: Option<HashMap<String, SetFailure>>,
    not_destroyed: Option<HashMap<String, SetFailure>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportResponse {
    created: Option<HashMap<String, ImportedEmail>>,
    not_created: Option<HashMap<String, SetFailure>>,
}

#[derive(Deserialize)]
struct ImportedEmail {
    id: String,
}

#[cfg(test)]
fn validate_import_result(response: Value) -> Result<()> {
    imported_email_id(response).map(|_| ())
}

fn imported_email_id(response: Value) -> Result<String> {
    let response: ImportResponse = decode_response(response, "Email/import")?;
    let created = response.created.unwrap_or_default();
    let failed = response.not_created.unwrap_or_default();
    if created.len() == 1
        && failed.is_empty()
        && let Some(email) = created.get("m1").filter(|email| !email.id.is_empty())
    {
        return Ok(email.id.clone());
    }
    if created.is_empty()
        && failed.len() == 1
        && failed.get("m1").is_some_and(|error| !error.kind.is_empty())
    {
        return Err(BackendError::new(
            ErrorKind::PartialMutation,
            "JMAP Email/import rejected; message not created",
        )
        .into());
    }
    Err(BackendError::new(
        ErrorKind::InvalidResponse,
        "JMAP Email/import result uncertain; refresh to reconcile",
    )
    .into())
}

#[derive(Deserialize)]
struct SetFailure {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Default)]
struct MutationReport {
    confirmed: usize,
    rejected: usize,
}

impl MutationReport {
    fn finish(&self) -> Result<()> {
        if self.rejected == 0 {
            return Ok(());
        }
        Err(BackendError::new(
            ErrorKind::PartialMutation,
            format!(
                "JMAP message changes: {} confirmed, {} rejected; refresh to reconcile",
                self.confirmed, self.rejected
            ),
        )
        .into())
    }
}

fn validate_set_result(
    response: Value,
    expected_updates: &HashSet<String>,
    expected_destroy: &HashSet<String>,
) -> Result<MutationReport> {
    let response: SetResponse = decode_response(response, "Email/set")?;
    let updated = response.updated.unwrap_or_default();
    let destroyed = response.destroyed.unwrap_or_default();
    let not_updated = response.not_updated.unwrap_or_default();
    let not_destroyed = response.not_destroyed.unwrap_or_default();
    if !valid_partition(updated.keys(), &not_updated, expected_updates)
        || !valid_partition(destroyed.iter(), &not_destroyed, expected_destroy)
    {
        return Err(BackendError::new(ErrorKind::InvalidResponse,
            "JMAP message change result uncertain: response IDs do not match request; refresh to reconcile").into());
    }
    Ok(MutationReport {
        confirmed: updated.len() + destroyed.len(),
        rejected: not_updated.len() + not_destroyed.len(),
    })
}

fn valid_partition<'a>(
    ok: impl ExactSizeIterator<Item = &'a String>,
    failed: &'a HashMap<String, SetFailure>,
    expected: &HashSet<String>,
) -> bool {
    let count = ok.len() + failed.len();
    let actual: HashSet<&String> = ok.chain(failed.keys()).collect();
    actual.len() == count
        && actual.len() == expected.len()
        && actual.iter().all(|id| expected.contains(*id))
        && failed.values().all(|error| !error.kind.is_empty())
}

/// A delete in a multi-label JMAP mailbox must only remove that label.
/// Permanently destroying an Email is appropriate only when it has no other
/// mailbox memberships. Always submit this plan with ifInState from preflight.
fn deletion_plan(
    source: &str,
    ids: &[MessageId],
    memberships: &HashMap<String, HashSet<String>>,
) -> Result<(serde_json::Map<String, Value>, Vec<String>)> {
    let mut updates = serde_json::Map::new();
    let mut destroy = Vec::new();
    for id in ids {
        let member_of = memberships
            .get(&id.0)
            .ok_or_else(|| anyhow!("JMAP message membership missing during delete"))?;
        ensure!(
            member_of.contains(source),
            "JMAP message is no longer in the selected mailbox"
        );
        if member_of.len() == 1 {
            destroy.push(id.0.clone());
        } else {
            updates.insert(
                id.0.clone(),
                json!({
                    (format!("mailboxIds/{}", pointer_segment(source))): null
                }),
            );
        }
    }
    Ok((updates, destroy))
}

fn unique_message_ids(ids: &[MessageId]) -> Result<Vec<MessageId>> {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(ids.len());
    for id in ids {
        ensure!(!id.0.is_empty(), "invalid empty JMAP message ID");
        if seen.insert(id) {
            unique.push(id.clone());
        }
    }
    Ok(unique)
}

/// Continue after explicit per-ID failures, but never replay a write or send
/// another batch after an ambiguous response. Preserve acknowledged progress
/// in diagnostics even when a later HTTP request fails.
fn run_mutation_batches(
    ids: &[MessageId],
    max_set: usize,
    mut apply: impl FnMut(&[MessageId]) -> Result<MutationReport>,
) -> Result<()> {
    ensure!(max_set > 0, "invalid JMAP mutation batch limit");
    let mut total = MutationReport::default();
    for chunk in ids.chunks(max_set) {
        let report = apply(chunk).with_context(|| format!(
            "JMAP message changes: {} confirmed, {} rejected before failed batch; remaining results not confirmed; refresh to reconcile",
            total.confirmed, total.rejected))?;
        total.confirmed += report.confirmed;
        total.rejected += report.rejected;
    }
    total.finish()
}

fn extract_method_response(reply: Value, expected: &str) -> Result<Value> {
    let envelope: MethodResponseEnvelope = decode_response(reply, "methodResponses")?;
    ensure!(
        envelope.method_responses.len() == 1,
        "JMAP returned an unexpected number of method responses"
    );
    extract_correlated(
        envelope.method_responses.into_iter().next().unwrap(),
        expected,
        "m1",
    )
}

fn extract_correlated(
    response: (String, Value, String),
    expected: &str,
    call_id: &str,
) -> Result<Value> {
    let (name, arguments, response_id) = response;
    ensure!(response_id == call_id, "JMAP response correlation mismatch");
    if name == "error" {
        let failure: MethodFailure = decode_response(arguments, "method error")?;
        return Err(method_failure(expected, failure.kind));
    }
    ensure!(
        name == expected && arguments.is_object(),
        "JMAP server returned an unexpected method or invalid arguments"
    );
    Ok(arguments)
}

fn changes_unavailable(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<MethodErrorType>(),
        Some(
            MethodErrorType::CannotCalculateChanges
                | MethodErrorType::UnknownMethod
                | MethodErrorType::RequestTooLarge
                | MethodErrorType::TooManyChanges
        )
    )
}

fn expand_url(template: &str, account: &str, blob: &str, name: &str) -> Result<String> {
    let encoded = |value: &str| utf8_percent_encode(value, NON_ALPHANUMERIC).to_string();
    let url = template
        .replace("{accountId}", &encoded(account))
        .replace("{blobId}", &encoded(blob))
        .replace("{name}", &encoded(name))
        .replace("{type}", "application%2Foctet-stream");
    ensure!(
        !url.contains('{') && !url.contains('}'),
        "JMAP endpoint uses unknown template variables"
    );
    Ok(url)
}

fn role(input: Option<&str>) -> MailboxRole {
    match input.unwrap_or("") {
        "inbox" => MailboxRole::Inbox,
        "sent" => MailboxRole::Sent,
        "drafts" => MailboxRole::Drafts,
        "trash" => MailboxRole::Trash,
        "junk" => MailboxRole::Junk,
        "archive" => MailboxRole::Archive,
        _ => MailboxRole::Other,
    }
}

fn mailbox_path(mailboxes: &HashMap<String, JmapMailbox>, id: &str) -> Result<String> {
    let mut seen = HashSet::new();
    let mut components = Vec::new();
    let mut current = id;
    loop {
        ensure!(
            seen.insert(current.to_owned()) && components.len() < 32,
            "JMAP mailbox hierarchy contains a cycle"
        );
        let mailbox = mailboxes
            .get(current)
            .ok_or_else(|| anyhow!("JMAP mailbox has an unknown parent"))?;
        ensure!(!mailbox.name.is_empty(), "JMAP mailbox has an empty name");
        components.push(mailbox.name.as_str());
        let Some(parent) = mailbox.parent.as_deref() else {
            break;
        };
        current = parent;
    }
    components.reverse();
    Ok(components.join("/"))
}

fn pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}
fn value_string(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("JMAP response missing {key}"))
}
fn addresses(email: &Value, field: &str) -> String {
    email[field].as_array().map_or(String::new(), |list| {
        list.iter()
            .filter_map(|item| {
                let addr = item["email"].as_str()?;
                let display = item["name"].as_str().unwrap_or("");
                Some(if display.is_empty() {
                    addr.to_owned()
                } else {
                    format!("{display} <{addr}>")
                })
            })
            .collect::<Vec<_>>()
            .join(", ")
    })
}
fn message_ids(email: &Value, field: &str) -> Result<Vec<String>> {
    match email.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("JMAP email has a malformed message-id array"))
            })
            .collect(),
        _ => bail!("JMAP email has a malformed message-id field"),
    }
}

fn summary_from_email(email: &Value) -> Result<MessageSummary> {
    let id = value_string(email, "id")?;
    let parsed_date = email["receivedAt"]
        .as_str()
        .and_then(|date| DateTime::parse_from_rfc3339(date).ok());
    let timestamp = parsed_date.as_ref().map_or(0, DateTime::timestamp);
    let date_label: Box<str> = parsed_date
        .as_ref()
        .map_or(String::new(), |date| date.format("%m-%d").to_string())
        .into();
    let from = email["from"].as_array().and_then(|a| a.first());
    let from_name = from
        .and_then(|v| v["name"].as_str())
        .unwrap_or("")
        .to_owned();
    let from_address = from
        .and_then(|v| v["email"].as_str())
        .unwrap_or("")
        .to_owned();
    let keywords = &email["keywords"];
    let message_id = message_ids(email, "messageId")?.into_iter().next();
    let in_reply_to = message_ids(email, "inReplyTo")?.into_iter().next();
    let references = message_ids(email, "references")?;
    let mut flags = MessageFlags::empty();
    for (keyword, bit) in [
        ("$seen", MessageFlags::SEEN),
        ("$answered", MessageFlags::ANSWERED),
        ("$flagged", MessageFlags::FLAGGED),
        ("$draft", MessageFlags::DRAFT),
        ("$forwarded", MessageFlags::FORWARDED),
    ] {
        flags.set(bit, keywords[keyword].as_bool() == Some(true));
    }
    Ok(MessageSummary {
        headers: Arc::new(MessageHeaders {
            id: MessageId(id),
            thread_id: email["threadId"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
            message_id,
            in_reply_to,
            references: references.into(),
            from_name,
            from_address,
            to: addresses(email, "to"),
            cc: addresses(email, "cc"),
            bcc: addresses(email, "bcc"),
            subject: email["subject"]
                .as_str()
                .unwrap_or("(no subject)")
                .to_owned(),
            timestamp,
            date_label,
            size: email["size"].as_u64().unwrap_or(0),
            has_attachment_hint: email["hasAttachment"].as_bool() == Some(true),
            attachment_names_hint: Vec::new().into(),
        }),
        flags,
    })
}

#[cfg(test)]
mod tests;
