//! UI-only acknowledgment persistence. An acknowledgment never modifies or
//! authorizes a retry of a durable mutation or SMTP Outbox operation.
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

const VERSION: u8 = 1;
const MAX_FILE_BYTES: u64 = 160 * 1024;
const MAX_KEYS: usize = 2048;

#[derive(Debug)]
pub(super) struct NoticeAcknowledgments {
    path: PathBuf,
    readable: bool,
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    version: u8,
    keys: Vec<String>,
    #[serde(default)]
    first_seen: HashMap<String, u64>,
}

pub(super) fn key_digest(key: &str) -> String {
    digest(&SHA256, key.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl NoticeAcknowledgments {
    pub(super) fn open(
        config_path: Option<&Path>,
    ) -> (Option<Self>, HashSet<String>, HashMap<String, u64>) {
        // No implicit user-global writes in synthetic/test App instances.
        let Some(config_path) = config_path else {
            return (None, HashSet::new(), HashMap::new());
        };
        let Some(root) = dirs::cache_dir() else {
            return (None, HashSet::new(), HashMap::new());
        };
        // Distinguish independent configurations. No account names, subjects,
        // mailbox names or message UIDs are written to the acknowledgment file.
        let filename = format!("{}.json", key_digest(&config_path.to_string_lossy()));
        let mut store = Self {
            path: root.join("jerc").join("notice-ack").join(filename),
            readable: true,
        };
        match store.read_state() {
            Ok((keys, seen)) => (Some(store), keys, seen),
            Err(_) => {
                // Preserve unreadable/corrupt files; never overwrite them.
                store.readable = false;
                (Some(store), HashSet::new(), HashMap::new())
            }
        }
    }

    #[cfg(test)]
    pub(super) fn read(&self) -> std::io::Result<HashSet<String>> {
        self.read_state().map(|(keys, _)| keys)
    }

    pub(super) fn read_state(&self) -> std::io::Result<(HashSet<String>, HashMap<String, u64>)> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((HashSet::new(), HashMap::new()));
            }
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err(std::io::Error::other("invalid notice acknowledgment file"));
        }
        let mut bytes = Vec::new();
        File::open(&self.path)?
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(std::io::Error::other(
                "notice acknowledgment file too large",
            ));
        }
        let file: FileFormat = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        if file.version != VERSION
            || file.keys.len() > MAX_KEYS
            || file.first_seen.len() > MAX_KEYS
            || !file
                .keys
                .iter()
                .chain(file.first_seen.keys())
                .all(|key| key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(std::io::Error::other("invalid notice acknowledgments"));
        }
        Ok((file.keys.into_iter().collect(), file.first_seen))
    }

    #[cfg(test)]
    pub(super) fn save(&self, keys: &HashSet<String>) -> std::io::Result<()> {
        self.save_state(keys, &HashMap::new())
    }

    pub(super) fn save_state(
        &self,
        keys: &HashSet<String>,
        first_seen: &HashMap<String, u64>,
    ) -> std::io::Result<()> {
        if !self.readable {
            return Err(std::io::Error::other(
                "notice acknowledgments unreadable; original file preserved",
            ));
        }
        if keys.len() > MAX_KEYS || first_seen.len() > MAX_KEYS {
            return Err(std::io::Error::other("too many notice acknowledgments"));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("no notice acknowledgment directory"))?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        // Do not replace a symlink or an unexpected directory.
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(std::io::Error::other(
                    "invalid notice acknowledgment target",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let mut sorted = keys.iter().cloned().collect::<Vec<_>>();
        sorted.sort();
        serde_json::to_writer(
            &mut tmp,
            &FileFormat {
                version: VERSION,
                keys: sorted,
                first_seen: first_seen.clone(),
            },
        )
        .map_err(std::io::Error::other)?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path).map_err(|error| error.error)?;
        #[cfg(unix)]
        {
            let _ = File::open(parent).and_then(|dir| dir.sync_all());
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn for_test(path: PathBuf) -> Self {
        Self {
            path,
            readable: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledgment_round_trip_preserves_only_digests_and_file_privacy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices").join("ack.json");
        let store = NoticeAcknowledgments::for_test(path.clone());
        let secret = "review-mutation:Burner:INBOX:1:4225";
        let expected = HashSet::from([key_digest(secret)]);
        store.save(&expected).unwrap();
        assert_eq!(store.read().unwrap(), expected);
        assert!(!std::fs::read_to_string(&path).unwrap().contains(secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn corrupt_acknowledgments_cannot_hide_notices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not-json").unwrap();
        let store = NoticeAcknowledgments::for_test(path);
        assert!(store.read().is_err());
    }
    #[test]
    fn legacy_ack_file_without_timestamps_is_still_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.json");
        let id = key_digest("connection:Burner:1");
        std::fs::write(&path, format!(r#"{{"version":1,"keys":["{id}"]}}"#)).unwrap();
        let (keys, first_seen) = NoticeAcknowledgments::for_test(path.clone())
            .read_state()
            .unwrap();
        assert!(keys.contains(&id));
        assert!(first_seen.is_empty());
        let mut now = HashMap::new();
        now.insert(id.clone(), 123456);
        NoticeAcknowledgments::for_test(path)
            .save_state(&keys, &now)
            .unwrap();
        assert_eq!(
            now,
            NoticeAcknowledgments::for_test(dir.path().join("old.json"))
                .read_state()
                .unwrap()
                .1
        );
    }
}
