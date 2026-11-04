//! Native desktop OAuth authorization-code + PKCE sign-in for IMAP/SMTP.
//! The application publisher must register public-client IDs with providers;
//! end users authorize in a browser and never handle refresh tokens manually.
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
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

#[derive(Clone)]
pub struct OAuthSettings {
    pub provider: String,
    pub client_id: String,
    client_secret: Option<String>,
    pub token_endpoint: String,
    pub scope: String,
    auth_endpoint: String,
    account_name: String,
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

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    refresh_token: Option<String>,
}

impl OAuthSettings {
    pub fn for_account(account: &AccountConfig) -> Result<Option<Self>> {
        let Some(oauth) = account.oauth.as_ref() else { return Ok(None); };
        Ok(Some(Self::from_config(&account.name, oauth)?))
    }

    fn from_config(name: &str, oauth: &OAuthConfig) -> Result<Self> {
        let provider = oauth.provider.trim().to_ascii_lowercase();
        let client_id = oauth.client_id.clone().or_else(|| match provider.as_str() {
            "google" => option_env!("AERC_RS_GOOGLE_CLIENT_ID").map(str::to_owned),
            "microsoft" => option_env!("AERC_RS_MICROSOFT_CLIENT_ID").map(str::to_owned),
            _ => None,
        }).filter(|id| !id.trim().is_empty()).ok_or_else(|| anyhow!(
            "OAuth account {name:?} needs oauth.client_id or an app-publisher client ID at build time"
        ))?;
        let client_secret = oauth.client_secret.clone().or_else(|| match provider.as_str() {
            "google" => option_env!("AERC_RS_GOOGLE_CLIENT_SECRET").map(str::to_owned),
            _ => None,
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
        Ok(Self { provider, client_id, client_secret, token_endpoint, scope, auth_endpoint, account_name: name.to_owned() })
    }

    fn token_file(&self) -> Result<PathBuf> {
        let base = dirs::config_dir().ok_or_else(|| anyhow!("cannot locate config directory for OAuth token"))?;
        // Hash the name to keep a user-supplied account label out of paths.
        let digest = digest::digest(&digest::SHA256, self.account_name.as_bytes());
        let key: String = digest.as_ref()[..16].iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(base.join("aerc-rs").join("oauth").join(format!("{key}.json")))
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
        let refresh = self.refresh_token()?;
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", self.client_id.as_str()),
        ];
        if let Some(secret) = self.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let reply = self.request_token(&form)?;
        if let Some(rotated) = reply.refresh_token.as_deref().filter(|value| !value.is_empty()) {
            self.save_refresh_token(rotated)?;
        }
        Ok(reply.access_token)
    }

    fn request_token(&self, form: &[(&str, &str)]) -> Result<TokenReply> {
        let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT)).https_only(true).build());
        // Do not print the response body or the URL query parameters. They may
        // contain tokens or provider-specific credential details.
        let mut response = agent.post(&self.token_endpoint).send_form(form.iter().copied())
            .map_err(|_| anyhow!("OAuth token exchange failed; check consent and provider configuration"))?;
        let token: TokenReply = response.body_mut().read_json()
            .context("decoding OAuth token response")?;
        if token.access_token.is_empty() { bail!("OAuth token endpoint returned no access token"); }
        Ok(token)
    }

    pub fn browser_login(&self) -> Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("binding OAuth callback to loopback")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");
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
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("scope", &self.scope)
                .append_pair("state", &state)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256");
            if self.provider == "google" {
                params.append_pair("access_type", "offline").append_pair("prompt", "consent");
            }
        }
        println!("Authorize account {:?} in your browser.", self.account_name);
        if !open_browser(authorization.as_str()) {
            println!("Open this authorization URL in your browser:\n{}", authorization);
        }
        let started = Instant::now();
        let code = loop {
            if started.elapsed() >= CALLBACK_TIMEOUT { bail!("OAuth sign-in timed out"); }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut buffer = [0u8; 8192];
                    let count = stream.read(&mut buffer)?;
                    let request = std::str::from_utf8(&buffer[..count]).unwrap_or("");
                    let first_line = request.lines().next().unwrap_or("");
                    let Some(target) = first_line.strip_prefix("GET ").and_then(|v| v.split_once(' ').map(|(p, _)| p)) else { continue; };
                    let url = Url::parse(&format!("http://127.0.0.1:{port}{target}"))?;
                    if url.path() != "/oauth/callback" { continue; }
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
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", self.client_id.as_str()),
            ("code_verifier", verifier.as_str()),
        ];
        if let Some(secret) = self.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let reply = self.request_token(&form)?;
        let refresh = reply.refresh_token.ok_or_else(|| anyhow!(
            "OAuth provider did not return a refresh token; revoke authorization and retry with offline access"
        ))?;
        self.save_refresh_token(&refresh)?;
        println!("OAuth sign-in succeeded for {:?}; refresh token stored locally with private permissions.", self.account_name);
        Ok(())
    }
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "linux")]
    { return Command::new("xdg-open").arg(url).spawn().is_ok(); }
    #[cfg(target_os = "macos")]
    { return Command::new("open").arg(url).spawn().is_ok(); }
    #[cfg(windows)]
    { return Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn().is_ok(); }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    { let _ = url; false }
}

pub fn login_account(accounts: &[AccountConfig], name: &str) -> Result<()> {
    let account = accounts.iter().find(|account| account.name == name)
        .ok_or_else(|| anyhow!("no account named {name:?} in config"))?;
    let settings = OAuthSettings::for_account(account)?
        .ok_or_else(|| anyhow!("account {name:?} has no [accounts.oauth] configuration"))?;
    settings.browser_login()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn google_and_microsoft_use_distinct_imap_smtp_scopes() {
        let google = OAuthSettings::from_config("Personal", &OAuthConfig { provider: "google".into(), client_id: Some("public-client".into()), tenant: None, client_secret: None }).unwrap();
        assert_eq!(google.scope, "https://mail.google.com/");
        let microsoft = OAuthSettings::from_config("Work", &OAuthConfig { provider: "microsoft".into(), client_id: Some("public-client".into()), tenant: None, client_secret: None }).unwrap();
        assert!(microsoft.scope.contains("IMAP.AccessAsUser.All"));
        assert!(microsoft.scope.contains("SMTP.Send"));
        assert!(microsoft.scope.contains("offline_access"));
        assert!(!format!("{google:?}").contains("public-client"));
    }
    #[test]
    fn rejects_unexpected_providers_and_tenant_paths() {
        assert!(OAuthSettings::from_config("Work", &OAuthConfig { provider: "example".into(), client_id: Some("id".into()), tenant: None, client_secret: None }).is_err());
        assert!(OAuthSettings::from_config("Work", &OAuthConfig { provider: "microsoft".into(), client_id: Some("id".into()), tenant: Some("../x".into()), client_secret: None }).is_err());
    }
}
