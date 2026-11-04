# aerc-rs — milestone 5: fast modal mail workflow

A Rust terminal email client inspired by aerc's backend/worker model and lazygitrs' responsive, Vim-first UI. Milestone 5 moves the browser from a fixed header window toward a real working mail client: paged mailbox caches, live filtering, split preview, bulk triage, richer HTML, and a modal composer.

## What works

- Maildir and TLS IMAP backends with password/app-password, OAUTHBEARER/XOAUTH2, refresh-token exchange, UID mutations, SMTP sending (including RFC 7628 OAUTHBEARER when advertised), and bounded worker queues.
- **Paged mailbox loading:** the TUI fetches `page_size` headers at a time (100 by default), keeps loaded mail cached per mailbox, and automatically requests older pages near the bottom. Returning to an already-open mailbox restores its cached list immediately.
- **Fast mailbox switching:** `J/K` changes folders, while `Ctrl-Shift-j/Ctrl-Shift-k` smoothly moves the folder cursor without loading every intermediate folder.
- **Neovim-style message scrolling:** `j/k`, counts, `Ctrl-j/Ctrl-k`, `Ctrl-d/Ctrl-u`, `g/G`, and a four-row scrolloff. The renderer owns the visible slice, avoiding Ratatui offset artifacts and stale gutter contents.
- **Visual selection:** `v` starts an inclusive message range; `Esc` exits visual mode. Bulk archive/delete operates on the selected range.
- **Mail actions:** `d` confirms delete/trash, `D` performs the same action immediately, and `a` archives. Read/unread toggles update optimistically in the UI while IMAP confirms them in the background. Successful mutations update the current cache locally rather than reloading the mailbox tree or jumping back to INBOX.
- **Search/filter:** `/` edits a case-insensitive filter over the current mailbox. If the loaded cache does not contain enough matches, older pages continue loading progressively.
- **Layout controls:** `b` hides/shows the mailbox sidebar; `P` toggles a vertical message preview. `Enter` still opens the full-screen reader.
- **Rich HTML mail by default:** the HTML renderer is now the primary view whenever a real HTML part exists; `T` always switches to a plain view (including a generated fallback for HTML-only mail). Layout-table noise, literal Markdown-style heading markers, generic image labels and broken open table edges are normalized while semantic receipt/data tables remain boxed. Sender CSS colours never override the terminal theme.
- **Terminal images:** `ratatui-image` supports Kitty, Sixel and iTerm2 graphics with a half-block fallback. The first meaningful remote image is shown in the reader when remote images are enabled. Tracking pixels are filtered, downloads are bounded and happen on a background worker. Remote images default off for privacy; press `I` to toggle them for the session or set `ui.remote_images = true`.
- **Compose polish:** `j/k` in Normal mode moves through To/Cc/Bcc/Subject and enters Body, where Vim motions remain unchanged. The attachment picker now matches lazygitrs's three-pane Parent/Files/Preview layout with themed full-row selection, relative line numbers, file text previews, count motions and Ctrl-j/k smooth scrolling. `Space` selects files, `Ctrl+A` opens the picker (listed in `?`, not the footer). Attachments wrap to three rows before ellipsizing; empty fields are muted and filled ones use theme accents. `Enter` in Normal opens a full outgoing-message review with headers, recipients, attachments and body; `T` toggles HTML/plain for inline HTML forwards, `O` opens the outgoing HTML in a browser, `j/k` scroll, `Enter` sends and `Esc` edits.
- **Modal compose/reply/forward:** the composer has NORMAL, INSERT, and VISUAL modes, with lazygitrs-style filled mode badges (blue, light green, and purple) in its bottom-right footer, white block cursor outside Insert mode, relative body line numbers, expanded Vim motions/text objects/undo, and a searchable compose `?` keymap plus concise footer hints. `Tab`/`Shift-Tab` cycles To → Cc → Bcc → Subject → Body. `Ctrl+A` opens a directory picker where Space/Enter adds or removes files; `Ctrl+O` opens advanced options for custom headers. `Enter` from NORMAL opens a centered send-confirmation popup; Ctrl-S is not used. `r` replies without quoted text, `R` uses an aerc-style dated attribution plus gray quoted text, `A` replies to all (minus own identities), `L` replies to List-Post, and `f` forwards with a Gmail-style original From/Date/Subject/To/Cc attribution and the original plain/HTML contents inline with MIME attachments preserved instead of attaching `forwarded.eml`. The original MIME bytes remain available to the composer for drafts and extraction.
- **Compose attachments:** selected files are kept with postponed local drafts and sent as normal MIME attachments (16 MiB per file). Files with spaces or semicolons in their names are supported. The three-pane picker uses j/k, h/l, Backspace, Space/Enter and Esc, and does not depend on an external file manager. Press `v`, move with counted motions (such as `5j`), and use Enter to attach or `x` to detach the highlighted range; `Space` also detaches a visual range when all its files are already attached (folders are skipped); the selection has a blue side marker. Press `/` to search folders in the current directory (or files when no folder matches), Down/Up to cycle suggestions, Enter to finish searching on the selected item, Tab to open a matching folder, and Esc to leave search. Space advances except on the last row. Press `?` inside the picker for its full searchable keymap. `Ctrl+D` in compose opens a separate list of attachments from all directories; j/k moves, `x` or `d` detaches, and Esc closes it. PNG/JPEG/GIF/WebP images preview inside the picker; first-page PDF thumbnails require Poppler's `pdftoppm` (package `poppler` on Arch Linux). PDF thumbnails now render asynchronously after a short debounce and are cached during the picker session, so fast navigation does not wait for Poppler. Other file types retain the text/binary preview.
- **Received attachments:** IMAP headers are fetched with BODYSTRUCTURE so filenames and the separate theme-yellow `⧉` marker appear in the message browser without opening the email; rows with attachments show a second, muted filename line. The MIME parser remains authoritative after a body is loaded. `z` opens the received-attachment browser: a left-side list with relative line numbers, Space marks and `v`/`V` range selection, and a right-side automatic image/text preview. `o`/Enter opens the highlighted file with the system handler. `s` prompts for a save directory, and saves every marked file (or just the highlighted one), without overwriting existing files and without trusting MIME filename paths. PDF/binary attachments show metadata and an Open action; received PDF thumbnails are not yet inlined. Terminal graphics decode is cached per selection instead of repeated on every frame.
- **Mailing-list patches:** unified Git diff additions, deletions, headers and hunk ranges are styled distinctly, including `> `-quoted patches.
- **New-mail updates:** Inbox STATUS is checked by background mail workers every 20 seconds by default, without blocking navigation. Desktop notifications are enabled by default using `notify-send`, with a `message-new-email` freedesktop sound-name hint for COSMIC or other configured sound themes (actual audio playback depends on the notification daemon). When the known Inbox count rises, its cache is invalidated and the visible Inbox is refreshed. `B` toggles an optional terminal bell (off by default); the terminal determines whether the BEL control character makes a sound. This is a lightweight polling fallback, not IMAP IDLE; if new and deleted mail cancel out in a single interval, a count-only poll may miss the arrival.
- **Reader envelope:** displays Date, To, and any Cc/Bcc headers physically present in the mail. Bcc must not be guessed in recipient copies.
- **Theme polish:** default and monochrome themes now have a visibly shaded, bold current line in the message reader, mailbox list, and message browser.
- **Gmail archiving:** from INBOX, `a` removes the Inbox membership using Gmail's IMAP delete/expunge semantics; `U` from All Mail copies back to INBOX to add the Inbox membership. IMAP behavior is affected by your account's Gmail auto-expunge/deletion settings, so test this on disposable mail first.
- Empty folders render an explicit empty-state message instead of a blank pane.
- Searchable help and JSON themes; custom themes in `~/.config/aerc-rs/themes/*.json` can override built-ins by id.

