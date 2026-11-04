mod imap;
mod maildir;

use std::path::PathBuf;

use anyhow::{Result, anyhow};

use crate::config::AccountConfig;
use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessagePage};

pub use imap::{ImapBackend, ImapSource};
pub use maildir::MaildirBackend;

/// Construct a backend for an account. Credential commands are resolved by the
/// backend worker during connect(), never on the UI thread.
pub fn from_account(account: &AccountConfig) -> Result<Box<dyn MailBackend>> {
    if account.source.starts_with("maildir://") {
        return Ok(Box::new(MaildirBackend::new(maildir_path_from_source(
            &account.source,
        )?)));
    }

    if ImapSource::supports(&account.source) {
        return Ok(Box::new(ImapBackend::new(
            ImapSource::parse(&account.source)?,
            account.source_cred_cmd.clone(),
            account.max_messages,
        )));
    }

    Err(anyhow!(
        "unsupported backend scheme; currently implemented: maildir://, imaps://, imaps+login://, imaps+oauthbearer://, imaps+xoauth2://"
    ))
}

/// Convenience factory for callers that do not need an external credential
/// command. Tests and the direct Maildir CLI use this path.
pub fn from_source(source: &str) -> Result<Box<dyn MailBackend>> {
    let account = AccountConfig {
        name: "direct".to_owned(),
        source: source.to_owned(),
        default_mailbox: "INBOX".to_owned(),
        source_cred_cmd: None,
        from: None,
        outgoing: None,
        outgoing_cred_cmd: None,
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

    match ImapSource::parse(source) {
        Ok(parsed) => parsed.redacted(),
        Err(_) => source
            .split_once("://")
            .map_or_else(|| "<invalid source>".to_owned(), |(scheme, _)| format!("{scheme}://<redacted>")),
    }
}

pub trait MailBackend: Send {
    fn capabilities(&self) -> Capabilities;
    fn connect(&mut self) -> Result<()>;
    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>>;
    fn mailbox_status(&mut self, name: &str) -> Result<(usize, usize)>;
    /// UIDVALIDITY from the currently selected mailbox, if provided by IMAP.
    fn mailbox_uid_validity(&self, _name: &str) -> Option<u32> { None }
    fn open_mailbox(&mut self, name: &str, offset: usize, limit: usize) -> Result<MessagePage>;
    fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody>;
    fn fetch_raw(&mut self, mailbox: &str, id: &MessageId) -> Result<Vec<u8>>;
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
    use super::{ImapSource, maildir_path_from_source, redact_source};

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
