//! Raw MIME, attachment download and message preview helpers.
use super::*;

/// A verified UIDVALIDITY-scoped raw MIME hit avoids IMAP entirely.
/// Cache misses still fetch the exact original bytes and persist them for
/// later forwards/attachment previews. All file I/O occurs on mail workers.
pub(super) fn fetch_raw_cached(
    backend: &mut dyn MailBackend,
    mailbox: &str,
    id: &MessageId,
    cache: Option<&(PathBuf, CacheIdentity)>,
) -> anyhow::Result<Vec<u8>> {
    if let Some((dir, validity)) = cache
        && let Some(raw) = (RawStore { dir: dir.clone() }).read(mailbox, validity, id)
    {
        crate::perf::record("compose_raw_cache_hit", None, Duration::ZERO, true);
        return Ok(raw);
    }
    crate::perf::record("compose_raw_cache_miss", None, Duration::ZERO, true);
    let raw = backend.fetch_raw(mailbox, id)?;
    if let Some((dir, validity)) = cache {
        let _ = (RawStore { dir: dir.clone() }).write(mailbox, validity, id, &raw);
    }
    Ok(raw)
}

pub(super) fn fetch_compose_raw(
    backend: &mut dyn MailBackend,
    connected: &mut bool,
    mailbox: &str,
    id: &MessageId,
    cache: Option<&(PathBuf, CacheIdentity)>,
) -> anyhow::Result<Vec<u8>> {
    if let Some((dir, validity)) = cache
        && let Some(raw) = (RawStore { dir: dir.clone() }).read(mailbox, validity, id)
    {
        crate::perf::record("compose_raw_cache_hit", None, Duration::ZERO, true);
        return Ok(raw);
    }
    if !*connected {
        backend.connect()?;
        *connected = true;
    }
    let raw = fetch_raw_cached(backend, mailbox, id, cache);
    if raw.is_err() {
        *connected = false;
    }
    raw
}

pub(super) fn attachment_event(
    backend: &mut dyn MailBackend,
    mailbox: &str,
    id: &MessageId,
    cache: Option<&(PathBuf, CacheIdentity)>,
) -> anyhow::Result<WorkerEventKind> {
    if let Some((dir, identity)) = cache
        && let Some(raw) = (RawStore { dir: dir.clone() }).read(mailbox, identity, id)
        && let Ok(attachments) = crate::mail::extract_attachments(&raw)
    {
        return Ok(WorkerEventKind::AttachmentsLoaded {
            mailbox: mailbox.to_owned(),
            id: id.clone(),
            attachments,
        });
    }
    if let Some(attachments) = backend.list_native_attachments(mailbox, id)? {
        return Ok(WorkerEventKind::RemoteAttachmentsListed {
            mailbox: mailbox.to_owned(),
            id: id.clone(),
            attachments,
        });
    }
    fetch_attachments(backend, mailbox, id, cache).map(|attachments| {
        WorkerEventKind::AttachmentsLoaded {
            mailbox: mailbox.to_owned(),
            id: id.clone(),
            attachments,
        }
    })
}

