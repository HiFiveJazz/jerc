use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use io_imap::client::{
    ImapClient, ImapClientError, ImapClientStd, ImapClientStdConnectOptions,
    ImapMailboxWatchStreamOptions,
};
use io_imap::rfc3501::{
    append::ImapMessageAppendOptions, copy::ImapMessageCopyOptions, fetch::ImapMessageFetchOptions,
    login::ImapLoginOptions, select::ImapMailboxSelectOptions, store::ImapMessageStoreOptions,
};
use io_imap::rfc6851::r#move::ImapMessageMoveOptions;
use io_imap::rfc7628::auth_oauthbearer::ImapAuthOauthbearerOptions;
use io_imap::sasl::auth_xoauth2::ImapAuthXoauth2Options;
use io_imap::send::ImapOutput;
use io_imap::types::{
    body::{BodyStructure, SpecificFields},
    fetch::{MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName},
    flag::{Flag, FlagFetch, FlagNameAttribute, StoreType},
    mailbox::{ListMailbox, Mailbox as ImapMailbox},
    response::Capability,
    sequence::SequenceSet,
    status::{StatusDataItem, StatusDataItemName},
};
use io_imap::watch::{ImapMailboxWatchError, ImapMailboxWatchState};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use url::Url;

use crate::backend::{MailBackend, MailboxSyncHint, MailboxView};
mod threads;
use self::threads as imap_threads;
use crate::mail::{
    MAX_BODY_MESSAGE_BYTES, MAX_RAW_MESSAGE_BYTES, parse_body_compact, parse_summary,
};
use crate::model::{
    Capabilities, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId, MessagePage,
};

const IO_TIMEOUT: Duration = Duration::from_secs(90);
const OAUTH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_IDLE_SHADOW_MESSAGES: usize = 3_000;
const IDLE_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// io-imap 0.11 can recover individual FETCH fields while reporting the rest
/// in `partial` / `skipped`. A message page is atomic from Jerc's point of
/// view: accepting only the decoded subset can silently drop cached mail or
/// advance pagination past messages the user has never seen.
fn complete_imap_output<T>(output: ImapOutput<T>, operation: &str) -> Result<T> {
    if !output.partial.is_empty() || !output.skipped.is_empty() {
        let missing_items: usize = output.partial.iter().map(|part| part.failed.len()).sum();
        bail!(
            "incomplete IMAP {operation}: {} partially decoded responses ({} missing items), {} undecodable responses; refresh to retry",
            output.partial.len(),
            missing_items,
            output.skipped.len()
        );
    }
    // ALERT response text is intentionally not copied to terminal diagnostics:
    // servers can include untrusted content, and the TUI currently has no
    // dedicated sanitized alert surface.
    Ok(output.data)
}

/// A previous IDLE watch can be resumed only for the identical mailbox.
/// Its 3,000-message cap is enforced again before handing it to io-imap.
fn take_idle_checkpoint(
    checkpoint: &mut Option<(String, ImapMailboxWatchState)>,
    mailbox: &str,
) -> Option<ImapMailboxWatchState> {
    checkpoint.take().and_then(|(name, state)| {
        (name == mailbox && state.flags.len() <= MAX_IDLE_SHADOW_MESSAGES).then_some(state)
    })
}

fn watch_error_label(error: &ImapClientError) -> &'static str {
    if matches!(
        error,
        ImapClientError::MailboxWatch(ImapMailboxWatchError::UidValidityChanged { .. })
    ) {
        "mailbox UIDVALIDITY changed"
    } else if error.is_connection_lost() {
        "connection lost"
    } else {
        "IMAP watcher command failed"
    }
}

/// Reject data at the IMAP stream boundary, before a malicious literal can
/// allocate an arbitrary-sized Vec. The writer is not a disk cache.
struct LimitedMessageWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl LimitedMessageWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl Write for LimitedMessageWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other(format!(
                "IMAP message exceeds the {} MiB in-memory limit",
                self.limit / (1024 * 1024)
            )));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

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
    account_oauth: Option<crate::oauth::OAuthSettings>,
}

impl ImapSource {
    #[must_use]
    pub fn supports(source: &str) -> bool {
        source.split_once("://").is_some_and(|(scheme, _)| {
            matches!(
                scheme,
                "imaps"
                    | "imaps+login"
                    | "imaps+oauthbearer"
                    | "imaps+xoauth2"
                    | "imap+starttls"
                    | "imap+starttls+login"
                    | "imap+starttls+oauthbearer"
                    | "imap+starttls+xoauth2"
            )
        })
    }

    pub fn parse(source: &str) -> Result<Self> {
        let url = Url::parse(source).context("parsing IMAP source URI")?;
        let auth = match url.scheme() {
            "imaps" | "imap+starttls" => ImapAuth::Password,
            "imaps+login" | "imap+starttls+login" => ImapAuth::Login,
            "imaps+oauthbearer" | "imap+starttls+oauthbearer" => ImapAuth::OAuthBearer,
            "imaps+xoauth2" | "imap+starttls+xoauth2" => ImapAuth::XOAuth2,
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

        if matches!(auth, ImapAuth::OAuthBearer | ImapAuth::XOAuth2)
            && let Some(endpoint) = &token_endpoint
        {
            let parsed = Url::parse(endpoint).context("parsing OAuth token endpoint")?;
            if parsed.scheme() != "https" {
                bail!("OAuth token_endpoint must use https");
            }
        }

        Ok(Self {
            scheme: url.scheme().to_owned(),
            host,
            port: url
                .port()
                .unwrap_or(if url.scheme().starts_with("imap+starttls") {
                    143
                } else {
                    993
                }),
            username,
            secret,
            auth,
            token_endpoint,
            client_id,
            client_secret,
            scope,
            account_oauth: None,
        })
    }

    /// Supply an in-memory credential for a one-shot setup probe. This never
    /// changes the saved account or inserts passwords into a URI or log.
    pub(crate) fn with_probe_password(mut self, password: &str) -> Self {
        self.secret = Some(password.to_owned());
        self
    }

    /// The `imap+starttls` schemes require the STARTTLS upgrade before any
    /// authentication. No plain `imap://` credential scheme is accepted.
    pub fn requires_starttls(&self) -> bool {
        self.scheme.starts_with("imap+starttls")
    }

    /// Separate the transport options from the connection so tests can verify
    /// the mandatory TLS upgrade without depending on a public mail provider.
    fn connection_settings(&self) -> Result<(Url, ImapClientStdConnectOptions)> {
        let transport_scheme = if self.requires_starttls() {
            "imap"
        } else {
            "imaps"
        };
        let url = Url::parse(&format!("{transport_scheme}://{}:{}", self.host, self.port))
            .context("building IMAP connection URL")?;
        let mut options = ImapClientStdConnectOptions::default();
        options.session.starttls = self.requires_starttls();
        Ok((url, options))
    }

    pub fn with_account_oauth(mut self, account: &crate::config::AccountConfig) -> Result<Self> {
        if let Some(settings) = crate::oauth::OAuthSettings::for_account(account)? {
            self.auth = ImapAuth::XOAuth2;
            self.account_oauth = Some(settings);
        }
        Ok(self)
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
        if self.account_oauth.is_some() {
            // The OAuth token manager supplies short-lived access tokens.
            // Never read/copy the raw refresh token into an IMAP/SMTP worker
            // just to discard it during XOAUTH2 authentication.
            return Ok(String::new());
        }
        let command = credential_command.ok_or_else(|| {
            anyhow!(
                "IMAP source contains no credential; add a URI password/token or source_cred_cmd"
            )
        })?;
        run_credential_command(command)
    }

    fn access_token(&self, credential: &str) -> Result<String> {
        if let Some(oauth) = &self.account_oauth {
            // Stored refresh tokens can rotate; always read via the OAuth store.
            return oauth.access_token();
        }
        let Some(endpoint) = self.token_endpoint.as_deref() else {
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

        // An OAuth HTTP refresh is distinct from the IMAP SASL round trip.
        // Keep them separate in perf logs so a slow token server is not
        // misdiagnosed as an io-imap / Gmail IMAP delay. Never log secrets.
        let started = Instant::now();
        let refreshed = (|| -> Result<String> {
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
        })();
        crate::perf::record(
            "imap_oauth_http_refresh",
            None,
            started.elapsed(),
            refreshed.is_ok(),
        );
        refreshed
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
    session: Option<ImapClientStd>,
    selected_mailbox: Option<String>,
    selected_exists: Option<u32>,
    selected_uid_validity: Option<u32>,
    selected_uid_next: Option<u32>,
    // Discovered lazily: ordinary mailbox opening needs no extra round trip.
    thread_capabilities: Option<(bool, bool)>,
    thread_snapshot: Option<ImapThreadSnapshot>,
    // Kept only by the dedicated IDLE backend between reconnect attempts.
    // Never persisted, shared with the UI, or reused for another mailbox.
    idle_checkpoint: Option<(String, ImapMailboxWatchState)>,
}

/// The snapshot is mailbox- and UIDVALIDITY-scoped, and only reused for
/// pagination of the same view. An offset-zero refresh always rebuilds it.
struct ImapThreadSnapshot {
    mailbox: String,
    validity: u32,
    view: MailboxView,
    rows: Vec<(u32, Option<String>)>,
    memberships: HashMap<u32, String>,
}

impl std::fmt::Debug for ImapBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapBackend")
            .field("source", &self.source.redacted())
            .field(
                "credential_command",
                &self.credential_command.as_ref().map(|_| "<configured>"),
            )
            .field("max_messages", &self.max_messages)
            .field("connected", &self.session.is_some())
            .field("selected_mailbox", &self.selected_mailbox)
            .field("selected_exists", &self.selected_exists)
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
            selected_exists: None,
            selected_uid_validity: None,
            selected_uid_next: None,
            thread_capabilities: None,
            thread_snapshot: None,
            idle_checkpoint: None,
        }
    }

