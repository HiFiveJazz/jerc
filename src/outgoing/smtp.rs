use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{SmtpConnection, TlsParameters};
use lettre::transport::smtp::commands::Ehlo;
use lettre::transport::smtp::extension::ClientId;
use lettre::transport::smtp::response::Response;
use lettre::{SmtpTransport, Transport};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use url::Url;

use crate::model::OutgoingMessage;
use crate::outgoing::{MailSender, SendReceipt};

const SMTP_TIMEOUT: Duration = Duration::from_secs(60);
const OAUTH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpAuth {
    None,
    Auto,
    Plain,
    Login,
    XOAuth2,
    OAuthBearer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmtpTls {
    Implicit,
    StartTls,
}

#[derive(Clone)]
pub struct SmtpSource {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    secret: Option<String>,
    pub auth: SmtpAuth,
    tls: SmtpTls,
    token_endpoint: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    scope: Option<String>,
    account_oauth: Option<crate::oauth::OAuthSettings>,
}

impl SmtpSource {
    pub fn parse(source: &str) -> Result<Self> {
        let url = Url::parse(source).context("parsing SMTP outgoing URI")?;
        let (tls, auth) = match url.scheme() {
            "smtps" | "smtps+auto" => (SmtpTls::Implicit, SmtpAuth::Auto),
            "smtps+plain" => (SmtpTls::Implicit, SmtpAuth::Plain),
            "smtps+login" => (SmtpTls::Implicit, SmtpAuth::Login),
            "smtps+xoauth2" => (SmtpTls::Implicit, SmtpAuth::XOAuth2),
            "smtps+oauthbearer" => (SmtpTls::Implicit, SmtpAuth::OAuthBearer),
            "smtps+none" => (SmtpTls::Implicit, SmtpAuth::None),
            "smtp" | "smtp+auto" => (SmtpTls::StartTls, SmtpAuth::Auto),
            "smtp+plain" => (SmtpTls::StartTls, SmtpAuth::Plain),
            "smtp+login" => (SmtpTls::StartTls, SmtpAuth::Login),
            "smtp+xoauth2" => (SmtpTls::StartTls, SmtpAuth::XOAuth2),
            "smtp+oauthbearer" => (SmtpTls::StartTls, SmtpAuth::OAuthBearer),
            "smtp+none" => (SmtpTls::StartTls, SmtpAuth::None),
            _ => bail!("unsupported SMTP outgoing scheme"),
        };

        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| anyhow!("SMTP outgoing URI has no hostname"))?
            .to_owned();
        let username = decode_component(url.username())?;
        if auth != SmtpAuth::None && username.is_empty() {
            bail!("SMTP outgoing URI has no username");
        }
        let secret = url.password().map(decode_component).transpose()?;

        let mut token_endpoint = None;
        let mut client_id = None;
        let mut client_secret = None;
        let mut scope = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "token_endpoint" => token_endpoint = Some(value.into_owned()),
                "client_id" => client_id = Some(value.into_owned()),
                "client_secret" => client_secret = Some(value.into_owned()),
                "scope" => scope = Some(value.into_owned()),
                _ => {}
            }
        }

        let auth = if auth == SmtpAuth::Auto && token_endpoint.is_some() {
            SmtpAuth::OAuthBearer // negotiate OAuth only; never downgrade to password
        } else { auth };
        if matches!(auth, SmtpAuth::XOAuth2 | SmtpAuth::OAuthBearer) {
            if let Some(endpoint) = &token_endpoint {
                let parsed = Url::parse(endpoint).context("parsing SMTP OAuth token endpoint")?;
                if parsed.scheme() != "https" {
                    bail!("SMTP OAuth token_endpoint must use https");
                }
            }
        }

        let default_port = match tls {
            SmtpTls::Implicit => 465,
            SmtpTls::StartTls => 587,
        };

        Ok(Self {
            scheme: url.scheme().to_owned(),
            host,
            port: url.port().unwrap_or(default_port),
            username,
            secret,
            auth,
            tls,
            token_endpoint,
            client_id,
            client_secret,
            scope,
            account_oauth: None,
        })
    }

    pub fn with_account_oauth(mut self, account: &crate::config::AccountConfig) -> Result<Self> {
        if let Some(settings) = crate::oauth::OAuthSettings::for_account(account)? {
            if matches!(self.auth, SmtpAuth::Plain | SmtpAuth::Login | SmtpAuth::None) {
                bail!("OAuth account cannot use SMTP password or no-auth scheme; use smtps:// or +xoauth2");
            }
            self.account_oauth = Some(settings);
            // An unsuffixed SMTP URI must not try PLAIN/LOGIN with an OAuth
            // token; force token-only capability negotiation for this account.
            if self.auth == SmtpAuth::Auto { self.auth = SmtpAuth::OAuthBearer; }
        }
        Ok(self)
    }

    #[must_use]
    pub fn redacted(&self) -> String {
        format!(
            "{}://{}@{}:{}",
            self.scheme, self.username, self.host, self.port
        )
    }

    fn credential(&self, command: Option<&str>) -> Result<String> {
        if self.auth == SmtpAuth::None {
            return Ok(String::new());
        }
        if let Some(secret) = &self.secret {
            return Ok(secret.clone());
        }
        if let Some(oauth) = &self.account_oauth {
            return oauth.refresh_token();
        }
        let command = command.ok_or_else(|| {
            anyhow!(
                "SMTP outgoing URI contains no credential; add a URI password/token or outgoing_cred_cmd"
            )
        })?;
        run_credential_command(command)
    }

    fn access_token(&self, credential: &str) -> Result<String> {
        if let Some(oauth) = &self.account_oauth {
            return oauth.access_token();
        }
        let Some(endpoint) = self.token_endpoint.as_deref() else {
            return Ok(credential.to_owned());
        };

        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", credential),
        ];
        if let Some(value) = self.client_id.as_deref() {
            form.push(("client_id", value));
        }
        if let Some(value) = self.client_secret.as_deref() {
            form.push(("client_secret", value));
        }
        if let Some(value) = self.scope.as_deref() {
            form.push(("scope", value));
        }

        let config = ureq::Agent::config_builder()
            .timeout_global(Some(OAUTH_TIMEOUT))
            .https_only(true)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let mut response = agent
            .post(endpoint)
            .send_form(form)
            .context("exchanging SMTP OAuth refresh token")?;
        let token: TokenResponse = response
            .body_mut()
            .read_json()
            .context("decoding SMTP OAuth token response")?;
        if token.access_token.is_empty() {
            bail!("SMTP OAuth token endpoint returned an empty access token");
        }
        Ok(token.access_token)
    }
}

