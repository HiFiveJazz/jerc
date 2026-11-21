//! Explicit, privacy-conscious Thunderbird-style mail autoconfiguration.
//!
//! The caller opts in with `--discover-email`; no lookup occurs on startup.
//! Prefer domain-owner HTTPS files and Thunderbird ISPDB, then consult DNS MX
//! and the Exchange autodiscover CNAME. Host-based hints are proposals, never
//! credentials or proof that a specific mailbox is hosted by that provider.
//! Discovery never grants OAuth application permissions.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use roxmltree::{Document, Node};
use url::Url;

const MAX_XML_BYTES: u64 = 128 * 1024;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(7);
const MAX_DNS_BYTES: u64 = 64 * 1024;
const MAX_PACC_BYTES: u64 = 128 * 1024;

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
    /// OAuth issuer was explicitly advertised and its server metadata validated.
    pub oauth_issuer: Option<String>,
    /// HTTPS JMAP endpoint detected without submitting a username or credential.
    pub jmap_host: Option<String>,
    pub imap: MailServer,
    pub smtp: MailServer,
}

impl MailConfiguration {
    /// Transport security comes from XML socketType, not a port-number guess.
    pub fn imap_implicit_tls(&self) -> bool {
        self.imap.socket == SocketType::Ssl
    }

