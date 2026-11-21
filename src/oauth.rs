//! Native desktop OAuth authorization-code + PKCE sign-in for IMAP/SMTP.
//! Google/Microsoft use publisher-registered IDs; open-public-client servers
//! use standards-based discovery and (when available) dynamic registration.
//! Users authenticate in their system browser; Jerc never handles their 2FA.
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::{AccountConfig, OAuthConfig};

mod open;
pub use open::validate_open_issuer;

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const MICROSOFT_NATIVE_REDIRECT: &str =
    "https://login.microsoftonline.com/common/oauth2/nativeclient";

/// Application-publisher defaults embedded at compile time. The user may
/// override them per-account, but ordinary users should not need to register
/// OAuth clients when using an appropriately registered app build.
/// Register your own IDs with each provider; don't copy another app's identity.
#[must_use]
pub fn embedded_client_id(provider: &str) -> Option<&'static str> {
    match provider {
        "google" => option_env!("JERC_GOOGLE_CLIENT_ID"),
        "microsoft" => option_env!("JERC_MICROSOFT_CLIENT_ID"),
        _ => None,
    }
    .filter(|id| !id.trim().is_empty())
}

#[must_use]
pub fn embedded_google_client_secret() -> Option<&'static str> {
    option_env!("JERC_GOOGLE_CLIENT_SECRET").filter(|secret| !secret.trim().is_empty())
}

/// Explicit nonempty account values override publisher defaults. Treat a
/// blank field in an older generated config as absent, not as a request to
/// disable the embedded registration. A release build can therefore work on
/// a second machine without users supplying OAuth application IDs.
fn account_or_embedded<'a>(
    account_value: Option<&'a str>,
    embedded: Option<&'a str>,
) -> Option<&'a str> {
    account_value
        .filter(|value| !value.trim().is_empty())
        .or_else(|| embedded.filter(|value| !value.trim().is_empty()))
}

/// Google Desktop application OAuth client IDs include this suffix verbatim.
/// The local-part is checked only for safe syntax; this does not verify that
/// the client is registered or permitted to use this redirect URI.
#[must_use]
pub fn valid_google_desktop_client_id(id: &str) -> bool {
    let Some(prefix) = id.strip_suffix(".apps.googleusercontent.com") else {
        return false;
    };
    !prefix.is_empty()
        && prefix
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginFlow {
    Browser,
    NativeClient,
    DeviceCode,
}

impl LoginFlow {
    fn parse(value: Option<&str>, provider: &str) -> Result<Self> {
        let flow = match value
            .unwrap_or("browser")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
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
    revocation_endpoint: Option<String>,
    pub scope: String,
    auth_endpoint: String,
    account_name: String,
    flow: LoginFlow,
    callback_host: String,
    callback_path: String,
    prompt: Option<String>,
    login_hint: Option<String>,
    credential_namespace: Option<String>,
    issuer: Option<String>,
    require_issuer_response: bool,
    credential_slot: Arc<CredentialSlot>,
    credential_generation: u64,
}

impl std::fmt::Debug for OAuthSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthSettings")
            .field("provider", &self.provider)
            .field("account_name", &self.account_name)
            .finish_non_exhaustive()
    }
}

// One gate per credential path, shared by IMAP, SMTP, background and IDLE
// workers. Keep retired gates for the process lifetime: an old worker must not
// be able to acquire a fresh gate after account deletion.
#[derive(Default)]
struct CredentialSession {
    cached_access: Option<CachedAccess>,
}

struct CachedAccess {
    token: String,
    scope: Option<String>,
    provider: String,
    client_id: String,
    requested_scope: String,
    expires_at: Instant,
    // Unlike CLOCK_MONOTONIC/Instant on Linux, wall time includes suspend.
    // Checking both clocks prevents reusing an expired token after resume.
    expires_at_wall: SystemTime,
}

impl CachedAccess {
    fn valid_at(&self, monotonic: Instant, wall: SystemTime) -> bool {
        monotonic < self.expires_at && wall < self.expires_at_wall
    }
}

#[derive(Default)]
struct CredentialSlot {
    state: Mutex<CredentialSession>,
    generation: AtomicU64,
    retired: AtomicBool,
}

static CREDENTIAL_SLOTS: OnceLock<Mutex<HashMap<PathBuf, Arc<CredentialSlot>>>> = OnceLock::new();

// A refresh already under the gate may finish during deletion. Persist its
// rotation before cleanup reads the final token, but never after replacement
// by a NEW login generation.
fn refresh_may_persist(slot: &CredentialSlot, original_generation: u64) -> bool {
    let generation = slot.generation.load(Ordering::Acquire);
    (generation == original_generation && !slot.retired.load(Ordering::Acquire))
        || (generation == original_generation.wrapping_add(1)
            && slot.retired.load(Ordering::Acquire))
}

fn credential_slot(path: &Path) -> Arc<CredentialSlot> {
    let slots = CREDENTIAL_SLOTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut slots = slots
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        slots
            .entry(path.to_owned())
            .or_insert_with(|| Arc::new(CredentialSlot::default())),
    )
}

/// Nonblocking invalidation: workers already in an HTTP refresh observe it
/// before writing a rotated token. Cleanup subsequently waits for the gate.
pub(crate) fn retire_account_token(account: &AccountConfig) -> Option<u64> {
    let oauth = account.oauth.as_ref()?;
    let base = dirs::config_dir()?;
    let path =
        token_path_for_account(&base, &account.name, oauth.credential_namespace.as_deref()).ok()?;
    let slot = credential_slot(&path);
    slot.retired.store(true, Ordering::Release);
    Some(slot.generation.fetch_add(1, Ordering::AcqRel) + 1)
}

