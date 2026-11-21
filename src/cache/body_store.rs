//! Lazy disk-backed parsed-body cache, reads, writes and pruning.
use super::*;

impl BodyStore {
    pub(super) fn body_path(
        &self,
        mailbox: &str,
        identity: &CacheIdentity,
        id: &MessageId,
    ) -> PathBuf {
        self.dir
            .join(format!("{}.json", cache_message_key(mailbox, identity, id)))
    }

    pub(super) fn legacy_body_path(
        &self,
        mailbox: &str,
        identity: &CacheIdentity,
        id: &MessageId,
    ) -> Option<PathBuf> {
        Some(self.dir.join(format!(
            "{}.json",
            legacy_jmap_message_key(mailbox, identity, id)?
        )))
    }

    /// Cheap existence probe for prefetch filtering. The reader validates the
    /// complete record on actual use, falling back to IMAP if it is corrupt.
    pub(crate) fn has_candidate(
        &self,
        mailbox: &str,
        validity: &CacheIdentity,
        id: &MessageId,
    ) -> bool {
        if !validity.accepts(id) {
            return false;
        }
        let eligible = |path: &Path| {
            fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.file_type().is_file() && metadata.len() <= MAX_BODY_FILE_BYTES
            })
        };
        eligible(&self.body_path(mailbox, validity, id))
            || self
                .legacy_body_path(mailbox, validity, id)
                .as_deref()
                .is_some_and(eligible)
    }

    pub(crate) fn read(
        &self,
        mailbox: &str,
        validity: &CacheIdentity,
        id: &MessageId,
    ) -> Option<MessageBody> {
        if !validity.accepts(id) {
            return None;
        }
        let normalized = account_body_identity(validity);
        let expected_mailbox = account_body_mailbox(mailbox, validity);
        if let Some(body) = self.read_record(
            &self.body_path(mailbox, validity, id),
            expected_mailbox,
            &normalized,
            id,
        ) {
            return Some(body);
        }
        // Legacy JMAP records remain valid only for their original mailbox.
        self.legacy_body_path(mailbox, validity, id)
            .and_then(|path| self.read_record(&path, mailbox, &validity.generation(), id))
    }

    pub(super) fn read_record(
        &self,
        path: &Path,
        mailbox: &str,
        validity: &CacheIdentity,
        id: &MessageId,
    ) -> Option<MessageBody> {
        let metadata = fs::symlink_metadata(path).ok()?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_BODY_FILE_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        File::open(path)
            .ok()?
            .take(MAX_BODY_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_BODY_FILE_BYTES {
            return None;
        }
        let record: BodyFile = serde_json::from_slice(&bytes).ok()?;
        (record.version == VERSION
            && record.mailbox == mailbox
            && record.validity == *validity
            && record.body.id == *id)
            .then_some(record.body)
    }

    pub(super) fn write(
        &self,
        mailbox: &str,
        validity: &CacheIdentity,
        body: &MessageBody,
    ) -> std::io::Result<bool> {
        if !validity.accepts(&body.id) {
            return Err(std::io::Error::other("body cache identity mismatch"));
        }
        let path = self.body_path(mailbox, validity, &body.id);
        let record_identity = account_body_identity(validity);
        let record_mailbox = account_body_mailbox(mailbox, validity);
        // An immutable UID-scoped MIME body is often scheduled for persistence
        // more than once. Read/compare the existing entry before creating a
        // temporary file; enriched metadata will still trigger replacement.
        if self
            .read_record(&path, record_mailbox, &record_identity, &body.id)
            .as_ref()
            == Some(body)
        {
            return Ok(false);
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
        write_json_buffered(
            &mut tmp,
            &BodyFileRef {
                version: VERSION,
                mailbox: record_mailbox,
                validity: &record_identity,
                body,
            },
        )?;
        if tmp.as_file().metadata()?.len() > MAX_BODY_FILE_BYTES {
            return Err(std::io::Error::other("body exceeds offline cache limit"));
        }
        tmp.persist(path).map_err(|error| error.error)?;
        Ok(true)
    }

    pub(super) fn prune(&self) -> std::io::Result<()> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("json")) {
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
        if total <= MAX_BODY_STORE_BYTES {
            return Ok(());
        }
        files.sort_by_key(|(_, modified, _)| *modified);
        for (path, _, size) in files {
            if total <= MAX_BODY_STORE_BYTES {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
        Ok(())
    }
}
