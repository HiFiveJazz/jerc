mod smtp;

use anyhow::Result;

use crate::config::AccountConfig;
use crate::model::OutgoingMessage;

pub use smtp::{SmtpAuth, SmtpSender, SmtpSource};

pub trait MailSender: Send {
    fn send(&mut self, message: &OutgoingMessage) -> Result<()>;
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
