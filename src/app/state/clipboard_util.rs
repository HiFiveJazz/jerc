//! Clipboard and Vim cut/register policy.
use super::*;

/// Both yanks and explicit Vim cuts share a clipboard/register policy.
/// The private register keeps Vim p/P working even without an OS clipboard.
///
/// Record a yank both in the private Vim register and the OS clipboard.
/// Empty selections must not destroy an earlier successful yank.
pub(super) fn record_yank(
    register: &mut Option<YankRegister>,
    yank: YankRegister,
) -> Option<StatusLine> {
    record_yank_with(register, yank, copy_text_to_clipboard)
}

pub(super) fn record_yank_with(
    register: &mut Option<YankRegister>,
    yank: YankRegister,
    copy: impl FnOnce(&str) -> std::io::Result<()>,
) -> Option<StatusLine> {
    if yank.text.is_empty() {
        return None;
    }
    let copied = copy(&yank.text).is_ok();
    *register = Some(yank);
    Some(if copied {
        StatusLine::temporary("yanked to system clipboard", Duration::from_secs(2))
    } else {
        StatusLine::temporary(
            "yanked to internal register (system clipboard unavailable)",
            Duration::from_secs(3),
        )
    })
}

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

// Every jerc yank is text. Do not let wl-copy MIME-sniff message bodies:
// git-send-email cover letters often begin with "From:", which can be
// misidentified as message/rfc822 and rejected by other applications.
#[cfg(all(unix, not(target_os = "macos")))]
fn unix_clipboard_commands() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("wl-copy", &["--type", "text/plain"]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ]
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn copy_text_to_clipboard(text: &str) -> std::io::Result<()> {
    let candidates = unix_clipboard_commands();
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

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::unix_clipboard_commands;

    #[test]
    fn wayland_copy_declares_text_plain_instead_of_sniffing_email_body() {
        // A git-send-email cover letter starts with "From:"; MIME sniffing
        // may otherwise advertise message/rfc822 rather than plain text.
        let (program, args) = unix_clipboard_commands()[0];
        assert_eq!(program, "wl-copy");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--type");
        assert_eq!(args[1], "text/plain");
    }
}
