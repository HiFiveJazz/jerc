//! Standards-based OAuth public-client discovery and registration.
//!
//! Self-hosted servers advertise RFC 8414 metadata; Stalwart supports
//! unauthenticated RFC 7591 registration with a loopback redirect and PKCE.
//! This module does not guess identities from DNS or send mailbox credentials.
use super::{HTTP_TIMEOUT, OAuthSettings};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use url::Url;

pub(super) const MAIL_SCOPE: &str = "urn:ietf:params:oauth:scope:mail";
const PUBLIC_REDIRECT: &str = "http://127.0.0.1/oauth/callback";

/// An issuer explicitly chosen by the user or derived from an edited IMAP
/// hostname. Reject credentials, fragments, and non-HTTPS endpoints.
pub fn validate_open_issuer(issuer: &str) -> Result<()> {
    let url = Url::parse(issuer).context("OAuth issuer must be an HTTPS URL")?;
    ensure!(
        url.scheme() == "https" && url.host_str().is_some(),
        "OAuth issuer must use HTTPS"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "OAuth issuer cannot contain credentials"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "OAuth issuer cannot contain a query or fragment"
    );
    ensure!(url.as_str().len() <= 2048, "OAuth issuer URL is too long");
    Ok(())
}

fn valid_endpoint(value: &str) -> Result<()> {
    let url = Url::parse(value).context("OAuth metadata contained an invalid endpoint URL")?;
    ensure!(
        url.scheme() == "https" && url.host_str().is_some(),
        "OAuth endpoints must use HTTPS"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "OAuth endpoint URL contains prohibited credentials or fragment"
    );
    ensure!(value.len() <= 2048, "OAuth endpoint URL is too long");
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
    #[serde(default)]
    code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    grant_types_supported: Option<Vec<String>>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ClientRecord {
    issuer: String,
    client_id: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    requested_scope: String,
    #[serde(default)]
    require_issuer_response: bool,
}

#[derive(Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

pub(super) fn valid_client_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 4096 && !id.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn parse_metadata(data: &str, expected_issuer: &str, scope: &str) -> Result<Metadata> {
    let metadata: Metadata =
        serde_json::from_str(data).context("decoding OAuth authorization-server metadata")?;
    // RFC 8414 issuer comparison is exact. Accept a single root slash only
    // as a user-facing input convenience, never a different host or path.
    fn normalize(s: &str) -> &str {
        s.trim_end_matches('/')
    }
    ensure!(
        normalize(&metadata.issuer) == normalize(expected_issuer),
        "OAuth metadata issuer does not match configured issuer"
    );
    validate_open_issuer(&metadata.issuer)?;
    for url in [&metadata.authorization_endpoint, &metadata.token_endpoint] {
        valid_endpoint(url)?;
    }
    if let Some(url) = metadata.registration_endpoint.as_deref() {
        valid_endpoint(url)?;
    }
    if let Some(methods) = metadata.code_challenge_methods_supported.as_ref() {
        ensure!(
            methods.iter().any(|m| m == "S256"),
            "OAuth provider does not advertise PKCE S256"
        );
    }
    if let Some(grants) = metadata.grant_types_supported.as_ref() {
        ensure!(
            grants.iter().any(|g| g == "authorization_code"),
            "OAuth provider does not support authorization-code login"
        );
        ensure!(
            grants.iter().any(|g| g == "refresh_token"),
            "OAuth provider does not support token refresh"
        );
    }
    if let Some(scopes) = metadata.scopes_supported.as_ref() {
        ensure!(
            scope
                .split_whitespace()
                .all(|requested| scopes.iter().any(|s| s == requested)
                    || requested == "offline_access"),
            "OAuth provider does not advertise the requested mail access scopes"
        );
    }
    Ok(metadata)
}

fn metadata_urls(issuer: &str) -> Result<Vec<String>> {
    validate_open_issuer(issuer)?;
    let url = Url::parse(issuer)?;
    let origin = url.origin().ascii_serialization();
    let suffix = url.path().trim_end_matches('/');
    let mut candidates = vec![format!(
        "{origin}/.well-known/oauth-authorization-server{suffix}"
    )];
    // Some mail servers (including Mailu) advertise an issuer with a path,
    // while serving OAuth authorization-server metadata at the origin root.
    let root = format!("{origin}/.well-known/oauth-authorization-server");
    if candidates[0] != root {
        candidates.push(root);
    }
    candidates.push(format!("{origin}{suffix}/.well-known/openid-configuration"));
    Ok(candidates)
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .https_only(true)
            .http_status_as_error(false)
            .build(),
    )
}

fn discover(issuer: &str, scope: &str) -> Result<Metadata> {
    let agent = http_agent();
    for endpoint in metadata_urls(issuer)? {
        let Ok(mut response) = agent.get(&endpoint).call() else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let value: serde_json::Value = response
            .body_mut()
            .read_json()
            .context("decoding OAuth metadata response")?;
        return parse_metadata(&value.to_string(), issuer, scope);
    }
    bail!(
        "No OAuth authorization-server metadata found for the configured HTTPS issuer; use an app password or configure the correct OAuth issuer"
    )
}

