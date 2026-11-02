mod maildir;

use std::path::PathBuf;

use anyhow::{Result, anyhow};

use crate::model::{Capabilities, Mailbox, MessageBody, MessageId, MessageSummary};

pub use maildir::MaildirBackend;

/// Construct a backend from an account source URI.
///
/// Keeping scheme dispatch here mirrors aerc's worker factory boundary: the
/// runtime and UI do not need to know which storage/protocol implementation
/// owns an account.
pub fn from_source(source: &str) -> Result<Box<dyn MailBackend>> {
    let path = maildir_path_from_source(source)?;
    Ok(Box::new(MaildirBackend::new(path)))
}

pub fn maildir_path_from_source(source: &str) -> Result<PathBuf> {
    let Some(raw) = source.strip_prefix("maildir://") else {
        return Err(anyhow!(
            "unsupported backend source {source:?}; currently implemented: maildir://"
        ));
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

pub trait MailBackend: Send {
    fn capabilities(&self) -> Capabilities;
    fn connect(&mut self) -> Result<()>;
    fn list_mailboxes(&mut self) -> Result<Vec<Mailbox>>;
    fn open_mailbox(&mut self, name: &str) -> Result<Vec<MessageSummary>>;
    fn fetch_body(&mut self, mailbox: &str, id: &MessageId) -> Result<MessageBody>;
    fn set_seen(&mut self, mailbox: &str, id: &MessageId, seen: bool) -> Result<()>;
    fn delete(&mut self, mailbox: &str, id: &MessageId) -> Result<()>;
    fn move_to(&mut self, source: &str, destination: &str, id: &MessageId) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::maildir_path_from_source;

    #[test]
    fn parses_maildir_source() {
        assert_eq!(
            maildir_path_from_source("maildir:///tmp/mail").unwrap(),
            std::path::PathBuf::from("/tmp/mail")
        );
    }

    #[test]
    fn rejects_unknown_scheme() {
        assert!(maildir_path_from_source("imap://mail.example.com").is_err());
    }
}
