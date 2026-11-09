//! Explicit, privacy-conscious Thunderbird-style mail autoconfiguration.
//!
//! The caller opts in with `--discover-email`; no lookup occurs on startup.
//! Prefer domain-owner HTTPS files and Thunderbird ISPDB, then consult DNS MX
//! and the Exchange autodiscover CNAME. Host-based hints are proposals, never
//! credentials or proof that a specific mailbox is hosted by that provider.
//! Discovery never grants OAuth application permissions.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use roxmltree::{Document, Node};
use url::Url;

const MAX_XML_BYTES: u64 = 128 * 1024;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(7);
const MAX_DNS_BYTES: u64 = 64 * 1024;

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
    /// Suggest a login identity separately from the visible email address.
    /// This never persists account settings, and the caller must obtain the
    /// user's confirmation before using an OAuth claim as the login name.
    pub fn with_login_username(self, username: &str) -> Result<Self> {
        self.with_login_usernames(Some(username), Some(username))
    }

    /// Preserve independent IMAP and SMTP identities when the account has
    /// explicitly configured them. `None` keeps the discovered value.
    pub fn with_login_usernames(
        mut self,
        imap_username: Option<&str>,
        smtp_username: Option<&str>,
    ) -> Result<Self> {
        if let Some(username) = imap_username {
            address_parts(username).context("IMAP login must be an email-style UPN")?;
            self.imap.username = username.to_owned();
        }
        if let Some(username) = smtp_username {
            address_parts(username).context("SMTP login must be an email-style UPN")?;
            self.smtp.username = username.to_owned();
        }
        Ok(self)
    }

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
            self.provider
                .chars()
                .filter(|ch| !ch.is_control())
                .take(100)
                .collect::<String>(),
            quoted("Discovered"),
            quoted(&self.email),
            quoted(if self.smtp.socket == SocketType::Ssl {
                "smtps"
            } else {
                "starttls"
            }),
            quoted(&self.imap.host),
            self.imap.port,
            quoted(&self.smtp.host),
            self.smtp.port,
        );
        if self.imap.username != self.email {
            out.push_str(&format!(
                "imap_username = {}\n",
                quoted(&self.imap.username)
            ));
        }
        if self.smtp.username != self.email {
            out.push_str(&format!(
                "smtp_username = {}\n",
                quoted(&self.smtp.username)
            ));
        }
        if self.imap.host.eq_ignore_ascii_case("outlook.office365.com") {
            out.push_str("# Microsoft login UPNs may differ from the visible email address.\n\
                          # Verify imap_username and smtp_username, especially if SMTP returns 535.\n");
        }
        if self.imap.auth.eq_ignore_ascii_case("OAuth2")
            || self.smtp.auth.eq_ignore_ascii_case("OAuth2")
        {
            out.push_str("# This provider advertises OAuth2. Configure [accounts.oauth] with\n\
                          # your own authorized provider/client_id; discovery does not grant consent.\n");
            if self.imap.host.eq_ignore_ascii_case("outlook.office365.com") {
                out.push_str(
                    "# For Microsoft: provider = \"microsoft\" (under [accounts.oauth]).\n",
                );
            } else if self.imap.host.eq_ignore_ascii_case("imap.gmail.com") {
                out.push_str("# For Google: provider = \"google\" (under [accounts.oauth]).\n");
            }
        }
        out
    }
}

/// Don't let arbitrary URLs, ports, or credentials enter from account input.
fn address_parts(email: &str) -> Result<(&str, &str)> {
    let (local, domain) = email
        .rsplit_once('@')
        .ok_or_else(|| anyhow!("provide a complete email address"))?;
    if local.is_empty()
        || local.contains('@')
        || email.chars().any(|c| c.is_control() || c.is_whitespace())
        || !valid_domain(domain)
        || !domain.contains('.')
    {
        bail!("invalid email address or domain for HTTPS autoconfig");
    }
    Ok((local, domain))
}

