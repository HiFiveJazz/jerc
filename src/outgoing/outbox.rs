//! Crash-safe private outgoing queue, storing complete MIME inputs including
//! attachments (not paths that can disappear before reconnection).
//! A submitted SMTP operation is intentionally NOT retried after an ambiguous
//! failure; the relay may have accepted DATA before the connection dropped.
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::config::AccountConfig;
use crate::model::{MessageId, OutgoingMessage};

const VERSION: u8 = 1;
const MAX_OUTBOX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MESSAGES: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SendState {
    #[default]
    Queued,
    NeedsReview,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct QueuedSend {
    pub id: String,
    pub message: OutgoingMessage,
    pub sent_mailbox: Option<String>,
    pub copy_to: Option<Vec<String>>,
    pub draft_source: Option<(String, MessageId)>,
    #[serde(default)]
    pub state: SendState,
}

impl QueuedSend {
    pub fn new(
        message: OutgoingMessage,
        sent_mailbox: Option<String>,
        copy_to: Option<Vec<String>>,
        draft_source: Option<(String, MessageId)>,
    ) -> Result<Self> {
        let mut bytes = [0u8; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| anyhow!("could not generate outgoing queue ID"))?;
        let id = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(Self {
            id,
            message,
            sent_mailbox,
            copy_to,
            draft_source,
            state: SendState::Queued,
        })
    }
}

#[derive(Debug)]
pub(crate) struct Store {
    path: PathBuf,
    readable: bool,
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    version: u8,
    messages: Vec<QueuedSend>,
}

// Serialize outgoing messages by reference; attachments can be large.
#[derive(Serialize)]
struct FileFormatRef<'a> {
    version: u8,
    messages: &'a [QueuedSend],
}

impl Store {
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        Self {
            path,
            readable: true,
        }
    }

    pub(crate) fn is_readable(&self) -> bool {
        self.readable
    }

    pub(crate) fn open(account: &AccountConfig) -> (Option<Self>, Vec<QueuedSend>) {
        if std::env::var_os("JERC_DISABLE_CACHE").is_some() || account.outgoing.is_none() {
            return (None, Vec::new());
        }
        let Some(root) = dirs::data_local_dir() else {
            return (None, Vec::new());
        };
        use ring::digest::{SHA256, digest};
        let identity = format!(
            "{}\0{}",
            account.name,
            crate::backend::redact_source(&account.source)
        );
        let hash = digest(&SHA256, identity.as_bytes());
        let key = hash.as_ref()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let mut store = Self {
            path: root.join("jerc").join("outbox").join(format!("{key}.json")),
            readable: true,
        };
        let messages = match store.read() {
            Ok(messages) => messages,
            Err(_) => {
                // Never overwrite corrupt or unreadable unsent mail.
                store.readable = false;
                Vec::new()
            }
        };
        (Some(store), messages)
    }

    fn read(&self) -> Result<Vec<QueuedSend>> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("inspecting outbox"),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_OUTBOX_BYTES {
            return Err(anyhow!(
                "outbox is not a regular file or exceeds the size limit"
            ));
        }
        let mut bytes = Vec::new();
        File::open(&self.path)?
            .take(MAX_OUTBOX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_OUTBOX_BYTES {
            return Err(anyhow!("outbox exceeds the size limit"));
        }
        let stored: FileFormat = serde_json::from_slice(&bytes).context("decoding outbox")?;
        if stored.version != VERSION || stored.messages.len() > MAX_MESSAGES {
            return Err(anyhow!("unsupported outbox version or queue size"));
        }
        Ok(stored.messages)
    }

    pub(crate) fn save(&self, messages: &[QueuedSend]) -> Result<()> {
        if !self.readable {
            return Err(anyhow!(
                "unreadable outbox needs manual recovery (not overwritten)"
            ));
        }
        if messages.len() > MAX_MESSAGES {
            return Err(anyhow!("outbox full (max {MAX_MESSAGES} messages)"));
        }
        let root = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("outbox has no directory"))?;
        fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        }
        let mut file = tempfile::NamedTempFile::new_in(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        serde_json::to_writer(
            &mut file,
            &FileFormatRef {
                version: VERSION,
                messages,
            },
        )?;
        file.flush()?;
        if file.as_file().metadata()?.len() > MAX_OUTBOX_BYTES {
            return Err(anyhow!(
                "outgoing mail too large for the offline queue (64 MiB limit)"
            ));
        }
        file.as_file().sync_all()?;
        file.persist(&self.path)
            .context("atomically committing outbox")?;
        #[cfg(unix)]
        {
            let _ = File::open(root).and_then(|dir| dir.sync_all());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_outgoing_payload_survives_restart_and_is_private() {
        let root = tempfile::tempdir().unwrap();
        let store = Store {
            path: root.path().join("outbox").join("test.json"),
            readable: true,
        };
        let email = OutgoingMessage {
            from: "a@example.org".into(),
            to: "b@example.org".into(),
            cc: "".into(),
            bcc: "hidden@example.org".into(),
            subject: "test".into(),
            body: "hi".into(),
            html_body: None,
            attachments: vec![crate::model::OutgoingAttachment {
                filename: "a.txt".into(),
                content_type: "text/plain".into(),
                data: b"hello".to_vec(),
            }],
            custom_headers: vec![],
            in_reply_to: None,
            references: vec![],
            forwarded_raw: None,
        };
        let queued = QueuedSend::new(email, None, None, None).unwrap();
        store.save(std::slice::from_ref(&queued)).unwrap();
        let saved = store.read().unwrap();
        assert_eq!(saved[0].message.bcc, "hidden@example.org");
        assert_eq!(saved[0].message.attachments[0].data, b"hello");
        assert_eq!(saved[0].state, SendState::Queued);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
