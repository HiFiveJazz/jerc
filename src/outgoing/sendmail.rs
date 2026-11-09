//! Direct sendmail-compatible local submission, with no shell or SMTP credentials.
//! Recipients come from the RFC 5321 envelope, not just visible To/Cc fields,
//! so Bcc reaches the MTA without being disclosed as a message header.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::model::OutgoingMessage;
use crate::outgoing::{MailSender, SendReceipt};

#[derive(Debug)]
pub struct SendmailSender {
    executable: PathBuf,
}

impl SendmailSender {
    pub fn from_outgoing(outgoing: &str) -> Result<Option<Self>> {
        let path = if outgoing.starts_with("sendmail://") {
            let url = url::Url::parse(outgoing).context("parsing sendmail outgoing URL")?;
            if url.host_str().is_some() || url.query().is_some() || url.fragment().is_some() {
                bail!("sendmail:// must contain an absolute executable path only");
            }
            percent_encoding::percent_decode_str(url.path())
                .decode_utf8()
                .context("sendmail path is not valid UTF-8")?
                .into_owned()
        } else if outgoing.starts_with('/') && !outgoing.contains("://") {
            // sendmail compatibility: outgoing = "/usr/sbin/sendmail"
            outgoing.to_owned()
        } else {
            return Ok(None);
        };
        let executable = Path::new(&path);
        if !executable.is_absolute() || !executable.is_file() {
            bail!("sendmail outgoing requires an existing absolute executable path");
        }
        Ok(Some(Self {
            executable: executable.to_path_buf(),
        }))
    }
}

impl MailSender for SendmailSender {
    fn send(&mut self, message: &OutgoingMessage) -> Result<SendReceipt> {
        let email = crate::outgoing::build_outgoing(message, &message.from)?;
        let raw_message = email.formatted();
        let envelope = email.envelope();
        let mut command = Command::new(&self.executable);
        command.arg("-i"); // Do not treat a dot-only line as end of message.
        if let Some(from) = envelope.from() {
            command.arg("-f").arg(from);
        }
        // End options before user-provided recipients (required for Postfix's
        // sendmail compatibility interface). Never interpolate into a shell.
        command.arg("--");
        if envelope.to().is_empty() {
            bail!("no envelope recipients for sendmail");
        }
        for recipient in envelope.to() {
            command.arg(recipient);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting sendmail executable")?;
        let write_error = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("sendmail stdin unavailable"))?
            .write_all(&raw_message)
            .err();
        let output = child
            .wait_with_output()
            .context("waiting for sendmail submission")?;
        if let Some(error) = write_error {
            return Err(error).context("writing email to sendmail");
        }
        if !output.status.success() {
            // Avoid echoing arbitrary stderr which could contain mail headers or
            // secrets injected by the local MTA.
            bail!(
                "sendmail rejected submission (exit status {})",
                output.status
            );
        }
        Ok(SendReceipt {
            raw_message,
            smtp_status: "queued locally".to_owned(),
            server_files_sent_copy: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_absolute_sendmail_and_url_forms() {
        assert!(
            SendmailSender::from_outgoing("smtps://x@smtp.example.com")
                .unwrap()
                .is_none()
        );
        assert!(SendmailSender::from_outgoing("sendmail://wrong-host/usr/sbin/sendmail").is_err());
        assert!(SendmailSender::from_outgoing("sendmail:///tmp/nonexistent-jerc-binary").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn sendmail_uses_envelope_recipients_including_bcc_without_header_leak() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("fake-sendmail");
        let args_path = temp.path().join("args");
        let data_path = temp.path().join("message");
        let script = format!(
            "#!/bin/sh\nprintf '%s\n' \"$@\" > '{}'\ncat > '{}'\n",
            args_path.display(),
            data_path.display()
        );
        std::fs::write(&executable, script).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut sender = SendmailSender::from_outgoing(executable.to_str().unwrap())
            .unwrap()
            .unwrap();
        let message = OutgoingMessage {
            from: "Sender <sender@example.org>".into(),
            to: "Visible <to@example.org>".into(),
            cc: String::new(),
            bcc: "hidden@example.org".into(),
            subject: "Test".into(),
            body: "Hello".into(),
            html_body: None,
            attachments: Vec::new(),
            custom_headers: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            forwarded_raw: None,
        };
        let receipt = sender.send(&message).unwrap();
        let args = std::fs::read_to_string(args_path).unwrap();
        assert!(args.contains("-i"));
        assert!(args.contains("--"));
        assert!(args.contains("hidden@example.org"));
        assert!(args.contains("to@example.org"));
        let submitted = std::fs::read(data_path).unwrap();
        assert_eq!(submitted, receipt.raw_message);
        let body = String::from_utf8_lossy(&submitted);
        assert!(!body.contains("hidden@example.org"));
    }
}