impl std::fmt::Debug for SmtpSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpSource")
            .field("source", &self.redacted())
            .field("auth", &self.auth)
            .field("has_uri_credential", &self.secret.is_some())
            .field("has_token_endpoint", &self.token_endpoint.is_some())
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

pub struct SmtpSender {
    source: SmtpSource,
    credential_command: Option<String>,
}

impl SmtpSender {
    #[must_use]
    pub const fn new(source: SmtpSource, credential_command: Option<String>) -> Self {
        Self {
            source,
            credential_command,
        }
    }

    /// Exercise SMTP OAuth using the same TLS, EHLO and SASL negotiation as
    /// sending, but quit immediately after AUTH succeeds. Never sends MAIL FROM,
    /// RCPT TO or DATA; safe to run on an existing mailbox.
    pub fn probe_oauth(&self) -> Result<()> {
        if !matches!(self.source.auth, SmtpAuth::XOAuth2 | SmtpAuth::OAuthBearer) ||
            self.source.account_oauth.is_none() {
            bail!("SMTP OAuth probe requires an account with [accounts.oauth]");
        }
        let mut connection = self.authenticated_oauth_connection()?;
        let _ = connection.quit();
        Ok(())
    }

    /// Lettre's high-level transport does not expose RFC 7628 OAUTHBEARER.
    /// The low-level path is shared by actual sends and credential-only probes.
    fn authenticated_oauth_connection(&self) -> Result<SmtpConnection> {
        let credential = self.source.credential(self.credential_command.as_deref())?;
        let token = self.source.access_token(&credential)?;
        let hello = ClientId::default();
        let tls = TlsParameters::new(self.source.host.clone())
            .context("configuring SMTP TLS certificate verification")?;
        let mut connection = match self.source.tls {
            SmtpTls::Implicit => SmtpConnection::connect(
                (self.source.host.as_str(), self.source.port),
                Some(SMTP_TIMEOUT),
                &hello,
                Some(&tls),
                None,
            ),
            SmtpTls::StartTls => SmtpConnection::connect(
                (self.source.host.as_str(), self.source.port),
                Some(SMTP_TIMEOUT),
                &hello,
                None,
                None,
            ),
        }
        .context("connecting to SMTP server")?;

        if self.source.tls == SmtpTls::StartTls {
            // Required STARTTLS, never opportunistic: no token on plaintext.
            connection.starttls(&tls, &hello).context("upgrading SMTP connection to TLS")?;
        }
        if !connection.is_encrypted() {
            bail!("SMTP OAUTHBEARER requires an encrypted TLS connection");
        }

        // Lettre's ServerInfo currently drops unknown AUTH mechanisms; query
        // the post-TLS EHLO reply so we can fail clearly before sending a token.
        let extensions = connection
            .command(Ehlo::new(hello))
            .context("reading SMTP authentication capabilities")?;
        if smtp_advertises_oauthbearer(&extensions) {
            let sasl = oauthbearer_initial_response(
                &self.source.username, &self.source.host, self.source.port, &token,
            )?;
            // The command contains a bearer token; never log it or include it in an error.
            let auth = connection
                .command(format!("AUTH OAUTHBEARER {}\r\n", BASE64.encode(sasl)))
                .context("SMTP OAUTHBEARER authentication exchange")?;
            if auth.has_code(334) {
                // RFC 7628 section 3: acknowledge the server's error challenge
                // with a dummy 0x01 response, and let SMTP finish the exchange.
                // Do not decode/log its potentially sensitive JSON challenge.
                let _ = connection.command("AQ==\r\n");
                bail!("SMTP rejected OAUTHBEARER credentials (334 challenge)");
            }
            if !auth.has_code(235) {
                bail!("SMTP rejected OAUTHBEARER authentication (code {})", auth.code());
            }
        } else if connection.server_info().supports_auth_mechanism(Mechanism::Xoauth2) {
            // Gmail and some other providers advertise XOAUTH2, not RFC 7628.
            // Both use the same OAuth 2.0 access token. Never fall back to a
            // password mechanism, and never retry after rejected credentials.
            let auth = connection.auth(
                &[Mechanism::Xoauth2],
                &Credentials::new(self.source.username.clone(), token),
            ).map_err(|error| smtp_xoauth2_error(&self.source.host, error))?;
            if !auth.has_code(235) {
                bail!("SMTP rejected XOAUTH2 fallback authentication (code {})", auth.code());
            }
        } else {
            bail!("SMTP server supports neither AUTH OAUTHBEARER nor XOAUTH2 after TLS");
        }

        Ok(connection)
    }

