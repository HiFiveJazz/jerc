# Keybindings

[← Back to README](../README.md)

**Press `?` in Jerc for the searchable shortcut palette for your current view.** This document is a quick reference for the defaults; some actions depend on focus and mode. `Ctrl` / `Alt` are modifier keys, and uppercase letters mean `Shift` + that letter. 

> [!NOTE]
> Keybinding support in the client and in the config file are being worked on. As of now, not all keybinds are modifiable. This will be updated in feature update soon!

## Mail browser

| Key | Action |
| --- | --- |
| `j` / `k` or `↓` / `↑` | Next / previous message |
| `J` / `K` | Next / previous mailbox |
| `gg` / `G` | First / last item |
| `Ctrl-j` / `Ctrl-k` | Smooth-scroll messages |
| `Ctrl-Shift-j` / `Ctrl-Shift-k` | Smooth-scroll mailbox list |
| `Ctrl-d` / `Ctrl-u` | Page down / up |
| `Enter` | Open message in the reader |
| `/` | Filter current mailbox |
| `Space` | Toggle mark and advance one item |
| `v` / `V` | Start or end visual selection; `Space` marks selected range |
| `Esc` | Clear marks or visual selection before leaving the view |
| `a` | Archive selected mail |
| `d` / `D` | Delete with confirmation / immediately |
| `s` | Move to Junk or restore from Junk |
| `u` | Toggle read/unread |
| `U` | Restore archived mail to Inbox |
| `c` | Compose new mail |
| `r` / `R` | Reply / reply with quote |
| `A` / `L` | Reply all / reply to list |
| `f` | Forward |
| `p` | Toggle split preview |
| `b` | Show or hide mailbox sidebar |
| `Alt-h` / `Alt-l` | Resize mailbox sidebar |
| `Alt-Shift-h` / `Alt-Shift-l` | Resize message preview |

## Reader

| Key | Action |
| --- | --- |
| `j` / `k`, `gg` / `G` | Navigate lines |
| `w` / `b` / `e`, `0` / `$` | Vim word and line motions |
| `v` / `V` | Visual selection |
| `y` | Copy selected text |
| `t` | Toggle HTML / plain text |
| `Shift-C` (`C`) | Toggle conversation quote colors / quoted Git diff colors (session only) |
| `i` | Toggle remote images for the session |
| `m` | Toggle expanded message headers |
| `o` | Open link under the cursor |
| `O` | Open original HTML externally |
| `Ctrl-a` | Open received attachments |
| `e` / `E` | Export raw message |
| `Esc` | Exit selection / return to browser |

## Compose

Normal, Insert, and Visual modes share Vim-style movement. Search the Compose `?` palette for context-sensitive actions and text objects.

| Key | Action |
| --- | --- |
| `C` (Shift-C) in Normal/Visual | Toggle conversation quote / quoted Git diff colors (session only) |
| `Tab` / `Shift-Tab` | Next / previous field |
| `Enter` in Normal | Preview before sending |
| `Esc` in Insert | Return to Normal mode (also dismisses recipient autocomplete) |
| `yy` / `Y` (Normal) | Yank whole body line or entire To/Cc/Bcc/Subject field; prefix with a count for multiple body lines |
| `v` / `V`, then `y` (Visual) | Yank selected characters / full body lines (or the entire current header field with `V`) |
| `y$` (Normal) | Yank from cursor to end of line |
| `x` (Normal / Visual) | Cut character(s) or selected text into the register and system clipboard, when available |
| `p` / `P` (Normal) | Paste after / before cursor or below / above line |
| `Ctrl-W` (any mode) | Clear current body line or complete To/Cc/Bcc/Subject field |
| `p` (Visual) | Replace selection with last yanked text |
| `Esc` / `q` in Normal | Exit prompt: save or discard draft |
| `Ctrl-a` | Open attachment file picker |
| `Ctrl-d` | Manage attached files |
| `Ctrl-o` | Advanced headers |
| `Ctrl-p` | Save draft |
| `Alt-c` | Open Contacts |
| `Ctrl-j` / `Ctrl-k` | Navigate contact suggestions when shown |
| `Ctrl-n` / `Ctrl-p` | Alternative suggestion navigation when suggestions are active |
| `Tab` / `Enter` | Accept a contact suggestion when shown |

