//! Per-account local postponed drafts, intentionally separate from IMAP Drafts.
//! Write a temporary file then atomically rename it to avoid truncating an
//! existing draft on a crash. Restrict file permissions on Unix systems.
use std::fs;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use crate::app::Composer;

fn path_for(root: &Path, account: &str) -> PathBuf {
    // Hex encoding is an unambiguous, filesystem-safe account identifier.
    let name = account.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
    root.join(format!("{name}.json"))
}

pub fn save_draft(root: &Path, account: &str, draft: &Composer) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let path = path_for(root, account);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(draft).context("serializing postponed draft")?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).context("opening temporary postponed draft")?;
    use std::io::Write;
    file.write_all(&bytes).context("writing postponed draft")?;
    file.sync_all().context("syncing postponed draft")?;
    drop(file);
    fs::rename(&tmp, &path).context("committing postponed draft")?;
    Ok(())
}

pub fn load_draft(root: &Path, account: &str) -> Result<Option<Composer>> {
    let path = path_for(root, account);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("reading postponed draft"),
    };
    let mut draft: Composer = serde_json::from_slice(&bytes).context("decoding postponed draft")?;
    draft.sending = false;
    draft.confirm_send = false;
    draft.visual_anchor = None;
    Ok(Some(draft))
}

pub fn delete_draft(root: &Path, account: &str) -> Result<()> {
    match fs::remove_file(path_for(root, account)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("removing postponed draft"),
    }
}
