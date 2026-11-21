//! Clipboard and Vim cut/register policy.
use super::*;

/// Explicit Vim cuts (`x`) share one clipboard/register policy across
/// Compose and Contacts. Plain yanks stay in Jerc's in-memory register.
pub(super) fn record_cut(
    register: &mut Option<YankRegister>,
    text: String,
    linewise: bool,
) -> Option<StatusLine> {
    record_cut_with(register, text, linewise, copy_text_to_clipboard)
}

/// Keep the clipboard attempt injectable so success, failure, and an empty cut
/// can be tested without depending on wl-copy, pbcopy, or a running desktop.
pub(super) fn record_cut_with(
    register: &mut Option<YankRegister>,
    text: String,
    linewise: bool,
    copy: impl FnOnce(&str) -> std::io::Result<()>,
) -> Option<StatusLine> {
    if text.is_empty() {
        return None;
    }
    *register = Some(YankRegister {
        text: text.clone(),
        linewise,
    });
    Some(match copy(&text) {
        Ok(()) => StatusLine::temporary("cut copied to clipboard", Duration::from_secs(2)),
        Err(_) => StatusLine::temporary(
            "cut to internal register (system clipboard unavailable)",
            Duration::from_secs(3),
        ),
    })
}

pub(super) fn run_clipboard_command(
    program: &str,
    args: &[&str],
    text: &str,
) -> std::io::Result<()> {
    let mut child = ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{program} exited with status {status}"
        )))
    }
}

#[cfg(target_os = "windows")]
pub(super) fn copy_text_to_clipboard(text: &str) -> std::io::Result<()> {
    run_clipboard_command("clip.exe", &[], text)
}

#[cfg(target_os = "macos")]
pub(super) fn copy_text_to_clipboard(text: &str) -> std::io::Result<()> {
    run_clipboard_command("pbcopy", &[], text)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn copy_text_to_clipboard(text: &str) -> std::io::Result<()> {
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    let mut last_error = None;
    for (program, args) in candidates {
        match run_clipboard_command(program, args, text) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no clipboard helper found")
    }))
}

#[cfg(not(any(unix, target_os = "windows")))]
pub(super) fn copy_text_to_clipboard(_text: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "clipboard copy is unsupported on this platform",
    ))
}