## Compose / picker polish (follow-up)

- Typing in **Insert** mode uses the real terminal bar cursor, without adding
  a fake `▏` character that shifts text until returning to Normal.
- Closing an unsent compose with `q` or `Esc` in **Normal** asks whether to save
  a local postponed draft (**Y/Enter**), discard (**N**), or keep editing (**Esc**).
  `Ctrl+P` still postpones immediately. Empty new messages close normally.
- PDF conversion remains asynchronous, and `Ctrl+J/K` now updates the file
  preview only at the end of its smooth motion. Intermediate files are not read,
  decoded, or rendered. The currently-selected preview is refreshed on arrival.
- `.patch` and `.diff` files have Git hunk/add/delete coloring and Tree-sitter
  syntax coloring for Rust and C source lines in the *file-picker preview*.
  Unknown source languages retain safe Git diff colors.
- Attachment filenames in the message list start at the subject's left edge.

### Performance profiling on Arch Linux

Build a release binary, install `strace` if needed, and profile one specific
slowdown per session (e.g. fast Ctrl+J/K through a directory with many PDFs):

```sh
cargo build --release
sudo pacman -S strace
./scripts/profile-aerc.sh
```

Quit aerc-rs normally; the script prints syscall totals and saves a summary
under `/tmp/aerc-rs-profile.*`. This **does not collect message contents**;
aggregate syscall timings distinguish expensive I/O from CPU-bound rendering.
For CPU-bound stalls, the script also prints optional `perf` commands. Share
only summaries you have checked for sensitive file paths/account details.

