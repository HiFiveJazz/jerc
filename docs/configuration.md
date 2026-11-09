# Configuration

[← Back to README](../README.md) · [OAuth guide](oauth.md)

> [!NOTE]
> Jerc strives to never require users to edit a configuration file for normal use. That said, we leave options open for advanced/power users who desire configuring everything themselves!

Starting Jerc for the first time generates a `config.toml` from the operating system's user configuration directory. On Linux this is usually `~/.config/jerc/config.toml` (or `$XDG_CONFIG_HOME/jerc/config.toml`). Refer to the [example configuration](../config.example.toml), which documents additional fields.

## Accounts

You can configure multiple `[[accounts]]` entries. The following examples are **alternatives**, not all required at once.

### Local Maildir

```toml
[[accounts]]
name = "Local"
source = "maildir://~/Mail"
default_mailbox = "INBOX"
```

### IMAP and SMTP with an external password command

```toml
[[accounts]]
name = "Personal"
source = "imaps://you%40example.com@imap.example.com:993"
source_cred_cmd = "pass show mail/imap"
outgoing = "smtps://you%40example.com@smtp.example.com:465"
outgoing_cred_cmd = "pass show mail/smtp"
from = "Your Name <you@example.com>"
```


> [!CAUTION]
> Strive to use credential commands over placing reusable secrets directly in configuration files. Never commit a real account configuration to source control like git 

### OAuth accounts

```toml
[[accounts]]
name = "Personal"
email = "you@gmail.com"
source = "imap"
outgoing = "smtp"
copy_to = ""

[accounts.oauth]
provider = "google"
```

Sign in with `jerc --oauth-login Personal`. Google Workspace and Microsoft 365 setups, embedded client configuration, server discovery, and login-name overrides are documented in [OAuth & discovery](oauth.md).

### Hide unwanted mailboxes

```toml
[[accounts]]
name = "Local"
source = "maildir://~/Mail"
exclude_mailboxes = ["Spam", "Archive/*"]
```

`exclude_mailboxes` accepts exact mailbox names and `*`/`?` wildcards. It only hides entries from Jerc's sidebar; it does not unsubscribe from or delete the server folders.

## Interface

These are example choices you can set in a single `[ui]` table:

```toml
[ui]
theme = "tokyo-night"
border_style = "rounded"          # "square" or "rounded"
visual_selection_style = "gutter" # "gutter" or "row"
remote_images = false
new_mail_notifications = true
new_mail_bell = false
```

- **Themes:** Built-ins are in [`themes/`](../themes). Place custom JSON themes under your Jerc config directory's `themes/` folder. Use `T` to preview and save a theme.
- **Selection:** `gutter` shows colored selection markers, while `row` highlights the entire selected row, across mail, Contacts, and attachment lists.
- **Borders:** `square` is the default; `rounded` changes window corners without altering layout.
- **Remote images:** Disabled initially to avoid unsolicited network requests triggered by opening email. Press `i` in the reader to toggle for the session.
- **Mail refresh:** `inbox_poll_seconds` controls periodic Inbox status checks. Jerc doesn't yet use IMAP IDLE.

The [example config](../config.example.toml) documents preview widths, notification styling, contact learning, and other optional settings.

## Contact Data, Tokens, and Drafts

Jerc uses separate per-user directories for different kinds of data. The exact base directory depends on the operating system and XDG settings.

| Data | Linux Location |
| --- | --- |
| Config | `~/.config/jerc/config.toml` |
| Custom themes | `~/.config/jerc/themes/` |
| OAuth tokens | `~/.config/jerc/oauth/` |
| Contacts | `~/.local/share/jerc/contacts.json` |
| Local postponed drafts | `~/.local/share/jerc/postponed/` |
| Mail index cache | `~/.cache/jerc/mail-index/` |


## Command-line tools

```sh
jerc --version
jerc --discover-email you@example.com
jerc --oauth-login Personal
jerc --oauth-probe Personal
jerc --headless-check
```

All commands are available through `jerc --help`. For developer builds, substitute `cargo run --release --` for `jerc`.