fn retired_slot_is_current(slot: &CredentialSlot, generation: u64) -> bool {
    slot.retired.load(Ordering::Acquire) && slot.generation.load(Ordering::Acquire) == generation
}

/// Run local cleanup only after any in-flight refresh has released the gate.
/// This must be called from a background thread, never on the TUI thread.
pub(crate) fn with_retired_account_lock<T>(
    account: &AccountConfig,
    retired_generation: Option<u64>,
    action: impl FnOnce() -> T,
) -> Option<T> {
    let Some(oauth) = &account.oauth else {
        return Some(action());
    };
    let Some(base) = dirs::config_dir() else {
        return Some(action());
    };
    let Ok(path) =
        token_path_for_account(&base, &account.name, oauth.credential_namespace.as_deref())
    else {
        return Some(action());
    };
    let slot = credential_slot(&path);
    let mut session = slot
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(generation) = retired_generation
        && !retired_slot_is_current(&slot, generation)
    {
        // A newly signed-in account superseded this deletion. Never erase it.
        return None;
    }
    session.cached_access = None;
    Some(action())
}

/// The wizard is provisional until its account config is saved. Cancelled or
/// abandoned wizard grants must not remain usable on disk.
pub(crate) fn discard_provisional_login(account: &AccountConfig) {
    let generation = retire_account_token(account);
    let _ = with_retired_account_lock(account, generation, || {
        if let Some(oauth) = &account.oauth
            && let Some(base) = dirs::config_dir()
            && let Ok(path) =
                token_path_for_account(&base, &account.name, oauth.credential_namespace.as_deref())
        {
            let _ = fs::remove_file(path.with_extension("public-client.json"));
            let _ = fs::remove_file(path);
        }
    });
}

#[derive(Serialize, Deserialize)]
struct SavedToken {
    provider: String,
    client_id: String,
    refresh_token: String,
    #[serde(default)]
    scope: Option<String>,
}

// Avoid depending on the access token's internal format: Microsoft service
// access tokens can be opaque or encrypted. Inspect only response metadata.
fn protocol_scope_flags(scope: &str) -> (bool, bool) {
    let has = |wanted: &str| {
        scope.split_whitespace().any(|item| {
            item.rsplit('/')
                .next()
                .is_some_and(|scope| scope.eq_ignore_ascii_case(wanted))
        })
    };
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
        Some(
            "https://outlook.office.com"
            | "https://outlook.office365.com"
            | "00000002-0000-0ff1-ce00-000000000000",
        ) => "Exchange Online",
        Some(_) => "other (verify token audience)",
        None => "missing",
    };
    let scope = field("scp");
    let scoped = |permission: &str| {
        scope.is_some_and(|s| {
            s.split_whitespace()
                .any(|candidate| candidate.eq_ignore_ascii_case(permission))
        })
    };
    let flag = |bool_value| if bool_value { "yes" } else { "no" };
    let client_claim = field("azp").or_else(|| field("appid"));
    let client_match = match client_claim {
        Some(claim) => flag(claim.eq_ignore_ascii_case(client_id)),
        None => "unavailable",
    };
    let principal = field("preferred_username")
        .or_else(|| field("upn"))
        .or_else(|| field("unique_name"));
    let principal_match = match principal {
        Some(username) => flag(username.eq_ignore_ascii_case(smtp_username)),
        None => "unavailable",
    };
    format!(
        "Access token JWT claims (unverified): aud={audience}; scp.IMAP={}; scp.SMTP.Send={}; client_id_matches={client_match}; login_matches_SMTP={principal_match}; tenant_present={}",
        if scope.is_some() {
            flag(scoped("IMAP.AccessAsUser.All"))
        } else {
            "unavailable"
        },
        if scope.is_some() {
            flag(scoped("SMTP.Send"))
        } else {
            "unavailable"
        },
        flag(field("tid").is_some()),
    )
}

