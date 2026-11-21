//! Safe, structured errors shared across backend and worker boundaries.
use std::{fmt, io};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Offline,
    Timeout,
    Authentication,
    PermissionDenied,
    RateLimited,
    StaleState,
    InvalidResponse,
    PartialMutation,
    PreSmtpOAuth,
    PreJmapSubmission,
    Other,
}

impl ErrorKind {
    pub fn is_connection_failure(self) -> bool {
        matches!(
            self,
            Self::Offline | Self::Timeout | Self::PreSmtpOAuth | Self::PreJmapSubmission
        )
    }
}

/// Only sanitized diagnostics are stored in this type. Never attach HTTP
/// request URLs, headers, response bodies or untrusted server descriptions.
#[derive(Debug, Clone)]
pub struct BackendError {
    pub kind: ErrorKind,
    message: String,
}

impl BackendError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn http(operation: &str, status: u16) -> Self {
        let kind = match status {
            401 => ErrorKind::Authentication,
            403 => ErrorKind::PermissionDenied,
            429 => ErrorKind::RateLimited,
            _ => ErrorKind::Other,
        };
        Self::new(kind, format!("{operation} rejected (HTTP {status})"))
    }

    pub(crate) fn transport(operation: &str, error: &ureq::Error) -> Self {
        let (kind, reason) = match error {
            ureq::Error::Timeout(_) => (ErrorKind::Timeout, "timeout"),
            ureq::Error::HostNotFound => (ErrorKind::Offline, "DNS resolution failed"),
            ureq::Error::ConnectionFailed => (ErrorKind::Offline, "connection failed"),
            ureq::Error::Io(error) => match io_kind(error.kind()) {
                Some(ErrorKind::Timeout) => (ErrorKind::Timeout, "timeout"),
                Some(ErrorKind::Offline) => (ErrorKind::Offline, "connection lost"),
                _ => (ErrorKind::Other, "I/O error"),
            },
            ureq::Error::Tls(_) | ureq::Error::Rustls(_) => (ErrorKind::Other, "TLS error"),
            ureq::Error::BodyExceedsLimit(_) => (ErrorKind::InvalidResponse, "body exceeds limit"),
            _ => (ErrorKind::Other, "HTTP transport error"),
        };
        Self::new(kind, format!("{operation} failed ({reason})"))
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for BackendError {}

fn io_kind(kind: io::ErrorKind) -> Option<ErrorKind> {
    match kind {
        io::ErrorKind::TimedOut => Some(ErrorKind::Timeout),
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::UnexpectedEof
        | io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::HostUnreachable => Some(ErrorKind::Offline),
        _ => None,
    }
}

/// Classify older backends at the worker boundary while they are migrated.
/// Explicit typed errors always win over text (including error context).
pub(crate) fn classify(error: &anyhow::Error) -> ErrorKind {
    if let Some(error) = error.downcast_ref::<BackendError>() {
        return error.kind;
    }
    if let Some(error) = error.downcast_ref::<io_imap::client::ImapClientError>()
        && error.is_connection_lost()
    {
        return ErrorKind::Offline;
    }
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<io::Error>()
            && let Some(kind) = io_kind(error.kind())
        {
            return kind;
        }
    }
    legacy_kind(&format!("{error:#}"))
}

pub(crate) fn legacy_kind(message: &str) -> ErrorKind {
    if message.trim() == "OAuth token request failed (network/TLS error)" {
        return ErrorKind::PreSmtpOAuth;
    }
    let message = message.to_ascii_lowercase();
    if ["timed out", "timeout"]
        .iter()
        .any(|phrase| message.contains(phrase))
    {
        return ErrorKind::Timeout;
    }
    if [
        "oauth token request failed (network/tls error)",
        "not connected",
        "connection reset",
        "connection refused",
        "connection closed",
        "connection aborted",
        "connection lost",
        "broken pipe",
        "network is unreachable",
        "network unreachable",
        "no route to host",
        "failed to lookup address",
        "failed to resolve",
        "name resolution",
        "dns error",
        "failed to connect",
        "connecting to imap server",
        "unexpected eof",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
    {
        return ErrorKind::Offline;
    }
    ErrorKind::Other
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_classification_wins_over_context() {
        let error =
            anyhow::Error::new(BackendError::http("JMAP API", 403)).context("connection reset");
        assert_eq!(classify(&error), ErrorKind::PermissionDenied);
        assert!(!classify(&error).is_connection_failure());
    }
    #[test]
    fn transport_errors_are_typed_and_redacted() {
        let error = ureq::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "secret-token",
        ));
        let safe = BackendError::transport("JMAP API", &error);
        assert_eq!(safe.kind, ErrorKind::Offline);
        assert!(!safe.to_string().contains("secret-token"));
        assert_eq!(
            BackendError::http("JMAP API", 429).kind,
            ErrorKind::RateLimited
        );
    }
    #[test]
    fn oauth_classification_never_implies_safe_retry_from_substring() {
        assert_eq!(
            legacy_kind("OAuth token request failed (network/TLS error)"),
            ErrorKind::PreSmtpOAuth
        );
        assert_ne!(
            legacy_kind("SMTP DATA: OAuth token request failed (network/TLS error)"),
            ErrorKind::PreSmtpOAuth
        );
    }
}
