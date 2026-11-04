//! Explicit, privacy-conscious Thunderbird-style mail autoconfiguration.
//!
//! The caller opts in with `--discover-email`; no lookup occurs on startup.
//! Prefer the domain owner's HTTPS files, then Thunderbird ISPDB. Discovery
//! suggests server settings only; it cannot grant OAuth client permissions.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use roxmltree::{Document, Node};
use url::Url;

const MAX_XML_BYTES: u64 = 128 * 1024;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(7);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketType {
    Ssl,
    StartTls,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailServer {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: String,
    socket: SocketType,
}

#[derive(Debug, Clone)]
pub struct MailConfiguration {
    pub email: String,
    pub provider: String,
    pub source: &'static str,
    pub imap: MailServer,
    pub smtp: MailServer,
}

impl MailConfiguration {
    /// A reviewable suggestion, not an automatic modification to config.toml.
    #[must_use]
    pub fn to_toml(&self) -> String {
        fn quoted(s: &str) -> String {
            toml::Value::String(s.to_owned()).to_string()
        }
        let mut out = format!(
            "# Suggested by {} ({})\n# Review usernames, OAuth registration, and sending identity before use.\n\
             [[accounts]]\nname = {}\nemail = {}\nsource = \"imap\"\n\
             outgoing = {}\nimap_host = {}\nimap_port = {}\n\
             smtp_host = {}\nsmtp_port = {}\n",
            self.source,
            self.provider.chars().filter(|ch| !ch.is_control()).take(100).collect::<String>(),
            quoted("Discovered"),
            quoted(&self.email),
            quoted(if self.smtp.socket == SocketType::Ssl { "smtps" } else { "starttls" }),
            quoted(&self.imap.host),
            self.imap.port,
            quoted(&self.smtp.host),
            self.smtp.port,
        );
        if self.imap.username != self.email {
            out.push_str(&format!("imap_username = {}\n", quoted(&self.imap.username)));
        }
        if self.smtp.username != self.email {
            out.push_str(&format!("smtp_username = {}\n", quoted(&self.smtp.username)));
        }
        if self.imap.auth.eq_ignore_ascii_case("OAuth2") || self.smtp.auth.eq_ignore_ascii_case("OAuth2") {
            out.push_str("# This provider advertises OAuth2. Configure [accounts.oauth] with\n\
                          # your own authorized provider/client_id; discovery does not grant consent.\n");
            if self.imap.host.eq_ignore_ascii_case("outlook.office365.com") {
                out.push_str("# For Microsoft: provider = \"microsoft\" (under [accounts.oauth]).\n");
            } else if self.imap.host.eq_ignore_ascii_case("imap.gmail.com") {
                out.push_str("# For Google: provider = \"google\" (under [accounts.oauth]).\n");
            }
        }
        out
    }
}

/// Don't let arbitrary URLs, ports, or credentials enter from account input.
fn address_parts(email: &str) -> Result<(&str, &str)> {
    let (local, domain) = email.rsplit_once('@').ok_or_else(|| anyhow!("provide a complete email address"))?;
    if local.is_empty() || local.contains('@') || email.chars().any(|c| c.is_control() || c.is_whitespace())
        || !valid_domain(domain) || !domain.contains('.')
    {
        bail!("invalid email address or domain for HTTPS autoconfig");
    }
    Ok((local, domain))
}

fn valid_domain(domain: &str) -> bool {
    domain.len() <= 253 && !domain.is_empty()
        && !domain.starts_with('.') && !domain.ends_with('.')
        && domain.split('.').all(|label| !label.is_empty() && label.len() <= 63
            && !label.starts_with('-') && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        && domain.split('.').last().is_some_and(|label| label.len() >= 2
            && label.bytes().all(|b| b.is_ascii_alphabetic()))
}

fn field(node: Node<'_, '_>, key: &str) -> Option<String> {
    node.children().find(|child| child.is_element() && child.has_tag_name(key))
        .and_then(|child| child.text()).map(|text| text.trim().to_owned())
}

fn expand(text: &str, email: &str, local: &str, domain: &str) -> String {
    text.replace("%EMAILADDRESS%", email)
        .replace("%EMAILLOCALPART%", local)
        .replace("%EMAILDOMAIN%", domain)
}

fn parse_server(parent: Node<'_, '_>, tag: &str, ty: &str, email: &str, local: &str, domain: &str) -> Option<MailServer> {
    let mut candidates: Vec<_> = parent.children()
        .filter(|node| node.is_element() && node.has_tag_name(tag) && node.attribute("type") == Some(ty))
        .filter_map(|node| {
            let host = expand(&field(node, "hostname")?, email, local, domain);
            if !valid_domain(&host) { return None; }
            let port: u16 = field(node, "port")?.parse().ok()?;
            if port == 0 { return None; }
            let socket = match field(node, "socketType")?.to_ascii_uppercase().as_str() {
                "SSL" => SocketType::Ssl,
                "STARTTLS" if ty == "smtp" => SocketType::StartTls,
                // IMAP backend currently only supports implicit TLS.
                _ => return None,
            };
            let auth = field(node, "authentication").unwrap_or_default();
            if !matches!(auth.to_ascii_lowercase().as_str(), "oauth2" | "password-cleartext") {
                return None;
            }
            let username = expand(&field(node, "username").unwrap_or_else(|| "%EMAILADDRESS%".to_owned()), email, local, domain);
            if username.is_empty() || username.chars().any(char::is_control) { return None; }
            Some(MailServer { host, port, username, auth, socket })
        }).collect();
    // Prefer OAuth and conventional secure submission if the provider exposes
    // multiple configurations. Never select plaintext or STARTTLS-optional.
    candidates.sort_by_key(|server| (
        !server.auth.eq_ignore_ascii_case("OAuth2"),
        if ty == "imap" { server.port != 993 } else { server.port != 587 && server.port != 465 },
    ));
    candidates.into_iter().next()
}

pub fn parse_autoconfig(email: &str, xml: &str, source: &'static str) -> Result<MailConfiguration> {
    let (local, domain) = address_parts(email)?;
    let doc = Document::parse(xml).context("parsing email provider autoconfig XML")?;
    if !doc.root_element().has_tag_name("clientConfig") {
        bail!("autoconfig file has no clientConfig root");
    }
    let provider = doc.root_element().children()
        .find(|node| node.is_element() && node.has_tag_name("emailProvider"))
        .ok_or_else(|| anyhow!("autoconfig file has no emailProvider"))?;
    let domains: Vec<String> = provider.children().filter(|node| node.has_tag_name("domain"))
        .filter_map(|node| node.text().map(str::trim).map(str::to_owned)).collect();
    if !domains.iter().any(|value| value.eq_ignore_ascii_case(domain)) {
        bail!("autoconfig provider does not declare this email domain");
    }
    let imap = parse_server(provider, "incomingServer", "imap", email, local, domain)
        .ok_or_else(|| anyhow!("no supported TLS IMAP server in autoconfig"))?;
    let smtp = parse_server(provider, "outgoingServer", "smtp", email, local, domain)
        .ok_or_else(|| anyhow!("no supported TLS SMTP server in autoconfig"))?;
    let provider_name = field(provider, "displayName")
        .filter(|s| !s.is_empty()).unwrap_or_else(|| domain.to_owned());
    Ok(MailConfiguration { email: email.to_owned(), provider: provider_name, source, imap, smtp })
}

fn lookup_urls(email: &str) -> Result<Vec<(Url, &'static str)>> {
    let (_, domain) = address_parts(email)?;
    let mut provider = Url::parse(&format!("https://autoconfig.{domain}/mail/config-v1.1.xml"))?;
    provider.query_pairs_mut().append_pair("emailaddress", email);
    let mut well_known = Url::parse(&format!("https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml"))?;
    well_known.query_pairs_mut().append_pair("emailaddress", email);
    let ispdb = Url::parse(&format!("https://autoconfig.thunderbird.net/v1.1/{domain}"))?;
    Ok(vec![(provider, "provider HTTPS autoconfig"), (well_known, "provider HTTPS well-known"), (ispdb, "Thunderbird ISPDB")])
}

/// This is an explicit, read-only discovery command. The full email is sent
/// only to the email domain's own HTTPS autoconfig service. ISPDB gets domain
/// only. Never use discovered credentials or alter OAuth registrations.
pub fn discover(email: &str) -> Result<MailConfiguration> {
    let candidates = lookup_urls(email)?;
    let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder()
        .timeout_global(Some(LOOKUP_TIMEOUT))
        .https_only(true)
        .build());
    let mut failures = Vec::new();
    for (url, source) in candidates {
        let attempt = (|| -> Result<MailConfiguration> {
            let mut response = agent.get(url.as_str()).call().context("fetching provider XML")?;
            if response.status() != 200 { bail!("not found (HTTP {})", response.status()); }
            let xml = response.body_mut().with_config().limit(MAX_XML_BYTES).read_to_string()
                .context("reading bounded provider XML")?;
            parse_autoconfig(email, &xml, source)
        })();
        match attempt {
            Ok(config) => return Ok(config),
            Err(error) => failures.push(format!("{source}: {error:#}")),
        }
    }
    bail!("No supported secure IMAP/SMTP autoconfig for this domain. \
           Configure imap_host, smtp_host, and independent usernames manually. \
           Lookups: {}", failures.join("; "));
}

#[cfg(test)]
mod tests {
    use super::{parse_autoconfig, lookup_urls};

    const SAMPLE: &str = r#"<?xml version="1.0"?>
    <clientConfig version="1.1"><emailProvider id="example.edu">
    <domain>example.edu</domain><displayName>University</displayName>
    <incomingServer type="imap"><hostname>imap.example.edu</hostname>
      <port>993</port><socketType>SSL</socketType><authentication>OAuth2</authentication>
      <username>%EMAILADDRESS%</username></incomingServer>
    <outgoingServer type="smtp"><hostname>smtp.example.edu</hostname>
      <port>587</port><socketType>STARTTLS</socketType><authentication>OAuth2</authentication>
      <username>f00851y@example.edu</username></outgoingServer>
    </emailProvider></clientConfig>"#;

    #[test]
    fn parses_secure_servers_and_distinct_smtp_login() {
        let found = parse_autoconfig("alias@example.edu", SAMPLE, "fixture").unwrap();
        assert_eq!(found.imap.username, "alias@example.edu");
        assert_eq!(found.smtp.username, "f00851y@example.edu");
        assert_eq!(found.smtp.port, 587);
        assert!(found.to_toml().contains("smtp_username = \"f00851y@example.edu\""));
        assert!(!found.to_toml().contains("client_id ="));
        assert_eq!(found.source, "fixture");

        // Generated output must load as a real account, not just look right.
        let temporary = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temporary.path(), found.to_toml()).unwrap();
        let parsed = crate::config::Config::load(Some(temporary.path())).unwrap();
        let source = crate::backend::ImapSource::parse(&parsed.accounts[0].source).unwrap();
        let outgoing = crate::outgoing::SmtpSource::parse(
            parsed.accounts[0].outgoing.as_deref().unwrap(),
        ).unwrap();
        assert_eq!(source.username, "alias@example.edu");
        assert_eq!(outgoing.username, "f00851y@example.edu");
        assert_eq!(outgoing.port, 587);
    }

    #[test]
    fn rejects_plaintext_servers_and_wrong_domain() {
        let insecure = SAMPLE.replace("<socketType>STARTTLS</socketType>", "<socketType>plain</socketType>");
        assert!(parse_autoconfig("alias@example.edu", &insecure, "test").is_err());
        assert!(parse_autoconfig("alias@elsewhere.edu", SAMPLE, "test").is_err());
    }

    #[test]
    fn avoids_leaking_full_address_to_third_party_ispdb() {
        let urls = lookup_urls("test+tag@example.edu").unwrap();
        assert!(urls[0].0.as_str().contains("emailaddress="));
        assert!(urls[1].0.as_str().contains("emailaddress="));
        assert!(!urls[2].0.as_str().contains("test"));
        assert!(urls[2].0.as_str().ends_with("/example.edu"));
        assert!(lookup_urls("user@localhost").is_err());
        assert!(lookup_urls("user@127.0.0.1").is_err());
    }
}
