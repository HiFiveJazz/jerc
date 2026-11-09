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
- Organizational Microsoft 365 accounts can require administrator consent, independent of correct client configuration.
- If an account cannot send, verify its outgoing server, login identity, SMTP permissions, and OAuth scopes before changing tokens.

For full field examples, see [`config.example.toml`](../config.example.toml).
