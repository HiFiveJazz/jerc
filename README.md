# aerc-rs — milestone 3.1: aerc-style browser + themes

A Rust terminal email client inspired by aerc's backend/worker/model split and by lazygitrs' responsiveness model: bounded background work, event-driven redraws, Vim-first navigation, measurable performance, and tests around behavior.

Milestone 3.1 makes the browsing model much closer to aerc: mailbox + message list is the normal screen, Enter opens a full-screen reader, Esc returns to the list, and no body is fetched while you move through the inbox. It also adds live persistent themes and raw-message export for debugging difficult MIME/HTML mail.

## What works

- Rust 2024 + Ratatui 0.30/Crossterm 0.29, Linux/macOS/Windows-oriented design.
- Multiple accounts with one blocking backend worker per account; network stalls never block terminal input/rendering.
- Maildir and real TLS IMAP backends.
- IMAP password/app-password, OAUTHBEARER, XOAUTH2, refresh-token exchange, UID-based mutations, MOVE/delete, bounded header windows.
- Faster first paint:
  - `LIST` no longer waits for one `STATUS` round trip per folder;
  - mailbox unread counts fill lazily after the selected mailbox is usable;
  - IMAP list scans fetch only the header fields the model needs;
  - stale queued body requests collapse to the newest selected message.
- aerc-style browser/reader split:
  - account tabs on top;
  - browser view is a proportional mailbox column (33% by default) plus a message list using the remaining width;
  - there is no permanent preview pane;
  - `Enter`/`l` opens the selected message in a full-screen reader;
  - `Esc` returns to the same message-list selection;
  - browsing and smooth scrolling never trigger body downloads.
- Vim/lazygitrs navigation:
  - relative line numbers (current line is absolute);
  - `20j`, `20k`, etc.;
  - `Ctrl-j` / `Ctrl-k` interruptible smooth 22-row movement;
  - `g/G`, page movement, pane focus.
- Loading feedback for account/folder/message-body work.
- Viewing a body automatically sets `\\Seen`; merely passing over messages during smooth movement does not.
- `u` explicitly toggles seen/unseen and updates local unread counts after server acknowledgement.
- Built-in HTML-mail rendering: plain text is preferred when useful; HTML-only messages get a clean terminal rendering without requiring w3m. `h` switches alternatives where available.
- `?` opens a help popup; the status bar no longer permanently consumes space with key hints.
- Hot-swappable themes: press `t`, use `j/k` to preview themes live, `Enter` to persist, or `Esc` to revert. Built-ins: default, Nord, Gruvbox, Catppuccin, Tokyo Night, and monochrome.
- `e` exports the selected message's raw RFC 5322 source to the aerc-rs cache directory (`debug-message.eml`) with mode 0600 on Unix, making broken MIME/HTML mail easy to inspect outside the alternate-screen TUI.
- SMTP compose/send foundation:
  - `c` new message;
  - `R` reply with `In-Reply-To`/`References` threading metadata;
  - `Ctrl-s` send;
  - implicit TLS (`smtps://`) or required STARTTLS (`smtp://`);
  - PLAIN, LOGIN, and XOAUTH2;
  - refresh-token exchange for XOAUTH2;
  - `outgoing_cred_cmd` for external secret storage.
- `--headless-check` reports connect, LIST, header scan, and first-body load timing.

## Configure a real account

Copy `config.example.toml` to `~/.config/aerc-rs/config.toml` and keep it private:

```bash
mkdir -p ~/.config/aerc-rs
cp config.example.toml ~/.config/aerc-rs/config.toml
chmod 600 ~/.config/aerc-rs/config.toml
```

For Gmail with an app password:

```toml
[[accounts]]
name = "Personal"
source = "imaps://you%40gmail.com@imap.gmail.com:993"
source_cred_cmd = "pass show mail/personal-app-password"
from = "Your Name <you@gmail.com>"
outgoing = "smtps://you%40gmail.com@smtp.gmail.com:465"
outgoing_cred_cmd = "pass show mail/personal-app-password"
default_mailbox = "INBOX"
max_messages = 500
```

For an aerc-like OAuth refresh-token account, incoming IMAP can use OAUTHBEARER while Gmail submission uses XOAUTH2:

```toml
[[accounts]]
name = "University"
source = "imaps+oauthbearer://you%40example.edu@imap.gmail.com:993?client_id=YOUR_CLIENT_ID&client_secret=YOUR_CLIENT_SECRET&token_endpoint=https%3A%2F%2Foauth2.googleapis.com%2Ftoken"
source_cred_cmd = "pass show mail/university-refresh-token"
from = "Your Name <you@example.edu>"
outgoing = "smtp+xoauth2://you%40example.edu@smtp.gmail.com:587?client_id=YOUR_CLIENT_ID&client_secret=YOUR_CLIENT_SECRET&token_endpoint=https%3A%2F%2Foauth2.googleapis.com%2Ftoken"
outgoing_cred_cmd = "pass show mail/university-refresh-token"
default_mailbox = "INBOX"
max_messages = 500
```

The URI password component is still accepted for aerc compatibility, but external credential commands are preferred. There is intentionally no CLI secret flag.

## Validate and benchmark

```bash
./scripts/validate.sh
cargo run --release -- --headless-check
```

The headless output now includes a first-message body timing, for example:

```text
Personal: imaps://you@gmail.com@imap.gmail.com:993 | connect=... ms list=... folders/... ms scan=... messages/... ms (... msg/s) body=... ms
```

That lets us separately track the mailbox-list regression you saw, header-list speed, and actual message-open latency.

For a repeatable local workload:

```bash
cargo run --release --example generate_fixture -- /tmp/aerc-rs-maildir 10000
./scripts/perf-smoke.sh /tmp/aerc-rs-maildir 10000 5
```

## Main controls

Press `?` in the client for the complete map. The important controls are `j/k`, numeric prefixes such as `20j`, `Ctrl-j/Ctrl-k` smooth movement, `g/G`, `Tab`, `Enter` to open a full-screen message, `Esc` to return, `u` seen/unseen, `h` plain/HTML, `e` raw-message export, `t` themes, `c` compose, `R` reply, and `q` quit.

The composer is intentionally simple in this milestone: append-at-cursor-end editing, To/Cc/Subject/Body fields, and SMTP send. A proper editor integration, drafts, attachments, reply-all/forward, signatures, aliases, and sent-copy policy belong in subsequent parity work.

## Still intentionally incomplete versus aerc

Milestone 3 is not claiming full aerc parity yet. Major remaining pieces include IMAP IDLE/reconnect, header pagination/persistent cache, search/sort/thread views, drafts/sent synchronization, attachments, external editor composition, MIME attachment viewing/opening, address book/aliases, reply-all/forward, account/folder configuration parity, and the developer-mail features that are the eventual differentiator (patch-series grouping, rich diffs, and lazygitrs integration).

The important part is that those features now sit on stable backend-neutral seams instead of requiring UI/protocol rewrites.