    fn session(&mut self) -> Result<&mut ImapClientStd> {
        self.session
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP account is not connected"))
    }

    fn select(&mut self, mailbox: &str) -> Result<io_imap::rfc3501::select::ImapMailboxSelectData> {
        let mailbox_name = mailbox_from_name(mailbox)?;
        let started = Instant::now();
        let selected_result = self
            .session()?
            .select(mailbox_name, ImapMailboxSelectOptions::default())
            .with_context(|| format!("selecting IMAP mailbox {mailbox:?}"));
        crate::perf::record(
            "imap_select",
            None,
            started.elapsed(),
            selected_result.is_ok(),
        );
        // io-imap 0.11 returns both command data and ancillary responses.
        // Keep the existing selection state based on the typed command data.
        let selected = complete_imap_output(selected_result?, "SELECT")?;
        self.selected_mailbox = Some(mailbox.to_owned());
        self.selected_exists = Some(selected.exists.unwrap_or(0));
        self.selected_uid_validity = selected.uid_validity.map(|uid| uid.get());
        self.selected_uid_next = selected.uid_next.map(|uid| uid.get());
        Ok(selected)
    }

    fn mailbox_exists(&mut self, mailbox: &str) -> Result<u32> {
        if self.selected_mailbox.as_deref() == Some(mailbox)
            && let Some(exists) = self.selected_exists
        {
            return Ok(exists);
        }
        let selected = self.select(mailbox)?;
        Ok(selected.exists.unwrap_or(0))
    }

    fn note_removed(&mut self, mailbox: &str, count: usize) {
        if self.selected_mailbox.as_deref() == Some(mailbox) {
            let count = u32::try_from(count).unwrap_or(u32::MAX);
            self.selected_exists = self
                .selected_exists
                .map(|exists| exists.saturating_sub(count));
        }
    }

    fn decode_uid(id: &MessageId) -> Result<(Option<u32>, u32)> {
        let raw =
            id.0.strip_prefix("imap-uid:")
                .ok_or_else(|| anyhow!("message ID is not an IMAP UID"))?;
        if let Some((validity, uid)) = raw.split_once(':') {
            let validity = validity.parse::<u32>().context("parsing UIDVALIDITY")?;
            let uid = uid.parse::<u32>().context("parsing IMAP UID")?;
            Ok((Some(validity), uid))
        } else {
            // Legacy IDs are supported for already-loaded messages, but never
            // written to the new disk cache without UIDVALIDITY.
            Ok((None, raw.parse::<u32>().context("parsing IMAP UID")?))
        }
    }

    fn uid_from_id(id: &MessageId) -> Result<u32> {
        Self::decode_uid(id).map(|(_, uid)| uid)
    }

    /// Reject stale cached IDs after SELECT if the server reset this mailbox.
    fn check_uid_generation(&self, id: &MessageId) -> Result<()> {
        let (expected, _) = Self::decode_uid(id)?;
        if let Some(expected) = expected
            && self.selected_uid_validity != Some(expected)
        {
            bail!("mailbox UIDVALIDITY changed; refresh this folder before acting on cached mail");
        }
        Ok(())
    }

    fn check_many_uid_generations(&self, ids: &[MessageId]) -> Result<()> {
        for id in ids {
            self.check_uid_generation(id)?;
        }
        Ok(())
    }

    fn uid_sequence(id: &MessageId) -> Result<SequenceSet> {
        let sequence = Self::uid_from_id(id)?.to_string();
        sequence
            .as_str()
            .try_into()
            .context("building IMAP UID sequence")
    }

    fn uid_sequence_many(ids: &[MessageId]) -> Result<SequenceSet> {
        let text = ids
            .iter()
            .map(Self::uid_from_id)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        if text.is_empty() {
            bail!("cannot build an empty IMAP UID sequence");
        }
        text.as_str()
            .try_into()
            .context("building IMAP UID sequence set")
    }