## Configure a real account

Copy `config.example.toml` to `~/.config/aerc-rs/config.toml` and keep it private.

```toml
[[accounts]]
name = "Personal"
source = "imaps://you%40gmail.com@imap.gmail.com:993"
source_cred_cmd = "pass show mail/personal-app-password"
from = "Your Name <you@gmail.com>"
outgoing = "smtps://you%40gmail.com@smtp.gmail.com:465"
outgoing_cred_cmd = "pass show mail/personal-app-password"
default_mailbox = "INBOX"
page_size = 100

[ui]
remote_images = false # privacy-safe default; press I to toggle in-session
inbox_poll_seconds = 20 # 10..3600 supported; lower values create more server traffic
new_mail_bell = false # optional terminal BEL; independent of desktop notifications
new_mail_notifications = true # requires notify-send and a notification daemon
new_mail_sound_name = "message-new-email" # name in your active freedesktop/COSMIC sound theme
```

`page_size` controls interactive pagination. Existing `max_messages` settings are retained for compatibility/headless checks and no longer make the TUI wait for one giant initial fetch. Add more `[[accounts]]` blocks to the same config to use multiple accounts. `Alt-n`/`Alt-p` cycles accounts; `Alt-1` through `Alt-9` jumps directly to an account. `[ / ]` and Ctrl-PageUp/Ctrl-PageDown also switch accounts; click an account in the top tab bar to select it. Account switches preserve each account's mailbox and cached state.

Invalid `outgoing` URLs no longer prevent startup: receiving remains available on that account, but sending is disabled until you fix its SMTP configuration. An invalid incoming `source` causes only that account to be skipped, provided another account is usable. Look for the persistent configuration-issues indicator in the top tab bar and press `!` for actionable, credential-redacted warnings. If **every** incoming account is invalid, startup still fails with a safe error.

## Main controls

Press `?` for the searchable complete map. Common controls:

- `j/k` messages; `J/K` mailboxes; `Ctrl-j/Ctrl-k` smooth messages; `Ctrl-Shift-j/Ctrl-Shift-k` smooth mailboxes.
- `v` visual selection; `Esc` exits visual mode first.
- `d` confirmed trash/delete; `D` immediate trash/delete; `a` archive; `U` restore an archived message to INBOX.
- `/` filter current mailbox; `b` sidebar; `P` vertical preview; `Enter` full reader.
- `r` reply without quote; `R` reply with quote; `A` reply all; `L` list reply; `f` inline forward; `c` compose.
- HTML reader/preview: rich HTML is default; `T` toggles rich/plain, `I` toggles remote images, `o` opens the link directly under the reader cursor, and `O` opens the original HTML in the browser. `m` toggles expanded recipients (Cc/Bcc) and Message-ID; the reader starts in compact-header mode. `z` opens received attachments with automatic previews; `j/k` moves, Space marks and advances, `v` selects a range then Space marks it, `a` marks/unmarks all, `o`/Enter opens, and `s` saves marked files to an entered directory. Clicking a filename previews it; footer Open/Save/Mark actions are mouse-clickable. The mailbox divider can be dragged to resize, or use Alt-h/Alt-l to adjust it from the keyboard.
- In composer, Tab/Shift-Tab visits To, Cc, Bcc, Subject and Body. Ctrl+A opens an internal multi-select attachment picker (Space/Enter toggles files, h/Backspace moves to parent, l enters folders, . shows hidden files, Esc closes). Ctrl+O edits optional custom headers in advanced options (`X-Header: value; ...`). In NORMAL press `?` for the searchable compose-only shortcuts (`gg/G`, `w/W/b/B/e/E`, `vi` text objects, `dd/dw`, `f/F`, `o/O`, `u/Ctrl-r`, Visual and more). Enter shows a centered send confirmation; Ctrl-p postpones locally; Ctrl-n resumes the active account’s postponed draft. These are local files, not IMAP Drafts synchronization.
- `Alt-n`/`Alt-p` next/previous account; `Alt-1`–`Alt-9` direct account selection; `[ / ]` and Ctrl-PageUp/PageDown also navigate, or click the top account tabs. `!` reviews configuration warnings; `Ctrl-r` refreshes the current mailbox; `B` toggles the optional terminal bell. Desktop notifications can be toggled separately with `ui.new_mail_notifications`.

