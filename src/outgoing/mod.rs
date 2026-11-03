mod smtp;
pub mod drafts;
mod wire;

use anyhow::Result;

use crate::config::AccountConfig;
use crate::model::OutgoingMessage;

pub use smtp::{SmtpAuth, SmtpSender, SmtpSource};
pub use wire::{InlineForwardParts, ReplyAudience, ReplyRecipients, build_outgoing, forward_html, forward_attribution, FORWARD_SEPARATOR, inline_forward_parts, forward_subject, reply_recipients, reply_subject, send_warnings};

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

pub fn from_account(account: &AccountConfig) -> Result<Option<Box<dyn MailSender>>> {
    let Some(source) = account.outgoing.as_deref() else {
        return Ok(None);
    };
    let source = SmtpSource::parse(source)?;
    Ok(Some(Box::new(SmtpSender::new(
        source,
        account.outgoing_cred_cmd.clone(),
    ))))
}
