//! Non-sending, non-persisting account configuration tests.
//! Run only from the dedicated background worker, never on the TUI thread.

use anyhow::{Result, anyhow};

use crate::backend::{ImapBackend, ImapSource, MailBackend};
use crate::config::AccountConfig;
use crate::outgoing::{SmtpSender, SmtpSource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub imap: std::result::Result<(), String>,
    pub smtp: std::result::Result<(), String>,
}

impl ProbeReport {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.imap.is_ok() && self.smtp.is_ok()
    }

    #[must_use]
    pub fn summary(&self) -> String {
        match (&self.imap, &self.smtp) {
            (Ok(()), Ok(())) => "IMAP ✓ · SMTP ✓ — Authentication verified; no mail sent.".into(),
            (Err(imap), Ok(())) => format!("IMAP ✗ {imap} · SMTP ✓"),
            (Ok(()), Err(smtp)) => format!("IMAP ✓ · SMTP ✗ {smtp}"),
            (Err(imap), Err(smtp)) => format!("IMAP ✗ {imap} · SMTP ✗ {smtp}"),
        }
    }
}

/// Test the *previewed* settings, including the same normalized host, port,
/// STARTTLS and OAuth choices used by the real worker. Do not save an account,
/// create a credential file, access mailboxes, or send an SMTP message.
#[must_use]
pub fn test_account(mut account: AccountConfig, password: Option<String>) -> ProbeReport {
    if account.normalize_shorthand().is_err() {
        let explanation = "Invalid IMAP/SMTP server configuration".to_owned();
        return ProbeReport {
            imap: Err(explanation.clone()),
            smtp: Err(explanation),
        };
    }
    let credential = password.as_deref();
    let imap = test_imap(&account, credential).map_err(|error| safe_failure(&error, "IMAP"));
    // Always test both services, even when IMAP authentication failed.
    let smtp = test_smtp(&account, credential).map_err(|error| safe_failure(&error, "SMTP"));
    ProbeReport { imap, smtp }
}

fn test_imap(account: &AccountConfig, credential: Option<&str>) -> Result<()> {
    let mut source = ImapSource::parse(&account.source)?.with_account_oauth(account)?;
    if account.oauth.is_none() {
        let secret = credential.ok_or_else(|| anyhow!("missing IMAP password"))?;
        source = source.with_probe_password(secret);
    }
    let mut backend = ImapBackend::new(source, None, 1);
    backend.connect()?;
    // Dropping the backend closes the temporary session; no SELECT or FETCH.
    Ok(())
}

fn test_smtp(account: &AccountConfig, credential: Option<&str>) -> Result<()> {
    let outgoing = account
        .outgoing
        .as_deref()
        .ok_or_else(|| anyhow!("missing SMTP outgoing server"))?;
    let mut source = SmtpSource::parse(outgoing)?.with_account_oauth(account)?;
    if account.oauth.is_none() && source.auth != crate::outgoing::SmtpAuth::None {
        let secret = credential.ok_or_else(|| anyhow!("missing SMTP password"))?;
        source = source.with_probe_password(secret);
    }
    SmtpSender::new(source, None).probe_connection()
}

/// Remote servers may echo submitted identities or secrets in protocol errors.
/// Deliberately show only vetted, actionable categories in the setup UI.
/// Full backend diagnostics are not copied into a user-visible error string.
fn safe_failure(error: &anyhow::Error, protocol: &str) -> String {
    let reason = format!("{error:#}").to_ascii_lowercase();
    if reason.contains("authentication failed")
        || reason.contains("login failed")
        || reason.contains("authentication rejected")
    {
        "Authentication failed (check username, method and credentials)".into()
    } else if reason.contains("certificate") || reason.contains("tls") || reason.contains("ssl") {
        "TLS negotiation failed (check security, port and certificate)".into()
    } else if reason.contains("auth")
        || reason.contains("login")
        || reason.contains("credential")
        || reason.contains("password")
        || reason.contains("token")
    {
        "Authentication failed (check username, method and credentials)".into()
    } else if reason.contains("connect")
        || reason.contains("timeout")
        || reason.contains("dns")
        || reason.contains("resolve")
    {
        "Connection failed (check server, port and network)".into()
    } else if reason.contains("missing") || reason.contains("invalid") {
        format!("Invalid {protocol} configuration")
    } else {
        format!("{protocol} verification failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_reports_both_services_without_leaking_arbitrary_errors() {
        let report = ProbeReport {
            imap: Ok(()),
            smtp: Err("Authentication failed (check username, method and credentials)".into()),
        };
        assert!(!report.passed());
        assert!(report.summary().contains("IMAP ✓"));
        assert!(report.summary().contains("SMTP ✗"));
    }

    #[test]
    fn success_never_claims_to_send_a_message() {
        let report = ProbeReport {
            imap: Ok(()),
            smtp: Ok(()),
        };
        assert!(report.passed());
        assert!(report.summary().contains("no mail sent"));
    }

    #[test]
    fn credential_echo_in_server_error_is_not_displayed() {
        let raw = anyhow!("authentication rejected: account password super-secret-123");
        let message = safe_failure(&raw, "SMTP");
        assert!(message.contains("Authentication"));
        assert!(!message.contains("super-secret-123"));
    }

    #[test]
    fn tls_error_is_classified_without_server_string() {
        let raw = anyhow!("tls alert: remote host echoed secret123");
        let message = safe_failure(&raw, "IMAP");
        assert!(message.contains("TLS"));
        assert!(!message.contains("secret123"));
    }

    #[test]
    fn test_does_not_create_any_account_or_credential_files_on_bad_settings() {
        let mut account: AccountConfig = toml::from_str(concat!(
            "name = \"Bad\"\n",
            "email = \"hi@fivejazz.com\"\n",
            "source = \"imap\"\n",
            "outgoing = \"smtps\"\n",
        ))
        .unwrap();
        account.imap_host = None;
        let outcome = test_account(account, Some("secret-test".into()));
        assert!(outcome.imap.is_err());
        assert!(outcome.smtp.is_err());
    }
}