    /// Reuse immutable headers and BODYSTRUCTURE only for server-confirmed
    /// UIDs. A first FLAGS-only FETCH identifies the actual current page;
    /// only unknown UIDs need an RFC822.HEADER / BODYSTRUCTURE FETCH.
    /// Incomplete results (e.g. EXPUNGE races) trigger the full fallback.
    fn try_incremental_page(
        &mut self,
        exists: u32,
        limit: usize,
        hint: &MailboxSyncHint,
    ) -> Result<Option<MessagePage>> {
        let Some(validity) = hint.identity.uid_validity() else {
            return Ok(None);
        };
        let window = if limit == 0 { self.max_messages } else { limit };
        let width = if window == 0 {
            exists
        } else {
            u32::try_from(window).unwrap_or(u32::MAX).min(exists)
        };
        let start = exists.saturating_sub(width.saturating_sub(1)).max(1);
        let range_text = format!("{start}:{exists}");
        let range: SequenceSet = range_text
            .as_str()
            .try_into()
            .context("building incremental IMAP range")?;
        let items: MacroOrMessageDataItemNames<'static> =
            vec![MessageDataItemName::Uid, MessageDataItemName::Flags].into();
        let started = Instant::now();
        let fetched = self
            .session()?
            .fetch(range, items, ImapMessageFetchOptions::default());
        crate::perf::record(
            "imap_fetch_uid_flags",
            None,
            started.elapsed(),
            fetched.is_ok(),
        );
        let fetched = complete_imap_output(fetched?, "FETCH")?;
        if fetched.len() != width as usize {
            return Ok(None);
        }

        let mut seen = HashSet::new();
        let mut current = Vec::with_capacity(fetched.len());
        for items in fetched.values() {
            let (mut uid, mut flags) = (None, None);
            for item in items.as_ref() {
                match item {
                    MessageDataItem::Uid(value) => uid = Some(value.get()),
                    MessageDataItem::Flags(value) => flags = Some(flags_from_imap(value.as_ref())),
                    _ => {}
                }
            }
            let (Some(uid), Some(flags)) = (uid, flags) else {
                return Ok(None);
            };
            if !seen.insert(uid) {
                return Ok(None);
            }
            current.push((uid, flags));
        }

        let known = cached_uid_map(validity, &hint.messages);
        let missing = current
            .iter()
            .filter(|(uid, _)| !known.contains_key(uid))
            .map(|(uid, _)| *uid)
            .collect::<Vec<_>>();
        let mut new_summaries = HashMap::new();
        if !missing.is_empty() {
            // UID FETCH is essential: sequence numbers are not stable across
            // EXPUNGE, while UIDVALIDITY+UID unambiguously identify a message.
            let text = missing
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let sequence: SequenceSet = text
                .as_str()
                .try_into()
                .context("building UID FETCH for uncached headers")?;
            let items: MacroOrMessageDataItemNames<'static> = vec![
                MessageDataItemName::Uid,
                MessageDataItemName::Flags,
                MessageDataItemName::Rfc822Size,
                MessageDataItemName::Rfc822Header,
                MessageDataItemName::BodyStructure,
            ]
            .into();
            let opts = ImapMessageFetchOptions {
                uid: true,
                ..Default::default()
            };
            let started = Instant::now();
            let fetched = self.session()?.fetch(sequence, items, opts);
            crate::perf::record(
                "imap_fetch_new_headers",
                None,
                started.elapsed(),
                fetched.is_ok(),
            );
            let fetched = complete_imap_output(fetched?, "FETCH")?;
            for data in fetched.values() {
                if let Some(summary) = summary_from_fetch(data.as_ref(), self.selected_uid_validity)
                    && let Ok((_, uid)) = Self::decode_uid(&summary.id)
                {
                    new_summaries.insert(uid, summary);
                }
            }
            if new_summaries.len() != missing.len() {
                return Ok(None);
            }
        }

