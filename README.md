# aerc-rs — milestone 2: real IMAP

A Rust terminal email client foundation inspired by aerc's backend/worker/model split, optimized around the same principles we have been using for lazygitrs: fast startup, event-driven redraws, bounded background work, measurable performance, and tests around behavior rather than visual guesswork.

Milestone 2 adds **real TLS IMAP and aerc-compatible authentication**. The TUI is still intentionally rough; protocol/model correctness and responsiveness come first.

## What works now

- Rust 2024 project using Ratatui 0.30.2 + Crossterm 0.29.
- Cross-platform terminal/runtime design (Linux/macOS/Windows).
- Multiple accounts in core state/runtime.
- aerc-like typed worker action/event boundary with request correlation.
- Maildir backend for local/offline testing and archives.
- IMAP backend:
  - TLS IMAP (`imaps://`) using rustls and the platform certificate verifier,
  - password/app-password login,
  - SASL `OAUTHBEARER` and `XOAUTH2`,
  - aerc-compatible refresh-token exchange when `token_endpoint` is present,
  - `LIST` + `STATUS` mailbox discovery/counts,
  - bounded newest-message header fetches using UID-bearing `FETCH`,
  - lazy full-message fetch by UID,
  - seen/unseen updates,
  - MOVE with COPY/delete fallback,
  - UID-scoped delete/expunge.
- `source_cred_cmd` support so reusable credentials do not have to live in TOML.
- URI/debug/output redaction: passwords, tokens, client secrets, and OAuth query parameters are not printed by backend diagnostics or `--headless-check`.
- Three-pane TUI: folders, message list, preview.
- Vim-style navigation: `j/k`, `g/G`, `Tab`, `Enter`, `u`, `r`, `q`; preview scrolls independently.
- Message model keeps `Message-ID`, `In-Reply-To`, and `References` for kernel-style threading/patch series.
- No periodic redraw loop. Terminal input and backend workers wake one bounded event queue; UI→worker requests are non-blocking.
- Unit/integration tests plus a 10k-message Maildir fixture generator and repeated performance-smoke script.
- CI definition for formatting/Clippy, Linux/macOS/Windows tests, and Rust 1.98.1 MSRV checking.

## Test a real IMAP account

Copy `config.example.toml` to:

```bash
mkdir -p ~/.config/aerc-rs
cp config.example.toml ~/.config/aerc-rs/config.toml
chmod 600 ~/.config/aerc-rs/config.toml
```

For a Gmail account using an app password, an account can look like:

```toml
[[accounts]]
name = "Personal"
source = "imaps://you%40gmail.com@imap.gmail.com:993"
source_cred_cmd = "pass show mail/personal-app-password"
default_mailbox = "INBOX"
max_messages = 500
```

For the same style of OAuth refresh-token setup used by aerc:

```toml
[[accounts]]
name = "University"
source = "imaps+oauthbearer://you%40example.edu@imap.gmail.com:993?client_id=YOUR_CLIENT_ID&client_secret=YOUR_CLIENT_SECRET&token_endpoint=https%3A%2F%2Foauth2.googleapis.com%2Ftoken"
source_cred_cmd = "pass show mail/university-refresh-token"
default_mailbox = "INBOX"
max_messages = 500
```

With `token_endpoint` present, the supplied credential is treated as a **refresh token** and exchanged for a short-lived access token. Without `token_endpoint`, the supplied credential is treated as the access token itself. This mirrors aerc's source-URI model.

For a one-off compatibility test you can also put the percent-encoded password/refresh token directly in the source URI just as aerc does. Prefer `source_cred_cmd` afterward; command-line arguments are deliberately not provided for secrets because shell history and process listings are poor credential stores.

Before opening the TUI, test authentication and mailbox/header retrieval:

```bash
cargo run --release -- --headless-check
```

Expected shape (the URI is redacted):

```text
Personal: imaps://you@gmail.com@imap.gmail.com:993 | connect=... ms list=... folders/... ms scan=500 messages/... ms (... msg/s)
```

Then run the TUI:

```bash
cargo run --release
```

`max_messages` bounds the initial newest-message window while the UI has no pagination. Set it to `0` to fetch all headers, but that is intentionally not the default for very large Gmail folders.

## Run against Maildir

```bash
cargo run --release -- --maildir ~/Mail/kernel
```

The Maildir backend accepts both normal `:2,` flags and the Windows-safe `!2,` spelling.

## Validate

```bash
./scripts/validate.sh
```

For a repeatable local scan benchmark:

```bash
cargo run --release --example generate_fixture -- /tmp/aerc-rs-maildir 10000
cargo run --release -- --maildir /tmp/aerc-rs-maildir --headless-check
./scripts/perf-smoke.sh /tmp/aerc-rs-maildir 10000 5
```

## Key bindings

| Key | Action |
| --- | --- |
| `j` / `k` | move down / up |
| `g` / `G` | first / last |
| `Space` / `PgDn` | move 10 down |
| `PgUp` | move 10 up |
| `Tab` / `Shift-Tab` | change pane |
| `Enter` / `l` | open mailbox / preview |
| `u` | toggle seen/unseen |
| `r` | refresh |
| `[` / `]` | previous / next account |
| `q` / `Ctrl-C` | quit |

## Why blocking IMAP is intentional here

Each account already owns a dedicated backend worker thread. IMAP/TLS/OAuth can therefore block on network I/O without blocking Crossterm input or Ratatui rendering. This keeps Tokio out of the hot UI path and preserves a simple backend trait. If we later need multiplexed connections, IDLE management, or JMAP concurrency, those details can live behind the same worker/backend boundary.

## Next milestones

1. Capability negotiation + IDLE/reconnect/new-mail updates.
2. IMAP pagination/cache so huge folders never require a large header window.
3. SMTP/sendmail abstraction and composer/reply/forward.
4. Local thread builder from `Message-ID`/`References`, then server THREAD where useful.
5. Patch-series grouping/rendering and lazygitrs-quality diffs.
6. Apply mailed patches to a selected repository/branch and integrate directly with lazygitrs.

The implementation is a clean Rust design informed by aerc's separation of protocol workers, backend-neutral models, account/message state, and UI; it is not a line-for-line port.
