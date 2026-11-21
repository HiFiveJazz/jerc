//! Shared syntax for native search. Protocol adapters decide which filters
//! can be evaluated exactly; unsupported filters must never be ignored.
use anyhow::{Context, Result, ensure};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Filter {
    Text(String),
    Field(&'static str, String),
    Attachment,
    Read(bool),
    Starred,
    Date(&'static str, chrono::NaiveDate),
    GmailRaw(String),
}

pub(crate) fn parse(text: &str) -> Result<Vec<Filter>> {
    ensure!(
        text.len() <= 4096 && !text.chars().any(char::is_control),
        "invalid or excessive server search text"
    );
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for ch in text.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    ensure!(!quoted, "unclosed quote in server search");
    if !word.is_empty() {
        words.push(word);
    }
    let mut filters = Vec::new();
    let mut free = Vec::new();
    for word in words {
        let Some((key, value)) = word.split_once(':') else {
            free.push(word);
            continue;
        };
        let field = match key {
            "from" => Some("from"),
            "to" => Some("to"),
            "cc" => Some("cc"),
            "bcc" => Some("bcc"),
            "subject" => Some("subject"),
            "body" => Some("body"),
            "text" => Some("text"),
            _ => None,
        };
        let filter = if let Some(field) = field {
            ensure!(!value.is_empty(), "empty server search filter");
            Filter::Field(field, value.to_owned())
        } else {
            match (key, value) {
                ("has", "attachment") => Filter::Attachment,
                ("is", "unread") => Filter::Read(false),
                ("is", "read") => Filter::Read(true),
                ("is", "starred") => Filter::Starred,
                ("before" | "after", value) => Filter::Date(
                    if key == "before" { "before" } else { "after" },
                    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                        .context("search date must be YYYY-MM-DD")?,
                ),
                ("gmail", value) => {
                    ensure!(!value.is_empty(), "empty Gmail search");
                    Filter::GmailRaw(value.to_owned())
                }
                ("has" | "is", _) => anyhow::bail!("unsupported server search filter"),
                _ => {
                    free.push(word);
                    continue;
                }
            }
        };
        filters.push(filter);
    }
    if !free.is_empty() {
        filters.push(Filter::Text(free.join(" ")));
    }
    ensure!(!filters.is_empty(), "empty server search");
    Ok(filters)
}

/// Raw IMAP quoted strings are restricted to ASCII. Escape both special
/// characters; reject controls/non-ASCII rather than inject a command or
/// incorrectly encode a search literal. Local search still supports Unicode.
fn quoted(value: &str) -> Result<String> {
    ensure!(
        value.is_ascii() && !value.chars().any(char::is_control),
        "native IMAP search currently requires ASCII; use local search for Unicode"
    );
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub(super) fn imap_criteria(text: &str, gmail: bool) -> Result<String> {
    let mut criteria = Vec::new();
    for filter in parse(text)? {
        criteria.push(match filter {
            Filter::Text(value) => format!("TEXT {}", quoted(&value)?),
            Filter::Field(key, value) => {
                format!("{} {}", key.to_ascii_uppercase(), quoted(&value)?)
            }
            Filter::Read(true) => "SEEN".into(),
            Filter::Read(false) => "UNSEEN".into(),
            Filter::Starred => "FLAGGED".into(),
            Filter::Date(key, date) => format!(
                "{} {}",
                if key == "before" { "BEFORE" } else { "SINCE" },
                date.format("%d-%b-%Y")
            ),
            Filter::Attachment if gmail => "X-GM-RAW \"has:attachment\"".into(),
            Filter::Attachment => anyhow::bail!(
                "this IMAP server has no native attachment search; use local filename search"
            ),
            Filter::GmailRaw(value) if gmail => format!("X-GM-RAW {}", quoted(&value)?),
            Filter::GmailRaw(_) => anyhow::bail!("gmail: requires X-GM-EXT-1"),
        });
    }
    Ok(criteria.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_imap_search_escapes_and_combines_filters() {
        assert_eq!(
            imap_criteria(
                r#"from:alice subject:"project update" is:unread after:2026-01-01 a\b"#,
                false
            )
            .unwrap(),
            "FROM \"alice\" SUBJECT \"project update\" UNSEEN SINCE 01-Jan-2026 TEXT \"a\\\\b\""
        );
        assert!(imap_criteria("subject:hi\r\nLOGOUT", false).is_err());
        assert!(imap_criteria("has:attachment", false).is_err());
        assert!(imap_criteria("gmail:foo", false).is_err());
        assert_eq!(
            imap_criteria("gmail:\"in:anywhere has:attachment\"", true).unwrap(),
            "X-GM-RAW \"in:anywhere has:attachment\""
        );
    }
}