    fn send_oauthbearer(&self, email: &lettre::Message, raw_message: &[u8]) -> Result<Response> {
        let mut connection = self.authenticated_oauth_connection()?;
        let result = connection
            .send(email.envelope(), raw_message)
            .context("sending message through authenticated SMTP connection")?;
        // QUIT is best effort: SMTP accepted DATA already; never resubmit here.
        let _ = connection.quit();
        Ok(result)
    }

    fn transport(&self) -> Result<SmtpTransport> {
        let credential = self
            .source
            .credential(self.credential_command.as_deref())?;
        let secret = if self.source.auth == SmtpAuth::XOAuth2 {
            self.source.access_token(&credential)?
        } else {
            credential
        };

        let mut builder = match self.source.tls {
            SmtpTls::Implicit => SmtpTransport::relay(&self.source.host),
            SmtpTls::StartTls => SmtpTransport::starttls_relay(&self.source.host),
        }
        .context("building secure SMTP transport")?
        .port(self.source.port)
        .timeout(Some(SMTP_TIMEOUT));

        if self.source.auth != SmtpAuth::None {
            builder = builder.credentials(Credentials::new(
                self.source.username.clone(),
                secret,
            ));
            // Lettre checks the post-TLS EHLO mechanism list and chooses the
            // first mutually supported option. Never retry a rejected password
            // using a different mechanism or downgrade to cleartext SMTP.
            let methods = match self.source.auth {
                SmtpAuth::Auto => vec![Mechanism::Plain, Mechanism::Login],
                SmtpAuth::Plain => vec![Mechanism::Plain],
                SmtpAuth::Login => vec![Mechanism::Login],
                SmtpAuth::XOAuth2 => vec![Mechanism::Xoauth2],
                SmtpAuth::None | SmtpAuth::OAuthBearer => unreachable!(),
            };
            builder = builder.authentication(methods);
        }
        Ok(builder.build())
    }
}

