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

### JMAP over HTTPS (including sending)

```toml
[[accounts]]
name = "Personal"
email = "you@example.com"
source = "jmaps://you%40example.com@mail.example.com"
source_cred_cmd = "pass show mail/personal"
outgoing = "jmap"
```

`outgoing = "jmap"` requires the server to advertise JMAP submission for the
primary mail account. It uses the incoming account credentials, discovers the
matching sending identity and submits through HTTPS; SMTP configuration is not
necessary. The setup wizard also supports this mode: select JMAP and clear the
SMTP hostname to disable the fallback.

Existing JMAP accounts with a configured SMTP URI prefer native submission when
advertised, using SMTP only if the capability is absent. A failed or uncertain
native submission is **never** retried through SMTP. As before, `outgoing = "none"`
or omitting `outgoing` disables sending; an explicit sendmail path keeps using
sendmail. The configured `from` (or `email`) must match a server identity.

The default copy policy moves the imported message from Drafts to Sent after
submission. `copy_to = "Archive, Sent"` files the same message in both folders;
`copy_to = "none"` removes the temporary imported draft after acceptance. If
filing fails, Jerc reports a warning without resending. Queued offline messages
use the same submission path, and ambiguous delivery results require manual review.

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

Sign in with `jerc --oauth-login Personal`.
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
threading = true            # group known conversations by default
mailbox_width_percent = 22  # Alt-h/l or drag the mailbox divider
preview_width_percent = 57  # Alt-Shift-h/l or drag the reader divider
new_mail_notifications = true
new_mail_bell = false
```

- **Themes:** Built-ins are in [`themes/`](../themes). Place custom JSON themes under your Jerc config directory's `themes/` folder. Use `T` to preview and save a theme.
- **Selection:** `gutter` shows colored selection markers, while `row` highlights the entire selected row, across mail, Contacts, and attachment lists.
- **Borders:** `square` is the default; `rounded` changes window corners without altering layout.
- **Remote images:** Disabled initially to avoid unsolicited network requests triggered by opening email. Press `i` in the reader to toggle for the session.
- **Panel widths:** The mailbox and split-preview percentages are persisted directly in this `[ui]` section when resized. Jerc no longer writes separate `mailbox-width` or `preview-width` files. Existing sidecar files are ignored; set the desired values here when upgrading.
- **Outgoing mail privacy:** Jerc does not announce its name/version through `User-Agent` or `X-Mailer` and uses random message identifiers. Standard `MIME-Version: 1.0` and UTF-8 `Content-Type` headers describe the format, not the client. Composing emails is plain-text does not create an HTML alternative, to keep Jerc in line with Mailing List Plain-Text workflows expectations from aerc. Providers can still add SMTP routing headers.

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
| Offline outgoing queue | `~/.local/share/jerc/outbox/` |
| Wizard-created password files | `~/.local/share/jerc/credentials/` |
| Mail index cache (including downloaded bodies and offline mutations) | `~/.cache/jerc/mail-index/` |


## Command-line tools

```sh
jerc -v
jerc -c ~/.config/jerc/config.toml
jerc -a Personal                   # Select a configured account
jerc --offline                    # Cached mail; no remote connections
jerc --check-config               # Validate without opening the TUI
jerc -m ~/Maildir -n Local
jerc --discover-email you@example.com
jerc --oauth-login Personal
jerc --oauth-probe Personal
jerc --headless-check

# View local files using the existing reader
jerc 0001-kernel-fix.patch
jerc message.eml
jerc mail-archive.mbox
jerc mbox:mail-archive.mbox
cat message.eml | jerc -
git format-patch -1 --stdout | jerc -

# Populate the existing composer; neither command sends automatically
jerc 'mailto:person@example.org?subject=Hello'
jerc --draft-patch 0001-kernel-fix.patch
jerc --draft-patch 0001-kernel-fix.patch --attach extra.txt
```

All options, `jerc -h` (or `jerc --help`). For developer builds, substitute `cargo run --release --` for `jerc`.
