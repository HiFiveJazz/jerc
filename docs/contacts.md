# Contacts and autocomplete

[← Back to README](../README.md) · [Keybindings](keybindings.md)

Jerc includes a local contacts manager and recipient suggestions./<

## Recipient Suggestions

When composing in **To**, **Cc**, or **Bcc**, type part of a known name or email address. A small popup offers fuzzy-ranked contacts. Use `Ctrl-j` / `Ctrl-k` (or `Ctrl-n` / `Ctrl-p`) to navigate and `Tab` or `Enter` to accept. `Esc` dismisses the popup. Open the full Contacts manager with `Alt-c`.

## Contacts Manager

Press **`Alt-c`** from Jerc to open Contacts. The left panel shows names; the right panel shows details. Press `?` for the searchable Contacts keymap.

- `/` filters contacts by name or email. Enter or Escape finishes editing the query.
- `j` / `k`, counts, `gg` / `G`, and `Ctrl-j` / `Ctrl-k` navigate the list.
- `v` / `V` creates a visual range; `Space` marks individual contacts or converts a range into marks.
- `a` adds a contact; `e` edits; `p` toggles favorites; `d` opens a deletion confirmation.
- Marked contacts use checkmarks in place of the usual favorite/saved icon, with a selection gutter beside selected rows.
- While composing, `Enter` can insert selected contacts into the active recipient field.

The active UI theme controls selection colors. Change `[ui] visual_selection_style` to `gutter` or `row` to control how selection is painted.

## Storage and Privacy

On Linux, contact records are typically stored in `~/.local/share/jerc/contacts.json`. They contain names, email addresses, and favorite/source metadata; they are **not end-to-end encrypted**. Storage uses a local JSON database and atomic replacement. Nothing is synced to a contacts service automatically.

Jerc can learn addresses from already-loaded message headers (Inbox senders and Sent recipients) without examining message bodies solely to build contacts. Junk and Trash are excluded. You can disable suggestions or learning:

```toml
[contacts]
autocomplete = true
learn_from_inbox = true
learn_from_sent = true
```

Previously stored contacts remain available even if learning is disabled. No Google People or Microsoft Graph contact syncing is currently implemented.

### Your own address

Configured account **From** identities are included in the contact search and
recipient autocomplete, including when typing a single character of your name.
You can address an email to yourself without adding a contact manually.
Existing saved contacts take precedence, and Jerc derives these identities from
local configuration without contacting an external address-book service.

**Editing contacts:** Name and Email share the composer's Insert (green), Normal (blue), and Visual (purple) modes. Press `Esc` to leave Insert/Visual, `i`/`a`/`A` to insert, `h`/`l`/`w`/`b` to navigate, `v`/`V` to select, `yy`/`Y` to yank, `p`/`P` to paste, and `u` to undo. `Ctrl-W` clears the current Name or Email field. Tab switches fields; Enter advances to Email or saves.