fn parse_registration(data: &str) -> Result<String> {
    let reply: RegistrationResponse =
        serde_json::from_str(data).context("OAuth registration response was malformed")?;
    ensure!(
        valid_client_id(&reply.client_id),
        "OAuth registration returned an invalid client ID"
    );
    ensure!(
        reply
            .token_endpoint_auth_method
            .as_deref()
            .is_none_or(|m| m == "none"),
        "OAuth server registered a confidential client; Jerc requires a public client"
    );
    Ok(reply.client_id)
}

fn register(metadata: &Metadata, scope: &str) -> Result<String> {
    let endpoint = metadata.registration_endpoint.as_ref().ok_or_else(|| anyhow!(
        "OAuth provider does not support public-client registration; configure your own client_id or use an app password"
    ))?;
    let body = serde_json::json!({
        "client_name": "Jerc",
        "redirect_uris": [PUBLIC_REDIRECT],
        "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "scope": scope
    });
    let mut response = http_agent()
        .post(endpoint)
        .send_json(body)
        .map_err(|_| anyhow!("OAuth public-client registration failed (network or TLS error)"))?;
    ensure!(
        response.status().is_success(),
        "OAuth server rejected public-client registration (HTTP {})",
        response.status()
    );
    let payload: serde_json::Value = response
        .body_mut()
        .read_json()
        .context("OAuth registration response was malformed")?;
    parse_registration(&payload.to_string())
}

fn record_path(settings: &OAuthSettings) -> Result<PathBuf> {
    let token = settings.token_file()?;
    Ok(token.with_extension("public-client.json"))
}

fn load_record(settings: &OAuthSettings) -> Result<Option<ClientRecord>> {
    let path = record_path(settings)?;
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        meta.file_type().is_file(),
        "OAuth registration path must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "OAuth registration file permissions must be private"
        );
    }
    let record: ClientRecord = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(
        valid_client_id(&record.client_id),
        "Cached OAuth client ID is invalid"
    );
    validate_open_issuer(&record.issuer)?;
    valid_endpoint(&record.authorization_endpoint)?;
    valid_endpoint(&record.token_endpoint)?;
    Ok(Some(record))
}

fn save_record(settings: &OAuthSettings, record: &ClientRecord) -> Result<()> {
    let path = record_path(settings)?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("invalid OAuth registration file path"))?;
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(&mut temp, record)?;
    temp.flush()?;
    temp.persist(&path)
        .map_err(|e| anyhow!("saving OAuth public-client registration: {}", e.error))?;
    Ok(())
}

fn with_record(mut settings: OAuthSettings, record: &ClientRecord) -> OAuthSettings {
    settings.client_id.clone_from(&record.client_id);
    settings
        .auth_endpoint
        .clone_from(&record.authorization_endpoint);
    settings.token_endpoint.clone_from(&record.token_endpoint);
    settings.issuer = Some(record.issuer.clone());
    settings.require_issuer_response = record.require_issuer_response;
    settings
}

/// Only read local state during account configuration validation or startup.
/// Registration and network discovery are explicitly deferred to Login.
pub(super) fn load_cached(settings: OAuthSettings) -> Result<OAuthSettings> {
    let Some(record) = load_record(&settings)? else {
        return Ok(settings);
    };
    let expected = settings
        .issuer
        .as_deref()
        .ok_or_else(|| anyhow!("OAuth issuer missing"))?;
    if record.issuer.trim_end_matches('/') != expected.trim_end_matches('/')
        || record.requested_scope != settings.scope
        || (!settings.client_id.is_empty() && settings.client_id != record.client_id)
    {
        return Ok(settings);
    }
    Ok(with_record(settings, &record))
}

