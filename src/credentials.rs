//! Read generated password files without shell execution or credentials in TOML.
use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::path::Path;

pub(crate) fn private_file_credential(command: &str) -> Result<Option<String>> {
    let Some(filename) = command.strip_prefix("@file:") else {
        return Ok(None);
    };
    let path = Path::new(filename);
    if !path.is_absolute() {
        bail!("credential file path must be absolute");
    }
    let meta = fs::symlink_metadata(path).context("opening private credential file")?;
    if !meta.file_type().is_file() {
        bail!("credential path must be a regular file, not a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            bail!("credential file must be owner-only (chmod 600)");
        }
    }
    if meta.len() > 16 * 1024 {
        bail!("credential file exceeds maximum supported size");
    }
    let data = fs::read_to_string(path).context("reading private credential file")?;
    let secret = data.trim_end_matches(['\n', '\r']).to_owned();
    if secret.is_empty() {
        return Err(anyhow!("credential file is empty"));
    }
    Ok(Some(secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn does_not_intercept_existing_commands() {
        assert!(
            private_file_credential("pass show mail/example")
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn rejects_relative_paths() {
        assert!(private_file_credential("@file:relative").is_err());
    }
    #[test]
    fn reads_private_file_without_truncating_password() {
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::write(f.path(), "abc  !\n").unwrap();
        assert_eq!(
            private_file_credential(&format!("@file:{}", f.path().display()))
                .unwrap()
                .as_deref(),
            Some("abc  !")
        );
    }
    #[test]
    fn rejects_missing_private_file() {
        let d = tempfile::tempdir().unwrap();
        assert!(
            private_file_credential(&format!(
                "@file:{}",
                d.path().join("no-such-file").display()
            ))
            .is_err()
        );
    }
    #[test]
    fn rejects_empty_secret_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        assert!(private_file_credential(&format!("@file:{}", f.path().display())).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_and_world_readable_secrets() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("secret");
        fs::write(&target, "secret").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(private_file_credential(&format!("@file:{}", target.display())).is_err());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = d.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(private_file_credential(&format!("@file:{}", link.display())).is_err());
    }
}
