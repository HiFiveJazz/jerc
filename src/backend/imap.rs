use std::borrow::Cow;
use std::num::NonZeroU32;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use io_imap::client::{ImapClient, ImapClientStd, ImapClientStdConnectOptions};
use io_imap::rfc3501::{
    append::ImapMessageAppendOptions,
    copy::ImapMessageCopyOptions,
    fetch::ImapMessageFetchOptions,
    login::ImapLoginOptions,
    select::ImapMailboxSelectOptions,
    store::ImapMessageStoreOptions,
};
use io_imap::rfc6851::r#move::ImapMessageMoveOptions;
use io_imap::rfc7628::auth_oauthbearer::ImapAuthOauthbearerOptions;
use io_imap::sasl::auth_xoauth2::ImapAuthXoauth2Options;
use io_imap::types::{
    fetch::{MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName},
    flag::{Flag, FlagFetch, FlagNameAttribute, StoreType},
    mailbox::{ListMailbox, Mailbox as ImapMailbox},
    sequence::SequenceSet,
    status::{StatusDataItem, StatusDataItemName},
};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use url::Url;

use crate::backend::MailBackend;
use crate::mail::{parse_body, parse_summary};
use crate::model::{
    Capabilities, Mailbox, MailboxRole, MessageBody, MessageFlags, MessageId, MessagePage,
};

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
        source.split_once("://").is_some_and(|(scheme, _)| {
            matches!(
                scheme,
                "imaps" | "imaps+login" | "imaps+oauthbearer" | "imaps+xoauth2"
            )
        })
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
    session: Option<ImapClientStd>,
    selected_mailbox: Option<String>,
    selected_exists: Option<u32>,
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
        }
    }

    fn session(&mut self) -> Result<&mut ImapClientStd> {
        self.session
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP account is not connected"))
    }

    fn select(&mut self, mailbox: &str) -> Result<io_imap::rfc3501::select::ImapMailboxSelectData> {
        let mailbox_name = mailbox_from_name(mailbox)?;
        let selected = self
            .session()?
            .select(mailbox_name, ImapMailboxSelectOptions::default())
            .with_context(|| format!("selecting IMAP mailbox {mailbox:?}"))?;
        self.selected_mailbox = Some(mailbox.to_owned());
        self.selected_exists = Some(selected.exists.unwrap_or(0));
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
            self.selected_exists = self.selected_exists.map(|exists| exists.saturating_sub(count));
        }
    }

    fn uid_from_id(id: &MessageId) -> Result<u32> {
        id.0
            .strip_prefix("imap-uid:")
            .ok_or_else(|| anyhow!("message ID is not an IMAP UID"))?
            .parse::<u32>()
            .context("parsing IMAP UID")
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
        let credential = self.source.credential(self.credential_command.as_deref())?;
        let connect_url = Url::parse(&format!(
            "imaps://{}:{}",
            self.source.host, self.source.port
        ))
        .context("building IMAP connection URL")?;
        let (mut client, _) = ImapClientStd::connect(
            &connect_url,
            ImapClientStdConnectOptions::default(),
        )
        .context("connecting to IMAP server")?;
        client
            .stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .context("setting IMAP read timeout")?;

        match self.source.auth {
            ImapAuth::Password | ImapAuth::Login => {
                client
                    .login(
                        &self.source.username,
                        &credential,
                        ImapLoginOptions::default(),
                    )
                    .context("IMAP authentication failed")?;
            }
            ImapAuth::OAuthBearer => {
                let access_token = self.source.access_token(&credential)?;
                client
                    .auth_oauthbearer(
                        &self.source.username,
                        &self.source.host,
                        self.source.port,
                        &access_token,
                        ImapAuthOauthbearerOptions::default(),
                    )
                    .context("IMAP OAUTHBEARER authentication failed")?;
            }
            ImapAuth::XOAuth2 => {
                let access_token = self.source.access_token(&credential)?;
                client
                    .auth_xoauth2(
                        &self.source.username,
                        &access_token,
                        ImapAuthXoauth2Options::default(),
                    )
                    .context("IMAP XOAUTH2 authentication failed")?;
            }
        }

        self.session = Some(client);
        self.selected_mailbox = None;
        self.selected_exists = None;
        Ok(())
    }

    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>> {
        let reference: ImapMailbox<'static> = "".to_owned().try_into().context("building LIST reference")?;
        let pattern: ListMailbox<'static> = "*".to_owned().try_into().context("building LIST pattern")?;
        let names = self
            .session()?
            .list(reference, pattern)
            .context("listing IMAP mailboxes")?;

        let mut selectable: Vec<(String, MailboxRole)> = names
            .into_iter()
            .filter(|(_, _, attrs)| !attrs.iter().any(|attr| matches!(attr, FlagNameAttribute::Noselect)))
            .map(|(mailbox, _, attrs)| {
                let mailbox_name = mailbox_to_name(&mailbox);
                let role = role_from_attributes(&attrs)
                    .unwrap_or_else(|| role_for_name(&mailbox_name));
                (mailbox_name, role)
            })
            .collect();
        selectable.sort_by(|a, b| mailbox_sort_key(&a.0, &a.1).cmp(&mailbox_sort_key(&b.0, &b.1)));

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
        let mailbox = mailbox_from_name(name)?;
        let items = self
            .session()?
            .status(
                mailbox,
                Cow::Owned(vec![StatusDataItemName::Messages, StatusDataItemName::Unseen]),
            )
            .with_context(|| format!("reading status for {name:?}"))?;
        let mut total = 0usize;
        let mut unread = 0usize;
        for item in items {
            match item {
                StatusDataItem::Messages(value) => total = value as usize,
                StatusDataItem::Unseen(value) => unread = value as usize,
                _ => {}
            }
        }
        Ok((total, unread))
    }

    fn open_mailbox(&mut self, name: &str, offset: usize, limit: usize) -> Result<MessagePage> {
        // Always re-SELECT the first page so a manual refresh sees new arrivals.
        // Later pages can reuse the selected mailbox's EXISTS count without an
        // extra round trip.
        let exists = if offset == 0 {
            self.select(name)?.exists.unwrap_or(0)
        } else {
            self.mailbox_exists(name)?
        };
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
        ]
        .into();
        let fetched = self
            .session()?
            .fetch(sequence, items, ImapMessageFetchOptions::default())
            .with_context(|| format!("fetching headers from {name:?}"))?;

        let mut messages = Vec::with_capacity(fetched.len());
        for data_items in fetched.values() {
            let mut uid = None;
            let mut flags = MessageFlags::empty();
            let mut size = 0u64;
            let mut headers = None;
            for item in data_items.as_ref() {
                match item {
                    MessageDataItem::Uid(value) => uid = Some(value.get()),
                    MessageDataItem::Flags(value) => flags = flags_from_imap(value),
                    MessageDataItem::Rfc822Size(value) => size = u64::from(*value),
                    MessageDataItem::Rfc822Header(value) => {
                        headers = value.clone().into_option().map(Cow::into_owned)
                    }
                    _ => {}
                }
            }
            let (Some(uid), Some(headers)) = (uid, headers) else {
                continue;
            };
            let id = MessageId(format!("imap-uid:{uid}"));
            if let Ok(summary) = parse_summary(id, &headers, size, flags) {
                messages.push(summary);
            }
        }
        messages.sort_by(|a, b| {
            b.timestamp
                .cmp(&a.timestamp)
                .then_with(|| b.id.cmp(&a.id))
        });
        let consumed = usize::try_from(end.saturating_sub(start).saturating_add(1)).unwrap_or(usize::MAX);
        let next_offset = offset.saturating_add(consumed);
        Ok(MessagePage {
            messages,
            offset,
            next_offset,
            has_more: start > 1,
        })
    }

    fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody> {
        let raw = self.fetch_raw(mailbox, id)?;
        parse_body(id.clone(), &raw).context("parsing IMAP message body")
    }

    fn fetch_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>> {
        self.ensure_selected(mailbox)?;
        let uid = Self::uid_from_id(id)?;
        let uid = NonZeroU32::new(uid).ok_or_else(|| anyhow!("IMAP UID cannot be zero"))?;
        let mut raw = Vec::new();
        self.session()?
            .fetch_body_stream(uid, true, &mut raw)
            .with_context(|| format!("fetching IMAP message UID {uid}"))?;
        if raw.is_empty() {
            bail!("IMAP server did not return requested UID {uid}");
        }
        Ok(raw)
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

    fn set_seen(&mut self, mailbox: &str, id: &MessageId, seen: bool) -> Result<()> {
        self.ensure_selected(mailbox)?;
        let sequence = Self::uid_sequence(id)?;
        let mut opts = ImapMessageStoreOptions::default();
        opts.uid = true;
        let kind = if seen { StoreType::Add } else { StoreType::Remove };
        self.session()?
            .store(sequence, kind, vec![Flag::Seen], opts)
            .context("updating IMAP seen flag")?;
        Ok(())
    }

    fn delete(&mut self, mailbox: &str, id: &MessageId) -> Result<()> {
        self.ensure_selected(mailbox)?;
        let sequence = Self::uid_sequence(id)?;
        let mut opts = ImapMessageStoreOptions::default();
        opts.uid = true;
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
        let sequence = Self::uid_sequence_many(ids)?;
        let mut opts = ImapMessageStoreOptions::default();
        opts.uid = true;
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
        if ids.is_empty() { return Ok(()); }
        if is_gmail_archive(&self.source.host, source, destination) {
            // In Gmail, All Mail is a virtual view that already contains INBOX
            // messages. Copy/MOVE into All Mail need not remove the INBOX label.
            // The Gmail IMAP archive operation removes messages from the INBOX
            // by marking only their selected UIDs deleted and expunging those
            // UIDs. This is *not* used for a generic mail server or other folder.
            self.ensure_selected(source)?;
            let uids = Self::uid_sequence_many(ids)?;
            let mut options = ImapMessageStoreOptions::default();
            options.uid = true;
            self.session()?.store(uids.clone(), StoreType::Add,
                vec![Flag::Deleted], options)
                .context("marking Gmail INBOX labels for removal")?;
            self.session()?.uid_expunge(uids)
                .context("removing Gmail INBOX labels")?;
            self.note_removed(source, ids.len());
            Ok(())
        } else {
            self.move_many(source, destination, ids)
        }
    }

    fn unarchive_many(&mut self, source: &str, destination: &str, ids: &[MessageId]) -> Result<()> {
        if ids.is_empty() { return Ok(()); }
        if is_gmail_unarchive(&self.source.host, source, destination) {
            // Adding INBOX in Gmail must not remove the message from the All Mail
            // view, so COPY (add label) instead of MOVE (remove source).
            self.ensure_selected(source)?;
            let uids = Self::uid_sequence_many(ids)?;
            let mut options = ImapMessageCopyOptions::default();
            options.uid = true;
            self.session()?.copy(uids, mailbox_from_name(destination)?, options)
                .context("restoring Gmail message to INBOX")?;
            Ok(())
        } else {
            self.move_many(source, destination, ids)
        }
    }

    fn move_to(&mut self, source: &str, destination: &str, id: &MessageId) -> Result<()> {
        self.ensure_selected(source)?;
        let destination_mailbox = mailbox_from_name(destination)?;
        let sequence = Self::uid_sequence(id)?;
        let mut move_opts = ImapMessageMoveOptions::default();
        move_opts.uid = true;
        if self
            .session()?
            .r#move(sequence.clone(), destination_mailbox.clone(), move_opts)
            .is_ok()
        {
            self.note_removed(source, 1);
            return Ok(());
        }

        let mut copy_opts = ImapMessageCopyOptions::default();
        copy_opts.uid = true;
        self.session()?
            .copy(sequence.clone(), destination_mailbox, copy_opts)
            .with_context(|| format!("copying IMAP message to {destination:?}"))?;
        let mut store_opts = ImapMessageStoreOptions::default();
        store_opts.uid = true;
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
        let destination_mailbox = mailbox_from_name(destination)?;
        let sequence = Self::uid_sequence_many(ids)?;
        let mut move_opts = ImapMessageMoveOptions::default();
        move_opts.uid = true;
        if self
            .session()?
            .r#move(sequence.clone(), destination_mailbox.clone(), move_opts)
            .is_ok()
        {
            self.note_removed(source, ids.len());
            return Ok(());
        }

        let mut copy_opts = ImapMessageCopyOptions::default();
        copy_opts.uid = true;
        self.session()?
            .copy(sequence.clone(), destination_mailbox, copy_opts)
            .with_context(|| format!("copying IMAP messages to {destination:?}"))?;
        let mut store_opts = ImapMessageStoreOptions::default();
        store_opts.uid = true;
        self.session()?
            .store(sequence.clone(), StoreType::Add, vec![Flag::Deleted], store_opts)
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
    matches!(host.to_ascii_lowercase().as_str(), "imap.gmail.com" | "imap.googlemail.com")
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
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    if secret.is_empty() {
        bail!("source_cred_cmd returned an empty credential");
    }
    Ok(secret)
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
mod tests {
    use super::{ImapAuth, ImapSource, role_for_name};

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
    fn common_mailbox_names_map_to_roles() {
        assert_eq!(role_for_name("INBOX"), crate::model::MailboxRole::Inbox);
        assert_eq!(role_for_name("[Gmail]/Sent Mail"), crate::model::MailboxRole::Sent);
        assert_eq!(role_for_name("[Gmail]/All Mail"), crate::model::MailboxRole::Archive);
        assert_eq!(role_for_name("[Gmail]/Trash"), crate::model::MailboxRole::Trash);
    }
}