fn valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && !domain.is_empty()
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && domain
            .split('.')
            .next_back()
            .is_some_and(|label| label.len() >= 2 && label.bytes().all(|b| b.is_ascii_alphabetic()))
}

fn field(node: Node<'_, '_>, key: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.has_tag_name(key))
        .and_then(|child| child.text())
        .map(|text| text.trim().to_owned())
}

fn expand(text: &str, email: &str, local: &str, domain: &str) -> String {
    text.replace("%EMAILADDRESS%", email)
        .replace("%EMAILLOCALPART%", local)
        .replace("%EMAILDOMAIN%", domain)
}

fn parse_server(
    parent: Node<'_, '_>,
    tag: &str,
    ty: &str,
    email: &str,
    local: &str,
    domain: &str,
) -> Option<MailServer> {
    let mut candidates: Vec<_> = parent
        .children()
        .filter(|node| {
            node.is_element() && node.has_tag_name(tag) && node.attribute("type") == Some(ty)
        })
        .filter_map(|node| {
            let host = expand(&field(node, "hostname")?, email, local, domain);
            if !valid_domain(&host) {
                return None;
            }
            let port: u16 = field(node, "port")?.parse().ok()?;
            if port == 0 {
                return None;
            }
            let socket = match field(node, "socketType")?.to_ascii_uppercase().as_str() {
                "SSL" => SocketType::Ssl,
                "STARTTLS" if ty == "smtp" => SocketType::StartTls,
                // IMAP backend currently only supports implicit TLS.
                _ => return None,
            };
            let auth = field(node, "authentication").unwrap_or_default();
            if !matches!(
                auth.to_ascii_lowercase().as_str(),
                "oauth2" | "password-cleartext"
            ) {
                return None;
            }
            let username = expand(
                &field(node, "username").unwrap_or_else(|| "%EMAILADDRESS%".to_owned()),
                email,
                local,
                domain,
            );
            if username.is_empty() || username.chars().any(char::is_control) {
                return None;
            }
            Some(MailServer {
                host,
                port,
                username,
                auth,
                socket,
            })
        })
        .collect();
    // Prefer OAuth and conventional secure submission if the provider exposes
    // multiple configurations. Never select plaintext or STARTTLS-optional.
    candidates.sort_by_key(|server| {
        (
            !server.auth.eq_ignore_ascii_case("OAuth2"),
            if ty == "imap" {
                server.port != 993
            } else {
                server.port != 587 && server.port != 465
            },
        )
    });
    candidates.into_iter().next()
}

pub fn parse_autoconfig(email: &str, xml: &str, source: &'static str) -> Result<MailConfiguration> {
    parse_autoconfig_with_domain(email, xml, source, None)
}

/// MX-derived ISPDB data describes the *mail host* domain, not necessarily
/// the user's address domain. Accept the additional domain only after the MX
/// result led us to fetch that exact ISPDB URL.
fn parse_autoconfig_with_domain(
    email: &str,
    xml: &str,
    source: &'static str,
    mx_domain: Option<&str>,
) -> Result<MailConfiguration> {
    let (local, domain) = address_parts(email)?;
    let doc = Document::parse(xml).context("parsing email provider autoconfig XML")?;
    if !doc.root_element().has_tag_name("clientConfig") {
        bail!("autoconfig file has no clientConfig root");
    }
    let provider = doc
        .root_element()
        .children()
        .find(|node| node.is_element() && node.has_tag_name("emailProvider"))
        .ok_or_else(|| anyhow!("autoconfig file has no emailProvider"))?;
    let domains: Vec<String> = provider
        .children()
        .filter(|node| node.has_tag_name("domain"))
        .filter_map(|node| node.text().map(str::trim).map(str::to_owned))
        .collect();
    if !domains.iter().any(|value| {
        value.eq_ignore_ascii_case(domain)
            || mx_domain.is_some_and(|mx| value.eq_ignore_ascii_case(mx))
    }) {
        bail!("autoconfig provider does not declare this email domain");
    }
    let imap = parse_server(provider, "incomingServer", "imap", email, local, domain)
        .ok_or_else(|| anyhow!("no supported TLS IMAP server in autoconfig"))?;
    let smtp = parse_server(provider, "outgoingServer", "smtp", email, local, domain)
        .ok_or_else(|| anyhow!("no supported TLS SMTP server in autoconfig"))?;
    let provider_name = field(provider, "displayName")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| domain.to_owned());
    Ok(MailConfiguration {
        email: email.to_owned(),
        provider: provider_name,
        source,
        imap,
        smtp,
    })
}