Plain yanks (`y`, `yy`, `Y`) use a session-only internal register shared with the reader; they do not automatically copy to the system clipboard. Explicit cuts with `x` also attempt a system-clipboard copy and retain the internal register if the clipboard is unavailable. The register is not written to postponed drafts. The reader's `y` also attempts to copy to the system clipboard as before. For externally copied text, use your terminal's normal paste shortcut in Insert mode.

From the send preview, `Enter` sends; `Esc` returns to the editor. Saving a draft is not the same as sending it.

Jerc highlights mailing-list reply attributions (such as “On ... wrote:”), `>` quoted text, and deeper `>>`/`> >` quote levels using the active theme. The author’s unquoted reply keeps the normal foreground color. Git diff highlighting still takes precedence inside actual patch hunks. This is a native Rust text renderer; no Tree-sitter grammar or additional parser is required.

## Contacts (`Alt-c`)

| Key | Action |
| --- | --- |
| `j` / `k`, `gg` / `G` | Move / first / last contact |
| `H` / `M` / `L` | Top / middle / bottom visible contact |
| `Ctrl-j` / `Ctrl-k` | Smooth-scroll contact list |
| `/` | Fuzzy search |
| `Space` | Mark contact; Visual mode marks range |
| `v` / `V` | Select a visual range |
| `a` / `e` | Add / edit contact |
| `p` | Toggle favorite |
| `d` | Delete selection with confirmation |
| `Enter` | Edit or insert into an active recipient field |
| `?` | Search contact shortcuts |
| `Esc` / `q` | Clear selection or close Contacts |

See [Contacts](contacts.md) for importing/learning behavior and storage details.

## Received attachments

| Key | Action |
| --- | --- |
| `j` / `k` | Choose an attachment |
| `Space` / `a` | Toggle mark / mark all |
| `v` / `V` | Visual selection |
| `o` / `Enter` | Open externally |
| `s` | Save selected attachments |
| `p` | Refresh preview |
| `?` | Attachment shortcuts |
| `Esc` / `q` | Clear selection or close |

## Attachment file picker

| Key | Action |
| --- | --- |
| `j` / `k` | Move through entries |
| `h` / `l` | Go back / enter directory |
| `Space` / `Enter` | Toggle attachment / open directory |
| `v` / `V` | Select a range |
| `.` | Toggle hidden files |
| `/` | Search current directory |
| `~` / `d` | Home / Downloads directory |
| `x` | Review selected attachments for detaching |
| `?` | Search picker shortcuts |
| `Esc` / `q` | Exit selection or close picker |

## Accounts and general

| Key | Action |
| --- | --- |
| `Alt-n` / `Alt-p` | Next / previous account |
| `Alt-1`–`Alt-9` | Jump to a numbered account |
| `Alt-a` / `Alt-d` | Add / remove account |
| `Alt-c` | Contacts |
| `T` | Theme picker |
| `Ctrl-r` | Refresh mailbox |
| `B` | Toggle terminal bell |
| `n` | Toggle desktop notifications |
| `!` | Configuration warnings |
| `?` | Contextual help |
| `q` / `Ctrl-c` | Quit from mail browser |

Terminal key handling can vary with your terminal and modifiers. When unsure, use `?` in the relevant view.

Saved drafts: open a message inside the **Drafts** mailbox with `Enter` to preview it; select **[Y]es** to edit or **[N]o** to keep reading. `Ctrl-N` still resumes a locally postponed draft. The original server draft is retained until a replacement is saved or the message is sent successfully.
