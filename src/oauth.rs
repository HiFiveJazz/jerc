//! Native desktop OAuth authorization-code + PKCE sign-in for IMAP/SMTP.
//! The application publisher must register public-client IDs with providers;
//! end users authorize in a browser and never handle refresh tokens manually.
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{digest, rand::{SecureRandom, SystemRandom}};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::{AccountConfig, OAuthConfig};

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const MICROSOFT_NATIVE_REDIRECT: &str = "https://login.microsoftonline.com/common/oauth2/nativeclient";

/// Application-publisher defaults embedded at compile time. The user may
/// override them per-account, but ordinary users should not need to register
/// OAuth clients when using an appropriately registered app build.
/// Register your own IDs with each provider; don't copy another app's identity.
#[must_use]
pub fn embedded_client_id(provider: &str) -> Option<&'static str> {
    match provider {
        "google" => option_env!("AERC_RS_GOOGLE_CLIENT_ID"),
        "microsoft" => option_env!("AERC_RS_MICROSOFT_CLIENT_ID"),
        _ => None,
    }.filter(|id| !id.trim().is_empty())
}

#[must_use]
pub fn embedded_google_client_secret() -> Option<&'static str> {
    option_env!("AERC_RS_GOOGLE_CLIENT_SECRET").filter(|secret| !secret.is_empty())
}

