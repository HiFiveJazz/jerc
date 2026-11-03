use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use lettre::message::{Mailbox, Mailboxes};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Message, SmtpTransport, Transport};
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
    Plain,
    Login,
    XOAuth2,
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
}

impl SmtpSource {
    pub fn parse(source: &str) -> Result<Self> {
        let url = Url::parse(source).context("parsing SMTP outgoing URI")?;
        let (tls, auth) = match url.scheme() {
            "smtps" | "smtps+plain" => (SmtpTls::Implicit, SmtpAuth::Plain),
            "smtps+login" => (SmtpTls::Implicit, SmtpAuth::Login),
            "smtps+xoauth2" => (SmtpTls::Implicit, SmtpAuth::XOAuth2),
            "smtps+none" => (SmtpTls::Implicit, SmtpAuth::None),
            "smtp" | "smtp+plain" => (SmtpTls::StartTls, SmtpAuth::Plain),
            "smtp+login" => (SmtpTls::StartTls, SmtpAuth::Login),
            "smtp+xoauth2" => (SmtpTls::StartTls, SmtpAuth::XOAuth2),
            "smtp+none" => (SmtpTls::StartTls, SmtpAuth::None),
            "smtp+oauthbearer" | "smtps+oauthbearer" => {
                bail!(
                    "SMTP OAUTHBEARER is not implemented yet; use +xoauth2 when your provider supports it (Gmail does)"
                );
            }
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

        if auth == SmtpAuth::XOAuth2 {
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
        })
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
        let command = command.ok_or_else(|| {
            anyhow!(
                "SMTP outgoing URI contains no credential; add a URI password/token or outgoing_cred_cmd"
            )
        })?;
        run_credential_command(command)
    }

    fn access_token(&self, credential: &str) -> Result<String> {
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
            builder = builder.authentication(vec![match self.source.auth {
                SmtpAuth::None => unreachable!(),
                SmtpAuth::Plain => Mechanism::Plain,
                SmtpAuth::Login => Mechanism::Login,
                SmtpAuth::XOAuth2 => Mechanism::Xoauth2,
            }]);
        }
        Ok(builder.build())
    }
}

impl MailSender for SmtpSender {
    fn send(&mut self, message: &OutgoingMessage) -> Result<SendReceipt> {
        let from_value = if message.from.trim().is_empty() {
            self.source.username.as_str()
        } else {
            message.from.as_str()
        };
        let from: Mailbox = from_value
            .parse()
            .with_context(|| format!("invalid From address {from_value:?}"))?;
        let to: Mailboxes = message
            .to
            .parse()
            .with_context(|| format!("invalid To address list {:?}", message.to))?;
        if to.iter().next().is_none() {
            bail!("message has no recipients");
        }

        let mut builder = Message::builder()
            .from(from)
            .subject(message.subject.clone())
            .user_agent(format!("aerc-rs/{}", env!("CARGO_PKG_VERSION")));
        for mailbox in to {
            builder = builder.to(mailbox);
        }
        if !message.cc.trim().is_empty() {
            let cc: Mailboxes = message
                .cc
                .parse()
                .with_context(|| format!("invalid Cc address list {:?}", message.cc))?;
            for mailbox in cc {
                builder = builder.cc(mailbox);
            }
        }
        if let Some(id) = &message.in_reply_to {
            builder = builder.in_reply_to(format!("<{id}>"));
        }
        for id in &message.references {
            builder = builder.references(format!("<{id}>"));
        }

        let email = builder
            .body(message.body.clone())
            .context("building outgoing message")?;
        let raw_message = email.formatted();
        let response = self
            .transport()?
            .send_raw(email.envelope(), &raw_message)
            .context("sending message through SMTP")?;
        if !response.is_positive() {
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
    use super::{SmtpAuth, SmtpSource};

    #[test]
    fn parses_secure_password_smtp() {
        let source = SmtpSource::parse(
            "smtps://user%40example.com:app%20password@smtp.example.com:465",
        )
        .unwrap();
        assert_eq!(source.username, "user@example.com");
        assert_eq!(source.auth, SmtpAuth::Plain);
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
}
