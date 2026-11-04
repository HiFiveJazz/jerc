# aerc-rs — milestone 5: fast modal mail workflow

A Rust terminal email client inspired by aerc's backend/worker model and lazygitrs' responsive, Vim-first UI. Milestone 5 moves the browser from a fixed header window toward a real working mail client: paged mailbox caches, live filtering, split preview, bulk triage, richer HTML, and a modal composer.

## What works

- Maildir and TLS IMAP backends with password/app-password, OAUTHBEARER/XOAUTH2, refresh-token exchange, UID mutations, SMTP sending, and bounded worker queues.
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
- **Attachments:** IMAP headers are fetched with BODYSTRUCTURE so filenames and the separate theme-yellow `⧉` marker appear in the message browser without opening the email; rows with attachments show a second, muted filename line. The MIME parser remains authoritative after a body is loaded, preventing false positives from `multipart/mixed` wrappers. The reader shows decoded attachment filenames under the subject; `z` opens a proper attachment list. Image preview uses the same terminal graphics picker; PDF and other files are extracted to a private temporary file and opened using the OS file handler. External opener stdout/stderr are suppressed.
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

`page_size` controls interactive pagination. Existing `max_messages` settings are retained for compatibility/headless checks and no longer make the TUI wait for one giant initial fetch. Add more `[[accounts]]` blocks to the same config to use multiple accounts; `[ / ]` rotates accounts and the top tab bar shows the active one.

## Main controls

Press `?` for the searchable complete map. Common controls:

- `j/k` messages; `J/K` mailboxes; `Ctrl-j/Ctrl-k` smooth messages; `Ctrl-Shift-j/Ctrl-Shift-k` smooth mailboxes.
- `v` visual selection; `Esc` exits visual mode first.
- `d` confirmed trash/delete; `D` immediate trash/delete; `a` archive; `U` restore an archived message to INBOX.
- `/` filter current mailbox; `b` sidebar; `P` vertical preview; `Enter` full reader.
- `r` reply without quote; `R` reply with quote; `A` reply all; `L` list reply; `f` inline forward; `c` compose.
- HTML reader/preview: rich HTML is default; `T` toggles rich/plain, `I` toggles remote images, `o` opens the link directly under the reader cursor, and `O` opens the original HTML in the browser. `m` toggles the Message-ID header. `z` in the reader or message list opens attachments, `p` previews decoded images in-terminal and Enter opens with `xdg-open` (on Linux).
- In composer, Tab/Shift-Tab visits To, Cc, Bcc, Subject and Body. Ctrl+A opens an internal multi-select attachment picker (Space/Enter toggles files, h/Backspace moves to parent, l enters folders, . shows hidden files, Esc closes). Ctrl+O edits optional custom headers in advanced options (`X-Header: value; ...`). In NORMAL press `?` for the searchable compose-only shortcuts (`gg/G`, `w/W/b/B/e/E`, `vi` text objects, `dd/dw`, `f/F`, `o/O`, `u/Ctrl-r`, Visual and more). Enter shows a centered send confirmation; Ctrl-p postpones locally; Ctrl-n resumes the active account’s postponed draft. These are local files, not IMAP Drafts synchronization.
- `[ / ]` previous/next account; `Ctrl-r` refreshes the current mailbox; `B` toggles the optional terminal bell. Desktop notifications can be toggled separately with `ui.new_mail_notifications`.

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

Put the file in `~/.config/aerc-rs/themes/my-theme.json`, then press `t`.

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