## Themes

Built-ins live in `themes/*.json`. Custom themes use the same flat semantic palette idea as lazygitrs:

```json
{
  "id": "my-theme",
  "name": "My Theme",
  "primary": "#2ac3de",
  "success": "#9ece6a",
  "error": "#f7768e",
  "warning": "#e0af68",
  "text_strong": "#ffffff",
  "background": "reset"
}
```

Put the file in `~/.config/aerc-rs/themes/my-theme.json`, set `[ui] theme = "my-theme"` in `config.toml`, or select it with `t`. The picker writes the selected theme back into `config.toml`; old standalone `theme` state files no longer override that setting.

## Validate and benchmark

```bash
./scripts/validate.sh
cargo run --release -- --headless-check
```

The TUI paints its shell before terminal-image capability probing, so a slow graphics query no longer delays first paint. `--headless-check` reports backend connect/list/header/body timings independently; use those numbers before reaching for `strace`.

For a repeatable local workload:

```bash
cargo run --release --example generate_fixture -- /tmp/aerc-rs-maildir 10000
./scripts/perf-smoke.sh /tmp/aerc-rs-maildir 10000 5
```

## Still intentionally incomplete versus aerc

Milestone 5 is much closer to the daily aerc workflow, but it does not claim full parity. Important remaining work includes IMAP IDLE/reconnect/unsolicited updates, persistent on-disk header/body cache, server-side search/sort/thread views, IMAP Drafts synchronization, inline forward with attached MIME parts, bounce/resend, address book/aliases, external-editor composition, and the developer-mail features we ultimately want: patch-series grouping, rich diffs, patch application, and direct lazygitrs integration.

### Drafts mailbox and attachment manager

Saving a nonempty composer with **Y** on exit (or **Ctrl+P**) now writes a
local recovery copy and asynchronously APPENDs an unsent MIME message to the
account's discovered IMAP/Maildir **Drafts** folder. The message is marked as
a draft; attachments are included and partially typed recipient fields are
preserved. There is **no SMTP submission** when saving a draft. If the server
is offline, the Drafts folder is unavailable, or APPEND fails, the local copy
remains and the status line reports that the mailbox sync did not succeed.
Press **Ctrl+N** outside compose to resume the editable local copy. Server
Drafts currently appear as ordinary readable messages; editing arbitrary
existing server-side drafts in place is not implemented yet. Re-saving a
resumed draft can create an additional server-side copy, so clear older draft
copies if necessary. The file-attachment manager (**Ctrl+D**) now supports
**u** to undo a detach made with **d** or **x**.

Message-list **p** toggles split preview (lowercase); the preview area has no
label. Subjects preserve their original emoji variation selectors and use
the full available column width instead of coercing text-mode glyphs.

## Diagnosing intermittent IMAP startup stalls

The optional `AERC_RS_PERF_LOG` environment variable records worker action queue
waits and IMAP stage durations without printing into the TUI. Each line contains timestamp, per-launch `run_id`, worker name, operation label,
queue duration, operation duration, and success/failure. Mailbox navigation
lifecycle events also contain a numerical `request_id` so enqueued operations,
worker execution, and UI application can be correlated. It does **not** record
mailbox names, recipients, Message-IDs, credentials, or email contents.
The file is created with mode 0600 on Unix.

```bash
cargo build --release
AERC_RS_PERF_LOG=/tmp/aerc-rs-timings.log ./target/release/aerc-rs
```