    pub fn smtp_implicit_tls(&self) -> bool {
        self.smtp.socket == SocketType::Ssl
    }

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
        let incoming_source = if let Some(host) = &self.jmap_host {
            // Credentials are never included in discovered configuration.
            let mut url =
                Url::parse(&format!("jmaps://{host}")).expect("validated JMAP discovery hostname");
            url.set_username(&self.imap.username)
                .expect("valid JMAP login identity");
            url.to_string()
        } else if self.imap.socket == SocketType::Ssl {
            "imap".to_owned()
        } else {
            "imap+starttls".to_owned()
        };
        let mut out = format!(
            "# Suggested by {} ({})\n# Review usernames, OAuth registration, and sending identity before use.\n\
             [[accounts]]\nname = {}\nemail = {}\nsource = {}\n\
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
            quoted(&incoming_source),
            quoted(
                match (
                    self.smtp.socket,
                    self.smtp.auth.eq_ignore_ascii_case("none")
                ) {
                    (SocketType::Ssl, true) => "smtps+none",
                    (SocketType::StartTls, true) => "smtp+none",
                    (SocketType::Ssl, false) => "smtps",
                    (SocketType::StartTls, false) => "starttls",
                }
            ),
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
        if let Some(host) = &self.jmap_host {
            out.push_str(&format!(
                "# Incoming JMAP HTTPS endpoint: {host} (verify credentials before using).\n"
            ));
        }
        if let Some(issuer) = &self.oauth_issuer {
            out.push_str(&format!(
                "[accounts.oauth]\nprovider = \"open\"\nflow = \"browser\"\ncallback_host = \"127.0.0.1\"\ncallback_path = \"/oauth/callback\"\nissuer = {}\n",
                quoted(issuer),
            ));
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
                "STARTTLS" => SocketType::StartTls,
                _ => return None,
            };
            let auth = field(node, "authentication").unwrap_or_default();
            if !matches!(
                auth.to_ascii_lowercase().as_str(),
                "oauth2" | "password-cleartext" | "none"
            ) {
                return None;
            }
            // IMAP requires a login; unauthenticated SMTP is a separate
            // encrypted submission mode and must not imply plaintext transport.
            if ty == "imap" && auth.eq_ignore_ascii_case("none") {
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
            if server.auth.eq_ignore_ascii_case("OAuth2") {
                0
            } else if server.auth.eq_ignore_ascii_case("none") {
                2
            } else {
                1
            },
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
        oauth_issuer: None,
        jmap_host: None,
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
        oauth_issuer: None,
        jmap_host: None,
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
fn valid_dns_query_name(name: &str, record_type: u16) -> bool {
    // DNS discovery is for externally resolvable domains, not single-label
    // hosts such as "localhost". Keep this restriction local to discovery:
    // an explicitly configured mail server may legitimately use localhost.
    let valid_discovery_domain = |domain: &str| {
        domain.contains('.')
            && valid_domain(domain)
            && !domain
                .rsplit('.')
                .next()
                .is_some_and(|tld| tld.eq_ignore_ascii_case("localhost"))
    };
    match record_type {
        // Plain domain lookups are only needed for autodiscover CNAME and MX.
        5 | 15 => valid_discovery_domain(name),
        16 => name
            .strip_prefix("_ua-auto-config.")
            .is_some_and(valid_discovery_domain),
        33 => [
            "_imaps._tcp.",
            "_imap._tcp.",
            "_submissions._tcp.",
            "_submission._tcp.",
        ]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .is_some_and(valid_discovery_domain)
        }),
        _ => false,
    }
}

fn dns_answers(agent: &ureq::Agent, domain: &str, record_type: u16) -> Result<Vec<String>> {
    if !valid_dns_query_name(domain, record_type) {
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

/// PACC (draft-ietf-mailmaint-pacc) announces OAuth independently of
/// Thunderbird's XML, which normally advertises password authentication only.
/// Never treat an advertised issuer as proof that an individual IMAP or SMTP
/// service supports a bearer SASL mechanism: F5 probes still verify the login.
pub(crate) fn parse_pacc(email: &str, json: &str) -> Result<MailConfiguration> {
    let (_, domain) = address_parts(email)?;
    let value: serde_json::Value = serde_json::from_str(json).context("invalid PACC JSON")?;
    let protocols = value
        .get("protocols")
        .ok_or_else(|| anyhow!("PACC has no protocols"))?;
    let imap = protocols
        .pointer("/imap/host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("PACC has no IMAP host"))?;
    let smtp = protocols
        .pointer("/submit/host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("PACC has no SMTP submission host"))?;
    ensure!(
        valid_domain(imap) && valid_domain(smtp),
        "invalid PACC mail host"
    );

    let auth = value.get("authentication");
    let password = auth
        .and_then(|a| a.get("password"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let oauth_issuer = auth
        .and_then(|a| a.pointer("/oauth-public/issuer"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    if let Some(issuer) = oauth_issuer.as_deref() {
        crate::oauth::validate_open_issuer(issuer)?;
    }
    ensure!(
        password || oauth_issuer.is_some() || auth.is_none(),
        "PACC advertises no supported mail authentication"
    );
    let oauth = oauth_issuer.is_some();
    let provider = value
        .pointer("/info/provider/name")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty() && v.len() <= 60 && !v.chars().any(char::is_control))
        .unwrap_or(domain);
    Ok(MailConfiguration {
        email: email.into(),
        provider: provider.into(),
        source: "PACC (domain HTTPS with DNS digest)",
        oauth_issuer,
        jmap_host: None,
        imap: MailServer {
            host: imap.into(),
            port: 993,
            username: email.into(),
            auth: if oauth {
                "OAuth2"
            } else {
                "password-cleartext"
            }
            .into(),
            socket: SocketType::Ssl,
        },
        smtp: MailServer {
            host: smtp.into(),
            port: 465,
            username: email.into(),
            auth: if oauth {
                "OAuth2"
            } else {
                "password-cleartext"
            }
            .into(),
            socket: SocketType::Ssl,
        },
    })
}

/// DNS-over-HTTPS uses JSON-escaped TXT strings. Each TXT resource record
/// must be parsed separately; adjacent character-string segments within a
/// single record are concatenated without separators.
fn pacc_digest_matches(contents: &[u8], txt_records: &[String]) -> bool {
    use ring::digest::{SHA256, digest};
    let actual = digest(&SHA256, contents);
    txt_records.iter().any(|raw| {
        let record = raw.trim();
        let record = record.strip_prefix('"').unwrap_or(record);
        let record = record.strip_suffix('"').unwrap_or(record);
        let record = record.replace("\" \"", "");
        let mut version = None;
        let mut algorithm = None;
        let mut expected = None;
        for tag in record.split(';') {
            if let Some((key, value)) = tag.trim().split_once('=') {
                match key.trim() {
                    "v" => version = Some(value.trim()),
                    "a" => algorithm = Some(value.trim()),
                    "d" => expected = Some(value.trim()),
                    _ => {}
                }
            }
        }
        if version != Some("UAAC1") || algorithm != Some("sha256") {
            return false;
        }
        expected
            .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
            .is_some_and(|digest| digest.as_slice() == actual.as_ref())
    })
}

/// Validate the issuer before recommending OAuth. Never register a client or
/// send credentials during discovery. Invalid metadata means use password.
fn validate_public_oauth_metadata(json: &str, issuer: &str) -> Result<()> {
    crate::oauth::validate_open_issuer(issuer)?;
    let value: serde_json::Value = serde_json::from_str(json).context("invalid OAuth metadata")?;
    ensure!(
        value.get("issuer").and_then(|v| v.as_str()) == Some(issuer),
        "OAuth issuer mismatch"
    );
    for key in [
        "authorization_endpoint",
        "token_endpoint",
        "registration_endpoint",
    ] {
        let endpoint = value
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("OAuth metadata missing {key}"))?;
        crate::oauth::validate_open_issuer(endpoint)?;
    }
    let includes = |key: &str, wanted: &str| {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .is_some_and(|xs| xs.iter().any(|x| x.as_str() == Some(wanted)))
    };
    ensure!(
        includes("code_challenge_methods_supported", "S256"),
        "OAuth provider does not advertise PKCE S256"
    );
    ensure!(
        includes("grant_types_supported", "authorization_code")
            && includes("grant_types_supported", "refresh_token"),
        "OAuth provider does not advertise refreshable authorization codes"
    );
    ensure!(
        includes("scopes_supported", "urn:ietf:params:oauth:scope:mail"),
        "OAuth provider does not advertise mail scope"
    );
    Ok(())
}

fn check_oauth_issuer(agent: &ureq::Agent, issuer: &str) -> Result<()> {
    crate::oauth::validate_open_issuer(issuer)?;
    let parsed = Url::parse(issuer)?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    let url = format!("{origin}/.well-known/oauth-authorization-server{path}");
    let mut reply = agent.get(&url).call().context("fetching OAuth metadata")?;
    ensure!(
        reply.status() == 200,
        "OAuth metadata HTTP {}",
        reply.status()
    );
    let json = reply
        .body_mut()
        .with_config()
        .limit(MAX_PACC_BYTES)
        .read_to_string()?;
    validate_public_oauth_metadata(&json, issuer)
}

fn discover_pacc(agent: &ureq::Agent, email: &str) -> Result<MailConfiguration> {
    let (_, domain) = address_parts(email)?;
    let url = format!("https://ua-auto-config.{domain}/.well-known/user-agent-configuration.json");
    let mut reply = agent
        .get(&url)
        .header("accept", "application/json")
        .call()
        .context("fetching domain PACC document")?;
    ensure!(reply.status() == 200, "PACC HTTP {}", reply.status());
    let mime = reply
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    ensure!(
        mime.split(';')
            .next()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json")),
        "PACC content type is not application/json"
    );
    let bytes = reply
        .body_mut()
        .with_config()
        .limit(MAX_PACC_BYTES)
        .read_to_vec()?;
    // We have a trusted HTTPS DNS channel, so reject absent or mismatched
    // TXT digests instead of allowing an unverified OAuth issuer override.
    let txt = dns_answers(agent, &format!("_ua-auto-config.{domain}"), 16)?;
    ensure!(
        pacc_digest_matches(&bytes, &txt),
        "PACC DNS digest is missing or mismatched"
    );
    let json = std::str::from_utf8(&bytes)?;
    let mut config = parse_pacc(email, json)?;
    let password_supported = serde_json::from_str::<serde_json::Value>(json)?
        .pointer("/authentication/password")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(issuer) = config.oauth_issuer.clone()
        && check_oauth_issuer(agent, &issuer).is_err()
    {
        // Keep the verified server settings only if password really is
        // supported; never silently recommend credentials for OAuth-only mail.
        ensure!(
            password_supported,
            "OAuth metadata invalid and PACC disables passwords"
        );
        config.oauth_issuer = None;
        config.imap.auth = "password-cleartext".into();
        config.smtp.auth = "password-cleartext".into();
    }
    Ok(config)
}

/// Non-PACC fallback only for the exact first-party mail.<email-domain>
/// host shared by IMAP and SMTP. Avoid guessing OAuth for hosted MX providers,
/// and require validated RFC 8414 metadata rather than relying on a banner.
fn first_party_oauth_candidate(config: &MailConfiguration) -> Option<String> {
    let (_, domain) = address_parts(&config.email).ok()?;
    let hostname = format!("mail.{domain}");
    if config.oauth_issuer.is_some()
        || !config.imap.host.eq_ignore_ascii_case(&hostname)
        || !config.smtp.host.eq_ignore_ascii_case(&hostname)
        || config.smtp.auth.eq_ignore_ascii_case("none")
    {
        return None;
    }
    Some(format!("https://{hostname}"))
}

fn try_first_party_oauth(agent: &ureq::Agent, mut config: MailConfiguration) -> MailConfiguration {
    let Some(issuer) = first_party_oauth_candidate(&config) else {
        return config;
    };
    if check_oauth_issuer(agent, &issuer).is_ok() {
        config.oauth_issuer = Some(issuer);
        config.imap.auth = "OAuth2".into();
        config.smtp.auth = "OAuth2".into();
    }
    config
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

/// DNS SRV records are untrusted *suggestions*: Jerc always validates TLS
/// certificates when it actually connects, and no credentials are sent during
/// discovery. Parse only the secure IMAPS and SMTP submission variants.
fn srv_endpoint(records: &[String]) -> Option<(String, u16)> {
    records
        .iter()
        .filter_map(|record| {
            let parts: Vec<_> = record.split_whitespace().collect();
            if parts.len() != 4 {
                return None;
            }
            let priority = parts[0].parse::<u16>().ok()?;
            let _weight = parts[1].parse::<u16>().ok()?;
            let port = parts[2].parse::<u16>().ok()?;
            let host = parts[3].trim_end_matches('.').to_ascii_lowercase();
            (port != 0 && valid_domain(&host)).then_some((priority, host, port))
        })
        .min_by_key(|(priority, _, _)| *priority)
        .map(|(_, host, port)| (host, port))
}

/// Never infer a password, port, or server from an unrelated third-party MX.
fn self_hosted_mx_suggestion(email: &str, mxes: &[String]) -> Option<MailConfiguration> {
    let (_, domain) = address_parts(email).ok()?;
    let host = format!("mail.{domain}");
    if !mxes.iter().any(|mx| mx.eq_ignore_ascii_case(&host)) {
        return None;
    }
    Some(MailConfiguration {
        email: email.to_owned(),
        provider: "Self-hosted IMAP/SMTP (review suggested settings)".into(),
        source: "same-domain MX hostname hint (not a TLS connectivity test)",
        oauth_issuer: None,
        jmap_host: None,
        imap: MailServer {
            host: host.clone(),
            port: 993,
            username: email.into(),
            auth: "password-cleartext".into(),
            socket: SocketType::Ssl,
        },
        smtp: MailServer {
            host,
            port: 465,
            username: email.into(),
            auth: "password-cleartext".into(),
            socket: SocketType::Ssl,
        },
    })
}

fn srv_suggestion(
    email: &str,
    imap: (String, u16),
    imap_tls: SocketType,
    smtp: (String, u16),
    smtp_tls: SocketType,
) -> MailConfiguration {
    MailConfiguration {
        email: email.to_owned(),
        provider: "IMAP/SMTP via DNS SRV (review suggested settings)".into(),
        source: "DNS SRV (TLS endpoints not independently verified)",
        oauth_issuer: None,
        jmap_host: None,
        imap: MailServer {
            host: imap.0,
            port: imap.1,
            username: email.into(),
            auth: "password-cleartext".into(),
            socket: imap_tls,
        },
        smtp: MailServer {
            host: smtp.0,
            port: smtp.1,
            username: email.into(),
            auth: "password-cleartext".into(),
            socket: smtp_tls,
        },
    }
}

/// Read-only discovery. Domain-owner HTTPS autoconfig outranks ISPDB; after a
/// miss, use MX-based provider discovery and Exchange's autodiscover CNAME.
/// For universities with mixed mail systems, specify a provider explicitly.
pub fn discover(email: &str) -> Result<MailConfiguration> {
    discover_with_provider(email, "auto")
}

/// Public, unauthenticated HTTPS probe. Never send an email address, password,
/// OAuth token, or credential header during JMAP discovery. A protected JMAP
/// endpoint may return 401: that is a *hint*, not proof of account access.
fn jmap_hint(agent: &ureq::Agent, email: &str, suggested_imap: Option<&str>) -> Option<String> {
    let (_, domain) = address_parts(email).ok()?;
    let candidates = [suggested_imap, Some(domain), Some("")];
    let mut hosts = std::collections::HashSet::new();
    for candidate in candidates.into_iter().flatten() {
        let host = if candidate.is_empty() {
            format!("mail.{domain}")
        } else {
            candidate.to_owned()
        };
        // A provider-selected unrelated hostname must never expand the JMAP
        // discovery trust scope (nor silently force JMAP for Gmail/Microsoft).
        if !host.eq_ignore_ascii_case(domain)
            && !host.to_ascii_lowercase().ends_with(&format!(".{domain}"))
        {
            continue;
        }
        if !valid_domain(&host) || !hosts.insert(host.clone()) {
            continue;
        }
        if fetch_public_jmap_session(agent, &host).is_some() {
            return Some(host);
        }
    }
    None
}

/// Follow only same-origin HTTPS redirects during public discovery. RFC 8620
/// permits /.well-known/jmap to redirect to the actual session resource.
fn fetch_public_jmap_session(agent: &ureq::Agent, host: &str) -> Option<serde_json::Value> {
    let original = Url::parse(&format!("https://{host}/.well-known/jmap")).ok()?;
    let mut endpoint = original.clone();
    for _ in 0..=3 {
        let mut response = agent.get(endpoint.as_str()).call().ok()?;
        if response.status().is_redirection() {
            let location = response.headers().get("location")?.to_str().ok()?;
            let next = endpoint.join(location).ok()?;
            if next.origin() != original.origin()
                || !next.username().is_empty()
                || next.password().is_some()
            {
                return None;
            }
            endpoint = next;
            continue;
        }
        // A generic HTTP 401 is not evidence that JMAP is hosted here.
        if !response.status().is_success() {
            return None;
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_PACC_BYTES)
            .read_to_vec()
            .ok()?;
        let session: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        if session
            .pointer("/capabilities/urn:ietf:params:jmap:core")
            .is_some_and(serde_json::Value::is_object)
            && session
                .pointer("/capabilities/urn:ietf:params:jmap:mail")
                .is_some_and(serde_json::Value::is_object)
        {
            return Some(session);
        }
        return None;
    }
    None
}

/// Explicit JMAP selection also works if the server requires authentication
/// before its session metadata can be inspected. SMTP remains an editable hint.
fn jmap_suggestion(email: &str) -> Result<MailConfiguration> {
    let (_, domain) = address_parts(email)?;
    let host = format!("mail.{domain}");
    Ok(MailConfiguration {
        email: email.to_owned(),
        provider: "JMAP (review HTTPS endpoint and SMTP settings)".into(),
        source: "explicit JMAP HTTPS choice (endpoint not yet authenticated)",
        oauth_issuer: None,
        jmap_host: Some(host.clone()),
        imap: MailServer {
            host: host.clone(),
            port: 993,
            username: email.into(),
            auth: "password-cleartext".into(),
            socket: SocketType::Ssl,
        },
        smtp: MailServer {
            host,
            port: 465,
            username: email.into(),
            auth: "password-cleartext".into(),
            socket: SocketType::Ssl,
        },
    })
}

pub fn discover_with_provider(email: &str, provider: &str) -> Result<MailConfiguration> {
    if provider == "jmap" {
        return jmap_suggestion(email);
    }
    let discovered = discover_imap_with_provider(email, provider);
    if provider != "auto" {
        return discovered;
    }
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(3)))
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .build(),
    );
    let hint = jmap_hint(
        &agent,
        email,
        discovered.as_ref().ok().map(|c| c.imap.host.as_str()),
    );
    match (discovered, hint) {
        (Ok(mut config), host) => {
            config.jmap_host = host;
            Ok(config)
        }
        (Err(_), Some(host)) => {
            // JMAP-only servers still deserve a reviewable account proposal.
            // Outgoing SMTP is a hint to be verified, never silently tested.
            let mut proposal = jmap_suggestion(email)?;
            proposal.jmap_host = Some(host);
            Ok(proposal)
        }
        (Err(error), None) => Err(error),
    }
}

fn discover_imap_with_provider(email: &str, provider: &str) -> Result<MailConfiguration> {
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
    match discover_pacc(&agent, email) {
        Ok(configuration) => return Ok(configuration),
        Err(error) => failures.push(format!("PACC: {error:#}")),
    }
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
            Ok(config) => return Ok(try_first_party_oauth(&agent, config)),
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
            Ok(config) => return Ok(try_first_party_oauth(&agent, config)),
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
    // RFC 8314 _imaps and _submissions identify TLS-wrapped endpoints;
    // RFC 6186 _submission uses STARTTLS. All results remain editable hints.
    let imaps = dns_answers(&agent, &format!("_imaps._tcp.{domain}"), 33)
        .ok()
        .and_then(|answers| srv_endpoint(&answers));
    let imap_starttls = if imaps.is_none() {
        dns_answers(&agent, &format!("_imap._tcp.{domain}"), 33)
            .ok()
            .and_then(|answers| srv_endpoint(&answers))
    } else {
        None
    };
    let submissions = dns_answers(&agent, &format!("_submissions._tcp.{domain}"), 33)
        .ok()
        .and_then(|answers| srv_endpoint(&answers));
    let submission = if submissions.is_none() {
        dns_answers(&agent, &format!("_submission._tcp.{domain}"), 33)
            .ok()
            .and_then(|answers| srv_endpoint(&answers))
    } else {
        None
    };
    if let Some((imap, imap_tls)) = imaps
        .map(|endpoint| (endpoint, SocketType::Ssl))
        .or_else(|| imap_starttls.map(|endpoint| (endpoint, SocketType::StartTls)))
    {
        if let Some(smtp) = submissions {
            return Ok(try_first_party_oauth(
                &agent,
                srv_suggestion(email, imap, imap_tls, smtp, SocketType::Ssl),
            ));
        }
        if let Some(smtp) = submission {
            return Ok(try_first_party_oauth(
                &agent,
                srv_suggestion(email, imap, imap_tls, smtp, SocketType::StartTls),
            ));
        }
    }
    if let Some(suggested) = self_hosted_mx_suggestion(email, &mxes) {
        return Ok(try_first_party_oauth(&agent, suggested));
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

    const PACC_OAUTH: &str = r#"{
        "protocols": {
          "imap": {"host": "mail.fivejazz.com"},
          "submit": {"host": "mail.fivejazz.com"},
          "jmap": {"url": "https://mail.fivejazz.com/jmap/session"}
        },
        "authentication": {"oauth-public": {"issuer": "https://mail.fivejazz.com"}, "password": true},
        "info": {"provider": {"name": "FiveJazz Mail"}}
    }"#;

    const VALID_OAUTH_METADATA: &str = r#"{
        "issuer": "https://mail.fivejazz.com",
        "authorization_endpoint": "https://mail.fivejazz.com/auth/authorize",
        "token_endpoint": "https://mail.fivejazz.com/auth/token",
        "registration_endpoint": "https://mail.fivejazz.com/auth/register",
        "code_challenge_methods_supported": ["S256"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "scopes_supported": ["urn:ietf:params:oauth:scope:mail", "openid"]
    }"#;

    #[test]
    fn pacc_selects_oauth_and_preserves_non_google_issuer() {
        let config = super::parse_pacc("hi@fivejazz.com", PACC_OAUTH).unwrap();
        assert_eq!(config.imap.auth, "OAuth2");
        assert_eq!(config.smtp.auth, "OAuth2");
        assert_eq!(config.imap.port, 993);
        assert_eq!(config.smtp.port, 465);
        assert_eq!(config.provider, "FiveJazz Mail");
        assert_eq!(
            config.oauth_issuer.as_deref(),
            Some("https://mail.fivejazz.com")
        );
        assert!(config.to_toml().contains("provider = \"open\""));
        assert!(
            config
                .to_toml()
                .contains("issuer = \"https://mail.fivejazz.com\"")
        );
        let parsed: crate::config::Config = toml::from_str(&config.to_toml()).unwrap();
        assert_eq!(parsed.accounts[0].oauth.as_ref().unwrap().provider, "open");
    }

    #[test]
    fn pacc_allows_password_only_servers() {
        let input = PACC_OAUTH.replace(
            r#""oauth-public": {"issuer": "https://mail.fivejazz.com"}, "password": true"#,
            r#""password": true"#,
        );
        let config = super::parse_pacc("hi@fivejazz.com", &input).unwrap();
        assert_eq!(config.oauth_issuer, None);
        assert_eq!(config.imap.auth, "password-cleartext");
        assert!(!config.to_toml().contains("[accounts.oauth]"));
    }

    #[test]
    fn pacc_rejects_no_supported_auth_and_missing_hosts() {
        let no_auth = PACC_OAUTH.replace(
            r#""oauth-public": {"issuer": "https://mail.fivejazz.com"}, "password": true"#,
            r#""password": false"#,
        );
        assert!(super::parse_pacc("hi@fivejazz.com", &no_auth).is_err());
        let no_smtp = PACC_OAUTH.replace(
            r#""submit": {"host": "mail.fivejazz.com"}"#,
            r#""submit": {}"#,
        );
        assert!(super::parse_pacc("hi@fivejazz.com", &no_smtp).is_err());
        let invalid_host = PACC_OAUTH.replace("mail.fivejazz.com\"},", "evil.test:25\"},");
        assert!(super::parse_pacc("hi@fivejazz.com", &invalid_host).is_err());
    }

    #[test]
    fn pacc_rejects_unencrypted_or_malformed_issuer() {
        let unencrypted = PACC_OAUTH.replace(
            "https://mail.fivejazz.com\"}, \"password",
            "http://mail.fivejazz.com\"}, \"password",
        );
        assert!(super::parse_pacc("hi@fivejazz.com", &unencrypted).is_err());
        assert!(super::parse_pacc("missing-at", PACC_OAUTH).is_err());
        assert!(super::parse_pacc("hi@fivejazz.com", "not json").is_err());
    }

    #[test]
    fn dns_allows_only_expected_srv_and_pacc_labels() {
        assert!(super::valid_dns_query_name(
            "_ua-auto-config.fivejazz.com",
            16
        ));
        assert!(super::valid_dns_query_name("_imaps._tcp.fivejazz.com", 33));
        assert!(super::valid_dns_query_name(
            "_submissions._tcp.fivejazz.com",
            33
        ));
        assert!(super::valid_dns_query_name("_imap._tcp.fivejazz.com", 33));
        assert!(super::valid_dns_query_name(
            "_submission._tcp.fivejazz.com",
            33
        ));
        assert!(super::valid_dns_query_name("fivejazz.com", 15));
        assert!(super::valid_dns_query_name("autodiscover.fivejazz.com", 5));

        // Restrict the allowed record types and service labels.
        assert!(!super::valid_dns_query_name(
            "_ua-auto-config.fivejazz.com",
            15
        ));
        assert!(!super::valid_dns_query_name("fivejazz.com", 16));
        assert!(!super::valid_dns_query_name("fivejazz.com", 33));
        assert!(!super::valid_dns_query_name(
            "_something._tcp.fivejazz.com",
            33
        ));
        assert!(!super::valid_dns_query_name("_imaps._tcp.fivejazz.com", 15));
        assert!(!super::valid_dns_query_name("_imaps._tcp.fivejazz.com", 99));

        // Do not ask the public DNS resolver to discover single-label hosts.
        assert!(!super::valid_dns_query_name("_imaps._tcp.localhost", 33));
        assert!(!super::valid_dns_query_name(
            "_ua-auto-config.localhost",
            16
        ));
        assert!(!super::valid_dns_query_name("localhost", 15));
        assert!(!super::valid_dns_query_name("autodiscover.localhost", 5));
        assert!(!super::valid_dns_query_name("_imaps._tcp.bad..example", 33));
    }

    #[test]
    fn pacc_digest_uses_decoded_bytes_and_requires_valid_txt() {
        use base64::Engine as _;
        let expected = base64::engine::general_purpose::STANDARD
            .encode(ring::digest::digest(&ring::digest::SHA256, PACC_OAUTH.as_bytes()).as_ref());
        let correct = format!("\"v=UAAC1; a=sha256; d={expected}\"");
        assert!(super::pacc_digest_matches(
            PACC_OAUTH.as_bytes(),
            std::slice::from_ref(&correct)
        ));
        assert!(!super::pacc_digest_matches(
            b"changed",
            std::slice::from_ref(&correct)
        ));
        assert!(!super::pacc_digest_matches(PACC_OAUTH.as_bytes(), &[]));
        assert!(!super::pacc_digest_matches(
            PACC_OAUTH.as_bytes(),
            &[correct.replace("UAAC1", "UAAC2")]
        ));
        assert!(!super::pacc_digest_matches(
            PACC_OAUTH.as_bytes(),
            &[correct.replace("sha256", "sha1")]
        ));
    }

    #[test]
    fn oauth_metadata_requires_exact_issuer_and_registration() {
        assert!(
            super::validate_public_oauth_metadata(
                VALID_OAUTH_METADATA,
                "https://mail.fivejazz.com"
            )
            .is_ok()
        );
        assert!(
            super::validate_public_oauth_metadata(
                VALID_OAUTH_METADATA,
                "https://other.example.com"
            )
            .is_err()
        );
        assert!(super::validate_public_oauth_metadata("{}", "https://mail.fivejazz.com").is_err());
        assert!(
            super::validate_public_oauth_metadata(
                &VALID_OAUTH_METADATA.replace(
                    "https://mail.fivejazz.com/auth/register",
                    "http://mail.fivejazz.com/auth/register"
                ),
                "https://mail.fivejazz.com"
            )
            .is_err()
        );
    }

    #[test]
    fn oauth_metadata_rejects_missing_pkce_mail_scope_or_refresh() {
        for changed in [
            VALID_OAUTH_METADATA.replace("\"S256\"", "\"plain\""),
            VALID_OAUTH_METADATA.replace("urn:ietf:params:oauth:scope:mail", "openid"),
            VALID_OAUTH_METADATA.replace("\"refresh_token\"", "\"client_credentials\""),
        ] {
            assert!(
                super::validate_public_oauth_metadata(&changed, "https://mail.fivejazz.com")
                    .is_err()
            );
        }
    }

    #[test]
    fn first_party_oauth_is_only_probed_for_matching_mail_hosts() {
        let mut config = super::parse_pacc("hi@fivejazz.com", PACC_OAUTH).unwrap();
        config.oauth_issuer = None;
        assert_eq!(
            super::first_party_oauth_candidate(&config).as_deref(),
            Some("https://mail.fivejazz.com")
        );
        config.smtp.host = "smtp.other.com".into();
        assert_eq!(super::first_party_oauth_candidate(&config), None);
        config.smtp.host = "mail.fivejazz.com".into();
        config.smtp.auth = "none".into();
        assert_eq!(super::first_party_oauth_candidate(&config), None);
    }

    #[test]
    fn legacy_mozilla_xml_does_not_advertise_oauth_without_extra_evidence() {
        let xml = r#"<clientConfig><emailProvider id="fivejazz.com"><domain>fivejazz.com</domain>
            <incomingServer type="imap"><hostname>mail.fivejazz.com</hostname><port>993</port>
              <socketType>SSL</socketType><authentication>password-cleartext</authentication></incomingServer>
            <outgoingServer type="smtp"><hostname>mail.fivejazz.com</hostname><port>465</port>
              <socketType>SSL</socketType><authentication>password-cleartext</authentication></outgoingServer>
            </emailProvider></clientConfig>"#;
        let config = super::parse_autoconfig("hi@fivejazz.com", xml, "test").unwrap();
        assert_eq!(config.imap.auth, "password-cleartext");
        assert!(config.oauth_issuer.is_none());
    }

    #[test]
    fn imap_starttls_only_xml_is_now_accepted() {
        let xml = r#"<clientConfig version="1.1"><emailProvider id="example.com">
            <domain>example.com</domain>
            <incomingServer type="imap"><hostname>imap.example.com</hostname>
                <port>143</port><socketType>STARTTLS</socketType>
                <authentication>password-cleartext</authentication><username>%EMAILADDRESS%</username></incomingServer>
            <outgoingServer type="smtp"><hostname>smtp.example.com</hostname>
                <port>587</port><socketType>STARTTLS</socketType>
                <authentication>none</authentication><username>%EMAILADDRESS%</username></outgoingServer>
            </emailProvider></clientConfig>"#;
        let proposal = parse_autoconfig("user@example.com", xml, "fixture").unwrap();
        assert!(!proposal.imap_implicit_tls());
        assert!(!proposal.smtp_implicit_tls());
        assert_eq!(proposal.smtp.auth, "none");
        let text = proposal.to_toml();
        assert!(text.contains("source = \"imap+starttls\""));
        assert!(text.contains("outgoing = \"smtp+none\""));
    }

    #[test]
    fn noauth_imap_is_not_accepted_as_normal_mailbox() {
        let xml = r#"<clientConfig version="1.1"><emailProvider id="example.com">
            <domain>example.com</domain>
            <incomingServer type="imap"><hostname>imap.example.com</hostname>
                <port>143</port><socketType>STARTTLS</socketType>
                <authentication>none</authentication></incomingServer>
            <outgoingServer type="smtp"><hostname>smtp.example.com</hostname>
                <port>587</port><socketType>STARTTLS</socketType>
                <authentication>none</authentication></outgoingServer>
            </emailProvider></clientConfig>"#;
        assert!(parse_autoconfig("user@example.com", xml, "fixture").is_err());
    }

    #[test]
    fn unencrypted_xml_endpoints_remain_rejected() {
        let xml = r#"<clientConfig version="1.1"><emailProvider id="example.com">
            <domain>example.com</domain>
            <incomingServer type="imap"><hostname>imap.example.com</hostname>
                <port>143</port><socketType>plain</socketType>
                <authentication>password-cleartext</authentication></incomingServer>
            <outgoingServer type="smtp"><hostname>smtp.example.com</hostname>
                <port>25</port><socketType>plain</socketType>
                <authentication>none</authentication></outgoingServer>
            </emailProvider></clientConfig>"#;
        assert!(parse_autoconfig("user@example.com", xml, "fixture").is_err());
    }
    #[test]
    fn srv_parsing_ignores_invalid_and_prioritizes_valid_endpoints() {
        use super::srv_endpoint;
        let rows = vec![
            "x y 993 mail.example.org.".to_string(),
            "0 0 0 bad.example.org.".to_string(),
            "20 0 993 second.example.org.".to_string(),
            "10 0 1993 imap.example.org.".to_string(),
            "1 0 993 hostile@example.org.".to_string(),
        ];
        assert_eq!(srv_endpoint(&rows), Some(("imap.example.org".into(), 1993)));
        assert_eq!(srv_endpoint(&["0 0 0 .".into()]), None);
    }

    #[test]
    fn mx_suggestion_requires_same_domain_mail_hostname() {
        use super::self_hosted_mx_suggestion;
        let ok =
            self_hosted_mx_suggestion("hi@fivejazz.com", &["mail.fivejazz.com".into()]).unwrap();
        assert_eq!(ok.imap.port, 993);
        assert_eq!(ok.smtp.port, 465);
        assert_eq!(ok.smtp.auth, "password-cleartext");
        assert!(ok.to_toml().contains("imap_host = \"mail.fivejazz.com\""));
        assert!(self_hosted_mx_suggestion("hi@fivejazz.com", &["evil.com".into()]).is_none());
        assert!(self_hosted_mx_suggestion("invalid", &["mail.fivejazz.com".into()]).is_none());
    }

    #[test]
    fn dns_srv_suggestion_supports_submission_starttls_and_implicit() {
        use super::{SocketType, srv_suggestion};
        let input = ("imap.example.org".to_owned(), 993);
        let smtp = ("smtp.example.org".to_owned(), 587);
        let starttls = srv_suggestion(
            "hi@example.org",
            input.clone(),
            SocketType::Ssl,
            smtp.clone(),
            SocketType::StartTls,
        );
        assert!(!starttls.smtp_implicit_tls());
        assert!(starttls.to_toml().contains("outgoing = \"starttls\""));
        let tls = srv_suggestion(
            "hi@example.org",
            input,
            SocketType::Ssl,
            smtp,
            SocketType::Ssl,
        );
        assert!(tls.smtp_implicit_tls());
        assert!(tls.to_toml().contains("outgoing = \"smtps\""));
    }

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
    #[test]
    fn explicit_jmap_discovery_produces_credential_free_source() {
        let suggestion = super::jmap_suggestion("hi@fivejazz.com").unwrap();
        assert_eq!(suggestion.jmap_host.as_deref(), Some("mail.fivejazz.com"));
        let text = suggestion.to_toml();
        assert!(text.contains("source = \"jmaps://hi%40fivejazz.com@mail.fivejazz.com"));
        assert!(!text.contains("secret"));
        let parsed: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.accounts.len(), 1);
        assert!(crate::backend::JmapSource::supports(
            &parsed.accounts[0].source
        ));
    }
}