fn lookup_urls(email: &str) -> Result<Vec<(Url, &'static str)>> {
    let (_, domain) = address_parts(email)?;
    let mut provider = Url::parse(&format!("https://autoconfig.{domain}/mail/config-v1.1.xml"))?;
    provider
        .query_pairs_mut()
        .append_pair("emailaddress", email);
    let mut well_known = Url::parse(&format!(
        "https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml"
    ))?;
    well_known
        .query_pairs_mut()
        .append_pair("emailaddress", email);
    let ispdb = Url::parse(&format!("https://autoconfig.thunderbird.net/v1.1/{domain}"))?;
    Ok(vec![
        (provider, "provider HTTPS autoconfig"),
        (well_known, "provider HTTPS well-known"),
        (ispdb, "Thunderbird ISPDB"),
    ])
}

/// Propose canonical endpoints for an explicitly selected provider, or when
/// public DNS gives a sufficiently specific provider signal. This does *not*
/// prove which backend serves a particular user in a multi-provider tenant.
fn known_provider(email: &str, provider: &str, source: &'static str) -> Result<MailConfiguration> {
    address_parts(email)?;
    let (name, imap, smtp, smtp_port) = match provider {
        "microsoft" => (
            "Microsoft 365 / Exchange Online",
            "outlook.office365.com",
            "smtp.office365.com",
            587,
        ),
        "google" => (
            "Google Workspace / Gmail",
            "imap.gmail.com",
            "smtp.gmail.com",
            465,
        ),
        _ => bail!("unknown discovery provider: choose auto, microsoft, or google"),
    };
    Ok(MailConfiguration {
        email: email.to_owned(),
        provider: name.to_owned(),
        source,
        imap: MailServer {
            host: imap.to_owned(),
            port: 993,
            username: email.to_owned(),
            auth: "OAuth2".to_owned(),
            socket: SocketType::Ssl,
        },
        smtp: MailServer {
            host: smtp.to_owned(),
            port: smtp_port,
            username: email.to_owned(),
            auth: "OAuth2".to_owned(),
            socket: if smtp_port == 465 {
                SocketType::Ssl
            } else {
                SocketType::StartTls
            },
        },
    })
}

/// DNS-over-HTTPS is intentionally optional and invoked only for an explicit
/// discovery command. It sends domain names, never email addresses, to the
/// resolver. No externally supplied hostname is used as an auth endpoint.
fn dns_answers(agent: &ureq::Agent, domain: &str, record_type: u16) -> Result<Vec<String>> {
    if !valid_domain(domain) {
        bail!("invalid domain for DNS discovery");
    }
    let mut url = Url::parse("https://cloudflare-dns.com/dns-query")?;
    url.query_pairs_mut()
        .append_pair("name", domain)
        .append_pair("type", &record_type.to_string());
    let mut response = agent
        .get(url.as_str())
        .header("accept", "application/dns-json")
        .call()
        .context("querying DNS-over-HTTPS")?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_DNS_BYTES)
        .read_to_string()
        .context("reading bounded DNS result")?;
    parse_dns_answers(&body, record_type)
}

fn parse_dns_answers(json: &str, record_type: u16) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(json).context("parsing DNS response")?;
    if value.get("Status").and_then(serde_json::Value::as_u64) != Some(0) {
        bail!("DNS lookup returned a non-success status");
    }
    Ok(value
        .get("Answer")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.get("type").and_then(serde_json::Value::as_u64) == Some(u64::from(record_type))
        })
        .filter_map(|entry| entry.get("data").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect())
}