/// Extract a *suggested* SMTP/IMAP login from a Microsoft access token.
/// This is strictly a user-facing account-setup hint, never proof of identity
/// or authorization: JWT claims are not signature-verified here, and Microsoft
/// may issue opaque tokens. Do not cache, trust, or auto-apply this value.
#[must_use]
pub fn microsoft_login_username_hint(token: &str) -> Option<String> {
    if token.len() > 131_072 {
        return None;
    }
    let mut parts = token.split('.');
    let (_, Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    if payload.len() > 65_536 {
        return None;
    }
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    for key in ["preferred_username", "upn", "unique_name"] {
        let Some(value) = claims.get(key).and_then(serde_json::Value::as_str) else {
            continue;
        };
        // A display name, GUID, unsafe value, or truncated claim is not a
        // useful login suggestion. User-provided UPNs are validated separately.
        if value.len() > 254
            || value
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace())
        {
            continue;
        }
        if let Some((local, domain)) = value.rsplit_once('@')
            && (!local.is_empty()
                && !local.contains('@')
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && domain
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-'))
        {
            return Some(value.to_owned());
        }
    }
    None
}

#[derive(Clone, Deserialize)]
struct TokenReply {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
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
    if let Some(error) = failure.error.as_deref().filter(|error| {
        error.len() <= 64
            && error
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    }) {
        message.push_str(&format!(": {error}"));
    }
    if let Some(code) = failure.error_codes.first() {
        message.push_str(&format!(" (AADSTS{code})"));
    }
    message
}

/// The same token path is used by sign-in and explicit local account purge.
/// Reject unsafe namespaces even when the caller has only deserialized config.
pub(crate) fn token_path_for_account(
    base: &Path,
    name: &str,
    namespace: Option<&str>,
) -> Result<PathBuf> {
    if let Some(value) = namespace
        && (value.is_empty()
            || value.len() > 48
            || !value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')))
    {
        bail!("invalid OAuth credential namespace");
    }
    let digest = digest::digest(&digest::SHA256, name.as_bytes());
    let key: String = digest.as_ref()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let directory = base.join("jerc").join("oauth");
    let directory = match namespace {
        Some(value) => directory.join(value),
        None => directory,
    };
    Ok(directory.join(format!("{key}.json")))
}

/// Best-effort identity of the Google mailbox attached to this OAuth grant.
/// A login identity is more useful than the display address: Workspace mail
/// aliases may have a different `email` field. Full IMAP URLs encode `@`.
fn google_mailbox_identity(account: &AccountConfig) -> Option<String> {
    let uri_identity = Url::parse(&account.source).ok().and_then(|url| {
        if !url.scheme().starts_with("imap") || url.username().is_empty() {
            return None;
        }
        percent_encoding::percent_decode_str(url.username())
            .decode_utf8()
            .ok()
            .map(|username| username.into_owned())
    });
    let candidate = uri_identity
        .as_deref()
        .or(account.imap_username.as_deref())
        .or(account.email.as_deref())?
        .trim();
    if !candidate.contains('@') || candidate.chars().any(char::is_whitespace) {
        return None;
    }
    Some(candidate.to_ascii_lowercase())
}

/// Google's revocation invalidates authorization for this Google *user* and
/// OAuth project. Distinct Gmail users can safely share a desktop client ID.
/// If a configured account's login identity is unknown, err on the side of
/// not revoking a grant that might also be needed by another local account.
fn google_revocation_conflicts(
    deleted: &AccountConfig,
    remaining: &[AccountConfig],
    _client_id: &str,
) -> bool {
    let deleted_identity = google_mailbox_identity(deleted);
    // Different client IDs may still belong to the same Google project.
    remaining.iter().any(|other| {
        let is_google = other
            .oauth
            .as_ref()
            .is_some_and(|oauth| oauth.provider.trim().eq_ignore_ascii_case("google"));
        if !is_google {
            return false;
        }
        match (
            deleted_identity.as_deref(),
            google_mailbox_identity(other).as_deref(),
        ) {
            (Some(deleted), Some(other)) => deleted == other,
            _ => true,
        }
    })
}

/// Network-only revocation job. The secret is never logged or serialized and
/// is discarded when the detached worker finishes or times out.
pub(crate) struct RevocationRequest {
    settings: OAuthSettings,
    token: String,
}

impl RevocationRequest {
    pub(crate) fn revoke(self) -> Result<()> {
        let mut settings = self.settings;
        if settings.provider == "open" && settings.revocation_endpoint.is_none() {
            // Older public-client registration files predate revocation URL
            // caching. Discover it from the configured HTTPS issuer instead.
            settings.revocation_endpoint = settings
                .issuer
                .as_deref()
                .and_then(|issuer| open::revocation_endpoint(issuer, &settings.scope).ok())
                .flatten();
        }
        let Some(endpoint) = settings.revocation_endpoint.as_deref() else {
            bail!("OAuth provider did not advertise token revocation");
        };
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(8)))
                .https_only(true)
                .http_status_as_error(false)
                .build(),
        );
        let mut form = vec![
            ("token", self.token.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", settings.client_id.as_str()),
        ];
        if let Some(secret) = settings.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let response = agent
            .post(endpoint)
            .send_form(form)
            .map_err(|_| anyhow!("OAuth revocation unavailable"))?;
        if !response.status().is_success() {
            bail!("OAuth revocation HTTP {}", response.status());
        }
        Ok(())
    }
}

impl OAuthSettings {
    pub fn for_account(account: &AccountConfig) -> Result<Option<Self>> {
        let Some(oauth) = account.oauth.as_ref() else {
            return Ok(None);
        };
        Ok(Some(Self::from_config(&account.name, oauth)?))
    }

