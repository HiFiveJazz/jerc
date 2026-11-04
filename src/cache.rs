//! Small, private, per-account persistent mailbox index.
//!
//! The UI reads this once before connecting. Writes are serialized on a
//! dedicated thread so a slow disk cannot stall keyboard input or painting.
//! This is a display cache, never an authority for IMAP UID mutations.
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::AccountConfig;
use crate::model::{Mailbox, MessageBody, MessageId, MessageSummary};

const VERSION: u8 = 1;
const MAX_CACHE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FOLDERS: usize = 256;
const MAX_MESSAGES_PER_FOLDER: usize = 400;
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_BODIES_PER_FOLDER: usize = 12;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CachedMailbox {
    pub messages: Vec<MessageSummary>,
    pub bodies: HashMap<MessageId, MessageBody>,
    pub message_index: usize,
    pub message_view_offset: usize,
    pub next_page_offset: usize,
    pub has_more: bool,
    /// RFC 3501 UIDVALIDITY is required for safely reusing cached IMAP UIDs.
    pub uid_validity: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    #[serde(default)]
    pub mailboxes: Vec<Mailbox>,
    #[serde(default)]
    pub folders: HashMap<String, CachedMailbox>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    version: u8,
    snapshot: Snapshot,
}

// Serialize the live snapshot by reference: cloning a potentially large mailbox
// cache on every background write would add unnecessary allocations and latency.
#[derive(Serialize)]
struct CacheFileRef<'a> {
    version: u8,
    snapshot: &'a Snapshot,
}

pub(crate) enum Change {
    Mailboxes(Vec<Mailbox>),
    Folder(String, CachedMailbox),
    RemoveFolder(String),
}

pub(crate) struct Writer {
    tx: Option<Sender<Change>>,
    join: Option<JoinHandle<()>>,
}

impl Writer {
    pub(crate) fn send(&self, change: Change) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(change);
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Only IMAP data is persisted. Maildir remains authoritative on disk already.
/// The key uses the account's name and non-secret IMAP identity, not tokens or
/// credential commands. Files contain message metadata, so permissions matter.
pub(crate) fn open(account: &AccountConfig) -> (Snapshot, Option<Writer>) {
    if std::env::var_os("AERC_RS_DISABLE_CACHE").is_some()
        || !crate::backend::ImapSource::supports(&account.source) {
        return (Snapshot::default(), None);
    }
    let Some(dir) = dirs::cache_dir().map(|path| path.join("aerc-rs").join("mail-index")) else {
        return (Snapshot::default(), None);
    };
    let identity = crate::backend::redact_source(&account.source);
    let hash = stable_key(&format!("{}\0{identity}", account.name));
    let path = dir.join(format!("{hash:016x}.json"));
    let snapshot = read_snapshot(&path).unwrap_or_default();
    let (tx, rx) = mpsc::channel::<Change>();
    let mut stored = snapshot.clone();
    let join = thread::Builder::new()
        .name("mail-cache-writer".into())
        .spawn(move || {
            while let Ok(change) = rx.recv() {
                apply(&mut stored, change);
                // Combine the stream of 10+ STATUS replies on startup into a
                // single atomic write. Shutdown drains the channel before exit.
                while let Ok(change) = rx.recv_timeout(Duration::from_millis(160)) {
                    apply(&mut stored, change);
                }
                let _ = write_snapshot(&path, &stored);
            }
        });
    match join {
        Ok(join) => (snapshot, Some(Writer { tx: Some(tx), join: Some(join) })),
        Err(_) => (snapshot, None),
    }
}

fn apply(snapshot: &mut Snapshot, change: Change) {
    match change {
        Change::Mailboxes(mailboxes) => snapshot.mailboxes = mailboxes,
        Change::Folder(name, mut folder) => {
            // Never cache UID-based IMAP data without a server UIDVALIDITY.
            if folder.uid_validity.is_none() { return; }
            if folder.messages.len() > MAX_MESSAGES_PER_FOLDER {
                folder.messages.truncate(MAX_MESSAGES_PER_FOLDER);
                folder.next_page_offset = folder.messages.len();
                folder.has_more = true;
            }
            folder.bodies.retain(|id, body| {
                folder.messages.iter().any(|message| &message.id == id)
                    && body.plain_text.as_deref().unwrap_or_default().len()
                        + body.html_source.as_deref().unwrap_or_default().len()
                        + body.html_text.as_deref().unwrap_or_default().len()
                        <= MAX_BODY_BYTES
            });
            if folder.bodies.len() > MAX_BODIES_PER_FOLDER {
                let keep = folder.messages.iter().filter_map(|message| {
                    folder.bodies.contains_key(&message.id).then_some(message.id.clone())
                }).take(MAX_BODIES_PER_FOLDER).collect::<Vec<_>>();
                folder.bodies.retain(|id, _| keep.contains(id));
            }
            if snapshot.folders.len() < MAX_FOLDERS || snapshot.folders.contains_key(&name) {
                snapshot.folders.insert(name, folder);
            }
        }
        Change::RemoveFolder(name) => { snapshot.folders.remove(&name); }
    }
}

fn stable_key(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn read_snapshot(path: &PathBuf) -> Option<Snapshot> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_CACHE_BYTES { return None; }
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_CACHE_BYTES { return None; }
    let file: CacheFile = serde_json::from_slice(&bytes).ok()?;
    if file.version != VERSION || file.snapshot.folders.len() > MAX_FOLDERS {
        return None;
    }
    // Discard any legacy or invalid records lacking UIDVALIDITY.
    let mut snapshot = file.snapshot;
    snapshot.folders.retain(|_, folder| folder.uid_validity.is_some_and(|validity| {
        let prefix = format!("imap-uid:{validity}:");
        folder.messages.len() <= MAX_MESSAGES_PER_FOLDER
            && folder.messages.iter().all(|message| message.id.0.starts_with(&prefix))
            && folder.bodies.keys().all(|id| id.0.starts_with(&prefix))
    }));
    Some(snapshot)
}

fn write_snapshot(path: &PathBuf, snapshot: &Snapshot) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::other("no cache parent directory"));
    };
    fs::create_dir_all(parent)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(&mut tmp, &CacheFileRef { version: VERSION, snapshot })
        .map_err(std::io::Error::other)?;
    tmp.flush()?;
    if tmp.as_file().metadata()?.len() > MAX_CACHE_BYTES {
        return Err(std::io::Error::other("mail cache size limit exceeded"));
    }
    tmp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrip_and_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let data = Snapshot { mailboxes: vec![Mailbox::new("INBOX", crate::model::MailboxRole::Inbox)],
            folders: HashMap::new() };
        write_snapshot(&path, &data).unwrap();
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.mailboxes[0].name, "INBOX");
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn corrupted_or_old_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(read_snapshot(&path).is_none());
        std::fs::write(&path, br#"{"version":0,"snapshot":{}}"#).unwrap();
        assert!(read_snapshot(&path).is_none());
    }

    #[test]
    fn cache_requires_uidvalidity() {
        let mut snapshot = Snapshot::default();
        let folder = CachedMailbox { messages: vec![], bodies: HashMap::new(), message_index: 0,
            message_view_offset: 0, next_page_offset: 0, has_more: false, uid_validity: None };
        apply(&mut snapshot, Change::Folder("INBOX".into(), folder.clone()));
        assert!(!snapshot.folders.contains_key("INBOX"));
        apply(&mut snapshot, Change::Folder("INBOX".into(), CachedMailbox {
            uid_validity: Some(42), ..folder
        }));
        assert!(snapshot.folders.contains_key("INBOX"));
    }
}
