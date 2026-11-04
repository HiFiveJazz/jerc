mod smtp;
mod sendmail;
pub mod drafts;
mod wire;

use anyhow::Result;

use crate::config::AccountConfig;
use crate::model::OutgoingMessage;

pub use smtp::{SmtpAuth, SmtpSender, SmtpSource};
pub use sendmail::SendmailSender;
pub use wire::{build_draft, InlineForwardParts, ReplyAudience, ReplyRecipients, build_outgoing, forward_html, forward_attribution, FORWARD_SEPARATOR, inline_forward_parts, forward_subject, reply_recipients, reply_subject, send_warnings, validate_recipients};

#[derive(Debug, Clone)]
pub struct SendReceipt {
    /// Exact RFC 5322 bytes accepted by the SMTP relay. Reuse these bytes for
    /// the Sent copy so the server-side copy has the same Message-ID and body.
    pub raw_message: Vec<u8>,
    /// Short status token returned by the relay (for example `2.0.0`).
    pub smtp_status: String,
    /// Gmail stores SMTP-submitted messages in Sent itself; most generic SMTP
    /// relays do not. This lets the worker avoid creating a duplicate there.
    pub server_files_sent_copy: bool,
}

pub trait MailSender: Send {
    fn send(&mut self, message: &OutgoingMessage) -> Result<SendReceipt>;
}

/// Resolve the visible From address consistently with SMTP's fallback.
/// If `from` is omitted, SMTP uses the decoded outgoing URI username; the
/// Drafts builder must use the same identity instead of an empty string.
/// IMAP username is a last resort for transports with non-address logins.
#[must_use]
pub fn sender_from_account(account: &AccountConfig) -> String {
    if let Some(from) = account.from.as_deref().filter(|value| !value.trim().is_empty()) {
        // Respect explicit configuration, including an invalid value: it must
        // produce an actionable error, not silently send as another identity.
        return from.to_owned();
    }
    if let Some(outgoing) = account.outgoing.as_deref() {
        if let Ok(smtp) = SmtpSource::parse(outgoing) {
            if smtp.username.parse::<lettre::message::Mailbox>().is_ok() {
                return smtp.username;
            }
        }
    }
    if let Ok(imap) = crate::backend::ImapSource::parse(&account.source) {
        if imap.username.parse::<lettre::message::Mailbox>().is_ok() {
            return imap.username;
        }
    }
    String::new()
}

pub fn from_account(account: &AccountConfig) -> Result<Option<Box<dyn MailSender>>> {
    let Some(source) = account.outgoing.as_deref() else {
        return Ok(None);
    };
    if let Some(sender) = SendmailSender::from_outgoing(source)? {
        return Ok(Some(Box::new(sender)));
    }
    let source = SmtpSource::parse(source)?.with_account_oauth(account)?;
    Ok(Some(Box::new(SmtpSender::new(
        source,
        account.outgoing_cred_cmd.clone(),
    ))))
}

/// Explicit per-account copy-to policy. None uses legacy provider detection;
/// Some(empty) skips any APPEND; nonempty names are exact mailbox targets.
#[must_use]
pub fn sent_copy_targets(setting: Option<&str>) -> Option<Vec<String>> {
    setting.and_then(|value| {
        let value = value.trim();
        if value.eq_ignore_ascii_case("auto") { return None; }
        if value.is_empty() || value.eq_ignore_ascii_case("none") { return Some(Vec::new()); }
        let mut folders = Vec::<String>::new();
        for target in value.split(',').map(str::trim).filter(|name| !name.is_empty()) {
            if !folders.iter().any(|name| name == target) {
                folders.push(target.to_owned());
            }
        }
        Some(folders)
    })
}

#[cfg(test)]
mod sender_tests {
    use super::{sender_from_account, sent_copy_targets};
    use crate::config::AccountConfig;

    fn account(from: Option<&str>, outgoing: &str, source: &str) -> AccountConfig {
        AccountConfig {
            name: "Test".into(), source: source.into(), default_mailbox: "INBOX".into(),
            source_cred_cmd: None, from: from.map(str::to_owned),
            outgoing: Some(outgoing.into()), outgoing_cred_cmd: None,
            copy_to: None, oauth: None,
            max_messages: 100, page_size: 100,
        }
    }

    #[test]
    fn explicit_sent_policy_supports_disabled_multiple_and_auto() {
        assert!(sent_copy_targets(None).is_none());
        assert!(sent_copy_targets(Some("auto")).is_none());
        assert_eq!(sent_copy_targets(Some("")), Some(Vec::new()));
        assert_eq!(sent_copy_targets(Some("none")), Some(Vec::new()));
        assert_eq!(sent_copy_targets(Some(" Sent, Archive, Sent ")),
            Some(vec!["Sent".into(), "Archive".into()]));
    }

    #[test]
    fn missing_from_uses_decoded_smtp_login_for_drafts_and_send() {
        let config = account(None,
            "smtps://jazz%40gmail.com@smtp.gmail.com:465",
            "imaps://different%40gmail.com@imap.gmail.com:993");
        let from = sender_from_account(&config);
        assert_eq!(from, "jazz@gmail.com");
        let draft = crate::model::OutgoingMessage {
            from, to: String::new(), cc: String::new(), bcc: String::new(),
            subject: "Test unsent".into(), body: "Body".into(),
            html_body: None, attachments: Vec::new(), custom_headers: Vec::new(),
            in_reply_to: None, references: Vec::new(), forwarded_raw: None,
        };
        let wire = super::build_draft(&draft, "").unwrap();
        let headers = String::from_utf8_lossy(&wire);
        assert!(headers.contains("From: jazz@gmail.com"));
        assert!(headers.contains("X-Unsent: 1"));
    }

    #[test]
    fn explicit_from_preserves_display_name() {
        let config = account(Some("Jazz Bhatia <jazz@gmail.com>"),
            "smtps://jazz%40gmail.com@smtp.gmail.com",
            "imaps://jazz%40gmail.com@imap.gmail.com");
        assert_eq!(sender_from_account(&config), "Jazz Bhatia <jazz@gmail.com>");
    }

    #[test]
    fn falls_back_to_imap_login_if_smtp_uses_alias_or_empty_identity() {
        let config = account(None,
            "smtps://opaque-token@smtp.example.org",
            "imaps://jazz%40example.org@imap.example.org");
        assert_eq!(sender_from_account(&config), "jazz@example.org");
    }
}