        let Some(messages) = merge_uid_page(validity, &hint.messages, &current, new_summaries)
        else {
            return Ok(None);
        };
        Ok(Some(MessagePage {
            messages,
            offset: 0,
            next_offset: width as usize,
            has_more: start > 1,
        }))
    }

    fn open_mailbox_impl(
        &mut self,
        name: &str,
        offset: usize,
        limit: usize,
        hint: Option<&MailboxSyncHint>,
    ) -> Result<MessagePage> {
        self.thread_snapshot = None;
        // Always re-SELECT the first page so a manual refresh sees new arrivals.
        // Later pages can reuse the selected mailbox's EXISTS count without an
        // extra round trip.
        let exists = if offset == 0 {
            self.select(name)?.exists.unwrap_or(0)
        } else {
            self.mailbox_exists(name)?
        };
        if offset == 0
            && exists > 0
            && let Some(hint) = hint
            && !hint.messages.is_empty()
            && hint.identity.uid_validity().is_some()
            && self.selected_uid_validity == hint.identity.uid_validity()
            && self
                .selected_uid_next
                .zip(hint.uid_next)
                .is_none_or(|(server, previous)| server >= previous)
        {
            let started = Instant::now();
            let candidate = self.try_incremental_page(exists, limit, hint);
            crate::perf::record(
                "imap_incremental_reuse",
                None,
                started.elapsed(),
                matches!(&candidate, Ok(Some(_))),
            );
            if let Ok(Some(page)) = candidate {
                return Ok(page);
            }
            // Sequence numbers may have shifted while we were fetching
            // flags. A new SELECT makes the full-fetch fallback safe.
            let exists = self.select(name)?.exists.unwrap_or(0);
            return self.fetch_full_page(name, exists, offset, limit);
        }
        self.fetch_full_page(name, exists, offset, limit)
    }

    fn fetch_full_page(
        &mut self,
        name: &str,
        exists: u32,
        offset: usize,
        limit: usize,
    ) -> Result<MessagePage> {
        if exists == 0 || offset >= exists as usize {
            return Ok(MessagePage {
                messages: Vec::new(),
                offset,
                next_offset: offset,
                has_more: false,
            });
        }

        let configured = if limit == 0 { self.max_messages } else { limit };
        let window = if configured == 0 {
            exists
        } else {
            u32::try_from(configured).unwrap_or(u32::MAX).min(exists)
        };
        let skipped = u32::try_from(offset).unwrap_or(u32::MAX).min(exists);
        let end = exists.saturating_sub(skipped);
        if end == 0 {
            return Ok(MessagePage {
                messages: Vec::new(),
                offset,
                next_offset: offset,
                has_more: false,
            });
        }
        let start = end.saturating_sub(window.saturating_sub(1)).max(1);
        let sequence_text = format!("{start}:{end}");
        let sequence: SequenceSet = sequence_text
            .as_str()
            .try_into()
            .context("building IMAP fetch range")?;
        let items: MacroOrMessageDataItemNames<'static> = vec![
            MessageDataItemName::Uid,
            MessageDataItemName::Flags,
            MessageDataItemName::Rfc822Size,
            MessageDataItemName::Rfc822Header,
            // BODYSTRUCTURE is metadata only: no MIME body downloaded and no
            // \Seen flag mutation. This makes attachment chips immediate.
            MessageDataItemName::BodyStructure,
        ]
        .into();
        let started = Instant::now();
        let fetched_result = self
            .session()?
            .fetch(sequence, items, ImapMessageFetchOptions::default())
            .with_context(|| format!("fetching headers from {name:?}"));
        crate::perf::record(
            "imap_fetch_headers",
            None,
            started.elapsed(),
            fetched_result.is_ok(),
        );
        let fetched = complete_imap_output(fetched_result?, "header FETCH")?;

        let requested = usize::try_from(end - start + 1).unwrap_or(usize::MAX);
        if fetched.len() != requested {
            bail!(
                "incomplete IMAP header FETCH: expected {requested} messages, received {}; refresh to retry",
                fetched.len()
            );
        }
        let mut messages = fetched
            .values()
            .map(|items| {
                summary_from_fetch(items.as_ref(), self.selected_uid_validity).ok_or_else(|| {
                    anyhow!("incomplete IMAP FETCH: missing required summary fields")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        messages.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
        let consumed =
            usize::try_from(end.saturating_sub(start).saturating_add(1)).unwrap_or(usize::MAX);
        let next_offset = offset.saturating_add(consumed);
        Ok(MessagePage {
            messages,
            offset,
            next_offset,
            has_more: start > 1,
        })
    }

    fn ensure_selected(&mut self, mailbox: &str) -> Result<()> {
        if self.selected_mailbox.as_deref() != Some(mailbox) {
            let _ = self.select(mailbox)?;
        }
        Ok(())
    }
}

/// Map only UIDs from the expected mailbox generation. Legacy IDs cannot
/// participate in incremental synchronization, even if their UID matches.
fn cached_uid_map(
    validity: u32,
    summaries: &[crate::model::MessageSummary],
) -> HashMap<u32, &crate::model::MessageSummary> {
    summaries
        .iter()
        .filter_map(|message| {
            let (generation, uid) = ImapBackend::decode_uid(&message.id).ok()?;
            (generation == Some(validity)).then_some((uid, message))
        })
        .collect()
}

/// Pure reconciliation used by the network path and regression tests. Only
/// server-confirmed UIDs are retained, stale flags are overwritten, and each
/// new UID must have a freshly fetched summary. Returns None on any gap.
fn merge_uid_page(
    validity: u32,
    cached: &[crate::model::MessageSummary],
    current: &[(u32, MessageFlags)],
    mut newly_fetched: HashMap<u32, crate::model::MessageSummary>,
) -> Option<Vec<crate::model::MessageSummary>> {
    let known = cached_uid_map(validity, cached);
    let mut seen = HashSet::new();
    let mut messages = Vec::with_capacity(current.len());
    for &(uid, flags) in current {
        if !seen.insert(uid) {
            return None;
        }
        let mut summary = known
            .get(&uid)
            .map(|item| (**item).clone())
            .or_else(|| newly_fetched.remove(&uid))?;
        let (generation, summary_uid) = ImapBackend::decode_uid(&summary.id).ok()?;
        if generation != Some(validity) || summary_uid != uid {
            return None;
        }
        summary.flags = flags;
        messages.push(summary);
    }
    messages.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
    Some(messages)
}

fn summary_from_fetch(
    items: &[MessageDataItem<'_>],
    uid_validity: Option<u32>,
) -> Option<crate::model::MessageSummary> {
    let (mut uid, mut flags, mut size) = (None, MessageFlags::empty(), 0u64);
    let mut headers = None;
    let mut attachment_hints = Vec::new();
    for item in items {
        match item {
            MessageDataItem::Uid(value) => uid = Some(value.get()),
            MessageDataItem::Flags(value) => flags = flags_from_imap(value),
            MessageDataItem::Rfc822Size(value) => size = u64::from(*value),
            MessageDataItem::Rfc822Header(value) => {
                headers = value.clone().into_option().map(Cow::into_owned)
            }
            MessageDataItem::BodyStructure(structure) => {
                collect_bodystructure_attachments(structure, &mut attachment_hints)
            }
            _ => {}
        }
    }
    let (uid, headers) = (uid?, headers?);
    let id = match uid_validity {
        Some(validity) => MessageId(format!("imap-uid:{validity}:{uid}")),
        None => MessageId(format!("imap-uid:{uid}")),
    };
    let mut summary = parse_summary(id, &headers, size, flags).ok()?;
    // BODYSTRUCTURE provides no decoded bytes. Prefer named .ics over an
    // additional unnamed calendar alternative in the summary, and let the
    // full MIME parser decide by byte comparison when the email is opened.
    let has_named_calendar = attachment_hints
        .iter()
        .any(|(name, _)| name.to_ascii_lowercase().ends_with(".ics"));
    let attachment_names = attachment_hints
        .into_iter()
        .filter(|(_, unnamed_calendar)| !(has_named_calendar && *unnamed_calendar))
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    summary.has_attachment_hint |= !attachment_names.is_empty();
    summary.attachment_names_hint = attachment_names.into_boxed_slice();
    Some(summary)
}

impl ImapBackend {
    fn thread_raw(&mut self, command: &str) -> Result<Vec<Vec<u8>>> {
        let command = imap_threads::command(command);
        self.session()?
            .raw(command.as_bytes())
            .context("IMAP native conversation request failed")
    }

    fn query_conversations(
        &mut self,
        name: &str,
        view: &MailboxView,
        offset: usize,
        limit: usize,
    ) -> Result<MessagePage> {
        if self.thread_capabilities.is_none() {
            let response = self.thread_raw("CAPABILITY")?;
            self.thread_capabilities = Some(imap_threads::capabilities(
                &imap_threads::checked_response(&response)?,
            ));
        }
        let (references, gmail) = self.thread_capabilities.unwrap_or_default();
        anyhow::ensure!(
            matches!(view, MailboxView::Search(_)) || references || gmail,
            "server does not advertise native conversation threading"
        );
        let exists = self.select(name)?.exists.unwrap_or(0) as usize;
        let validity = self
            .selected_uid_validity
            .context("IMAP threading requires UIDVALIDITY")?;
        let seed = match view {
            MailboxView::Thread(id) => {
                let (generation, uid) = Self::decode_uid(&MessageId(id.clone()))?;
                if generation != Some(validity) || uid == 0 {
                    return Err(crate::backend::BackendError::new(
                        crate::backend::ErrorKind::StaleState,
                        "stale IMAP conversation message UID",
                    )
                    .into());
                }
                Some(uid)
            }
            _ => None,
        };
        let reuse = offset != 0
            && self
                .thread_snapshot
                .as_ref()
                .is_some_and(|s| s.mailbox == name && s.validity == validity && &s.view == view);
        if !reuse {
            anyhow::ensure!(
                offset == 0,
                "IMAP conversation checkpoint changed; refresh the view"
            );
            let mut rows = Vec::new();
            let mut memberships = HashMap::new();
            if let MailboxView::Search(text) = view {
                let criteria = super::search::imap_criteria(text, gmail)?;
                let response = self.thread_raw(&format!("UID SEARCH {criteria}"))?;
                rows.extend(
                    imap_threads::search_uids(&imap_threads::checked_response(&response)?)?
                        .into_iter()
                        .map(|uid| (uid, None)),
                );
            } else if let Some(seed) = seed.filter(|_| exists > 0 && gmail) {
                // One exact provider ID, then server search for all members in
                // this mailbox, even when All Mail exceeds our metadata budget.
                let response = self.thread_raw(&format!("UID FETCH {seed} (UID X-GM-THRID)"))?;
                let groups =
                    imap_threads::gmail_groups(&imap_threads::checked_response(&response)?)?;
                let thread = groups
                    .iter()
                    .find(|(_, uids)| uids.as_slice() == [seed])
                    .map(|(id, _)| *id)
                    .context("Gmail conversation ID unavailable")?;
                let response = self.thread_raw(&format!("UID SEARCH X-GM-THRID {thread}"))?;
                let uids = imap_threads::search_uids(&imap_threads::checked_response(&response)?)?;
                anyhow::ensure!(
                    uids.contains(&seed),
                    "Gmail conversation changed during lookup"
                );
                for uid in uids {
                    let id = format!("gmail:{thread}");
                    memberships.insert(uid, id.clone());
                    rows.push((uid, Some(id)));
                }
            } else if exists > 0 {
                anyhow::ensure!(
                    exists <= imap_threads::MAX_THREAD_MESSAGES,
                    "mailbox too large for bounded native conversation list; use cached conversations"
                );
                if gmail {
                    let response = self.thread_raw("UID FETCH 1:* (UID X-GM-THRID)")?;
                    let groups =
                        imap_threads::gmail_groups(&imap_threads::checked_response(&response)?)?;
                    anyhow::ensure!(
                        groups.values().map(Vec::len).sum::<usize>() == exists,
                        "incomplete Gmail conversation metadata; refresh"
                    );
                    for (id, uids) in groups {
                        let thread_id = format!("gmail:{id}");
                        if let Some(uid) = uids.iter().max() {
                            rows.push((*uid, Some(thread_id.clone())));
                        }
                        memberships.extend(uids.into_iter().map(|uid| (uid, thread_id.clone())));
                    }
                } else {
                    let response = self.thread_raw("UID THREAD REFERENCES UTF-8 ALL")?;
                    let groups = imap_threads::references_groups(&imap_threads::checked_response(
                        &response,
                    )?)?;
                    anyhow::ensure!(
                        groups.iter().map(Vec::len).sum::<usize>() == exists,
                        "incomplete IMAP conversation metadata; refresh"
                    );
                    for group in groups {
                        let Some(root) = group.iter().min() else {
                            continue;
                        };
                        let thread_id = format!("imap-ref:{validity}:{root}");
                        if let Some(seed) = seed {
                            if group.contains(&seed) {
                                rows.extend(
                                    group.iter().map(|uid| (*uid, Some(thread_id.clone()))),
                                );
                            }
                        } else if let Some(uid) = group.iter().max() {
                            rows.push((*uid, Some(thread_id.clone())));
                        }
                        memberships.extend(group.into_iter().map(|uid| (uid, thread_id.clone())));
                    }
                    anyhow::ensure!(
                        seed.is_none() || !rows.is_empty(),
                        "IMAP conversation no longer exists"
                    );
                }
            }
            // UIDs are arrival order, so pagination has a stable total order
            // even if RFC 5322 Date headers are missing or malformed.
            rows.sort_unstable_by_key(|row| std::cmp::Reverse(row.0));
            self.thread_snapshot = Some(ImapThreadSnapshot {
                mailbox: name.to_owned(),
                validity,
                view: view.clone(),
                rows,
                memberships,
            });
        }
        let snapshot = self
            .thread_snapshot
            .as_ref()
            .context("missing IMAP thread snapshot")?;
        let total = snapshot.rows.len();
        let limit = if limit == 0 { 100 } else { limit.min(200) };
        let window: Vec<_> = snapshot
            .rows
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();
        let mut messages = Vec::new();
        if !window.is_empty() {
            let text = window
                .iter()
                .map(|(uid, _)| uid.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let sequence: SequenceSet = text
                .as_str()
                .try_into()
                .context("building IMAP conversation UID set")?;
            let items: MacroOrMessageDataItemNames<'static> = vec![
                MessageDataItemName::Uid,
                MessageDataItemName::Flags,
                MessageDataItemName::Rfc822Size,
                MessageDataItemName::Rfc822Header,
                MessageDataItemName::BodyStructure,
            ]
            .into();
            let fetched = complete_imap_output(
                self.session()?.fetch(
                    sequence,
                    items,
                    ImapMessageFetchOptions {
                        uid: true,
                        ..Default::default()
                    },
                )?,
                "conversation header FETCH",
            )?;
            let expected: HashMap<_, _> =
                window.iter().map(|(uid, thread)| (*uid, thread)).collect();
            let mut seen = HashSet::new();
            for items in fetched.values() {
                let mut message = summary_from_fetch(items.as_ref(), Some(validity))
                    .context("incomplete IMAP conversation header")?;
                let uid = Self::uid_from_id(&message.id)?;
                let thread = expected
                    .get(&uid)
                    .context("unexpected IMAP conversation UID")?;
                anyhow::ensure!(seen.insert(uid), "duplicate IMAP conversation UID");
                message.thread_id = (**thread).clone();
                messages.push(message);
            }
            anyhow::ensure!(
                messages.len() == window.len(),
                "IMAP conversation changed; refresh"
            );
            messages.sort_unstable_by(|a, b| {
                Self::uid_from_id(&b.id)
                    .unwrap_or(0)
                    .cmp(&Self::uid_from_id(&a.id).unwrap_or(0))
            });
        }
        let next_offset = offset.saturating_add(window.len());
        Ok(MessagePage {
            messages,
            offset,
            next_offset,
            has_more: next_offset < total,
        })
    }

    fn fetch_raw_limited(
        &mut self,
        mailbox: &str,
        id: &MessageId,
        limit: usize,
    ) -> Result<Vec<u8>> {
        self.ensure_selected(mailbox)?;
        self.check_uid_generation(id)?;
        let uid = Self::uid_from_id(id)?;
        let uid = NonZeroU32::new(uid).ok_or_else(|| anyhow!("IMAP UID cannot be zero"))?;
        let mut raw = LimitedMessageWriter::new(limit);
        let started = Instant::now();
        let result = self
            .session()?
            .fetch_body_stream(uid, true, &mut raw)
            .with_context(|| format!("fetching IMAP message UID {uid}"));
        crate::perf::record("imap_fetch_body", None, started.elapsed(), result.is_ok());
        result?;
        if raw.bytes.is_empty() {
            bail!("IMAP server did not return requested UID {uid}");
        }
        Ok(raw.bytes)
    }
}

type MailboxStatusWithUids = ((usize, usize), Option<u32>, Option<u32>);

impl ImapBackend {
    fn status_with_uids(
        &mut self,
        name: &str,
        include_uids: bool,
    ) -> Result<MailboxStatusWithUids> {
        let mailbox = mailbox_from_name(name)?;
        let started = Instant::now();
        let mut requested = vec![StatusDataItemName::Messages, StatusDataItemName::Unseen];
        if include_uids {
            requested.extend([StatusDataItemName::UidNext, StatusDataItemName::UidValidity]);
        }
        let items_result = self
            .session()?
            .status(mailbox, Cow::Owned(requested))
            .with_context(|| format!("reading status for {name:?}"));
        crate::perf::record("imap_status", None, started.elapsed(), items_result.is_ok());
        let items = complete_imap_output(items_result?, "STATUS")?;
        let mut total = 0usize;
        let mut unread = 0usize;
        let (mut validity, mut next) = (None, None);
        for item in items {
            match item {
                StatusDataItem::Messages(value) => total = value as usize,
                StatusDataItem::Unseen(value) => unread = value as usize,
                StatusDataItem::UidNext(value) => next = Some(value.get()),
                StatusDataItem::UidValidity(value) => validity = Some(value.get()),
                _ => {}
            }
        }
        Ok(((total, unread), validity, next))
    }
}

impl MailBackend for ImapBackend {
    fn release_idle_resources(&mut self) {
        self.session = None;
        self.idle_checkpoint = None;
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            server_thread: self
                .thread_capabilities
                .is_some_and(|(references, gmail)| references || gmail),
            ..Capabilities::IMAP_BASIC
        }
    }

    fn query_mailbox(
        &mut self,
        name: &str,
        view: &MailboxView,
        offset: usize,
        limit: usize,
    ) -> Result<MessagePage> {
        self.query_conversations(name, view, offset, limit)
    }

    fn cached_thread_memberships(&self, ids: &[MessageId]) -> HashMap<MessageId, String> {
        let Some(snapshot) = &self.thread_snapshot else {
            return HashMap::new();
        };
        ids.iter()
            .filter_map(|id| {
                let (validity, uid) = Self::decode_uid(id).ok()?;
                if validity != Some(snapshot.validity) {
                    return None;
                }
                Some((id.clone(), snapshot.memberships.get(&uid)?.clone()))
            })
            .collect()
    }

    fn connect(&mut self) -> Result<()> {
        let started = Instant::now();
        let credential_result = self.source.credential(self.credential_command.as_deref());
        crate::perf::record(
            "imap_credential",
            None,
            started.elapsed(),
            credential_result.is_ok(),
        );
        let credential = credential_result?;
        let (connect_url, connect_options) = self.source.connection_settings()?;
        // Emit a start marker too. When TCP/TLS/greeting stalls, the matching
        // completion cannot be recorded until connect() returns. This changes
        // logging only; the working io-imap transport remains untouched.
        crate::perf::record("imap_tls_connect_begin", None, Duration::ZERO, true);
        let started = Instant::now();
        let client_result = ImapClientStd::connect(&connect_url, connect_options)
            .context("connecting to IMAP server");
        crate::perf::record(
            "imap_tls_connect",
            None,
            started.elapsed(),
            client_result.is_ok(),
        );
        let (mut client, _) = client_result?;
        client
            .stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .context("setting IMAP read timeout")?;

        crate::perf::record("imap_auth_begin", None, Duration::ZERO, true);
        let started = Instant::now();
        let auth_result = (|| -> Result<()> {
            match self.source.auth {
                ImapAuth::Password | ImapAuth::Login => {
                    let sasl_started = Instant::now();
                    let result = client
                        .login(
                            &self.source.username,
                            &credential,
                            ImapLoginOptions::default(),
                        )
                        .context("IMAP authentication failed");
                    crate::perf::record(
                        "imap_login_exchange",
                        None,
                        sasl_started.elapsed(),
                        result.is_ok(),
                    );
                    result?;
                }
                ImapAuth::OAuthBearer => {
                    let access_token = self.source.access_token(&credential)?;
                    let sasl_started = Instant::now();
                    let result = client
                        .auth_oauthbearer(
                            &self.source.username,
                            &self.source.host,
                            self.source.port,
                            &access_token,
                            ImapAuthOauthbearerOptions::default(),
                        )
                        .context("IMAP OAUTHBEARER authentication failed");
                    crate::perf::record(
                        "imap_oauthbearer_exchange",
                        None,
                        sasl_started.elapsed(),
                        result.is_ok(),
                    );
                    result?;
                }
                ImapAuth::XOAuth2 => {
                    let access_token = self.source.access_token(&credential)?;
                    let sasl_started = Instant::now();
                    let result = client
                        .auth_xoauth2(
                            &self.source.username,
                            &access_token,
                            ImapAuthXoauth2Options::default(),
                        )
                        .context("IMAP XOAUTH2 authentication failed");
                    crate::perf::record(
                        "imap_xoauth2_exchange",
                        None,
                        sasl_started.elapsed(),
                        result.is_ok(),
                    );
                    result?;
                }
            }
            Ok(())
        })();
        crate::perf::record("imap_auth", None, started.elapsed(), auth_result.is_ok());
        auth_result?;

        self.thread_capabilities = None;
        self.thread_snapshot = None;
        self.session = Some(client);
        self.selected_mailbox = None;
        self.selected_exists = None;
        self.selected_uid_validity = None;
        self.selected_uid_next = None;
        Ok(())
    }

    fn watch_mailbox(
        &mut self,
        name: &str,
        cancel: &AtomicBool,
        changed: &mut dyn FnMut(),
    ) -> Result<()> {
        // io-imap retains a UID+FLAGS shadow for the whole folder. Cap it,
        // and use ordinary STATUS polling for unusually large mailboxes.
        let (total, _) = self.mailbox_status(name)?;
        if total > MAX_IDLE_SHADOW_MESSAGES || cancel.load(Ordering::Relaxed) {
            self.idle_checkpoint = None;
            return Ok(());
        }
        let mailbox = mailbox_from_name(name)?;
        let mut session = self
            .session
            .take()
            .ok_or_else(|| anyhow!("IMAP not connected"))?;
        let capability = session.capability().context("reading IMAP capabilities")?;
        if !capability.contains(&Capability::Idle) {
            self.idle_checkpoint = None;
            return Ok(());
        }
        let resume_from = take_idle_checkpoint(&mut self.idle_checkpoint, name);
        let started_with_checkpoint = resume_from.is_some();
        let watch = session
            .watch_mailbox(
                mailbox,
                &capability,
                ImapMailboxWatchStreamOptions {
                    shutdown_poll: Duration::from_secs(1),
                    idle_timeout: Some(Duration::from_secs(25 * 60)),
                    poll: None,
                    resume_from,
                },
            )
            .context("starting IMAP IDLE mailbox watcher")?;

        // A created watcher is not necessarily an opened watcher. Wait for
        // EXAMINE to succeed so failures don't masquerade as live sync.
        let opened_at = Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                watch
                    .into_state()
                    .context("stopping cancelled IMAP watcher")?;
                return Ok(());
            }
            match watch.wait_opened(Duration::from_millis(250)) {
                Ok(()) => break,
                Err(RecvTimeoutError::Timeout) if opened_at.elapsed() < IDLE_OPEN_TIMEOUT => {}
                Err(RecvTimeoutError::Timeout) => {
                    watch
                        .into_state()
                        .context("stopping unopened IMAP watcher")?;
                    bail!("IMAP IDLE watcher did not open mailbox within 30 seconds");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let failure = watch.try_recv().ok().and_then(Result::err);
                    watch.into_state().context("stopping failed IMAP watcher")?;
                    if let Some(error) = failure {
                        return Err(anyhow!(
                            "IMAP watcher failed before opening ({}): {error}",
                            watch_error_label(&error)
                        ));
                    }
                    bail!("IMAP watcher ended before opening mailbox");
                }
            }
        }

        // Initial watch baseline doesn't produce change events. Ask the
        // regular worker to reconcile once, ensuring the gap between its
        // last STATUS and this new watch isn't silently missed.
        if !started_with_checkpoint {
            changed();
        }
        let mut error = None;
        while !cancel.load(Ordering::Relaxed) {
            match watch.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(_)) => changed(),
                Ok(Err(failure)) => {
                    error = Some(failure);
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        // Always join the worker and recover its shadow, including after a
        // broken socket. Never resume after UIDVALIDITY changed.
        let invalid_generation = error.as_ref().is_some_and(|failure| {
            matches!(
                failure,
                ImapClientError::MailboxWatch(ImapMailboxWatchError::UidValidityChanged { .. })
            )
        });
        let state = watch.into_state().context("joining IMAP IDLE watcher")?;
        self.idle_checkpoint = if invalid_generation {
            None
        } else {
            state
                .filter(|checkpoint| checkpoint.flags.len() <= MAX_IDLE_SHADOW_MESSAGES)
                .map(|checkpoint| (name.to_owned(), checkpoint))
        };
        if let Some(failure) = error {
            bail!(
                "IMAP watcher stopped ({}): {failure}",
                watch_error_label(&failure)
            );
        }
        if !cancel.load(Ordering::Relaxed) {
            bail!("IMAP IDLE stream disconnected");
        }
        Ok(())
    }

    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
        let reference: ImapMailbox<'static> = ""
            .to_owned()
            .try_into()
            .context("building LIST reference")?;
        let pattern: ListMailbox<'static> =
            "*".to_owned().try_into().context("building LIST pattern")?;
        let started = Instant::now();
        let names_result = self
            .session()?
            .list(reference, pattern)
            .context("listing IMAP mailboxes");
        crate::perf::record("imap_list", None, started.elapsed(), names_result.is_ok());
        let names = complete_imap_output(names_result?, "LIST")?;

        let mut selectable: Vec<(String, MailboxRole)> = names
            .into_iter()
            .filter(|(_, _, attrs)| {
                !attrs
                    .iter()
                    .any(|attr| matches!(attr, FlagNameAttribute::Noselect))
            })
            .map(|(mailbox, _, attrs)| {
                let mailbox_name = mailbox_to_name(&mailbox);
                let role =
                    role_from_attributes(&attrs).unwrap_or_else(|| role_for_name(&mailbox_name));
                (mailbox_name, role)
            })
            .collect();
        selectable.sort_by_key(|a| mailbox_sort_key(&a.0, &a.1));

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

    fn mailbox_uid_validity(&self, name: &str) -> Option<u32> {
        (self.selected_mailbox.as_deref() == Some(name))
            .then_some(self.selected_uid_validity)
            .flatten()
    }

    fn mailbox_status(&mut self, name: &str) -> Result<(usize, usize)> {
        self.status_with_uids(name, false)
            .map(|(counts, _, _)| counts)
    }

    fn inbox_status(
        &mut self,
        name: &str,
        previous: Option<&super::InboxCheckpoint>,
    ) -> Result<((usize, usize), super::InboxCheck)> {
        use super::{InboxCheck, InboxCheckpoint};
        let (counts, validity, next) = self.status_with_uids(name, true)?;
        let (Some(validity), Some(next)) = (validity, next) else {
            return Ok((counts, InboxCheck::default()));
        };
        let mut check = InboxCheck {
            checkpoint: Some(InboxCheckpoint::Imap { validity, next }),
            ..InboxCheck::default()
        };
        if let Some(InboxCheckpoint::Imap {
            validity: old_validity,
            next: old_next,
        }) = previous
            && *old_validity == validity
            && *old_next < next
            && next - *old_next <= super::notifications::MAX_ARRIVAL_RANGE
        {
            // UIDNEXT may skip numbers. Count actual surviving messages, not
            // the difference between two allocated UID watermarks.
            self.select(name)?;
            if self.selected_uid_validity != Some(validity) {
                return Ok((counts, InboxCheck::default()));
            }
            let range = format!("{}:{}", old_next, next - 1);
            let sequence: SequenceSet = range
                .as_str()
                .try_into()
                .context("building notification UID range")?;
            let fetched = self.session()?.fetch(
                sequence,
                vec![MessageDataItemName::Uid].into(),
                ImapMessageFetchOptions {
                    uid: true,
                    ..Default::default()
                },
            )?;
            let fetched = complete_imap_output(fetched, "notification UID FETCH")?;
            let mut ids = HashSet::new();
            for items in fetched.values() {
                for item in items.as_ref() {
                    if let MessageDataItem::Uid(value) = item {
                        let uid = value.get();
                        if uid >= *old_next && uid < next {
                            ids.insert(uid);
                        }
                    }
                }
            }
            check.count = ids.len();
            check.candidate = ids
                .into_iter()
                .max()
                .map(|uid| MessageId(format!("imap-uid:{validity}:{uid}")));
        }
        Ok((counts, check))
    }

    fn notification_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        self.fetch_raw_limited(mailbox, id, MAX_BODY_MESSAGE_BYTES)
    }

    fn mailbox_uid_next(&self, name: &str) -> Option<u32> {
        (self.selected_mailbox.as_deref() == Some(name))
            .then_some(self.selected_uid_next)
            .flatten()
    }

    fn open_mailbox(&mut self, name: &str, offset: usize, limit: usize) -> Result<MessagePage> {
        self.open_mailbox_impl(name, offset, limit, None)
    }

    fn open_mailbox_with_hint(
        &mut self,
        name: &str,
        offset: usize,
        limit: usize,
        hint: Option<MailboxSyncHint>,
    ) -> Result<MessagePage> {
        self.open_mailbox_impl(name, offset, limit, hint.as_ref())
    }

    fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody> {
        let raw = self.fetch_raw_limited(mailbox, id, MAX_BODY_MESSAGE_BYTES)?;
        parse_body_compact(id.clone(), &raw).context("parsing IMAP message body")
    }

    fn fetch_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        self.fetch_raw_limited(mailbox, id, MAX_RAW_MESSAGE_BYTES)
    }

    fn export_raw_into(
        &mut self,
        mailbox: &str,
        id: &MessageId,
        writer: &mut dyn Write,
    ) -> Result<()> {
        self.ensure_selected(mailbox)?;
        self.check_uid_generation(id)?;
        let uid = Self::uid_from_id(id)?;
        let uid = NonZeroU32::new(uid).ok_or_else(|| anyhow!("IMAP UID cannot be zero"))?;
        self.session()?
            .fetch_body_stream(uid, true, writer)
            .with_context(|| format!("streaming IMAP message UID {uid} to disk"))?;
        Ok(())
    }

    fn append_message(&mut self, mailbox: &str, raw: &[u8], seen: bool) -> Result<()> {
        let mailbox_name = mailbox_from_name(mailbox)?;
        let options = ImapMessageAppendOptions {
            flags: if seen { vec![Flag::Seen] } else { Vec::new() },
            ..Default::default()
        };
        let (exists, _) = self
            .session()?
            .append(mailbox_name, raw, options)
            .with_context(|| format!("appending message to IMAP mailbox {mailbox:?}"))?;
        if self.selected_mailbox.as_deref() == Some(mailbox)
            && let Some(exists) = exists
        {
            self.selected_exists = Some(exists);
        }
        Ok(())
    }

    fn append_draft(&mut self, mailbox: &str, raw: &[u8]) -> Result<()> {
        let mailbox_name = mailbox_from_name(mailbox)?;
        let options = ImapMessageAppendOptions {
            flags: vec![Flag::Seen, Flag::Draft],
            ..Default::default()
        };
        let (exists, _) = self
            .session()?
            .append(mailbox_name, raw, options)
            .with_context(|| format!("saving draft to IMAP mailbox {mailbox:?}"))?;
        if self.selected_mailbox.as_deref() == Some(mailbox)
            && let Some(exists) = exists
        {
            self.selected_exists = Some(exists);
        }
        Ok(())
    }

    fn set_seen(&mut self, mailbox: &str, id: &MessageId, seen: bool) -> Result<()> {
        self.ensure_selected(mailbox)?;
        self.check_uid_generation(id)?;
        let sequence = Self::uid_sequence(id)?;
        let opts = ImapMessageStoreOptions { uid: true };
        let kind = if seen {
            StoreType::Add
        } else {
            StoreType::Remove
        };
        self.session()?
            .store(sequence, kind, vec![Flag::Seen], opts)
            .context("updating IMAP seen flag")?;
        Ok(())
    }

    fn delete(&mut self, mailbox: &str, id: &MessageId) -> Result<()> {
        self.ensure_selected(mailbox)?;
        self.check_uid_generation(id)?;
        let sequence = Self::uid_sequence(id)?;
        let opts = ImapMessageStoreOptions { uid: true };
        self.session()?
            .store(sequence.clone(), StoreType::Add, vec![Flag::Deleted], opts)
            .context("marking IMAP message deleted")?;
        self.session()?
            .uid_expunge(sequence)
            .context("expunging IMAP message")?;
        self.note_removed(mailbox, 1);
        Ok(())
    }

    fn delete_many(&mut self, mailbox: &str, ids: &[MessageId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.ensure_selected(mailbox)?;
        self.check_many_uid_generations(ids)?;
        let sequence = Self::uid_sequence_many(ids)?;
        let opts = ImapMessageStoreOptions { uid: true };
        self.session()?
            .store(sequence.clone(), StoreType::Add, vec![Flag::Deleted], opts)
            .context("marking IMAP messages deleted")?;
        self.session()?
            .uid_expunge(sequence)
            .context("expunging IMAP messages")?;
        self.note_removed(mailbox, ids.len());
        Ok(())
    }

    fn archive_many(&mut self, source: &str, destination: &str, ids: &[MessageId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        if is_gmail_archive(&self.source.host, source, destination) {
            // In Gmail, All Mail is a virtual view that already contains INBOX
            // messages. Copy/MOVE into All Mail need not remove the INBOX label.
            // The Gmail IMAP archive operation removes messages from the INBOX
            // by marking only their selected UIDs deleted and expunging those
            // UIDs. This is *not* used for a generic mail server or other folder.
            self.ensure_selected(source)?;
            self.check_many_uid_generations(ids)?;
            let uids = Self::uid_sequence_many(ids)?;
            let options = ImapMessageStoreOptions { uid: true };
            self.session()?
                .store(uids.clone(), StoreType::Add, vec![Flag::Deleted], options)
                .context("marking Gmail INBOX labels for removal")?;
            self.session()?
                .uid_expunge(uids)
                .context("removing Gmail INBOX labels")?;
            self.note_removed(source, ids.len());
            Ok(())
        } else {
            self.move_many(source, destination, ids)
        }
    }

    fn unarchive_many(&mut self, source: &str, destination: &str, ids: &[MessageId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        if is_gmail_unarchive(&self.source.host, source, destination) {
            // Adding INBOX in Gmail must not remove the message from the All Mail
            // view, so COPY (add label) instead of MOVE (remove source).
            self.ensure_selected(source)?;
            self.check_many_uid_generations(ids)?;
            let uids = Self::uid_sequence_many(ids)?;
            let options = ImapMessageCopyOptions { uid: true };
            self.session()?
                .copy(uids, mailbox_from_name(destination)?, options)
                .context("restoring Gmail message to INBOX")?;
            Ok(())
        } else {
            self.move_many(source, destination, ids)
        }
    }

    fn move_to(&mut self, source: &str, destination: &str, id: &MessageId) -> Result<()> {
        self.ensure_selected(source)?;
        self.check_uid_generation(id)?;
        let destination_mailbox = mailbox_from_name(destination)?;
        let sequence = Self::uid_sequence(id)?;
        let move_opts = ImapMessageMoveOptions { uid: true };
        if self
            .session()?
            .r#move(sequence.clone(), destination_mailbox.clone(), move_opts)
            .is_ok()
        {
            self.note_removed(source, 1);
            return Ok(());
        }

        let copy_opts = ImapMessageCopyOptions { uid: true };
        self.session()?
            .copy(sequence.clone(), destination_mailbox, copy_opts)
            .with_context(|| format!("copying IMAP message to {destination:?}"))?;
        let store_opts = ImapMessageStoreOptions { uid: true };
        self.session()?
            .store(
                sequence.clone(),
                StoreType::Add,
                vec![Flag::Deleted],
                store_opts,
            )
            .context("marking moved IMAP message deleted")?;
        self.session()?
            .uid_expunge(sequence)
            .context("expunging moved IMAP message")?;
        self.note_removed(source, 1);
        Ok(())
    }

    fn move_many(&mut self, source: &str, destination: &str, ids: &[MessageId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.ensure_selected(source)?;
        self.check_many_uid_generations(ids)?;
        let destination_mailbox = mailbox_from_name(destination)?;
        let sequence = Self::uid_sequence_many(ids)?;
        let move_opts = ImapMessageMoveOptions { uid: true };
        if self
            .session()?
            .r#move(sequence.clone(), destination_mailbox.clone(), move_opts)
            .is_ok()
        {
            self.note_removed(source, ids.len());
            return Ok(());
        }

        let copy_opts = ImapMessageCopyOptions { uid: true };
        self.session()?
            .copy(sequence.clone(), destination_mailbox, copy_opts)
            .with_context(|| format!("copying IMAP messages to {destination:?}"))?;
        let store_opts = ImapMessageStoreOptions { uid: true };
        self.session()?
            .store(
                sequence.clone(),
                StoreType::Add,
                vec![Flag::Deleted],
                store_opts,
            )
            .context("marking moved IMAP messages deleted")?;
        self.session()?
            .uid_expunge(sequence)
            .context("expunging moved IMAP messages")?;
        self.note_removed(source, ids.len());
        Ok(())
    }
}

// Gmail's All Mail is a view, not a separate message store. Do not apply
// these label semantics to arbitrary IMAP servers or user-created Archive dirs.
fn gmail_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "imap.gmail.com" | "imap.googlemail.com"
    )
}
fn gmail_all_mail(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    (lower.starts_with("[gmail]/") || lower.starts_with("[googlemail]/"))
        && lower.ends_with("all mail")
}
fn is_gmail_archive(host: &str, source: &str, destination: &str) -> bool {
    gmail_host(host) && source.eq_ignore_ascii_case("INBOX") && gmail_all_mail(destination)
}
fn is_gmail_unarchive(host: &str, source: &str, destination: &str) -> bool {
    gmail_host(host) && gmail_all_mail(source) && destination.eq_ignore_ascii_case("INBOX")
}

fn decode_component(raw: &str) -> Result<String> {
    percent_decode_str(raw)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .context("decoding percent-encoded IMAP source field")
}

fn mailbox_from_name(name: &str) -> Result<ImapMailbox<'static>> {
    name.to_owned()
        .try_into()
        .map_err(|error| anyhow!("invalid IMAP mailbox {name:?}: {error}"))
}

