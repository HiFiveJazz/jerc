# OAuth and server discovery

[← Back to README](../README.md) · [Configuration](configuration.md)

Jerc can authenticate to supported IMAP and SMTP servers using OAuth or password-based credentials. Provider settings and server policy determine which options work for an account.

## Discover provider settings

Before creating an account, inspect suggested IMAP/SMTP settings:

```sh
jerc --discover-email you@example.com
jerc --discover-email you@example.edu --discover-provider microsoft
```

Discovery prints proposed configuration for you to inspect; it does not edit your configuration or authorize an account.

## Sign in with OAuth

Add an account to `config.toml`, including an `[accounts.oauth]` provider block. For Google Desktop OAuth, you can use the built-in application credentials if present or supply a separately registered public-client ID. On Microsoft 365, registration and administrator approval vary by organization.

```sh
jerc --oauth-build-info
jerc --oauth-login Personal
jerc --oauth-probe Personal
```

The build-info command reports whether compile-time OAuth settings are embedded without printing the client secret. A successful OAuth sign-in does **not** guarantee that the provider permits SMTP sending; the probe can test protocol authentication without submitting a message.

Build-time public-client variables for custom application registration:

```sh
JERC_GOOGLE_CLIENT_ID=... \
JERC_GOOGLE_CLIENT_SECRET=... \
JERC_MICROSOFT_CLIENT_ID=... \
cargo build --release
```

Only use these with an appropriately configured **desktop/public OAuth application**. User refresh tokens, passwords, and private keys must never be embedded in builds or committed to the repository.

## Microsoft 365 login names

Some providers expose a display email address that differs from the IMAP/SMTP login name. In that case, set `imap_username` and `smtp_username` separately in your `[[accounts]]` entry.

```toml
[[accounts]]
name = "University"
email = "person@example.edu"
source = "imap"
outgoing = "starttls"
imap_username = "login@example.edu"
smtp_username = "login@example.edu"

[accounts.oauth]
provider = "microsoft"
client_id = "YOUR_REGISTERED_PUBLIC_CLIENT_ID"
```

The discovery helper can suggest login names, but token claims are **hints**, not proof that a provider accepts the identity:

```sh
jerc --discover-email person@example.edu --discover-provider microsoft \
  --discover-username login@example.edu
jerc --discover-email person@example.edu --discover-oauth-account University
```

Check any suggested username against your provider's documentation or an existing working client.

## Security notes

- IMAP/SMTP OAuth bearer tokens and refresh tokens are credentials; treat them as secrets.
- Jerc's account configuration supports external credential commands to avoid embedding passwords in TOML.
- Organizational Microsoft 365 accounts can require administrator consent, independent of correct client configuration. If your organization supports Thunderbird, Jerc should work out of the box. Ask your administrator to approve using your email with Thunderbird in order to use it with Jerc.
- If an account cannot send, verify its outgoing server, login identity, SMTP permissions, and OAuth scopes before changing tokens.

For full field examples, see [`config.example.toml`](../config.example.toml).

## Browser OAuth for Stalwart and compatible self-hosted servers (experimental)

Jerc supports standards-based **Open Public Client** OAuth with PKCE, RFC 8414
server metadata and RFC 7591 public-client registration. The server must
provide a mail-capable OAuth token accepted by IMAP/SMTP using `OAUTHBEARER` or
`XOAUTH2`. The browser handles your provider's 2FA or passkey; Jerc does not
handle verification codes or WebAuthn secrets.

In the wizard, discover the usual IMAP/SMTP settings, set **Authentication →
OAuth2** for **Other IMAP/SMTP**, then sign in through the browser. The OAuth
issuer defaults to `https://<IMAP hostname>` and can be overridden under F3
advanced settings. For Stalwart, this is normally its HTTPS mail-server hostname.

Example for a Stalwart server using its own OAuth authorization server:

```toml
[[accounts]]
name = "FiveJazz"
email = "hi@fivejazz.com"
source = "imap"
outgoing = "smtps"
imap_host = "mail.fivejazz.com"
imap_port = 993
smtp_host = "mail.fivejazz.com"
smtp_port = 465

[accounts.oauth]
provider = "open"
issuer = "https://mail.fivejazz.com"
flow = "browser"
callback_host = "127.0.0.1"
callback_path = "/oauth/callback"
```

Run `jerc --oauth-login FiveJazz` and complete sign-in. No client ID is
needed for providers that allow public-client dynamic registration. Jerc
registers `http://127.0.0.1/oauth/callback`, and chooses an ephemeral local
port when signing in. The client registration is stored in a private local
file next to your OAuth refresh-token file so it can be reused after restart.

For another self-hosted OAuth identity provider, set `issuer` to its **exact
HTTPS issuer URL**. If dynamic registration is unavailable, register a native
public client with the issuer and add its `client_id` to `[accounts.oauth]`.
Some identity providers require different scopes; advanced settings allow
`scope` override, but the mail server must accept the resulting access tokens.

**Mailu (newer development versions):** Mailu documents a built-in OAuth issuer
at `https://<primary-hostname>/admin`, a public `mailu` client ID, and the
`imap smtp offline_access` scopes. In Jerc's advanced OAuth settings, provide
those values explicitly when that Mailu version's server supports them.
Stable or older Mailu releases may differ; confirm your server's metadata
first. Unlike Stalwart, this path uses a pre-established public client ID,
not dynamic registration.

This feature is **not** universal OAuth login for all self-hosted mail stacks:
mailcow, Mailu, and Dovecot deployments may use passwords, app passwords,
or an external identity provider depending on their configuration. If their
server does not advertise compatible OAuth metadata or accept OAuth bearer
tokens on IMAP/SMTP, use an app password instead. TLS remains mandatory; Jerc
never falls back from OAuth to basic authentication automatically.

**Security:** Only enter OAuth issuers you trust. Jerc validates HTTPS and
issuer identity, uses PKCE and callback state, and stores refresh tokens in a
private user-owned file (not encrypted at rest). An OAuth authorization-server
response can contain `iss`; if present, it must match the configured issuer.

### Automatic OAuth discovery (PACC)

The `--discover-email` flow first tries PACC at
`https://ua-auto-config.<email-domain>/.well-known/user-agent-configuration.json`.
When available, Jerc verifies the DNS TXT SHA-256 digest at
`_ua-auto-config.<email-domain>`, loads the IMAP/SMTP hosts and checks the
advertised OAuth issuer's authorization-server metadata before recommending
browser sign-in. OAuth registration and credential exchange happen **only**
after the user chooses to authenticate, not during discovery.

For self-hosted installations without a published PACC hostname, Jerc also
checks the same-domain `mail.<email-domain>` HTTPS authorization-server metadata
when both IMAP and SMTP were discovered at exactly that hostname. This
fallback is a suggestion, not a protocol-level authentication probe. Always
use **Test Configuration** after browser sign-in to verify IMAP and SMTP.

For Stalwart, the PACC domain alias must be configured in public DNS and
routed via HTTPS to Stalwart by a reverse proxy if one is in front of the
server. Normal/app-password authentication remains available when OAuth is
not advertised, or when the user selects it explicitly.