fn mx_hosts(records: &[String]) -> Vec<String> {
    let mut found: Vec<(u16, String)> = records
        .iter()
        .filter_map(|record| {
            let mut parts = record.split_whitespace();
            let priority = parts.next()?.parse::<u16>().ok()?;
            let host = parts.next()?.trim_end_matches('.').to_ascii_lowercase();
            valid_domain(&host).then_some((priority, host))
        })
        .collect();
    found.sort_by_key(|(priority, _)| *priority);
    found.into_iter().map(|(_, host)| host).collect()
}

fn is_microsoft_mx(host: &str) -> bool {
    host.ends_with(".mail.protection.outlook.com") || host == "mail.protection.outlook.com"
}

fn is_google_mx(host: &str) -> bool {
    host == "smtp.google.com"
        || host.ends_with(".aspmx.l.google.com")
        || host == "aspmx.l.google.com"
        || host.ends_with(".google.com")
}

fn is_microsoft_autodiscover(host: &str) -> bool {
    host == "autodiscover.outlook.com" || host.ends_with(".autodiscover.outlook.com")
}

/// Never silently select one mail service when DNS signals multiple distinct
/// backends; university domains often run Gmail and Microsoft 365 together.
fn provider_from_dns(cnames: &[String], mxes: &[String]) -> Result<Option<&'static str>> {
    let microsoft = cnames
        .iter()
        .any(|name| is_microsoft_autodiscover(name.trim_end_matches('.')))
        || mxes.iter().any(|name| is_microsoft_mx(name));
    let google = mxes.iter().any(|name| is_google_mx(name));
    match (microsoft, google) {
        (true, true) => bail!(
            "DNS indicates both Microsoft 365 and Google. Re-run with \
            --discover-provider microsoft or --discover-provider google; neither is safe to select automatically"
        ),
        (true, false) => Ok(Some("microsoft")),
        (false, true) => Ok(Some("google")),
        (false, false) => Ok(None),
    }
}

/// Conservative domain extraction for MX-based ISPDB requests. Avoid guessing
/// public suffixes; if the provider domain isn't simple, manual selection is
/// safer until a public-suffix-list implementation is available.
fn mx_base_domain(mx: &str) -> Option<String> {
    let labels: Vec<_> = mx.split('.').collect();
    if labels.len() < 3 {
        return None;
    }
    let last = &labels[labels.len() - 1..];
    let penultimate = labels[labels.len() - 2];
    if last[0].len() == 2 && matches!(penultimate, "co" | "com" | "ac" | "gov" | "org" | "net") {
        if labels.len() < 4 {
            return None;
        }
        return Some(labels[labels.len() - 3..].join("."));
    }
    Some(labels[labels.len() - 2..].join("."))
}

/// Read-only discovery. Domain-owner HTTPS autoconfig outranks ISPDB; after a
/// miss, use MX-based provider discovery and Exchange's autodiscover CNAME.
/// For universities with mixed mail systems, specify a provider explicitly.
pub fn discover(email: &str) -> Result<MailConfiguration> {
    discover_with_provider(email, "auto")
}