    fn from_config(name: &str, oauth: &OAuthConfig) -> Result<Self> {
        let provider = oauth.provider.trim().to_ascii_lowercase();
        let client_id = if provider == "open" {
            oauth
                .client_id
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_owned()
        } else {
            account_or_embedded(oauth.client_id.as_deref(), embedded_client_id(&provider))
                .ok_or_else(|| anyhow!(
                    "OAuth account {name:?} needs oauth.client_id or an app-publisher client ID at build time"
                ))?.to_owned()
        };
        if provider == "open" && !client_id.is_empty() && !open::valid_client_id(&client_id) {
            bail!("Open OAuth client ID contains invalid characters");
        }
        let client_secret = account_or_embedded(
            oauth.client_secret.as_deref(),
            if provider == "google" {
                embedded_google_client_secret()
            } else {
                None
            },
        )
        .map(str::to_owned);
        if provider == "open" && client_secret.is_some() {
            bail!("Open OAuth public clients must not configure client_secret");
        }
        let (auth_endpoint, token_endpoint, scope) = match provider.as_str() {
            "google" => (
                "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
                "https://oauth2.googleapis.com/token".to_owned(),
                "https://mail.google.com/".to_owned(),
            ),
            "microsoft" => {
                let tenant = oauth.tenant.as_deref().unwrap_or("common");
                if tenant.is_empty()
                    || !tenant
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
                {
                    bail!("invalid OAuth Microsoft tenant identifier");
                }
                (format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize"),
                 format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"),
                 "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send".to_owned())
            }
            "open" => (String::new(), String::new(), open::MAIL_SCOPE.to_owned()),
            _ => bail!("unsupported OAuth provider; use google, microsoft or open"),
        };
        let flow = LoginFlow::parse(oauth.flow.as_deref(), &provider)?;
        let callback_host = oauth.callback_host.as_deref().unwrap_or("127.0.0.1").trim();
        if !matches!(callback_host, "127.0.0.1" | "localhost") {
            bail!("OAuth callback_host must be localhost or 127.0.0.1");
        }
        if provider == "open" && callback_host != "127.0.0.1" {
            bail!("public-client OAuth registration requires callback_host = \"127.0.0.1\"");
        }
        let callback_path = oauth.callback_path.as_deref().unwrap_or("/oauth/callback");
        if provider == "open" && callback_path != "/oauth/callback" {
            bail!("public-client OAuth requires callback_path = \"/oauth/callback\"");
        }
        if !valid_callback_path(callback_path) {
            bail!(
                "OAuth callback_path must begin with / and contain only letters, digits, /, _ or -"
            );
        }
        if flow != LoginFlow::Browser
            && (oauth.callback_host.is_some() || oauth.callback_path.is_some())
        {
            bail!("OAuth callback_host and callback_path only apply to flow = \"browser\"");
        }
        let prompt = oauth.prompt.as_deref().map(str::trim);
        if let Some(value) = prompt {
            let valid = match provider.as_str() {
                "google" => matches!(value, "consent" | "select_account" | "none"),
                "microsoft" => matches!(value, "login" | "consent" | "select_account" | "none"),
                _ => false,
            };
            if !valid {
                bail!("unsupported OAuth prompt for the selected provider");
            }
        }
        if let Some(hint) = &oauth.login_hint
            && (hint.len() > 254 || hint.is_empty() || hint.chars().any(char::is_control))
        {
            bail!("OAuth login_hint must be a nonempty single-line address or username");
        }
        if let Some(namespace) = oauth.credential_namespace.as_deref()
            && (namespace.is_empty()
                || namespace.len() > 48
                || !namespace
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')))
        {
            bail!("OAuth credential_namespace must contain 1-48 ASCII letters, digits, _ or -");
        }
        let issuer = if provider == "open" {
            let value = oauth.issuer.as_deref().ok_or_else(|| anyhow!(
                "Open OAuth provider needs [accounts.oauth].issuer (HTTPS mail server or authorization server)"
            ))?;
            validate_open_issuer(value)?;
            Some(value.to_owned())
        } else {
            if oauth.issuer.is_some() {
                bail!("OAuth issuer override applies only to provider = \"open\"");
            }
            None
        };
        let scope = if let Some(custom) = oauth.scope.as_deref() {
            if custom.trim().is_empty()
                || custom.len() > 2048
                || custom.chars().any(char::is_control)
            {
                bail!("OAuth scope must contain valid printable scopes");
            }
            custom.trim().to_owned()
        } else {
            scope
        };
        let revocation_endpoint =
            (provider == "google").then(|| "https://oauth2.googleapis.com/revoke".to_owned());
        let base = dirs::config_dir().context("finding OAuth credential directory")?;
        let path = token_path_for_account(&base, name, oauth.credential_namespace.as_deref())?;
        let credential_slot = credential_slot(&path);
        let credential_generation = credential_slot.generation.load(Ordering::Acquire);
        let settings = Self {
            provider,
            client_id,
            client_secret,
            token_endpoint,
            revocation_endpoint,
            scope,
            auth_endpoint,
            account_name: name.to_owned(),
            flow,
            callback_host: callback_host.to_owned(),
            callback_path: callback_path.to_owned(),
            prompt: prompt.map(str::to_owned),
            login_hint: oauth.login_hint.clone(),
            credential_namespace: oauth.credential_namespace.clone(),
            issuer,
            require_issuer_response: false,
            credential_slot,
            credential_generation,
        };
        if settings.provider == "open" {
            open::load_cached(settings)
        } else {
            Ok(settings)
        }
    }

    /// The wizard may edit server or account identity after sign-in. Never
    /// attach a provisional grant to a different name, client, or issuer.
    pub(crate) fn same_wizard_credential_identity(
        first: &AccountConfig,
        second: &AccountConfig,
    ) -> Result<bool> {
        let a = Self::for_account(first)?;
        let b = Self::for_account(second)?;
        Ok(match (a, b) {
            (Some(a), Some(b)) => {
                a.token_file()? == b.token_file()?
                    && a.provider == b.provider
                    && a.client_id == b.client_id
                    && a.token_endpoint == b.token_endpoint
                    && a.scope == b.scope
                    && a.issuer == b.issuer
            }
            _ => false,
        })
    }

    /// Prepare a one-shot remote revocation before the account's local token
    /// file is purged. No network I/O occurs on the terminal event thread.
    /// Google's project-wide grants are per Google user. Do not revoke a
    /// shared user's grant, but allow different Google users to share a client.
    pub(crate) fn prepare_revocation(
        account: &AccountConfig,
        remaining: &[AccountConfig],
    ) -> Option<RevocationRequest> {
        let settings = Self::for_account(account).ok()??;
        if settings.provider == "microsoft" {
            // Microsoft does not offer per-client RFC 7009 token revocation.
            // A tenant-wide session revocation is not equivalent.
            return None;
        }
        if settings.provider == "google"
            && google_revocation_conflicts(account, remaining, &settings.client_id)
        {
            return None;
        }
        let token = settings.read_refresh_token_unchecked().ok()?;
        Some(RevocationRequest { settings, token })
    }

    fn token_file(&self) -> Result<PathBuf> {
        let base = dirs::config_dir()
            .ok_or_else(|| anyhow!("cannot locate config directory for OAuth token"))?;
        token_path_for_account(
            &base,
            &self.account_name,
            self.credential_namespace.as_deref(),
        )
    }

    pub fn refresh_token(&self) -> Result<String> {
        self.check_credential_active()?;
        self.read_refresh_token_unchecked()
    }

    fn read_refresh_token_unchecked(&self) -> Result<String> {
        let path = self.token_file()?;
        let meta = fs::symlink_metadata(&path).with_context(|| {
            format!(
                "OAuth token missing for {:?}; run jerc --oauth-login {:?}",
                self.account_name, self.account_name
            )
        })?;
        if !meta.file_type().is_file() {
            bail!("OAuth token path is not a regular file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                bail!(
                    "OAuth token file permissions are too permissive; use chmod 600 on your token file"
                );
            }
        }
        let data: SavedToken =
            serde_json::from_slice(&fs::read(path)?).context("decoding saved OAuth credentials")?;
        if data.provider != self.provider
            || data.client_id != self.client_id
            || data.refresh_token.is_empty()
            || (self.provider == "open" && data.scope.as_deref() != Some(self.scope.as_str()))
        {
            bail!("OAuth credentials do not match the configured account/client; sign in again");
        }
        Ok(data.refresh_token)
    }