/// Google Desktop application OAuth client IDs include this suffix verbatim.
/// The local-part is checked only for safe syntax; this does not verify that
/// the client is registered or permitted to use this redirect URI.
#[must_use]
pub fn valid_google_desktop_client_id(id: &str) -> bool {
    let Some(prefix) = id.strip_suffix(".apps.googleusercontent.com") else { return false; };
    !prefix.is_empty() && prefix.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginFlow {
    Browser,
    NativeClient,
    DeviceCode,
}

impl LoginFlow {
    fn parse(value: Option<&str>, provider: &str) -> Result<Self> {
        let flow = match value.unwrap_or("browser").trim().to_ascii_lowercase().as_str() {
            "browser" | "localhost" => Self::Browser,
            "nativeclient" | "authcode" => Self::NativeClient,
            "devicecode" => Self::DeviceCode,
            _ => bail!("unsupported OAuth login flow; use browser, nativeclient, or devicecode"),
        };
        if provider != "microsoft" && flow != Self::Browser {
            bail!("OAuth nativeclient/devicecode flows are currently supported for Microsoft only");
        }
        Ok(flow)
    }
}

#[derive(Clone)]
pub struct OAuthSettings {
    pub provider: String,
    pub client_id: String,
    client_secret: Option<String>,
    pub token_endpoint: String,
    pub scope: String,
    auth_endpoint: String,
    account_name: String,
    flow: LoginFlow,
    callback_host: String,
    callback_path: String,
    prompt: Option<String>,
    login_hint: Option<String>,
    credential_namespace: Option<String>,
}

impl std::fmt::Debug for OAuthSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthSettings")
            .field("provider", &self.provider)
            .field("account_name", &self.account_name)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct SavedToken {
    provider: String,
    client_id: String,
    refresh_token: String,
}

// Avoid depending on the access token's internal format: Microsoft service
// access tokens can be opaque or encrypted. Inspect only response metadata.
fn protocol_scope_flags(scope: &str) -> (bool, bool) {
    let has = |wanted: &str| scope.split_whitespace().any(|item| {
        item.rsplit('/').next().is_some_and(|scope| scope.eq_ignore_ascii_case(wanted))
    });
    (has("IMAP.AccessAsUser.All"), has("SMTP.Send"))
}


/// A deliberately narrow, read-only summary of claims in the *actual* access
/// token used for SMTP. Do not print access tokens, account identifiers, or
/// arbitrary JWT payload values. JWT payloads here are NOT signature-verified:
/// this is diagnostic metadata, not authorization logic. Some Microsoft tokens
/// are encrypted/opaque; callers must treat those as inconclusive.
#[must_use]
pub fn safe_smtp_token_diagnostics(token: &str, client_id: &str, smtp_username: &str) -> String {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 || token.len() > 131_072 {
        return "Access token claims: opaque or encrypted; cannot inspect (inconclusive)".into();
    }
    let Ok(payload) = URL_SAFE_NO_PAD.decode(parts[1]) else {
        return "Access token claims: not a readable JWT (inconclusive)".into();
    };
    if payload.len() > 65_536 {
        return "Access token claims: payload too large (inconclusive)".into();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&payload) else {
        return "Access token claims: not JSON (inconclusive)".into();
    };
    let field = |key| value.get(key).and_then(serde_json::Value::as_str);
    let audience = match field("aud") {
        Some("https://outlook.office.com" | "https://outlook.office365.com" |
             "00000002-0000-0ff1-ce00-000000000000") => "Exchange Online",
        Some(_) => "other (verify token audience)",
        None => "missing",
    };
    let scope = field("scp");
    let scoped = |permission: &str| scope.is_some_and(|s| s.split_whitespace()
        .any(|candidate| candidate.eq_ignore_ascii_case(permission)));
    let flag = |bool_value| if bool_value { "yes" } else { "no" };
    let client_claim = field("azp").or_else(|| field("appid"));
    let client_match = match client_claim {
        Some(claim) => flag(claim.eq_ignore_ascii_case(client_id)),
        None => "unavailable",
    };
    let principal = field("preferred_username").or_else(|| field("upn"))
        .or_else(|| field("unique_name"));
    let principal_match = match principal {
        Some(username) => flag(username.eq_ignore_ascii_case(smtp_username)),
        None => "unavailable",
    };
    format!(
        "Access token JWT claims (unverified): aud={audience}; scp.IMAP={}; scp.SMTP.Send={}; client_id_matches={client_match}; login_matches_SMTP={principal_match}; tenant_present={}",
        if scope.is_some() { flag(scoped("IMAP.AccessAsUser.All")) } else { "unavailable" },
        if scope.is_some() { flag(scoped("SMTP.Send")) } else { "unavailable" },
        flag(field("tid").is_some()),
    )
}

/// Extract a *suggested* SMTP/IMAP login from a Microsoft access token.
/// This is strictly a user-facing account-setup hint, never proof of identity
/// or authorization: JWT claims are not signature-verified here, and Microsoft
/// may issue opaque tokens. Do not cache, trust, or auto-apply this value.
#[must_use]
pub fn microsoft_login_username_hint(token: &str) -> Option<String> {
    if token.len() > 131_072 { return None; }
    let mut parts = token.split('.');
    let (_, Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else { return None; };
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    if payload.len() > 65_536 { return None; }
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    for key in ["preferred_username", "upn", "unique_name"] {
        let Some(value) = claims.get(key).and_then(serde_json::Value::as_str) else { continue; };
        // A display name, GUID, unsafe value, or truncated claim is not a
        // useful login suggestion. User-provided UPNs are validated separately.
        if value.len() > 254 || value.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
            continue;
        }
        if let Some((local, domain)) = value.rsplit_once('@') {
            if !local.is_empty() && !local.contains('@') && domain.contains('.')
                && !domain.starts_with('.') && !domain.ends_with('.')
                && domain.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
            {
                return Some(value.to_owned());
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    refresh_token: Option<String>,
    /// Scopes in the token response, if the identity provider reports them.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct DevicePollReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OAuthFailure {
    error: Option<String>,
    #[serde(default)]
    error_codes: Vec<i64>,
}

fn safe_oauth_failure(status: u16, failure: &OAuthFailure) -> String {
    let mut message = format!("OAuth token endpoint rejected request (HTTP {status})");
    if let Some(error) = failure.error.as_deref().filter(|error| error.len() <= 64 &&
        error.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')) {
        message.push_str(&format!(": {error}"));
    }
    if let Some(code) = failure.error_codes.first() {
        message.push_str(&format!(" (AADSTS{code})"));
    }
    message
}

impl OAuthSettings {
    pub fn for_account(account: &AccountConfig) -> Result<Option<Self>> {
        let Some(oauth) = account.oauth.as_ref() else { return Ok(None); };
        Ok(Some(Self::from_config(&account.name, oauth)?))
    }

    fn from_config(name: &str, oauth: &OAuthConfig) -> Result<Self> {
        let provider = oauth.provider.trim().to_ascii_lowercase();
        let client_id = oauth.client_id.clone()
            .or_else(|| embedded_client_id(&provider).map(str::to_owned))
            .filter(|id| !id.trim().is_empty()).ok_or_else(|| anyhow!(
            "OAuth account {name:?} needs oauth.client_id or an app-publisher client ID at build time"
        ))?;
        let client_secret = oauth.client_secret.clone().or_else(|| {
            (provider == "google").then(|| embedded_google_client_secret())
                .flatten().map(str::to_owned)
        });
        let (auth_endpoint, token_endpoint, scope) = match provider.as_str() {
            "google" => (
                "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
                "https://oauth2.googleapis.com/token".to_owned(),
                "https://mail.google.com/".to_owned(),
            ),
            "microsoft" => {
                let tenant = oauth.tenant.as_deref().unwrap_or("common");
                if tenant.is_empty() || !tenant.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-') {
                    bail!("invalid OAuth Microsoft tenant identifier");
                }
                (format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize"),
                 format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"),
                 "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send".to_owned())
            }
            _ => bail!("unsupported OAuth provider; use google or microsoft"),
        };
        let flow = LoginFlow::parse(oauth.flow.as_deref(), &provider)?;
        let callback_host = oauth.callback_host.as_deref().unwrap_or("127.0.0.1").trim();
        if !matches!(callback_host, "127.0.0.1" | "localhost") {
            bail!("OAuth callback_host must be localhost or 127.0.0.1");
        }
        let callback_path = oauth.callback_path.as_deref().unwrap_or("/oauth/callback");
        if !valid_callback_path(callback_path) {
            bail!("OAuth callback_path must begin with / and contain only letters, digits, /, _ or -");
        }
        if flow != LoginFlow::Browser && (oauth.callback_host.is_some() || oauth.callback_path.is_some()) {
            bail!("OAuth callback_host and callback_path only apply to flow = \"browser\"");
        }
        let prompt = oauth.prompt.as_deref().map(str::trim);
        if let Some(value) = prompt {
            let valid = match provider.as_str() {
                "google" => matches!(value, "consent" | "select_account" | "none"),
                "microsoft" => matches!(value, "login" | "consent" | "select_account" | "none"),
                _ => false,
            };
            if !valid { bail!("unsupported OAuth prompt for the selected provider"); }
        }
        if let Some(hint) = &oauth.login_hint {
            if hint.len() > 254 || hint.is_empty() || hint.chars().any(char::is_control) {
                bail!("OAuth login_hint must be a nonempty single-line address or username");
            }
        }
        if let Some(namespace) = oauth.credential_namespace.as_deref() {
            if namespace.is_empty() || namespace.len() > 48 ||
                !namespace.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')) {
                bail!("OAuth credential_namespace must contain 1-48 ASCII letters, digits, _ or -");
            }
        }
        let scope = if let Some(custom) = oauth.scope.as_deref() {
            if custom.trim().is_empty() || custom.len() > 2048 || custom.chars().any(char::is_control) {
                bail!("OAuth scope must contain valid printable scopes");
            }
            custom.trim().to_owned()
        } else { scope };
        Ok(Self {
            provider, client_id, client_secret, token_endpoint, scope, auth_endpoint,
            account_name: name.to_owned(), flow, callback_host: callback_host.to_owned(),
            callback_path: callback_path.to_owned(), prompt: prompt.map(str::to_owned),
            login_hint: oauth.login_hint.clone(),
            credential_namespace: oauth.credential_namespace.clone(),
        })
    }

    fn token_file(&self) -> Result<PathBuf> {
        let base = dirs::config_dir().ok_or_else(|| anyhow!("cannot locate config directory for OAuth token"))?;
        // Hash the name to keep a user-supplied account label out of paths.
        let digest = digest::digest(&digest::SHA256, self.account_name.as_bytes());
        let key: String = digest.as_ref()[..16].iter().map(|byte| format!("{byte:02x}")).collect();
        let directory = base.join("aerc-rs").join("oauth");
        let directory = match self.credential_namespace.as_deref() {
            Some(namespace) => directory.join(namespace),
            None => directory,
        };
        Ok(directory.join(format!("{key}.json")))
    }

    pub fn refresh_token(&self) -> Result<String> {
        let path = self.token_file()?;
        let meta = fs::symlink_metadata(&path).with_context(|| format!(
            "OAuth token missing for {:?}; run aerc-rs --oauth-login {:?}", self.account_name, self.account_name
        ))?;
        if !meta.file_type().is_file() { bail!("OAuth token path is not a regular file"); }
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                bail!("OAuth token file permissions are too permissive; use chmod 600 on your token file");
            }
        }
        let data: SavedToken = serde_json::from_slice(&fs::read(path)?)
            .context("decoding saved OAuth credentials")?;
        if data.provider != self.provider || data.client_id != self.client_id || data.refresh_token.is_empty() {
            bail!("OAuth credentials do not match the configured account/client; sign in again");
        }
        Ok(data.refresh_token)
    }

    fn save_refresh_token(&self, token: &str) -> Result<()> {
        if token.is_empty() { bail!("OAuth server provided no refresh token"); }
        let path = self.token_file()?;
        let directory = path.parent().ok_or_else(|| anyhow!("invalid OAuth token directory"))?;
        fs::create_dir_all(directory).context("creating OAuth credential directory")?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        let mut staging = tempfile::NamedTempFile::new_in(directory)?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            staging.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let data = SavedToken { provider: self.provider.clone(), client_id: self.client_id.clone(), refresh_token: token.to_owned() };
        serde_json::to_writer(&mut staging, &data)?;
        staging.flush()?;
        staging.persist(&path).map_err(|err| anyhow!("writing OAuth credentials: {}", err.error))?;
        Ok(())
    }

    pub fn access_token(&self) -> Result<String> {
        Ok(self.refresh_access_token()?.access_token)
    }

    /// Refresh an existing Microsoft OAuth session and offer its username
    /// claim as an unverified setup hint. Never changes account configuration.
    /// The identity provider may rotate the cached refresh token as usual.
    pub fn suggested_login_username(&self) -> Result<Option<String>> {
        if self.provider != "microsoft" {
            bail!("OAuth username suggestions currently support Microsoft accounts only");
        }
        Ok(microsoft_login_username_hint(&self.access_token()?))
    }

    /// Inspect only the protocol permissions reported by the token endpoint.
    /// No access or refresh token, account ID, or raw scope string is printed.
    /// `None` means the token endpoint did not supply a scope field.
    pub fn reported_protocol_scopes(&self) -> Result<Option<(bool, bool)>> {
        let reply = self.refresh_access_token()?;
        Ok(reply.scope.as_deref().map(protocol_scope_flags))
    }

    fn refresh_access_token(&self) -> Result<TokenReply> {
        let refresh = self.refresh_token()?;
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", self.client_id.as_str()),
        ];
        // Microsoft permits an explicit scope on a refresh grant. Request the
        // same configured IMAP/SMTP permissions rather than depending on the
        // original grant's default scope selection. This cannot add scopes
        // which the user/tenant has not consented to.
        if self.provider == "microsoft" {
            form.push(("scope", self.scope.as_str()));
        }
        if let Some(secret) = self.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let reply = self.request_token(&form)?;
        if let Some(rotated) = reply.refresh_token.as_deref().filter(|value| !value.is_empty()) {
            self.save_refresh_token(rotated)?;
        }
        Ok(reply)
    }

    fn request_token(&self, form: &[(&str, &str)]) -> Result<TokenReply> {
        let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT)).https_only(true)
            .http_status_as_error(false).build());
        // Never print request parameters, raw error JSON, or token responses.
        let mut response = agent.post(&self.token_endpoint).send_form(form.iter().copied())
            .map_err(|_| anyhow!("OAuth token request failed (network/TLS error)"))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let failure: OAuthFailure = response.body_mut().read_json().unwrap_or_default();
            let message = safe_oauth_failure(status, &failure);
            if self.provider == "google" && status == 400 {
                bail!("{message}. Verify the FULL Google desktop client ID, any matching desktop client_secret issued for that registration, and the exact localhost redirect URI. A browser callback alone does not mean the token exchange succeeded");
            }
            bail!("{message}");
        }
        let token: TokenReply = response.body_mut().read_json()
            .context("decoding OAuth token response")?;
        if token.access_token.is_empty() { bail!("OAuth token endpoint returned no access token"); }
        Ok(token)
    }

    pub fn login(&self) -> Result<()> {
        match self.flow {
            LoginFlow::Browser => self.browser_login(),
            LoginFlow::NativeClient => self.nativeclient_login(),
            LoginFlow::DeviceCode => self.devicecode_login(),
        }
    }

    fn authorization_url(&self, redirect_uri: &str) -> Result<(Url, String, String)> {
        let random = SystemRandom::new();
        let mut verifier_bytes = [0u8; 32];
        let mut state_bytes = [0u8; 24];
        random.fill(&mut verifier_bytes).map_err(|_| anyhow!("generating OAuth PKCE secret"))?;
        random.fill(&mut state_bytes).map_err(|_| anyhow!("generating OAuth state"))?;
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let state = URL_SAFE_NO_PAD.encode(state_bytes);
        let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()).as_ref());
        let mut authorization = Url::parse(&self.auth_endpoint)?;
        {
            let mut params = authorization.query_pairs_mut();
            params.append_pair("response_type", "code")
                .append_pair("client_id", &self.client_id)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("scope", &self.scope)
                .append_pair("state", &state)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("response_mode", "query");
            if self.provider == "google" {
                params.append_pair("access_type", "offline");
                params.append_pair("prompt", self.prompt.as_deref().unwrap_or("consent"));
            } else if let Some(prompt) = self.prompt.as_deref() {
                params.append_pair("prompt", prompt);
            }
            if let Some(hint) = self.login_hint.as_deref() {
                params.append_pair("login_hint", hint);
            }
        }
        Ok((authorization, verifier, state))
    }

    fn exchange_code(&self, code: &str, redirect_uri: &str, verifier: &str, quiet: bool) -> Result<()> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", self.client_id.as_str()),
            ("code_verifier", verifier),
        ];
        if let Some(secret) = self.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let reply = self.request_token(&form)?;
        self.finish_login(reply, quiet)
    }

    fn finish_login(&self, reply: TokenReply, quiet: bool) -> Result<()> {
        if reply.access_token.is_empty() { bail!("OAuth server returned no access token"); }
        let refresh = reply.refresh_token.ok_or_else(|| anyhow!(
            "OAuth provider did not return a refresh token; revoke authorization and retry with offline access"
        ))?;
        self.save_refresh_token(&refresh)?;
        if !quiet {
            println!("OAuth sign-in succeeded for {:?}; refresh token stored locally with private permissions.", self.account_name);
        }
        Ok(())
    }

    pub fn browser_login(&self) -> Result<()> {
        self.browser_login_inner(false)
    }

    /// TUI wizard uses a browser-only flow and renders its own feedback.
    /// Never print sensitive authorization URLs into the alternate screen.
    fn wizard_login(&self) -> Result<()> {
        if self.flow != LoginFlow::Browser {
            bail!("the account wizard currently requires browser OAuth");
        }
        self.browser_login_inner(true)
    }

    fn browser_login_inner(&self, quiet: bool) -> Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("binding OAuth callback to loopback")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://{}:{port}{}", self.callback_host, self.callback_path);
        let (authorization, verifier, state) = self.authorization_url(&redirect_uri)?;
        if open_browser(authorization.as_str()) {
            if !quiet {
                println!("Check your browser to finish signing in to {:?}.", self.account_name);
            }
        } else if quiet {
            bail!("could not open a browser; run --oauth-login for the registered account from a normal terminal");
        } else {
            println!("Could not launch your browser. Open this URL to sign in:\n{}", authorization);
        }
        let started = Instant::now();
        let code = loop {
            if started.elapsed() >= CALLBACK_TIMEOUT {
                bail!("OAuth sign-in timed out. If the browser reports a redirect URI mismatch, run --oauth-inspect for this account and register the exact redirect host/path for your application");
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut buffer = [0u8; 8192];
                    let count = stream.read(&mut buffer)?;
                    let request = std::str::from_utf8(&buffer[..count]).unwrap_or("");
                    let first_line = request.lines().next().unwrap_or("");
                    let Some(target) = first_line.strip_prefix("GET ").and_then(|v| v.split_once(' ').map(|(p, _)| p)) else { continue; };
                    let url = Url::parse(&format!("http://{}:{port}{target}", self.callback_host))?;
                    if url.path() != self.callback_path || url.host_str() != Some(self.callback_host.as_str())
                        || url.port_or_known_default() != Some(port) { continue; }
                    let params: std::collections::HashMap<String, String> = url.query_pairs()
                        .map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
                    let matching_state = params.get("state").is_some_and(|value| value == &state);
                    let code = if matching_state { params.get("code").cloned() } else { None };
                    let message = if code.is_some() { "Authorization received. Return to aerc-rs." }
                        else { "Authorization unsuccessful. Return to aerc-rs." };
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}", message.len());
                    let _ = stream.write_all(reply.as_bytes());
                    if !matching_state { bail!("OAuth callback state mismatch"); }
                    if let Some(code) = code { break code; }
                    bail!("OAuth authorization declined or returned no authorization code");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
                Err(error) => return Err(error.into()),
            }
        };
        self.exchange_code(&code, &redirect_uri, &verifier, quiet)
    }

    /// Microsoft native-client redirect compatible with the authorization-code
    /// mode used by Mutt/NeoMutt. The browser returns to a registered HTTPS URI
    /// rather than an aerc-rs localhost listener, so the user pastes the URL.
    pub fn nativeclient_login(&self) -> Result<()> {
        let (authorization, verifier, state) = self.authorization_url(MICROSOFT_NATIVE_REDIRECT)?;
        if open_browser(authorization.as_str()) {
            println!("Check your browser to finish signing in to {:?}.", self.account_name);
        } else {
            println!("Open this URL to sign in:\n{authorization}");
        }
        println!("After authorization, copy the COMPLETE redirect URL from the browser address bar");
        println!("and paste it below. It will only be used locally to exchange the one-time code.");
        print!("Redirect URL: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        let code = parse_nativeclient_callback(&answer, &state)?;
        self.exchange_code(&code, MICROSOFT_NATIVE_REDIRECT, &verifier, false)
    }

    /// RFC 8628 / Microsoft device-code sign-in. This does not require any
    /// redirect URI, but the Entra application must permit public-client flow.
    pub fn devicecode_login(&self) -> Result<()> {
        let endpoint = self.auth_endpoint.replace("/authorize", "/devicecode");
        let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT)).https_only(true)
            .http_status_as_error(false).build());
        let mut response = agent.post(&endpoint).send_form([
            ("client_id", self.client_id.as_str()),
            ("scope", self.scope.as_str()),
        ]).map_err(|_| anyhow!("Microsoft device authorization request failed"))?;
        if !response.status().is_success() {
            bail!("Microsoft declined the device-code request; the application may not permit public-client authentication");
        }
        let device: DeviceAuthorization = response.body_mut().read_json()
            .context("decoding Microsoft device authorization")?;
        if device.device_code.is_empty() || device.user_code.is_empty() || device.expires_in == 0 {
            bail!("Microsoft returned an incomplete device-code challenge");
        }
        let verification = Url::parse(&device.verification_uri)
            .context("invalid Microsoft verification URL")?;
        if verification.scheme() != "https" || verification.host_str() != Some("microsoft.com") &&
            verification.host_str() != Some("www.microsoft.com") {
            bail!("Microsoft returned an unexpected device-code verification URL");
        }
        println!("Sign in to {:?} in your browser and enter code: {}", self.account_name, device.user_code);
        if !open_browser(verification.as_str()) {
            println!("Open {} in your browser.", verification);
        }
        let started = Instant::now();
        let mut interval = device.interval.unwrap_or(5).clamp(5, 60);
        while started.elapsed() < Duration::from_secs(device.expires_in) {
            thread::sleep(Duration::from_secs(interval));
            if started.elapsed() >= Duration::from_secs(device.expires_in) { break; }
            let mut result = agent.post(&self.token_endpoint).send_form([
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", self.client_id.as_str()),
                ("device_code", device.device_code.as_str()),
            ]).map_err(|_| anyhow!("Microsoft device-code token request failed"))?;
            let reply: DevicePollReply = result.body_mut().read_json()
                .context("decoding Microsoft device-code token response")?;
            if result.status().is_success() {
                let access_token = reply.access_token.ok_or_else(|| anyhow!("Microsoft returned no access token"))?;
                return self.finish_login(TokenReply { access_token, refresh_token: reply.refresh_token, scope: None }, false);
            }
            match reply.error.as_deref() {
                Some("authorization_pending") => {},
                Some("slow_down") => interval = interval.saturating_add(5).min(60),
                Some("authorization_declined" | "access_denied") => bail!("Microsoft authorization was declined"),
                Some("expired_token") => bail!("Microsoft device sign-in code expired; retry login"),
                Some("invalid_client" | "unauthorized_client") => bail!(
                    "This app registration cannot use Microsoft device-code sign-in; try flow = \"nativeclient\" or register an aerc-rs client"
                ),
                _ => bail!("Microsoft rejected device-code authorization; check permissions and account policy"),
            }
        }
        bail!("Microsoft device sign-in code expired; retry login")
    }
}