pub fn discover_with_provider(email: &str, provider: &str) -> Result<MailConfiguration> {
    let (_, domain) = address_parts(email)?;
    if provider != "auto" {
        return known_provider(email, provider, "explicit provider hint (not verified)");
    }
    let candidates = lookup_urls(email)?;
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(LOOKUP_TIMEOUT))
            .https_only(true)
            .build(),
    );
    let mut failures = Vec::new();
    for (url, source) in candidates {
        let attempt = (|| -> Result<MailConfiguration> {
            let mut response = agent
                .get(url.as_str())
                .call()
                .context("fetching provider XML")?;
            if response.status() != 200 {
                bail!("not found (HTTP {})", response.status());
            }
            let xml = response
                .body_mut()
                .with_config()
                .limit(MAX_XML_BYTES)
                .read_to_string()
                .context("reading bounded provider XML")?;
            parse_autoconfig(email, &xml, source)
        })();
        match attempt {
            Ok(config) => return Ok(config),
            Err(error) => failures.push(format!("{source}: {error:#}")),
        }
    }

    // Thunderbird's MX-derived ISPDB fallback. DoH is a best-effort
    // implementation that does not change the user's machine DNS settings.
    let mxes = match dns_answers(&agent, domain, 15) {
        Ok(answers) => mx_hosts(&answers),
        Err(error) => {
            failures.push(format!("MX DNS lookup: {error:#}"));
            Vec::new()
        }
    };
    let autodiscover_domain = format!("autodiscover.{domain}");
    let cnames = match dns_answers(&agent, &autodiscover_domain, 5) {
        Ok(answers) => answers
            .into_iter()
            .map(|x| x.trim_end_matches('.').to_ascii_lowercase())
            .collect(),
        Err(error) => {
            failures.push(format!("Exchange autodiscover DNS: {error:#}"));
            Vec::new()
        }
    };

    // Check for a university running both Google and Microsoft before the
    // MX ISPDB can silently select just one of its possible providers.
    let provider_hint = provider_from_dns(&cnames, &mxes)?;

    // Check whether a generic mail host ISPDB entry explains the domain,
    // before resorting to provider-specific guesses.
    if let Some(mx) = mxes.first()
        && let Some(base) = mx_base_domain(mx)
        && base != domain
    {
        let source = "Thunderbird ISPDB (MX-hosted provider)";
        let attempt = (|| -> Result<MailConfiguration> {
            let url = Url::parse(&format!("https://autoconfig.thunderbird.net/v1.1/{base}"))?;
            let mut response = agent
                .get(url.as_str())
                .call()
                .context("fetching MX ISPDB XML")?;
            let xml = response
                .body_mut()
                .with_config()
                .limit(MAX_XML_BYTES)
                .read_to_string()
                .context("reading bounded MX ISPDB XML")?;
            parse_autoconfig_with_domain(email, &xml, source, Some(&base))
        })();
        match attempt {
            Ok(config) => return Ok(config),
            Err(error) => failures.push(format!("MX-derived ISPDB ({base}): {error:#}")),
        }
    }
    if let Some(kind) = provider_hint {
        return known_provider(
            email,
            kind,
            "DNS provider hint (verify mailbox/account identity)",
        );
    }
    bail!(
        "No supported secure IMAP/SMTP autoconfig found for {domain}. \
           This domain may have multiple mail providers. If the account is Microsoft 365, \
           re-run with --discover-provider microsoft (or google for Gmail), \
           then verify the login username against Thunderbird. Lookups: {}",
        failures.join("; ")
    );
}

#[cfg(test)]
mod tests {
    use super::{
        known_provider, lookup_urls, mx_base_domain, mx_hosts, parse_autoconfig,
        parse_autoconfig_with_domain, parse_dns_answers, provider_from_dns,
    };

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
        assert!(
            found
                .to_toml()
                .contains("smtp_username = \"f00851y@example.edu\"")
        );
        assert!(!found.to_toml().contains("client_id ="));
        assert_eq!(found.source, "fixture");

