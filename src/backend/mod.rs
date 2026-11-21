mod error;
mod imap;
mod jmap;
mod maildir;
mod pop3;
pub(crate) mod search;

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow};

use crate::config::AccountConfig;
use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage, MessageSummary};

pub use error::{BackendError, ErrorKind};
pub(crate) use error::{classify, legacy_kind};
pub use imap::{ImapBackend, ImapSource};
pub use jmap::{JmapBackend, JmapSource};
pub use maildir::MaildirBackend;
pub use pop3::{Pop3Backend, Pop3Source};

/// One scheme classifier for factories, cache policy and worker selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Imap,
    Jmap,
    Pop3,
    Maildir,
}

impl BackendKind {
    pub fn from_source(source: &str) -> Option<Self> {
        if source.starts_with("maildir://") {
            Some(Self::Maildir)
        } else if ImapSource::supports(source) {
            Some(Self::Imap)
        } else if JmapSource::supports(source) {
            Some(Self::Jmap)
        } else if Pop3Source::supports(source) {
            Some(Self::Pop3)
        } else {
            None
        }
    }
    pub fn persistent_remote_cache(self) -> bool {
        matches!(self, Self::Imap | Self::Jmap)
    }
}

/// Construct a backend for an account. Credential commands are resolved by the
/// backend worker during connect(), never on the UI thread.
pub fn from_account(account: &AccountConfig) -> Result<Box<dyn MailBackend>> {
    let kind = BackendKind::from_source(&account.source);
    if kind == Some(BackendKind::Maildir) {
        return Ok(Box::new(MaildirBackend::new(maildir_path_from_source(
            &account.source,
        )?)));
    }

    if kind == Some(BackendKind::Jmap) {
        return Ok(Box::new(JmapBackend::new(
            JmapSource::parse(&account.source)?,
            account.source_cred_cmd.clone(),
            account,
        )?));
    }
    if kind == Some(BackendKind::Pop3) {
        let state_path = dirs::data_dir().map(|dir| {
            dir.join("jerc").join("pop3").join(format!(
                "{}.json",
                crate::cache::account_cache_stem(account)
            ))
        });
        return Ok(Box::new(Pop3Backend::new(
            Pop3Source::parse(&account.source)?,
            account.source_cred_cmd.clone(),
            state_path,
        )));
    }
    if kind == Some(BackendKind::Imap) {
        return Ok(Box::new(ImapBackend::new(
            ImapSource::parse(&account.source)?.with_account_oauth(account)?,
            account.source_cred_cmd.clone(),
            account.max_messages,
        )));
    }

    Err(anyhow!(
        "unsupported backend scheme; currently implemented: maildir://, imaps://, imap+starttls://, jmaps://, jmaps+bearer://, pop3s://, pop3+starttls://"
    ))
}

/// Convenience factory for callers that do not need an external credential
/// command. Tests and the direct Maildir CLI use this path.
pub fn from_source(source: &str) -> Result<Box<dyn MailBackend>> {
    let account = AccountConfig {
        name: "direct".to_owned(),
        source: source.to_owned(),
        email: None,
        imap_username: None,
        smtp_username: None,
        imap_host: None,
        smtp_host: None,
        imap_port: None,
        smtp_port: None,
        default_mailbox: "INBOX".to_owned(),
        exclude_mailboxes: Vec::new(),
        source_cred_cmd: None,
        from: None,
        outgoing: None,
        outgoing_cred_cmd: None,
        copy_to: None,
        oauth: None,
        max_messages: 200,
        page_size: 100,
    };
    from_account(&account)
}

pub fn maildir_path_from_source(source: &str) -> Result<PathBuf> {
    let Some(raw) = source.strip_prefix("maildir://") else {
        return Err(anyhow!("source is not a maildir:// URI"));
    };

    if raw == "~" {
        return dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return dirs::home_dir()
            .ok_or_else(|| anyhow!("cannot determine home directory"))
            .map(|home| home.join(rest));
    }
    Ok(PathBuf::from(raw))
}

/// Return a printable account source without ever exposing passwords, refresh
/// tokens, access tokens, client secrets, or OAuth query parameters.
#[must_use]
pub fn redact_source(source: &str) -> String {
    if source.starts_with("maildir://") {
        return source.to_owned();
    }

    if JmapSource::supports(source) {
        return JmapSource::parse(source)
            .map_or_else(|_| "jmaps://<redacted>".into(), |parsed| parsed.redacted());
    }
    if Pop3Source::supports(source) {
        return Pop3Source::parse(source)
            .map_or_else(|_| "pop3s://<redacted>".into(), |parsed| parsed.redacted());
    }
    match ImapSource::parse(source) {
        Ok(parsed) => parsed.redacted(),
        Err(_) => source.split_once("://").map_or_else(
            || "<invalid source>".to_owned(),
            |(scheme, _)| format!("{scheme}://<redacted>"),
        ),
    }
}