fn valid_callback_path(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//") && path.len() <= 128 &&
        path.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-'))
}

fn parse_nativeclient_callback(input: &str, state: &str) -> Result<String> {
    let result = Url::parse(input.trim()).context("expected the full Microsoft redirect URL")?;
    let expected = Url::parse(MICROSOFT_NATIVE_REDIRECT)?;
    if result.origin() != expected.origin() || result.path() != expected.path() {
        bail!("redirect URL is not the expected Microsoft native-client callback");
    }
    let params: std::collections::HashMap<String, String> = result.query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
    if params.get("state").is_none_or(|received| received != state) {
        bail!("OAuth redirect state mismatch");
    }
    let code = params.get("code").filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Microsoft authorization returned no code; check consent and retry"))?;
    Ok(code.clone())
}

fn open_browser(url: &str) -> bool {
    // Browser launchers may inherit our terminal and print GTK/desktop
    // warnings even after spawning the browser. Keep OAuth's CLI output
    // limited to actionable status messages; browser errors stay in the UI.
    #[cfg(target_os = "linux")]
    { return spawn_browser(Command::new("xdg-open").arg(url)); }
    #[cfg(target_os = "macos")]
    { return spawn_browser(Command::new("open").arg(url)); }
    #[cfg(windows)]
    { return spawn_browser(Command::new("rundll32").args(["url.dll,FileProtocolHandler", url])); }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    { let _ = url; false }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn spawn_browser(command: &mut Command) -> bool {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// Report exactly which redirect and delegated scopes must be authorized
/// on the application's registration, without printing tokens or secrets.
pub fn inspect_account(accounts: &[AccountConfig], name: &str) -> Result<()> {
    let account = accounts.iter().find(|account| account.name == name)
        .ok_or_else(|| anyhow!("no account named {name:?} in config"))?;
    let settings = OAuthSettings::for_account(account)?
        .ok_or_else(|| anyhow!("account {name:?} has no [accounts.oauth] configuration"))?;
    println!("OAuth configuration for {:?}:", account.name);
    println!("  Provider: {}", settings.provider);
    println!("  Client ID: configured (value intentionally not printed)");
    println!("  Authorization endpoint: {}", settings.auth_endpoint);
    match settings.flow {
        LoginFlow::Browser => println!(
            "  Redirect to register: http://{}:<ephemeral-port>{}",
            settings.callback_host, settings.callback_path,
        ),
        LoginFlow::NativeClient => println!("  Redirect to register: {MICROSOFT_NATIVE_REDIRECT}"),
        LoginFlow::DeviceCode => println!("  Redirect: none (device-code flow; requires public-client permission)"),
    }
    println!("  Requested scopes: {}", settings.scope);
    println!("  Login prompt: {}", settings.prompt.as_deref().unwrap_or(
        if settings.provider == "google" { "consent (default)" } else { "provider default" },
    ));
    if settings.login_hint.is_some() { println!("  Login hint: configured"); }
    println!("  Token store: {}", settings.credential_namespace.as_deref().unwrap_or("default"));
    if settings.flow == LoginFlow::Browser {
        println!("  Register the callback host and path; the loopback port is selected at runtime.");
    }
    println!("  Note: this does not verify which redirects, delegated scopes, or consent policies");
    println!("  are registered for the selected client ID. Provider/admin approval is still required.");
    Ok(())
}

/// Browser-only login for the editable account wizard. This never exposes
/// OAuth URL state, codes, or tokens in the TUI's terminal output.
pub fn login_account_quiet(accounts: &[AccountConfig], name: &str) -> Result<()> {
    let account = accounts.iter().find(|account| account.name == name)
        .ok_or_else(|| anyhow!("no account named {name:?} in config"))?;
    let settings = OAuthSettings::for_account(account)?
        .ok_or_else(|| anyhow!("account {name:?} has no OAuth settings"))?;
    settings.wizard_login()
}

pub fn login_account(accounts: &[AccountConfig], name: &str) -> Result<()> {
    let account = accounts.iter().find(|account| account.name == name)
        .ok_or_else(|| anyhow!("no account named {name:?} in config"))?;
    let settings = OAuthSettings::for_account(account)?
        .ok_or_else(|| anyhow!("account {name:?} has no [accounts.oauth] configuration"))?;
    settings.login()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn google_desktop_id_must_include_full_suffix() {
        assert!(valid_google_desktop_client_id("123-abc.apps.googleusercontent.com"));
        assert!(!valid_google_desktop_client_id("123-abc"));
        assert!(!valid_google_desktop_client_id("oops\n.apps.googleusercontent.com"));
    }

    #[test]
    fn microsoft_login_hint_prefers_human_readable_login_and_rejects_unsafe_claims() {
        fn token(claims: serde_json::Value) -> String {
            let data = serde_json::to_vec(&claims).unwrap();
            format!("header.{}.signature", URL_SAFE_NO_PAD.encode(data))
        }
        let ok = token(serde_json::json!({
            "preferred_username": "f00851y@dartmouth.edu",
            "upn": "other@dartmouth.edu"
        }));
        assert_eq!(super::microsoft_login_username_hint(&ok).as_deref(),
            Some("f00851y@dartmouth.edu"));
        let fallback = token(serde_json::json!({
            "preferred_username": "User Name",
            "upn": "login@dartmouth.edu"
        }));
        assert_eq!(super::microsoft_login_username_hint(&fallback).as_deref(),
            Some("login@dartmouth.edu"));
        assert!(super::microsoft_login_username_hint(&token(serde_json::json!({
            "preferred_username": "login\n@dartmouth.edu"
        }))).is_none());
        assert!(super::microsoft_login_username_hint("opaque-token").is_none());
    }

    #[test]
    fn google_and_microsoft_use_distinct_imap_smtp_scopes() {
        let google = OAuthSettings::from_config("Personal", &OAuthConfig { provider: "google".into(), client_id: Some("public-client".into()), tenant: None, client_secret: None, flow: None, ..Default::default() }).unwrap();
        assert_eq!(google.scope, "https://mail.google.com/");
        let microsoft = OAuthSettings::from_config("Work", &OAuthConfig { provider: "microsoft".into(), client_id: Some("public-client".into()), tenant: None, client_secret: None, flow: None, ..Default::default() }).unwrap();
        assert!(microsoft.scope.contains("IMAP.AccessAsUser.All"));
        assert!(microsoft.scope.contains("SMTP.Send"));
        assert!(microsoft.scope.contains("offline_access"));
        assert!(!format!("{google:?}").contains("public-client"));
    }
    #[test]
    fn microsoft_redirect_flows_do_not_assume_aerc_localhost_path() {
        assert_eq!(LoginFlow::parse(None, "microsoft").unwrap(), LoginFlow::Browser);
        assert_eq!(LoginFlow::parse(Some("nativeclient"), "microsoft").unwrap(), LoginFlow::NativeClient);
        assert_eq!(LoginFlow::parse(Some("devicecode"), "microsoft").unwrap(), LoginFlow::DeviceCode);
        assert!(LoginFlow::parse(Some("nativeclient"), "google").is_err());
        assert!(LoginFlow::parse(Some("unsafe"), "microsoft").is_err());
    }

    #[test]
    fn nativeclient_callback_requires_expected_origin_path_and_state() {
        let ok = format!("{MICROSOFT_NATIVE_REDIRECT}?code=abc%2B123&state=EXPECTED");
        assert_eq!(parse_nativeclient_callback(&ok, "EXPECTED").unwrap(), "abc+123");
        assert!(parse_nativeclient_callback(&ok, "WRONG").is_err());
        assert!(parse_nativeclient_callback("https://evil.test/?code=abc&state=EXPECTED", "EXPECTED").is_err());
        assert!(parse_nativeclient_callback("https://login.microsoftonline.com/other?code=abc&state=EXPECTED", "EXPECTED").is_err());
        assert!(parse_nativeclient_callback(&format!("{MICROSOFT_NATIVE_REDIRECT}?state=EXPECTED"), "EXPECTED").is_err());
    }

    #[test]
    fn authorization_url_binds_pkce_to_configured_redirect() {
        let settings = OAuthSettings::from_config("Work", &OAuthConfig {
            provider: "microsoft".into(), client_id: Some("test-id".into()),
            tenant: None, client_secret: None, flow: Some("nativeclient".into()),
            ..Default::default()
        }).unwrap();
        let (url, verifier, state) = settings.authorization_url(MICROSOFT_NATIVE_REDIRECT).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.get("redirect_uri").unwrap(), MICROSOFT_NATIVE_REDIRECT);
        assert_eq!(params.get("state").unwrap(), &state);
        assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(params.get("code_challenge").unwrap(),
            &URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()).as_ref()));
    }
    #[test]
    fn browser_redirect_and_consent_options_match_owned_registration() {
        let settings = OAuthSettings::from_config("Work", &OAuthConfig {
            provider: "microsoft".into(), client_id: Some("registered-client".into()),
            callback_host: Some("localhost".into()), callback_path: Some("/".into()),
            prompt: Some("select_account".into()), login_hint: Some("person@example.net".into()),
            ..Default::default()
        }).unwrap();
        let redirect = "http://localhost:54321/";
        let (url, _, _) = settings.authorization_url(redirect).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.get("redirect_uri").map(String::as_str), Some(redirect));
        assert_eq!(params.get("prompt").map(String::as_str), Some("select_account"));
        assert_eq!(params.get("login_hint").map(String::as_str), Some("person@example.net"));
        assert!(settings.scope.contains("offline_access"));
    }

    #[test]
    fn rejects_unsafe_redirects_and_incompatible_flows() {
        for bad in ["evil.example", "0.0.0.0", "localhost.evil.test", "::1"] {
            assert!(OAuthSettings::from_config("Work", &OAuthConfig {
                provider: "microsoft".into(), client_id: Some("owned".into()),
                callback_host: Some(bad.into()), ..Default::default()
            }).is_err());
        }
        for bad in ["not/a/path", "//attacker", "/foo?bar", "/foo#bar", "/foo%2fbar"] {
            assert!(OAuthSettings::from_config("Work", &OAuthConfig {
                provider: "microsoft".into(), client_id: Some("owned".into()),
                callback_path: Some(bad.into()), ..Default::default()
            }).is_err());
        }
        assert!(OAuthSettings::from_config("Work", &OAuthConfig {
            provider: "microsoft".into(), client_id: Some("owned".into()),
            callback_path: Some("/".into()), flow: Some("devicecode".into()),
            ..Default::default()
        }).is_err());
        assert!(OAuthSettings::from_config("Personal", &OAuthConfig {
            provider: "google".into(), client_id: Some("owned".into()),
            prompt: Some("login".into()), ..Default::default()
        }).is_err());
    }

    #[test]
    fn custom_scopes_are_used_for_explicit_consent_tests() {
        let settings = OAuthSettings::from_config("Work", &OAuthConfig {
            provider: "microsoft".into(), client_id: Some("owned".into()),
            scope: Some("offline_access https://outlook.office.com/IMAP.AccessAsUser.All".into()),
            ..Default::default()
        }).unwrap();
        let (url, _, _) = settings.authorization_url(MICROSOFT_NATIVE_REDIRECT).unwrap();
        assert_eq!(url.query_pairs().find(|(key, _)| key == "scope").unwrap().1,
            "offline_access https://outlook.office.com/IMAP.AccessAsUser.All");
    }

    #[test]
    fn experimental_credential_namespace_uses_a_separate_token_directory() {
        let ordinary = OAuthSettings::from_config("University", &OAuthConfig {
            provider: "microsoft".into(), client_id: Some("local-public-client".into()),
            ..Default::default()
        }).unwrap();
        let experimental = OAuthSettings::from_config("University", &OAuthConfig {
            provider: "microsoft".into(), client_id: Some("local-public-client".into()),
            credential_namespace: Some("oauth-lab".into()), ..Default::default()
        }).unwrap();
        assert_ne!(ordinary.token_file().unwrap(), experimental.token_file().unwrap());
        assert_eq!(experimental.token_file().unwrap().parent().unwrap().file_name().and_then(|name| name.to_str()), Some("oauth-lab"));
        for value in ["../escape", "a/b", "", "x.y", "a b"] {
            assert!(OAuthSettings::from_config("University", &OAuthConfig {
                provider: "microsoft".into(), client_id: Some("local-public-client".into()),
                credential_namespace: Some(value.into()), ..Default::default()
            }).is_err());
        }
    }

    #[test]
    fn smtp_claim_diagnostics_do_not_expose_token_or_identifiers() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        let payload = serde_json::json!({
            "aud": "https://outlook.office.com", "scp": "IMAP.AccessAsUser.All SMTP.Send",
            "azp": "client-identifier", "tid": "private-tenant",
            "preferred_username": "student@example.edu", "oid": "private-object"
        });
        let token = format!("{}.{}.signature", URL_SAFE_NO_PAD.encode("{}"),
            URL_SAFE_NO_PAD.encode(payload.to_string()));
        let diagnostic = safe_smtp_token_diagnostics(&token, "client-identifier", "student@example.edu");
        assert!(diagnostic.contains("aud=Exchange Online"));
        assert!(diagnostic.contains("scp.SMTP.Send=yes"));
        assert!(diagnostic.contains("client_id_matches=yes"));
        assert!(diagnostic.contains("login_matches_SMTP=yes"));
        for secret in ["private-tenant", "private-object", "student@example.edu", "client-identifier", &token] {
            assert!(!diagnostic.contains(secret));
        }
        assert!(safe_smtp_token_diagnostics("opaque-token", "id", "u").contains("inconclusive"));
    }

    #[test]
    fn protocol_permissions_are_read_only_from_token_response_scope_metadata() {
        assert_eq!(protocol_scope_flags(
            "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send"
        ), (true, true));
        assert_eq!(protocol_scope_flags(
            "https://outlook.office.com/IMAP.AccessAsUser.All"
        ), (true, false));
        assert_eq!(protocol_scope_flags(
            "https://graph.microsoft.com/Mail.Send"
        ), (false, false));
        let parsed: TokenReply = serde_json::from_str(
            r#"{"access_token":"private-token","refresh_token":null,"scope":"https://outlook.office.com/SMTP.Send"}"#
        ).unwrap();
        assert_eq!(parsed.scope.as_deref().map(protocol_scope_flags), Some((false, true)));
        let no_scope: TokenReply = serde_json::from_str(r#"{"access_token":"private-token","refresh_token":null}"#).unwrap();
        assert!(no_scope.scope.is_none());
    }

    #[test]
    fn safe_oauth_failure_does_not_include_descriptions_or_credentials() {
        let error = OAuthFailure { error: Some("invalid_grant".into()), error_codes: vec![65001] };
        let message = safe_oauth_failure(400, &error);
        assert!(message.contains("AADSTS65001"));
        assert!(message.contains("invalid_grant"));
        assert!(!message.contains("refresh_token"));
        let unsafe_error = OAuthFailure { error: Some("token=secret\n".into()), error_codes: vec![] };
        assert!(!safe_oauth_failure(400, &unsafe_error).contains("secret"));
    }

    #[test]
    fn rejects_unexpected_providers_and_tenant_paths() {
        assert!(OAuthSettings::from_config("Work", &OAuthConfig { provider: "example".into(), client_id: Some("id".into()), tenant: None, client_secret: None, flow: None, ..Default::default() }).is_err());
        assert!(OAuthSettings::from_config("Work", &OAuthConfig { provider: "microsoft".into(), client_id: Some("id".into()), tenant: Some("../x".into()), client_secret: None, flow: None, ..Default::default() }).is_err());
    }
}