        // Generated output must load as a real account, not just look right.
        let temporary = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temporary.path(), found.to_toml()).unwrap();
        let parsed = crate::config::Config::load(Some(temporary.path())).unwrap();
        let source = crate::backend::ImapSource::parse(&parsed.accounts[0].source).unwrap();
        let outgoing =
            crate::outgoing::SmtpSource::parse(parsed.accounts[0].outgoing.as_deref().unwrap())
                .unwrap();
        assert_eq!(source.username, "alias@example.edu");
        assert_eq!(outgoing.username, "f00851y@example.edu");
        assert_eq!(outgoing.port, 587);
    }

    #[test]
    fn rejects_plaintext_servers_and_wrong_domain() {
        let insecure = SAMPLE.replace(
            "<socketType>STARTTLS</socketType>",
            "<socketType>plain</socketType>",
        );
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

    #[test]
    fn microsoft_alias_login_round_trips_through_account_config() {
        let config = known_provider("alias@example.edu", "microsoft", "fixture")
            .unwrap()
            .with_login_username("login@example.edu")
            .unwrap();
        let toml = config.to_toml();
        assert!(toml.contains("email = \"alias@example.edu\""));
        assert!(toml.contains("imap_username = \"login@example.edu\""));
        assert!(toml.contains("smtp_username = \"login@example.edu\""));
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), toml).unwrap();
        let parsed = crate::config::Config::load(Some(file.path())).unwrap();
        let account = &parsed.accounts[0];
        assert_eq!(account.email.as_deref(), Some("alias@example.edu"));
        let outgoing =
            crate::outgoing::SmtpSource::parse(account.outgoing.as_deref().unwrap()).unwrap();
        assert_eq!(outgoing.username, "login@example.edu");
        assert!(
            config
                .clone()
                .with_login_username("unsafe\n@example.edu")
                .is_err()
        );
        let distinct = config
            .with_login_usernames(Some("imap@example.edu"), Some("smtp@example.edu"))
            .unwrap()
            .to_toml();
        assert!(distinct.contains("imap_username = \"imap@example.edu\""));
        assert!(distinct.contains("smtp_username = \"smtp@example.edu\""));
    }

    #[test]
    fn explicit_microsoft_provider_suggests_thunderbird_servers_without_claiming_auth() {
        let found = known_provider("student@university.edu", "microsoft", "explicit").unwrap();
        let proposal = found.to_toml();
        assert!(proposal.contains("imap_host = \"outlook.office365.com\""));
        assert!(proposal.contains("smtp_host = \"smtp.office365.com\""));
        assert!(proposal.contains("smtp_port = 587"));
        assert!(proposal.contains("outgoing = \"starttls\""));
        assert!(!proposal.contains("client_id ="));
        assert!(known_provider("student@university.edu", "unrecognized", "explicit").is_err());
    }

    #[test]
    fn mx_dns_fallback_handles_priority_and_ignores_irrelevant_answers() {
        let mx = parse_dns_answers(
            r#"{"Status":0,"Answer":[
            {"type":15,"data":"20 mx2.provider.example."},
            {"type":15,"data":"10 mx1.provider.example."},
            {"type":16,"data":"ignore-me"}] }"#,
            15,
        )
        .unwrap();
        assert_eq!(
            mx_hosts(&mx),
            ["mx1.provider.example", "mx2.provider.example"]
        );
        assert_eq!(
            mx_base_domain("mx1.provider.example").as_deref(),
            Some("provider.example")
        );
        assert_eq!(
            mx_base_domain("mx1.provider.co.uk").as_deref(),
            Some("provider.co.uk")
        );
        assert!(parse_dns_answers(r#"{"Status":3}"#, 15).is_err());
    }

    #[test]
    fn two_provider_university_requires_explicit_selection() {
        let cnames = ["autodiscover.outlook.com".to_owned()];
        let google_mx = ["aspmx.l.google.com".to_owned()];
        assert!(provider_from_dns(&cnames, &google_mx).is_err());
        assert_eq!(provider_from_dns(&cnames, &[]).unwrap(), Some("microsoft"));
        assert_eq!(provider_from_dns(&[], &google_mx).unwrap(), Some("google"));
    }

    #[test]
    fn mx_ispdb_accepts_mail_host_domain_without_accepting_unrelated_domain() {
        assert!(parse_autoconfig("user@campus.edu", SAMPLE, "mx-ispdb").is_err());
        assert!(
            parse_autoconfig_with_domain(
                "user@campus.edu",
                SAMPLE,
                "mx-ispdb",
                Some("example.edu")
            )
            .is_ok()
        );
        assert!(
            parse_autoconfig_with_domain(
                "user@campus.edu",
                SAMPLE,
                "mx-ispdb",
                Some("unrelated.org")
            )
            .is_err()
        );
    }
}