    fn save_refresh_token(&self, token: &str) -> Result<()> {
        if token.is_empty() {
            bail!("OAuth server provided no refresh token");
        }
        let path = self.token_file()?;
        let directory = path
            .parent()
            .ok_or_else(|| anyhow!("invalid OAuth token directory"))?;
        fs::create_dir_all(directory).context("creating OAuth credential directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        let mut staging = tempfile::NamedTempFile::new_in(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            staging
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let data = SavedToken {
            provider: self.provider.clone(),
            client_id: self.client_id.clone(),
            refresh_token: token.to_owned(),
            scope: if self.provider == "open" {
                Some(self.scope.clone())
            } else {
                None
            },
        };
        serde_json::to_writer(&mut staging, &data)?;
        staging.flush()?;
        staging
            .persist(&path)
            .map_err(|err| anyhow!("writing OAuth credentials: {}", err.error))?;
        Ok(())
    }

    pub fn access_token(&self) -> Result<String> {
        if self.provider == "open" && self.client_id.is_empty() {
            bail!(
                "Open OAuth client is not registered; sign in through Jerc's browser OAuth flow first"
            );
        }
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
        // Hold one per-credential gate across the network refresh, rotation and
        // persistence. A second caller receives the cached access token when
        // the server advertised a safe lifetime, or refreshes sequentially.
        let mut session = self
            .credential_slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.check_credential_active()?;
        if let Some(cached) = &session.cached_access
            && cached.provider == self.provider
            && cached.client_id == self.client_id
            && cached.requested_scope == self.scope
            && cached.valid_at(Instant::now(), SystemTime::now())
        {
            return Ok(TokenReply {
                access_token: cached.token.clone(),
                refresh_token: None,
                expires_in: None,
                scope: cached.scope.clone(),
            });
        }
        let refresh = self.refresh_token()?;
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", self.client_id.as_str()),
        ];
        if self.provider == "microsoft" {
            form.push(("scope", self.scope.as_str()));
        }
        if let Some(secret) = self.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let reply = self.request_token(&form)?;
        // Invalidation is atomic and does not wait for the HTTP timeout.
        if refresh_may_persist(&self.credential_slot, self.credential_generation)
            && let Some(rotated) = reply
                .refresh_token
                .as_deref()
                .filter(|value| !value.is_empty())
        {
            // If deletion began during HTTP, the cleanup job is waiting on
            // our gate and will revoke/purge this final rotated token.
            self.save_refresh_token(rotated)?;
        }
        self.check_credential_active()?;
        if let Some(ttl) = reply
            .expires_in
            .map(|seconds| seconds.saturating_sub(120))
            .filter(|seconds| *seconds > 0)
        {
            session.cached_access = Some(CachedAccess {
                token: reply.access_token.clone(),
                scope: reply.scope.clone(),
                provider: self.provider.clone(),
                client_id: self.client_id.clone(),
                requested_scope: self.scope.clone(),
                expires_at: Instant::now() + Duration::from_secs(ttl.min(45 * 60)),
                expires_at_wall: SystemTime::now() + Duration::from_secs(ttl.min(45 * 60)),
            });
        } else {
            session.cached_access = None;
        }
        Ok(reply)
    }

    fn check_credential_active(&self) -> Result<()> {
        if self.credential_slot.retired.load(Ordering::Acquire)
            || self.credential_slot.generation.load(Ordering::Acquire) != self.credential_generation
        {
            bail!("OAuth credentials were replaced or account was removed");
        }
        Ok(())
    }

