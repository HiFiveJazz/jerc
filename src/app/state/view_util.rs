//! Reader navigation, reply labels and message mark helpers.
use super::*;

pub(super) fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = ProcessCommand::new("rundll32.exe");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = ProcessCommand::new("open");
        command.arg(url);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = ProcessCommand::new("xdg-open");
        command.arg(url);
        command
    };
    #[cfg(not(any(unix, target_os = "windows")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "opening URLs is unsupported on this platform",
    ));

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().map(|_| ())
}

pub(super) fn message_view_offset(
    current_offset: usize,
    index: usize,
    len: usize,
    height: usize,
) -> usize {
    if len == 0 {
        return 0;
    }
    let height = height.max(1).min(len);
    reveal_row(
        current_offset,
        index.min(len - 1),
        height,
        len.saturating_sub(height),
        MESSAGE_SCROLL_OFF,
    )
}

pub(super) fn reply_date_label(message: &MessageSummary) -> String {
    if message.timestamp <= 0 {
        return "the previous message".to_owned();
    }
    DateTime::<Utc>::from_timestamp(message.timestamp, 0)
        .map(|utc| utc.with_timezone(&Local))
        .map_or_else(
            || "the previous message".to_owned(),
            |local| local.format("%a %b %-d, %Y at %-I:%M %p %Z").to_string(),
        )
}

pub(super) fn reply_attribution(message: &MessageSummary) -> String {
    let author = if message.from_name.trim().is_empty() {
        message.from_address.trim()
    } else {
        message.from_name.trim()
    };
    format!("On {}, {} wrote:", reply_date_label(message), author)
}

/// A range is marked as a group: if fully marked, Space unmarks all of it.
pub(super) fn toggle_message_marks(marked: &mut HashSet<MessageId>, ids: &[MessageId]) {
    crate::app::vim::toggle_group(marked, ids);
}
