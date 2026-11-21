//! Compose MIME assembly and file-backed attachments.
use super::*;

/// Shared MIME-input assembly for SMTP and IMAP Drafts. The draft builder
/// allows unfinished addresses; the Send path validates them separately.
pub(super) fn outgoing_from_composer(
    composer: &Composer,
    from: &str,
) -> anyhow::Result<OutgoingMessage> {
    let mut attachments =
        load_compose_attachments(&composer.attachments, &composer.attachment_paths)?;
    let custom_headers = parse_compose_headers(&composer.headers)?;
    let mut html_body = None;
    if let Some(raw) = composer.draft_raw.as_deref() {
        attachments.extend(crate::mail::extract_attachments(raw)?);
    }
    if let Some(raw) = composer.forwarded_raw.as_deref() {
        let parts = inline_forward_parts(raw)?;
        attachments.extend(parts.attachments);
        if let Some(html) = parts.html {
            let personal_note = composer.body.split(FORWARD_SEPARATOR).next().unwrap_or("");
            let original_headers = composer
                .body
                .split(FORWARD_SEPARATOR)
                .nth(1)
                .and_then(|tail| tail.split("\n\n").next())
                .unwrap_or("");
            html_body = Some(forward_html(
                &html,
                &format!("{FORWARD_SEPARATOR}{original_headers}"),
                personal_note,
            ));
        }
    }
    Ok(OutgoingMessage {
        from: from.to_owned(),
        to: composer.to.clone(),
        cc: composer.cc.clone(),
        bcc: composer.bcc.clone(),
        subject: composer.subject.clone(),
        body: composer.outgoing_plain_body(),
        html_body,
        attachments,
        custom_headers,
        in_reply_to: composer.in_reply_to.clone(),
        references: composer.references.clone(),
        forwarded_raw: None,
    })
}

pub(super) fn parse_compose_headers(raw: &str) -> anyhow::Result<Vec<(String, String)>> {
    raw.split(';')
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let (name, value) = part.split_once(':').ok_or_else(|| {
                anyhow::anyhow!("use Header-Name: value; separated by semicolons")
            })?;
            Ok((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

pub(super) fn load_compose_attachments(
    legacy: &str,
    selected: &[PathBuf],
) -> anyhow::Result<Vec<OutgoingAttachment>> {
    let mut output = Vec::new();
    // Older postponed drafts used a semicolon-delimited string. New selections
    // are distinct paths, so filenames containing semicolons work correctly.
    for part in legacy
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        output.push(load_compose_file(Path::new(part))?);
    }
    for path in selected {
        output.push(load_compose_file(path)?);
    }
    Ok(output)
}

pub(super) fn load_compose_file(path: &Path) -> anyhow::Result<OutgoingAttachment> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
        anyhow::bail!(
            "attachment must be a file of at most 16 MiB: {}",
            path.display()
        );
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("attachment has no valid UTF-8 filename"))?
        .to_owned();
    let content_type = if ["LICENSE", "COPYING", "Makefile", "Dockerfile", ".gitignore"]
        .iter()
        .any(|name| filename.eq_ignore_ascii_case(name))
    {
        "text/plain"
    } else {
        match path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "txt" | "patch" | "diff" | "log" | "md" | "rst" | "lock" | "rs" | "c" | "h" | "cpp"
            | "hpp" | "py" | "sh" | "bash" | "zsh" | "fish" | "ini" | "conf" | "cfg"
            | "gitignore" => "text/plain",
            "toml" => "application/toml",
            "json" => "application/json",
            "yaml" | "yml" => "application/yaml",
            "csv" => "text/csv",
            "xml" => "application/xml",
            "css" => "text/css",
            "js" | "mjs" => "text/javascript",
            "html" | "htm" => "text/html",
            "pdf" => "application/pdf",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "eml" => "message/rfc822",
            "zip" => "application/zip",
            _ => "application/octet-stream",
        }
    };
    Ok(OutgoingAttachment {
        filename,
        content_type: content_type.to_owned(),
        data: std::fs::read(path)?,
    })
}