    fn request_token(&self, form: &[(&str, &str)]) -> Result<TokenReply> {
        if self.provider == "open" && (self.token_endpoint.is_empty() || self.client_id.is_empty())
        {
            bail!("Open OAuth sign-in is incomplete; sign in through the browser first");
        }
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(HTTP_TIMEOUT))
                .https_only(true)
                .http_status_as_error(false)
                .build(),
        );
        // Never print request parameters, raw error JSON, or token responses.
        let mut response = agent
            .post(&self.token_endpoint)
            .send_form(form.iter().copied())
            .map_err(|_| anyhow!("OAuth token request failed (network/TLS error)"))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let failure: OAuthFailure = response.body_mut().read_json().unwrap_or_default();
            let message = safe_oauth_failure(status, &failure);
            if self.provider == "google" && status == 400 {
                bail!(
                    "{message}. Verify the FULL Google desktop client ID, any matching desktop client_secret issued for that registration, and the exact localhost redirect URI. A browser callback alone does not mean the token exchange succeeded"
                );
            }
            bail!("{message}");
        }
        let token: TokenReply = response
            .body_mut()
            .read_json()
            .context("decoding OAuth token response")?;
        if token.access_token.is_empty() {
            bail!("OAuth token endpoint returned no access token");
        }
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
        random
            .fill(&mut verifier_bytes)
            .map_err(|_| anyhow!("generating OAuth PKCE secret"))?;
        random
            .fill(&mut state_bytes)
            .map_err(|_| anyhow!("generating OAuth state"))?;
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let state = URL_SAFE_NO_PAD.encode(state_bytes);
        let challenge =
            URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()).as_ref());
        let mut authorization = Url::parse(&self.auth_endpoint)?;
        {
            let mut params = authorization.query_pairs_mut();
            params
                .append_pair("response_type", "code")
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

    fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
        quiet: bool,
    ) -> Result<()> {
        self.exchange_code_cancellable(code, redirect_uri, verifier, quiet, None)
    }

    fn exchange_code_cancellable(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
        quiet: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
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
        ensure_not_cancelled(cancel)?;
        self.finish_login_with_cancel(reply, quiet, cancel)
    }

    fn finish_login(&self, reply: TokenReply, quiet: bool) -> Result<()> {
        self.finish_login_with_cancel(reply, quiet, None)
    }

    fn finish_login_with_cancel(
        &self,
        reply: TokenReply,
        quiet: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
        if reply.access_token.is_empty() {
            bail!("OAuth server returned no access token");
        }
        let refresh = reply.refresh_token.ok_or_else(|| anyhow!(
            "OAuth provider did not return a refresh token; revoke authorization and retry with offline access"
        ))?;
        let mut session = self
            .credential_slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_not_cancelled(cancel)?;
        if self.credential_generation != self.credential_slot.generation.load(Ordering::Acquire) {
            bail!("OAuth sign-in superseded by account removal or a newer sign-in");
        }
        // Explicit sign-in begins a new credential generation. Older workers
        // can never overwrite these tokens, including after account recreation.
        self.save_refresh_token(&refresh)?;
        if let Err(error) = ensure_not_cancelled(cancel) {
            self.credential_slot.retired.store(true, Ordering::Release);
            self.credential_slot
                .generation
                .fetch_add(1, Ordering::AcqRel);
            let _ = fs::remove_file(self.token_file()?);
            return Err(error);
        }
        self.credential_slot
            .generation
            .fetch_add(1, Ordering::AcqRel);
        self.credential_slot.retired.store(false, Ordering::Release);
        session.cached_access = None;
        if !quiet {
            println!(
                "OAuth sign-in succeeded for {:?}; refresh token stored locally with private permissions.",
                self.account_name
            );
        }
        Ok(())
    }

    pub fn browser_login(&self) -> Result<()> {
        self.browser_login_inner(false)
    }

    /// TUI wizard uses a browser-only flow and renders its own feedback.
    /// Never print sensitive authorization URLs into the alternate screen.
    fn wizard_login(&self, cancel: Option<&AtomicBool>) -> Result<()> {
        if self.flow != LoginFlow::Browser {
            bail!("the account wizard currently requires browser OAuth");
        }
        self.browser_login_inner_with_cancel(true, cancel)
    }

    fn browser_login_inner(&self, quiet: bool) -> Result<()> {
        self.browser_login_inner_with_cancel(quiet, None)
    }

    fn browser_login_inner_with_cancel(
        &self,
        quiet: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
        ensure_not_cancelled(cancel)?;
        if self.provider == "open"
            && (self.client_id.is_empty()
                || self.auth_endpoint.is_empty()
                || self.token_endpoint.is_empty())
        {
            let resolved = open::resolve_for_login(self)?;
            ensure_not_cancelled(cancel)?;
            return resolved.browser_login_inner_with_cancel(quiet, cancel);
        }
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).context("binding OAuth callback to loopback")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://{}:{port}{}", self.callback_host, self.callback_path);
        let (authorization, verifier, state) = self.authorization_url(&redirect_uri)?;
        ensure_not_cancelled(cancel)?;
        if open_browser(authorization.as_str()) {
            if !quiet {
                println!(
                    "Check your browser to finish signing in to {:?}.",
                    self.account_name
                );
            }
        } else if quiet {
            bail!(
                "could not open a browser; run --oauth-login for the registered account from a normal terminal"
            );
        } else {
            println!(
                "Could not launch your browser. Open this URL to sign in:\n{}",
                authorization
            );
        }
        let started = Instant::now();
        let code = loop {
            ensure_not_cancelled(cancel)?;
            if started.elapsed() >= CALLBACK_TIMEOUT {
                bail!(
                    "OAuth sign-in timed out. If the browser reports a redirect URI mismatch, run --oauth-inspect for this account and register the exact redirect host/path for your application"
                );
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut buffer = [0u8; 8192];
                    let count = stream.read(&mut buffer)?;
                    let request = std::str::from_utf8(&buffer[..count]).unwrap_or("");
                    let first_line = request.lines().next().unwrap_or("");
                    let Some(target) = first_line
                        .strip_prefix("GET ")
                        .and_then(|v| v.split_once(' ').map(|(p, _)| p))
                    else {
                        continue;
                    };
                    let url = Url::parse(&format!("http://{}:{port}{target}", self.callback_host))?;
                    if url.path() != self.callback_path
                        || url.host_str() != Some(self.callback_host.as_str())
                        || url.port_or_known_default() != Some(port)
                    {
                        continue;
                    }
                    let params: std::collections::HashMap<String, String> = url
                        .query_pairs()
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                    let matching_state = params.get("state").is_some_and(|value| value == &state)
                        && (self.provider != "open"
                            || match params.get("iss") {
                                Some(reported) => self.issuer.as_deref().is_some_and(|issuer| {
                                    issuer.trim_end_matches('/') == reported.trim_end_matches('/')
                                }),
                                None => !self.require_issuer_response,
                            });
                    let code = if matching_state {
                        params.get("code").cloned()
                    } else {
                        None
                    };
                    let message = if code.is_some() {
                        "Authorization received. Return to jerc."
                    } else {
                        "Authorization unsuccessful. Return to jerc."
                    };
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
                        message.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                    if !matching_state {
                        // Other local processes can reach this ephemeral port.
                        // Ignore unrelated callbacks rather than abort login.
                        continue;
                    }
                    if let Some(code) = code {
                        break code;
                    }
                    bail!("OAuth authorization declined or returned no authorization code");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50))
                }
                Err(error) => return Err(error.into()),
            }
        };
        ensure_not_cancelled(cancel)?;
        self.exchange_code_cancellable(&code, &redirect_uri, &verifier, quiet, cancel)
    }

    /// Microsoft native-client redirect compatible with the authorization-code
    /// mode used by Mutt/NeoMutt. The browser returns to a registered HTTPS URI
    /// rather than an jerc localhost listener, so the user pastes the URL.
    pub fn nativeclient_login(&self) -> Result<()> {
        let (authorization, verifier, state) = self.authorization_url(MICROSOFT_NATIVE_REDIRECT)?;
        if open_browser(authorization.as_str()) {
            println!(
                "Check your browser to finish signing in to {:?}.",
                self.account_name
            );
        } else {
            println!("Open this URL to sign in:\n{authorization}");
        }
        println!(
            "After authorization, copy the COMPLETE redirect URL from the browser address bar"
        );
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
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(HTTP_TIMEOUT))
                .https_only(true)
                .http_status_as_error(false)
                .build(),
        );
        let mut response = agent
            .post(&endpoint)
            .send_form([
                ("client_id", self.client_id.as_str()),
                ("scope", self.scope.as_str()),
            ])
            .map_err(|_| anyhow!("Microsoft device authorization request failed"))?;
        if !response.status().is_success() {
            bail!(
                "Microsoft declined the device-code request; the application may not permit public-client authentication"
            );
        }
        let device: DeviceAuthorization = response
            .body_mut()
            .read_json()
            .context("decoding Microsoft device authorization")?;
        if device.device_code.is_empty() || device.user_code.is_empty() || device.expires_in == 0 {
            bail!("Microsoft returned an incomplete device-code challenge");
        }
        let verification =
            Url::parse(&device.verification_uri).context("invalid Microsoft verification URL")?;
        if verification.scheme() != "https"
            || verification.host_str() != Some("microsoft.com")
                && verification.host_str() != Some("www.microsoft.com")
        {
            bail!("Microsoft returned an unexpected device-code verification URL");
        }
        println!(
            "Sign in to {:?} in your browser and enter code: {}",
            self.account_name, device.user_code
        );
        if !open_browser(verification.as_str()) {
            println!("Open {} in your browser.", verification);
        }
        let started = Instant::now();
        let mut interval = device.interval.unwrap_or(5).clamp(5, 60);
        while started.elapsed() < Duration::from_secs(device.expires_in) {
            thread::sleep(Duration::from_secs(interval));
            if started.elapsed() >= Duration::from_secs(device.expires_in) {
                break;
            }
            let mut result = agent
                .post(&self.token_endpoint)
                .send_form([
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", self.client_id.as_str()),
                    ("device_code", device.device_code.as_str()),
                ])
                .map_err(|_| anyhow!("Microsoft device-code token request failed"))?;
            let reply: DevicePollReply = result
                .body_mut()
                .read_json()
                .context("decoding Microsoft device-code token response")?;
            if result.status().is_success() {
                let access_token = reply
                    .access_token
                    .ok_or_else(|| anyhow!("Microsoft returned no access token"))?;
                return self.finish_login(
                    TokenReply {
                        access_token,
                        refresh_token: reply.refresh_token,
                        expires_in: None,
                        scope: None,
                    },
                    false,
                );
            }
            match reply.error.as_deref() {
                Some("authorization_pending") => {}
                Some("slow_down") => interval = interval.saturating_add(5).min(60),
                Some("authorization_declined" | "access_denied") => {
                    bail!("Microsoft authorization was declined")
                }
                Some("expired_token") => {
                    bail!("Microsoft device sign-in code expired; retry login")
                }
                Some("invalid_client" | "unauthorized_client") => bail!(
                    "This app registration cannot use Microsoft device-code sign-in; try flow = \"nativeclient\" or register an jerc client"
                ),
                _ => bail!(
                    "Microsoft rejected device-code authorization; check permissions and account policy"
                ),
            }
        }
        bail!("Microsoft device sign-in code expired; retry login")
    }
}