pub(super) fn resolve_for_login(settings: &OAuthSettings) -> Result<OAuthSettings> {
    if !settings.client_id.is_empty()
        && !settings.auth_endpoint.is_empty()
        && !settings.token_endpoint.is_empty()
    {
        return Ok(settings.clone());
    }
    let issuer = settings
        .issuer
        .as_deref()
        .ok_or_else(|| anyhow!("OAuth issuer missing"))?;
    let metadata = discover(issuer, &settings.scope)?;
    let client_id = if settings.client_id.is_empty() {
        register(&metadata, &settings.scope)?
    } else {
        settings.client_id.clone()
    };
    let record = ClientRecord {
        issuer: metadata.issuer,
        client_id,
        authorization_endpoint: metadata.authorization_endpoint,
        token_endpoint: metadata.token_endpoint,
        requested_scope: settings.scope.clone(),
        require_issuer_response: metadata.authorization_response_iss_parameter_supported,
    };
    save_record(settings, &record)?;
    Ok(with_record(settings.clone(), &record))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_https_credentials_and_fragments() {
        for issuer in [
            "http://mail.example.org",
            "https://u:p@mail.example.org",
            "https://mail.example.org/a#frag",
            "https://mail.example.org/?q=1",
        ] {
            assert!(validate_open_issuer(issuer).is_err(), "{issuer}");
        }
        assert!(validate_open_issuer("https://mail.example.org").is_ok());
        assert!(validate_open_issuer("https://auth.example.org/realms/mail").is_ok());
    }
    #[test]
    fn rfc8414_metadata_paths_for_root_and_path_issuers() {
        let url = metadata_urls("https://mail.example.org").unwrap();
        assert_eq!(
            url[0],
            "https://mail.example.org/.well-known/oauth-authorization-server"
        );
        let url = metadata_urls("https://auth.example.org/realms/mail").unwrap();
        assert_eq!(
            url[0],
            "https://auth.example.org/.well-known/oauth-authorization-server/realms/mail"
        );
        assert_eq!(
            url[1],
            "https://auth.example.org/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            url[2],
            "https://auth.example.org/realms/mail/.well-known/openid-configuration"
        );
    }
    #[test]
    fn mailu_path_issuer_can_use_origin_root_metadata() {
        let urls = metadata_urls("https://mx.example.org/admin").unwrap();
        assert_eq!(
            urls[0],
            "https://mx.example.org/.well-known/oauth-authorization-server/admin"
        );
        assert_eq!(
            urls[1],
            "https://mx.example.org/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            urls[2],
            "https://mx.example.org/admin/.well-known/openid-configuration"
        );
    }
    #[test]
    fn metadata_must_match_issuer_and_require_pkce_mail_scope() {
        let json = serde_json::json!({
            "issuer": "https://mail.example.org", "authorization_endpoint": "https://mail.example.org/login",
            "token_endpoint": "https://mail.example.org/auth/token",
            "registration_endpoint": "https://mail.example.org/auth/register",
            "authorization_response_iss_parameter_supported": true,
            "code_challenge_methods_supported": ["S256"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "scopes_supported": [MAIL_SCOPE]
        }).to_string();
        assert!(parse_metadata(&json, "https://mail.example.org", MAIL_SCOPE).is_ok());
        assert!(parse_metadata(&json, "https://evil.example.org", MAIL_SCOPE).is_err());
        assert!(
            parse_metadata(
                &json.replace("S256", "plain"),
                "https://mail.example.org",
                MAIL_SCOPE
            )
            .is_err()
        );
        assert!(
            parse_metadata(
                &json.replace(MAIL_SCOPE, "openid"),
                "https://mail.example.org",
                MAIL_SCOPE
            )
            .is_err()
        );
        assert!(
            parse_metadata(
                &json.replace(
                    "https://mail.example.org/auth/token",
                    "http://mail.example.org/auth/token"
                ),
                "https://mail.example.org",
                MAIL_SCOPE
            )
            .is_err()
        );
    }
    #[test]
    fn registration_response_accepts_public_and_rejects_confidential_clients() {
        let valid = r#"{"client_id":"swc1.abc-123","token_endpoint_auth_method":"none"}"#;
        assert_eq!(parse_registration(valid).unwrap(), "swc1.abc-123");
        assert!(parse_registration(r#"{"client_id":""}"#).is_err());
        assert!(parse_registration(r#"{"client_id":"id with spaces"}"#).is_err());
        assert!(
            parse_registration(
                r#"{"client_id":"owned","token_endpoint_auth_method":"client_secret_basic"}"#
            )
            .is_err()
        );
    }
    #[test]
    fn metadata_scope_override_can_be_provider_specific() {
        let metadata = serde_json::json!({
            "issuer":"https://auth.example.org",
            "authorization_endpoint":"https://auth.example.org/authorize",
            "token_endpoint":"https://auth.example.org/token",
            "scopes_supported":["email","offline_access"]
        })
        .to_string();
        assert!(
            parse_metadata(
                &metadata,
                "https://auth.example.org",
                "email offline_access"
            )
            .is_ok()
        );
        assert!(parse_metadata(&metadata, "https://auth.example.org", MAIL_SCOPE).is_err());
    }
    #[test]
    fn metadata_rejects_insecure_registration_and_authorize_urls() {
        let basic = serde_json::json!({
            "issuer":"https://auth.example.org",
            "authorization_endpoint":"https://auth.example.org/authorize",
            "token_endpoint":"https://auth.example.org/token",
            "registration_endpoint":"http://auth.example.org/register"
        })
        .to_string();
        assert!(parse_metadata(&basic, "https://auth.example.org", MAIL_SCOPE).is_err());
        let hijack = basic.replace(
            "http://auth.example.org/register",
            "https://user:password@auth.example.org/register",
        );
        assert!(parse_metadata(&hijack, "https://auth.example.org", MAIL_SCOPE).is_err());
    }
    #[test]
    fn issuer_path_does_not_change_mail_scope() {
        assert!(valid_client_id("swc1.example"));
        assert!(!valid_client_id("  client id"));
        assert!(!valid_client_id("\n"));
    }
}