impl MailSender for SmtpSender {
    fn send(&mut self, message: &OutgoingMessage) -> Result<SendReceipt> {
        let email = crate::outgoing::build_outgoing(message, &self.source.username)?;
        let raw_message = email.formatted();
        let response = if self.source.auth == SmtpAuth::OAuthBearer {
            self.send_oauthbearer(&email, &raw_message)?
        } else {
            self.transport()?
                .send_raw(email.envelope(), &raw_message)
                .context("sending message through SMTP")?
        };
        if !response.has_code(250) {
            bail!("SMTP relay returned a non-success response");
        }
        Ok(SendReceipt {
            raw_message,
            smtp_status: response.first_word().unwrap_or("accepted").to_owned(),
            // Gmail files authenticated SMTP submissions in Sent itself. APPENDing
            // the same RFC 5322 bytes would therefore create a duplicate.
            server_files_sent_copy: self.source.host.eq_ignore_ascii_case("smtp.gmail.com"),
        })
    }
}

/// RFC 7628's GS2 header and bearer key/value fields. Do not put tokens in
/// log messages, Debug implementations, or anyhow error context.
fn oauthbearer_initial_response(user: &str, host: &str, port: u16, token: &str) -> Result<String> {
    if user.is_empty() || token.is_empty() || user.chars().any(char::is_control)
        || token.chars().any(char::is_control)
        || host.chars().any(char::is_control)
    {
        bail!("invalid SMTP OAuth identity or access token");
    }
    // RFC 5801 section 3.1: GS2 authzid uses =2C for comma and =3D for '='.
    let escaped_user = user.replace('=', "=3D").replace(',', "=2C");
    Ok(format!(
        "n,a={escaped_user},\x01host={host}\x01port={port}\x01auth=Bearer {token}\x01\x01"
    ))
}

/// Inspect the post-TLS EHLO reply, rather than Lettre's Mechanism enum (which
/// does not recognize OAUTHBEARER and consequently omits it from ServerInfo).
fn smtp_advertises_oauthbearer(response: &Response) -> bool {
    response.has_code(250) && response.message().any(|line| {
        let mut fields = line.split_ascii_whitespace();
        matches!(fields.next(), Some(prefix) if prefix.eq_ignore_ascii_case("AUTH"))
            && fields.any(|mechanism| mechanism.eq_ignore_ascii_case("OAUTHBEARER"))
    })
}

/// SMTP AUTH is a separate Exchange Online policy, even when IMAP OAuth works.
/// Never log bearer tokens; only enrich the server-authentication error.
fn smtp_xoauth2_error(host: &str, error: impl std::fmt::Display) -> anyhow::Error {
    let detail = error.to_string();
    if (host.eq_ignore_ascii_case("smtp.office365.com")
        || host.eq_ignore_ascii_case("smtp-mail.outlook.com"))
        && detail.contains("535") {
        anyhow!("Microsoft SMTP XOAUTH2 rejected authentication (535). \
            IMAP OAuth may still work. Check the mailbox's SMTP AUTH policy, \
            whether delegated SMTP.Send was consented, the outgoing username, \
            and that smtp.office365.com:587 uses STARTTLS. \
            Only your Exchange administrator can change tenant/mailbox SMTP AUTH restrictions. \
            Server response: {detail}")
    } else {
        anyhow!("SMTP XOAUTH2 authentication exchange: {detail}")
    }
}

fn decode_component(raw: &str) -> Result<String> {
    percent_decode_str(raw)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| anyhow!("URI component is not valid UTF-8"))
}