fn valid_callback_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= 128
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-'))
}

fn parse_nativeclient_callback(input: &str, state: &str) -> Result<String> {
    let result = Url::parse(input.trim()).context("expected the full Microsoft redirect URL")?;
    let expected = Url::parse(MICROSOFT_NATIVE_REDIRECT)?;
    if result.origin() != expected.origin() || result.path() != expected.path() {
        bail!("redirect URL is not the expected Microsoft native-client callback");
    }
    let params: std::collections::HashMap<String, String> = result
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if params.get("state").is_none_or(|received| received != state) {
        bail!("OAuth redirect state mismatch");
    }
    let code = params
        .get("code")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("Microsoft authorization returned no code; check consent and retry")
        })?;
    Ok(code.clone())
}

fn open_browser(url: &str) -> bool {
    // Browser launchers may inherit our terminal and print GTK/desktop
    // warnings even after spawning the browser. Keep OAuth's CLI output
    // limited to actionable status messages; browser errors stay in the UI.
    #[cfg(target_os = "linux")]
    {
        spawn_browser(Command::new("xdg-open").arg(url))
    }
    #[cfg(target_os = "macos")]
    {
        return spawn_browser(Command::new("open").arg(url));
    }
    #[cfg(windows)]
    {
        return spawn_browser(Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = url;
        false
    }
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
    let account = accounts
        .iter()
        .find(|account| account.name == name)
        .ok_or_else(|| anyhow!("no account named {name:?} in config"))?;
    let settings = OAuthSettings::for_account(account)?
        .ok_or_else(|| anyhow!("account {name:?} has no [accounts.oauth] configuration"))?;
    println!("OAuth configuration for {:?}:", account.name);
    println!("  Provider: {}", settings.provider);
    if !settings.client_id.is_empty() {
        println!("  Client ID: configured (value intentionally not printed)");
    }
    if let Some(issuer) = settings.issuer.as_deref() {
        println!("  OAuth issuer: {issuer}");
    }
    if settings.provider == "open" && settings.client_id.is_empty() {
        println!("  Client ID: will be registered dynamically on first browser sign-in");
    }
    if !settings.auth_endpoint.is_empty() {
        println!("  Authorization endpoint: {}", settings.auth_endpoint);
    }
    match settings.flow {
        LoginFlow::Browser => println!(
            "  Redirect to register: http://{}:<ephemeral-port>{}",
            settings.callback_host, settings.callback_path,
        ),
        LoginFlow::NativeClient => println!("  Redirect to register: {MICROSOFT_NATIVE_REDIRECT}"),
        LoginFlow::DeviceCode => {
            println!("  Redirect: none (device-code flow; requires public-client permission)")
        }
    }
    println!("  Requested scopes: {}", settings.scope);
    println!(
        "  Login prompt: {}",
        settings
            .prompt
            .as_deref()
            .unwrap_or(if settings.provider == "google" {
                "consent (default)"
            } else {
                "provider default"
            },)
    );
    if settings.login_hint.is_some() {
        println!("  Login hint: configured");
    }
    println!(
        "  Token store: {}",
        settings
            .credential_namespace
            .as_deref()
            .unwrap_or("default")
    );
    if settings.flow == LoginFlow::Browser {
        if settings.provider == "open" {
            println!("  A compatible server will register this loopback redirect automatically.");
        } else {
            println!(
                "  Register the callback host and path; the loopback port is selected at runtime."
            );
        }
    }
    println!("  Note: this does not verify which redirects, delegated scopes, or consent policies");
    println!(
        "  are registered for the selected client ID. Provider/admin approval is still required."
    );
    Ok(())
}

/// Browser-only login for the editable account wizard. This never exposes
/// OAuth URL state, codes, or tokens in the TUI's terminal output.
/// Esc sets this flag, aborting the loopback wait. Closing the external
/// browser cannot be observed by Jerc, so cancellation is user-controlled.
pub fn login_account_quiet_cancellable(
    accounts: &[AccountConfig],
    name: &str,
    cancel: &AtomicBool,
) -> Result<()> {
    ensure_not_cancelled(Some(cancel))?;
    let account = accounts
        .iter()
        .find(|account| account.name == name)
        .ok_or_else(|| anyhow!("no account named {name:?} in config"))?;
    let settings = OAuthSettings::for_account(account)?
        .ok_or_else(|| anyhow!("account {name:?} has no OAuth settings"))?;
    settings.wizard_login(Some(cancel))
}

fn ensure_not_cancelled(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        bail!("OAuth sign-in cancelled");
    }
    Ok(())
}

pub fn login_account(accounts: &[AccountConfig], name: &str) -> Result<()> {
    let account = accounts
        .iter()
        .find(|account| account.name == name)
        .ok_or_else(|| anyhow!("no account named {name:?} in config"))?;
    let settings = OAuthSettings::for_account(account)?
        .ok_or_else(|| anyhow!("account {name:?} has no [accounts.oauth] configuration"))?;
    settings.login()
}

#[cfg(test)]
mod tests;