/// Previously displayed remote summaries. Each backend must verify its
/// stable cache identity and synchronization checkpoint before reuse. This hint is
/// deliberately metadata-only: message bodies never cross the worker queue.
#[derive(Clone)]
pub struct MailboxSyncHint {
    pub identity: crate::model::CacheIdentity,
    pub uid_next: Option<u32>,
    pub messages: Vec<MessageSummary>,
}

impl std::fmt::Debug for MailboxSyncHint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailboxSyncHint")
            .field("identity", &self.identity.generation())
            .field("uid_next", &self.uid_next)
            .field("cached_message_count", &self.messages.len())
            .finish()
    }
}

/// Native submission has already been accepted; filing failures are warnings,
/// not send failures. No raw message copy is needed by the worker.
#[derive(Debug)]
pub struct NativeSendReceipt {
    pub sent_mailbox: Option<String>,
    pub affected_mailboxes: Vec<String>,
    pub sent_copy_saved: bool,
    pub sent_copy_warning: Option<String>,
}

/// A temporary query view, not a canonical mailbox cache snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxView {
    Search(String),
    Threads,
    Thread(String),
}

impl MailboxView {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Search(_) => "Server search",
            Self::Threads => "Conversations",
            Self::Thread(_) => "Conversation",
        }
    }
}

/// Server-side attachment descriptor. Never contains credentials or a direct URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAttachment {
    pub identity: crate::model::CacheIdentity,
    pub blob_id: String,
    pub filename: String,
    pub content_type: String,
    pub size: usize,
}

pub trait MailBackend: Send {
    fn query_mailbox(
        &mut self,
        _name: &str,
        _view: &MailboxView,
        _offset: usize,
        _limit: usize,
    ) -> Result<MessagePage> {
        Err(anyhow!(
            "native mailbox queries are not supported by this backend"
        ))
    }
    /// Last successful query's membership hints for the requested cached IDs.
    /// These never populate the canonical cache with additional messages.
    fn cached_thread_memberships(
        &self,
        _ids: &[MessageId],
    ) -> std::collections::HashMap<MessageId, String> {
        std::collections::HashMap::new()
    }
    /// Metadata only. None selects the original MIME path.
    fn list_native_attachments(
        &mut self,
        _mailbox: &str,
        _id: &MessageId,
    ) -> Result<Option<Vec<RemoteAttachment>>> {
        Ok(None)
    }
    fn download_native_attachment_into(
        &mut self,
        _mailbox: &str,
        _attachment: &RemoteAttachment,
        _writer: &mut dyn Write,
    ) -> Result<()> {
        Err(anyhow!("native attachment retrieval is not supported"))
    }

    /// None means native submission was not selected or is not advertised.
    /// Errors must never trigger fallback: delivery may already have started.
    fn submit_message(
        &mut self,
        _message: &crate::model::OutgoingMessage,
        _sent_mailbox: Option<&str>,
        _copy_to: Option<&[String]>,
    ) -> Result<Option<NativeSendReceipt>> {
        Ok(None)
    }

    fn capabilities(&self) -> Capabilities;
    fn connect(&mut self) -> Result<()>;
    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>>;
    /// Dedicated read-only IDLE connection. Never runs on the interactive
    /// session. Backends without push support retain the existing poll path.
    fn watch_mailbox(
        &mut self,
        _name: &str,
        _cancel: &AtomicBool,
        _changed: &mut dyn FnMut(),
    ) -> Result<()> {
        Err(anyhow!("Push watching is not supported by this backend"))
    }
    fn mailbox_status(&mut self, name: &str) -> Result<(usize, usize)>;
    /// UIDVALIDITY from the currently selected mailbox, if provided by IMAP.
    fn mailbox_uid_validity(&self, _name: &str) -> Option<u32> {
        None
    }
    fn mailbox_cache_identity(&self, name: &str) -> Option<crate::model::CacheIdentity> {
        self.mailbox_uid_validity(name)
            .map(crate::model::CacheIdentity::Imap)
    }
    /// JMAP push reports account-wide changes rather than a SELECTed folder.
    fn watch_is_account_wide(&self) -> bool {
        false
    }
    fn mailbox_uid_next(&self, _name: &str) -> Option<u32> {
        None
    }
    fn open_mailbox(&mut self, name: &str, offset: usize, limit: usize) -> Result<MessagePage>;
    /// Optional metadata-only optimization; Maildir and older backends use
    /// the regular open path without any protocol-specific coupling.
    fn open_mailbox_with_hint(
        &mut self,
        name: &str,
        offset: usize,
        limit: usize,
        _hint: Option<MailboxSyncHint>,
    ) -> Result<MessagePage> {
        self.open_mailbox(name, offset, limit)
    }
    fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody>;
    fn fetch_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>>;
    /// Stream a raw .eml export directly into a private temporary file, rather
    /// than retaining an arbitrarily large message in the process heap.
    fn export_raw_into(
        &mut self,
        mailbox: &str,
        id: &MessageId,
        writer: &mut dyn Write,
    ) -> Result<()> {
        writer.write_all(&self.fetch_raw(mailbox, id)?)?;
        Ok(())
    }