fn mailbox_to_name(mailbox: &ImapMailbox<'_>) -> String {
    match mailbox {
        ImapMailbox::Inbox => "INBOX".to_owned(),
        ImapMailbox::Other(other) => String::from_utf8_lossy(other.as_ref()).into_owned(),
    }
}

pub(super) fn run_credential_command(command: &str) -> Result<String> {
    if let Some(secret) = crate::credentials::private_file_credential(command)? {
        return Ok(secret);
    }
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
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    if secret.is_empty() {
        bail!("source_cred_cmd returned an empty credential");
    }
    Ok(secret)
}

/// MIME structure is available in the same header FETCH. A multipart/mixed
/// wrapper alone is not proof of any attachment. Only parts with an attachment
/// disposition, or a genuine filename/name parameter, count. This deliberately
/// does not count remote <img> URLs or CID inline images with no filename.
fn collect_bodystructure_attachments(
    structure: &BodyStructure<'_>,
    names: &mut Vec<(String, bool)>,
) {
    match structure {
        BodyStructure::Multi { bodies, .. } => {
            for part in bodies.as_ref().iter() {
                collect_bodystructure_attachments(part, names);
            }
        }
        BodyStructure::Single {
            body,
            extension_data,
        } => {
            let disposition = extension_data
                .as_ref()
                .and_then(|data| data.tail.as_ref())
                .and_then(|data| data.disposition.as_ref());
            let is_file = disposition.is_some_and(|(kind, _)| {
                let bytes: &[u8] = kind.as_ref();
                bytes.eq_ignore_ascii_case(b"attachment")
            });
            let filename = disposition
                .into_iter()
                .flat_map(|(_, pairs)| pairs.iter())
                .chain(body.basic.parameter_list.iter())
                .find(|(key, _)| {
                    let bytes: &[u8] = key.as_ref();
                    bytes.eq_ignore_ascii_case(b"filename") || bytes.eq_ignore_ascii_case(b"name")
                })
                .map(|(_, value)| {
                    let bytes: &[u8] = value.as_ref();
                    String::from_utf8_lossy(bytes).to_string()
                })
                .filter(|name| !name.trim().is_empty());
            // An inline image used in an HTML email is not a user-facing
            // attachment even when the sender provided a name parameter.
            let inline = disposition.is_some_and(|(kind, _)| {
                let bytes: &[u8] = kind.as_ref();
                bytes.eq_ignore_ascii_case(b"inline")
            });
            let inline_image = matches!(&body.specific,
            SpecificFields::Basic { r#type, .. } if {
                let bytes: &[u8] = r#type.as_ref();
                bytes.eq_ignore_ascii_case(b"image")
            });
            if is_file || (filename.is_some() && !(inline && inline_image)) {
                let unnamed = filename.is_none();
                let name = filename.unwrap_or_else(|| format!("attachment-{}", names.len() + 1));
                // Never treat MIME filenames as filesystem paths.
                let safe = name.rsplit(['/', '\\']).next().unwrap_or(&name);
                // An unnamed text/calendar part often duplicates a named
                // .ics part (Google Calendar). Keep the type information to
                // avoid inventing a second filename in the initial summary.
                let unnamed_calendar = unnamed
                    && matches!(&body.specific,
                    SpecificFields::Text { subtype, .. } if {
                        let sub: &[u8] = subtype.as_ref();
                        sub.eq_ignore_ascii_case(b"calendar")
                    });
                names.push((safe.to_owned(), unnamed_calendar));
            }
        }
    }
}

fn flags_from_imap(flags: &[FlagFetch<'_>]) -> MessageFlags {
    let mut out = MessageFlags::empty();
    for flag in flags {
        let FlagFetch::Flag(flag) = flag else {
            continue;
        };
        match flag {
            Flag::Seen => out.set(MessageFlags::SEEN, true),
            Flag::Answered => out.set(MessageFlags::ANSWERED, true),
            Flag::Flagged => out.set(MessageFlags::FLAGGED, true),
            Flag::Deleted => out.set(MessageFlags::DELETED, true),
            Flag::Draft => out.set(MessageFlags::DRAFT, true),
            Flag::Keyword(name) if name.as_ref().eq_ignore_ascii_case("$Forwarded") => {
                out.set(MessageFlags::FORWARDED, true);
            }
            _ => {}
        }
    }
    out
}

fn role_from_attributes(attributes: &[FlagNameAttribute<'_>]) -> Option<MailboxRole> {
    for attribute in attributes {
        match attribute.to_string().to_ascii_lowercase().as_str() {
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
mod tests;