Use the client normally and exit when you notice a slow startup or message load.
Inspect `/tmp/aerc-rs-timings.log` for slow `imap_auth`, `imap_list`,
`imap_select`, `imap_fetch_headers`, or `imap_fetch_body` stages and their
corresponding `worker_*` action. A large `queue_ms` indicates aerc-rs delayed
the action behind earlier work; a large `duration_ms` with a small queue means
the operation itself was slow. The file appends across restarts so multiple
fast and slow sessions can be compared. Remove it when finished profiling.

The tracing option is off by default.

### Background STATUS isolation

IMAP accounts now use one interactive session for opening folders and loading
messages plus a **lazily connected second session** solely for unread-count
`STATUS` checks. A slow count request no longer blocks interactive `SELECT` or
`FETCH`; local Maildir accounts retain one worker. Pending folder opens and
reader body requests are coalesced to the newest selection, and request IDs
prevent stale responses from overwriting the current browser. Count failures
are best-effort and don't reset the active view.

The foreground/background worker separation remains in place. The private
persistent message index described below now complements the per-session cache;
first-time IMAP login/SELECT/FETCH can still be slow when Gmail or the network
is slow.

### Persistent, private IMAP message index

IMAP accounts now keep a small local index under
`~/.cache/aerc-rs/mail-index/` (or the platform's user cache directory).
The mailbox list, recently viewed folder headers, and a limited number of
already-read message bodies are loaded before network connection so warm
startup and returning to folders can show messages immediately. The current
foreground/background worker model still refreshes headers and counts in the
background. Cache writes happen on a separate thread, use atomic file rename,
and have private directory (`0700`) and file (`0600`) permissions on Unix.

The cache is **not** the source of truth. An IMAP `SELECT` refresh revalidates
`UIDVALIDITY`; cached UID IDs include their validity generation, and stale IDs
are rejected before a server read or mutation. An empty, old, malformed, or
oversized cache is ignored. Maildir accounts still use their local filesystem.

This first disk-cache milestone improves perceived navigation and warm startup.
It does **not** yet implement full CONDSTORE/QRESYNC incremental flag sync,
server-side push/IDLE, or complete offline message mirroring: unchanged
first-page headers are still fetched for reconciliation when revisiting a
folder. The first launch and genuinely uncached messages remain subject to
Gmail/IMAP latency. To clear private cached data, stop aerc-rs and remove
`~/.cache/aerc-rs/mail-index/`. Set `AERC_RS_DISABLE_CACHE=1` when
benchmarking the uncached behavior without deleting previously saved data.

### Received attachment browser (PDF thumbnails and save destinations)

In the reader, `z` opens received attachments. The left list uses full-width
current-line highlighting, relative line numbers, `5j`/`5k`, `gg`/`G`, and
100 FPS Ctrl-j/Ctrl-k smooth navigation. Space marks files and advances; `v`
starts a visual range; Space marks the range; `a` marks all. `o` opens the
selected attachment with the system viewer.

Images and the **first page of PDFs** appear automatically on the right using
the same terminal graphics protocol as the compose file picker. PDFs require
Poppler's `pdftoppm` command (Arch: `sudo pacman -S poppler`). Rendering and
image decoding run in a background thread, with a short debounce and an
8-thumbnail cache; navigating the list never runs a PDF converter in the
foreground. Opening the same message's attachment browser again can reuse
up to 24 MiB of recently parsed attachments across six messages in memory,
keyed by account, mailbox, UIDVALIDITY and message ID. First-time attachment
fetches still use the provider's IMAP/other backend and can take longer.

`s` opens a three-pane destination browser shared with the compose picker.
Use `j/k` or counted motions to select folders, `l`/Enter to enter a folder,
`h` to go to its parent, `.` for hidden entries, `n` to create a directory,
and **`s` again to save** all marked attachments (or the highlighted one) into
the current folder. Existing files are never overwritten; collisions get
numbered filenames. The destination browser never reads file contents or
launches thumbnails merely to choose a folder.

### SMTP OAUTHBEARER (non-Google providers)

For SMTP servers advertising `AUTH OAUTHBEARER` after TLS, set `outgoing` to
`smtps+oauthbearer://` (port 465) or `smtp+oauthbearer://` (port 587, mandatory
STARTTLS). Use `outgoing_cred_cmd` and the optional `token_endpoint`, `client_id`,
`client_secret`, and `scope` query fields just like XOAUTH2. The credential is
an access token unless `token_endpoint` is present, in which case it is a
refresh token. The client checks advertised authentication capabilities before
sending any bearer token and reports a safe error if the server lacks this
mechanism. Sending uses Lettre's secure low-level SMTP connection and its
message-delivery code, with no separate SMTP implementation. If the server
instead advertises XOAUTH2 (as Gmail does), the OAuth-bearing scheme negotiates
XOAUTH2 as a compatibility fallback; it never falls back to password auth.

**Gmail / Google Workspace:** Google documents SMTP `XOAUTH2` rather than
`OAUTHBEARER`. Use `smtps+xoauth2://` or `smtp+xoauth2://` for Google accounts;
changing the scheme to OAUTHBEARER cannot enable an unadvertised server feature.

### Per-account Sent policy, SMTP negotiation, sendmail, and browser OAuth

Each `[[accounts]]` can set `copy_to` (alias `copy-to`) to an existing mailbox
name. `copy_to = ""` or `copy_to = "none"` disables the client-side Sent APPEND;
`copy_to = "Sent"` stores a local/server copy in that folder, and comma-separated
names copy to each requested folder. Omitting it preserves the previous
auto-detection behavior: Gmail SMTP normally files Sent itself, and other
providers use the detected Sent mailbox. **Do not set a Sent APPEND for Gmail
unless you want to risk duplicate messages.** SMTP/local submission is never
retried just because a Sent APPEND fails.

Outgoing transports accept secure `smtp://` (required STARTTLS on 587) and
`smtps://` (TLS on 465). Unsuffixed URLs automatically select a mutually
advertised PLAIN or LOGIN method after TLS; `+plain`, `+login`, `+xoauth2`,
`+oauthbearer`, and `+none` remain available as explicit choices. An OAuth
account or a URL with a refresh-token endpoint negotiates OAuth without
falling back to password authentication. A rejected credential is not retried
under a different mechanism. Local MTA users can use an absolute sendmail
path, e.g. `outgoing = "/usr/sbin/sendmail"`, or
`outgoing = "sendmail:///usr/sbin/sendmail"`. The executable must exist.
It is invoked directly (no shell) with envelope recipients so Bcc remains
private. Local queue acceptance is not a guarantee of final delivery.

For native OAuth desktop sign-in, add an OAuth table **under an account**:

```toml
[[accounts]]
name = "Work"
source = "imaps://you%40example.com@outlook.office365.com:993"
outgoing = "smtp://you%40example.com@smtp.office365.com:587"
copy_to = "Sent Items"
from = "You <you@example.com>"

[accounts.oauth]
provider = "microsoft"  # or "google" for a Gmail account
client_id = "YOUR_PUBLISHER_REGISTERED_PUBLIC_CLIENT_ID"
# tenant = "common"  # optional Microsoft tenant selector
```

For Gmail, the corresponding account uses `imap.gmail.com:993`,
`smtp.gmail.com:465`, and typically `copy_to = ""` because Gmail already
stores sent mail itself:

```toml
[[accounts]]
name = "Personal"
source = "imaps://you%40gmail.com@imap.gmail.com:993"
outgoing = "smtps://you%40gmail.com@smtp.gmail.com:465"
copy_to = ""

[accounts.oauth]
provider = "google"
client_id = "YOUR_REGISTERED_GOOGLE_DESKTOP_CLIENT_ID"
```

Then run `aerc-rs --oauth-login Work` (or `--oauth-login Personal`) once.
The browser flow uses PKCE and a
loopback callback, requesting IMAP/SMTP access with offline consent. Refresh
tokens are written under the user configuration directory (`aerc-rs/oauth/`)
with private file permissions; they are not copied into the TOML file.
Subsequent IMAP/SMTP connections automatically refresh their access tokens.
An app publisher can provide OAuth client IDs at build time using
`AERC_RS_GOOGLE_CLIENT_ID` or `AERC_RS_MICROSOFT_CLIENT_ID` (and an optional
`AERC_RS_GOOGLE_CLIENT_SECRET` for a Google desktop public client). This allows
end users to sign in without manually creating or pasting tokens. **A developer
must first register and, where required, verify these OAuth clients; this
patch does not bundle an approved application identity.** Microsoft tenants may
require administrator consent, and some disallow SMTP AUTH entirely.
Legacy refresh-token URLs and credential commands remain supported.