pub(super) fn download_attachment_job(
    backend: &mut dyn MailBackend,
    mailbox: &str,
    job: RemoteAttachmentJob,
) -> anyhow::Result<RemoteAttachmentResult> {
    match job {
        RemoteAttachmentJob::Preview { index, file } => {
            anyhow::ensure!(
                file.size <= 24 * 1024 * 1024,
                "attachment exceeds the preview limit; open or save it instead"
            );
            let mut data = Vec::new();
            backend.download_native_attachment_into(mailbox, &file, &mut data)?;
            Ok(RemoteAttachmentResult::Preview {
                index,
                file: OutgoingAttachment {
                    filename: file.filename,
                    content_type: file.content_type,
                    data,
                },
            })
        }
        RemoteAttachmentJob::Open(file) => {
            let suffix = file
                .filename
                .rsplit_once('.')
                .map(|(_, ext)| ext)
                .filter(|ext| {
                    !ext.is_empty()
                        && ext.len() <= 12
                        && ext.bytes().all(|c| c.is_ascii_alphanumeric())
                })
                .unwrap_or("bin");
            let mut temp = tempfile::Builder::new()
                .prefix("jerc-attachment-")
                .suffix(&format!(".{suffix}"))
                .tempfile()?;
            backend.download_native_attachment_into(mailbox, &file, &mut temp)?;
            temp.flush()?;
            Ok(RemoteAttachmentResult::Open(Arc::new(
                temp.into_temp_path(),
            )))
        }
        RemoteAttachmentJob::Save { directory, files } => {
            let mut count = 0;
            let mut first_error = None;
            for (index, file) in files {
                let result = (|| -> anyhow::Result<()> {
                    let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
                    backend.download_native_attachment_into(mailbox, &file, &mut temp)?;
                    temp.flush()?;
                    // Download into a private file and publish only complete bytes.
                    // persist_noclobber never replaces an existing file/symlink.
                    let name = crate::app::attachment_safe_name(&file.filename, index);
                    let (stem, ext) = name
                        .rsplit_once('.')
                        .filter(|(s, e)| !s.is_empty() && !e.is_empty())
                        .unwrap_or((&name, ""));
                    for attempt in 0..10_000 {
                        let candidate = if attempt == 0 {
                            name.clone()
                        } else if ext.is_empty() {
                            format!("{stem} ({attempt})")
                        } else {
                            format!("{stem} ({attempt}).{ext}")
                        };
                        match temp.persist_noclobber(directory.join(candidate)) {
                            Ok(_) => return Ok(()),
                            Err(error)
                                if error.error.kind() == std::io::ErrorKind::AlreadyExists =>
                            {
                                temp = error.file
                            }
                            Err(error) => return Err(error.error.into()),
                        }
                    }
                    anyhow::bail!("too many attachments with the same filename")
                })();
                match result {
                    Ok(()) => count += 1,
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(format!("{error:#}"));
                        }
                    }
                }
            }
            Ok(RemoteAttachmentResult::Saved {
                count,
                directory,
                error: first_error,
            })
        }
    }
}

/// One attachment fetch, shared by the foreground fallback and the
/// independent read-only worker. Reuse raw MIME across sessions; never block
/// the UI on reading, parsing, writing, or pruning the on-disk store.
pub(super) fn fetch_attachments(
    backend: &mut dyn MailBackend,
    mailbox: &str,
    id: &MessageId,
    cache: Option<&(PathBuf, CacheIdentity)>,
) -> anyhow::Result<Vec<OutgoingAttachment>> {
    if let Some((dir, validity)) = cache
        && let Some(raw) = (RawStore { dir: dir.clone() }).read(mailbox, validity, id)
        && let Ok(attachments) = crate::mail::extract_attachments(&raw)
    {
        crate::perf::record("attachment_raw_disk_hit", None, Duration::ZERO, true);
        return Ok(attachments);
    }
    crate::perf::record("attachment_raw_disk_miss", None, Duration::ZERO, true);
    let raw = backend.fetch_raw(mailbox, id)?;
    // Don't store corrupt raw MIME. A successful fetch and extraction can be
    // cached for future restarts, including when offline.
    let attachments = crate::mail::extract_attachments(&raw)?;
    if let Some((dir, validity)) = cache {
        let _ = (RawStore { dir: dir.clone() }).write(mailbox, validity, id, &raw);
    }
    Ok(attachments)
}

/// Best effort: a failed/empty body still yields the sender and subject.
/// fetch_body uses IMAP BODY.PEEK and must never change the Seen flag.
pub(super) fn new_mail_preview(
    backend: &mut dyn MailBackend,
    mailbox: &str,
    count: usize,
) -> WorkerEventKind {
    let mut sender = String::new();
    let mut subject = String::new();
    let mut excerpt = String::new();
    if let Ok(page) = backend.open_mailbox(mailbox, 0, 1)
        && let Some(message) = page.messages.first()
    {
        sender = if message.from_name.is_empty() {
            message.from_address.clone()
        } else {
            message.from_name.clone()
        };
        subject = message.subject.clone();
        if let Ok(body) = backend.fetch_body(mailbox, &message.id) {
            excerpt = body.display_text(false).chars().take(400).collect();
        }
    }
    WorkerEventKind::NewMailPreview {
        count,
        sender,
        subject,
        excerpt,
    }
}

pub(super) fn write_export_stream(
    path: &std::path::Path,
    stream: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Private from creation, atomic on success, and automatically removed
    // after a partial network failure.
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing export parent"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    stream(tmp.as_file_mut())?;
    if tmp.as_file().metadata()?.len() == 0 {
        anyhow::bail!("export returned an empty RFC 5322 message");
    }
    tmp.flush()?;
    tmp.persist(path)?;
    Ok(())
}
