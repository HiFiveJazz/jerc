# Native contacts and recipient autocomplete

No address-book daemon, Contacts API permission, external executable, or new dependency.

## Composer

In **To**, **Cc** or **Bcc**, type the first letter of a saved contact's name or email (or two
letters for a learned contact). A small popup lists fuzzy-ranked matching contacts; already-used
recipients are excluded, but your own email address can also be suggested. `Ctrl-n` / `Ctrl-p`
(or Up / Down) change the highlighted match. `Tab` or `Enter` inserts the full
`Name <email>` recipient; `Esc` dismisses without leaving Insert mode. A second
`Esc` returns to normal mode. Comma-separated recipients remain intact, including
quoted display names containing commas. `Alt-c` opens the full contact manager
from any composer field; `Enter` inserts a highlighted contact if To/Cc/Bcc is
active.

## Contact manager (`Alt-c`)

A vertically indexed list on the left and contact details on the right. The
current line has an absolute number; other rows display their distance from it.
Search with `/` (case-insensitive substring and subsequence matching), then Enter
or Esc to return to the list while keeping the filter. `?` opens searchable
shortcuts. No persistent bottom-row keymap is needed in the regular mail view.

- `j/k`, `5j`, `5k`, `gg`, `G`, `20G`, `H/M/L` — Vim-style list navigation.
- `Ctrl-j/k` — animated scrolling, including while searching; PageUp/PageDown move by the viewport.
- `a` — create contact; `e` or Enter — edit contact (Enter instead inserts into
  a recipient field when opened from To/Cc/Bcc).
- `p` — toggle favorite; `d` — delete with explicit confirmation.
- In the editor, Tab switches name/address, Enter advances from name to address
  or saves from address, and Esc cancels. `Ctrl-s` also saves.
- `q` or Esc closes the manager. Unsaved contact edits are discarded only when
  canceling their editor, not when simply opening/closing the manager.

## Storage, privacy and policies

`~/.local/share/aerc-rs/contacts.json` under the XDG data directory (or the
platform equivalent) contains only display names, email addresses and favorite
flags. Atomic replacement uses a private temporary file; malformed existing
JSON is **not** overwritten. Manual names take priority over learned metadata.
Up to 4,000 contacts can be stored; adding learned addresses never evicts a
saved or favorite entry. Nothing is uploaded to a contacts service.

Incoming Inbox senders and outgoing Sent recipients are learned incrementally
from already-loaded **message headers**, never by reading message bodies or
searching remote servers. Junk/Trash is excluded. Successful sends can teach the
book additional recipients. Changes are batched to avoid per-message disk I/O.

Optional `config.toml` settings (defaults shown):

```toml
[contacts]
autocomplete = true
learn_from_inbox = true
learn_from_sent = true
```

If you disable learning, previously stored contacts remain available for manual
editing and suggestions. This first version intentionally does **not** request
Google People or Microsoft Graph scopes or support remote contacts sync.
