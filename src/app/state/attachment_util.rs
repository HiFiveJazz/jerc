//! Attachment previews, collision-safe saves and private temporary files.
use super::*;

/// Only the final component of an attachment filename may be written to disk.
/// A malicious MIME filename must never escape the chosen destination folder.
pub(super) fn received_attachment_is_pdf(file: &OutgoingAttachment) -> bool {
    file.content_type.eq_ignore_ascii_case("application/pdf")
        || file
            .filename
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

pub(super) fn received_attachment_is_image(file: &OutgoingAttachment) -> bool {
    file.content_type.starts_with("image/")
}

pub(super) fn decode_received_attachment_preview(
    file: &OutgoingAttachment,
) -> Result<image::DynamicImage, String> {
    // No unbounded PDF subprocess inputs or huge speculative image decodes.
    if file.data.len() > 24 * 1024 * 1024 {
        return Err("preview over 24 MiB; press o to open the original".into());
    }
    let image_bytes: std::borrow::Cow<'_, [u8]> = if received_attachment_is_pdf(file) {
        let temp = tempfile::tempdir().map_err(|err| err.to_string())?;
        let input = temp.path().join("received.pdf");
        std::fs::write(&input, &file.data).map_err(|err| err.to_string())?;
        std::borrow::Cow::Owned(compose_pdf_thumbnail(&input).map_err(|err| err.to_string())?)
    } else {
        std::borrow::Cow::Borrowed(&file.data)
    };
    // Check declared dimensions before allocating decoded pixels. Huge
    // compressed images must not expand into gigabytes of cache data.
    let dimensions = image::ImageReader::new(std::io::Cursor::new(image_bytes.as_ref()))
        .with_guessed_format()
        .map_err(|err| err.to_string())?
        .into_dimensions()
        .map_err(|err| err.to_string())?;
    if u64::from(dimensions.0) * u64::from(dimensions.1) > 24_000_000 {
        return Err("image has too many pixels for automatic preview; press o to open".into());
    }
    let decoded = image::load_from_memory(image_bytes.as_ref())
        .map_err(|err| format!("attachment image decode: {err}"))?;
    // Cache a terminal-sized image, not an enormous decoded camera original.
    Ok(if decoded.width() > 1_200 || decoded.height() > 1_200 {
        decoded.thumbnail(1_200, 1_200)
    } else {
        decoded
    })
}

pub(crate) fn attachment_safe_name(name: &str, index: usize) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or("");
    let safe = basename
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '/' && *ch != '\\')
        .collect::<String>();
    let safe = safe.trim().trim_matches('.');
    if safe.is_empty() {
        format!("attachment-{}.bin", index + 1)
    } else {
        safe.to_owned()
    }
}

/// Create-only save with collision suffixes; existing files and symlinks are
/// never truncated. Duplicate MIME filenames are common in message threads.
pub(super) fn save_attachment_without_overwrite(
    directory: &Path,
    file: &OutgoingAttachment,
    index: usize,
) -> anyhow::Result<PathBuf> {
    use std::fs::OpenOptions;
    let name = attachment_safe_name(&file.filename, index);
    let (stem, suffix) = name
        .rsplit_once('.')
        .filter(|(stem, suffix)| !stem.is_empty() && !suffix.is_empty())
        .map_or((name.as_str(), ""), |(stem, suffix)| (stem, suffix));
    for attempt in 0..10_000 {
        let candidate = if attempt == 0 {
            name.clone()
        } else if suffix.is_empty() {
            format!("{stem} ({attempt})")
        } else {
            format!("{stem} ({attempt}).{suffix}")
        };
        let path = directory.join(candidate);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut handle) => {
                use std::io::Write as _;
                if let Err(error) = handle.write_all(&file.data) {
                    drop(handle);
                    let _ = std::fs::remove_file(&path);
                    return Err(error.into());
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("too many files named {name:?} in {}", directory.display())
}

pub(super) fn attachment_list_start(selected: usize, count: usize, visible: usize) -> usize {
    selected
        .saturating_sub(visible / 2)
        .min(count.saturating_sub(visible))
}

pub(super) fn mouse_width_percent(column: u16, total: u16) -> u16 {
    if total == 0 {
        return 22;
    }
    (((u32::from(column) + 1) * 100 / u32::from(total)) as u16).clamp(12, 50)
}

pub(super) fn preview_percent_at(column: u16, sidebar_width: u16, remaining: u16) -> u16 {
    if remaining == 0 {
        return 57;
    }
    let inside = column.saturating_sub(sidebar_width).min(remaining);
    (100 - (u32::from(inside) * 100 / u32::from(remaining)) as u16).clamp(25, 75)
}

/// Normal exit cleans up TempPath values automatically. After a crash, only
/// old, private, regular files with our prefix are eligible for cleanup.
#[cfg(unix)]
pub(super) fn cleanup_stale_attachment_files() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::time::SystemTime;

    let Ok(identity) = tempfile::NamedTempFile::new() else {
        return;
    };
    let Ok(owner) = identity.as_file().metadata() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("jerc-attachment-") {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.file_type().is_file()
            && meta.uid() == owner.uid()
            && meta.permissions().mode() & 0o777 == 0o600
            && meta
                .modified()
                .ok()
                .and_then(|time| SystemTime::now().duration_since(time).ok())
                .is_some_and(|age| age >= Duration::from_secs(7 * 24 * 60 * 60))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(not(unix))]
pub(super) fn cleanup_stale_attachment_files() {}

/// Keep the temporary path alive so external viewers can open it after the
/// TUI immediately regains control. tempfile creates files with mode 0600.
pub(super) fn export_attachment_for_open(
    file: &OutgoingAttachment,
) -> anyhow::Result<tempfile::TempPath> {
    use std::io::Write as _;
    if file.data.len() > 32 * 1024 * 1024 {
        anyhow::bail!("attachment is larger than the 32 MiB open limit");
    }
    let extension = file
        .filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 12
                && extension.bytes().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or("bin");
    let mut file_out = tempfile::Builder::new()
        .prefix("jerc-attachment-")
        .suffix(&format!(".{extension}"))
        .tempfile()?;
    file_out.write_all(&file.data)?;
    file_out.flush()?;
    Ok(file_out.into_temp_path())
}