fn run_credential_command(command: &str) -> Result<String> {
    #[cfg(unix)]
    let output = Command::new("sh").arg("-c").arg(command).output();
    #[cfg(windows)]
    let output = Command::new("cmd").arg("/C").arg(command).output();
    #[cfg(not(any(unix, windows)))]
    let output = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "credential commands are unsupported on this platform",
    ));

    let output = output.context("running outgoing_cred_cmd")?;
    if !output.status.success() {
        bail!("outgoing_cred_cmd exited unsuccessfully");
    }
    let secret = String::from_utf8(output.stdout)
        .context("outgoing_cred_cmd returned non-UTF-8 output")?
        .trim_end_matches(|ch| ch == '\r' || ch == '\n')
        .to_owned();
    if secret.is_empty() {
        bail!("outgoing_cred_cmd returned an empty credential");
    }
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::{SmtpAuth, SmtpSource, oauthbearer_initial_response, smtp_advertises_oauthbearer};
    use lettre::transport::smtp::response::Response;

    #[test]
    fn parses_secure_password_smtp() {
        let source = SmtpSource::parse(
            "smtps://user%40example.com:app%20password@smtp.example.com:465",
        )
        .unwrap();
        assert_eq!(source.username, "user@example.com");
        assert_eq!(source.auth, SmtpAuth::Auto);
        assert_eq!(source.port, 465);
        assert!(!source.redacted().contains("password"));
    }

    #[test]
    fn parses_xoauth2_refresh_source_without_exposing_token() {
        let source = SmtpSource::parse(
            "smtp+xoauth2://user%40example.com:refresh-token@smtp.example.com:587?token_endpoint=https%3A%2F%2Foauth.example.com%2Ftoken&client_id=abc",
        )
        .unwrap();
        assert_eq!(source.auth, SmtpAuth::XOAuth2);
        let debug = format!("{source:?}");
        assert!(!debug.contains("refresh-token"));
        assert!(!debug.contains("client_id=abc"));
    }

    #[test]
    fn auto_smtp_with_refresh_endpoint_never_downgrades_to_password() {
        let source = SmtpSource::parse(
            "smtps://a%40example.org:refresh@smtp.example.org:465?token_endpoint=https%3A%2F%2Foauth.example.org%2Ftoken"
        ).unwrap();
        assert_eq!(source.auth, SmtpAuth::OAuthBearer);
        let explicit = SmtpSource::parse(
            "smtp+plain://a%40example.org:pw@smtp.example.org:587"
        ).unwrap();
        assert_eq!(explicit.auth, SmtpAuth::Plain);
    }

    #[test]
    fn parses_both_oauthbearer_tls_schemes() {
        let implicit = SmtpSource::parse(
            "smtps+oauthbearer://alice%40example.org:refresh-secret@smtp.example.org:465?token_endpoint=https%3A%2F%2Foauth.example.org%2Ftoken",
        ).unwrap();
        assert_eq!(implicit.auth, SmtpAuth::OAuthBearer);
        assert_eq!(implicit.port, 465);
        let explicit = SmtpSource::parse(
            "smtp+oauthbearer://alice%40example.org:secret@smtp.example.org:587",
        ).unwrap();
        assert_eq!(explicit.auth, SmtpAuth::OAuthBearer);
        assert_eq!(explicit.port, 587);
        assert!(!format!("{implicit:?}").contains("refresh-secret"));
        assert!(!implicit.redacted().contains("refresh-secret"));
    }

    #[test]
    fn rejects_insecure_oauthbearer_token_endpoint() {
        let result = SmtpSource::parse(
            "smtp+oauthbearer://alice%40example.org:secret@smtp.example.org:587?token_endpoint=http%3A%2F%2Flocalhost%2Ftoken",
        );
        assert!(result.is_err());
    }

    #[test]
    fn oauthbearer_initial_response_uses_rfc_7628_layout_and_gs2_escape() {
        let frame = oauthbearer_initial_response(
            "alice,test=one@example.org", "smtp.example.org", 587, "opaque.token",
        ).unwrap();
        assert_eq!(frame, "n,a=alice=2Ctest=3Done@example.org,\x01host=smtp.example.org\x01port=587\x01auth=Bearer opaque.token\x01\x01");
        assert!(oauthbearer_initial_response("alice\r\nINJECT", "smtp.example.org", 587, "token").is_err());
        assert!(oauthbearer_initial_response("alice", "smtp.example.org", 587, "token\x01INJECT").is_err());
    }

    #[test]
    fn smtp_auth_capabilities_are_checked_after_tls() {
        let supported: Response = "250-smtp.example.org Hello\r\n250-AUTH LOGIN PLAIN OAUTHBEARER\r\n250 SIZE 12000\r\n".parse().unwrap();
        let unsupported: Response = "250-smtp.gmail.com Hello\r\n250-AUTH PLAIN LOGIN XOAUTH2\r\n250 SIZE 12000\r\n".parse().unwrap();
        assert!(smtp_advertises_oauthbearer(&supported));
        assert!(!smtp_advertises_oauthbearer(&unsupported));
    }
}