    /// Append an already-built RFC 5322 message to a mailbox. Outgoing mail uses
    /// this for a Sent copy after the SMTP relay accepts the exact same bytes.
    fn append_message(&mut self, mailbox: &str, raw: &[u8], seen: bool) -> Result<()> {
        let _ = (mailbox, raw, seen);
        Err(anyhow!("backend does not support appending messages"))
    }
    /// Store an unsent message in Drafts. Backends should set the \Draft flag;
    /// this fallback preserves interoperability with simpler backends.
    fn append_draft(&mut self, mailbox: &str, raw: &[u8]) -> Result<()> {
        self.append_message(mailbox, raw, true)
    }
    fn set_seen(&mut self, mailbox: &str, id: &MessageId, seen: bool) -> Result<()>;
    fn delete(&mut self, mailbox: &str, id: &MessageId) -> Result<()>;
    fn delete_many(&mut self, mailbox: &str, ids: &[MessageId]) -> Result<()> {
        for id in ids {
            self.delete(mailbox, id)?;
        }
        Ok(())
    }
    fn archive_many(&mut self, source: &str, destination: &str, ids: &[MessageId]) -> Result<()> {
        self.move_many(source, destination, ids)
    }
    fn unarchive_many(&mut self, source: &str, destination: &str, ids: &[MessageId]) -> Result<()> {
        self.move_many(source, destination, ids)
    }
    fn move_to(&mut self, source: &str, destination: &str, id: &MessageId) -> Result<()>;
    fn move_many(&mut self, source: &str, destination: &str, ids: &[MessageId]) -> Result<()> {
        for id in ids {
            self.move_to(source, destination, id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackendKind, ImapSource, JmapSource, Pop3Source, maildir_path_from_source, redact_source,
    };

    #[test]
    fn parses_maildir_source() {
        assert_eq!(
            maildir_path_from_source("maildir:///tmp/mail").unwrap(),
            std::path::PathBuf::from("/tmp/mail")
        );
    }

    #[test]
    fn recognizes_imap_schemes() {
        assert!(ImapSource::supports("imaps://user@example.com"));
        assert!(ImapSource::supports("imaps+oauthbearer://user@example.com"));
        assert!(!ImapSource::supports("smtp://user@example.com"));
    }

    #[test]
    fn recognizes_additional_secure_backends() {
        assert!(JmapSource::supports("jmaps://hi@example.org"));
        assert!(Pop3Source::supports("pop3s://hi@example.org"));
        assert!(Pop3Source::supports("pop3+starttls://hi@example.org"));
        assert!(!Pop3Source::supports("pop3://hi@example.org"));
        assert!(!JmapSource::supports("jmap://hi@example.org"));
        for source in [
            "jmaps://hi:secret@example.org",
            "pop3s://hi:secret@example.org",
        ] {
            assert!(!redact_source(source).contains("secret"));
        }
    }

    #[test]
    fn shared_backend_kind_selects_correct_cache_policy() {
        for (source, kind, cached) in [
            ("imaps+xoauth2://user@example.org", BackendKind::Imap, true),
            ("jmaps+bearer://user@example.org", BackendKind::Jmap, true),
            ("pop3+starttls://user@example.org", BackendKind::Pop3, false),
            ("maildir:///tmp/mail", BackendKind::Maildir, false),
        ] {
            assert_eq!(BackendKind::from_source(source), Some(kind));
            assert_eq!(kind.persistent_remote_cache(), cached);
        }
        assert_eq!(BackendKind::from_source("smtp://user@example.org"), None);
    }

    #[test]
    fn debug_sources_do_not_expose_passwords() {
        let jmap = JmapSource::parse("jmaps://user:secret-password@example.org").unwrap();
        let pop3 = Pop3Source::parse("pop3s://user:secret-password@example.org").unwrap();
        assert!(!format!("{jmap:?}").contains("secret-password"));
        assert!(!format!("{pop3:?}").contains("secret-password"));
    }

    #[test]
    fn source_redaction_never_keeps_secret_material() {
        let source = "imaps+oauthbearer://user%40example.com:refresh%20token@imap.example.com:993?client_id=abc&client_secret=super-secret&token_endpoint=https%3A%2F%2Foauth.example.com%2Ftoken";
        let redacted = redact_source(source);
        assert!(redacted.contains("user@example.com"));
        assert!(redacted.contains("imap.example.com:993"));
        assert!(!redacted.contains("refresh"));
        assert!(!redacted.contains("client_secret"));
        assert!(!redacted.contains("super-secret"));
        assert!(!redacted.contains("client_id"));
    }
}
