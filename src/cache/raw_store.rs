//! Disk-backed original MIME storage and bounded pruning.
use super::*;

impl RawStore {
    pub(super) fn raw_path(
        &self,
        mailbox: &str,
        identity: &CacheIdentity,
        id: &MessageId,
    ) -> PathBuf {
        self.dir
            .join(format!("{}.eml", cache_message_key(mailbox, identity, id)))
    }

    /// Disk reads happen on a worker, never in the terminal render loop.
    pub(crate) fn read(
        &self,
        mailbox: &str,
        validity: &CacheIdentity,
        id: &MessageId,
    ) -> Option<Vec<u8>> {
        if !validity.accepts(id) {
            return None;
        }
        let path = self.raw_path(mailbox, validity, id);
        let path = if fs::symlink_metadata(&path).is_ok() {
            path
        } else {
            let legacy = legacy_jmap_message_key(mailbox, validity, id)?;
            self.dir.join(format!("{legacy}.eml"))
        };
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_RAW_CACHE_FILE_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        File::open(path)
            .ok()?
            .take(MAX_RAW_CACHE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        (!bytes.is_empty() && bytes.len() as u64 <= MAX_RAW_CACHE_FILE_BYTES).then_some(bytes)
    }

    /// Large messages remain available through IMAP but aren't stored twice.
    /// Atomic replacement prevents partial/corrupt cache entries on crashes.
    pub(crate) fn write(
        &self,
        mailbox: &str,
        validity: &CacheIdentity,
        id: &MessageId,
        raw: &[u8],
    ) -> std::io::Result<()> {
        if !validity.accepts(id) {
            return Err(std::io::Error::other("raw MIME cache identity mismatch"));
        }
        if raw.is_empty() || raw.len() as u64 > MAX_RAW_CACHE_FILE_BYTES {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?;
        secure_private_dir(&self.dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        tmp.write_all(raw)?;
        tmp.flush()?;
        tmp.persist(self.raw_path(mailbox, validity, id))
            .map_err(|error| error.error)?;
        self.prune()
    }

    pub(super) fn prune(&self) -> std::io::Result<()> {
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(&self.dir)? {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("eml")) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_file() {
                continue;
            }
            total = total.saturating_add(metadata.len());
            files.push((
                path,
                metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                metadata.len(),
            ));
        }
        if total > MAX_RAW_CACHE_BYTES {
            files.sort_by_key(|(_, modified, _)| *modified);
            for (path, _, size) in files {
                if total <= MAX_RAW_CACHE_BYTES {
                    break;
                }
                if fs::remove_file(path).is_ok() {
                    total = total.saturating_sub(size);
                }
            }
        }
        Ok(())
    }
}
