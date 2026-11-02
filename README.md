# aerc-rs — milestone 1

A Rust terminal email client foundation inspired by aerc's backend/worker/model split, optimized around the same principles we have been using for lazygitrs: fast startup, event-driven redraws, bounded background work, measurable performance, and tests around behavior rather than visual guesswork.

This is **not yet an aerc replacement**. It is the first usable slice and is deliberately built so the remaining protocols/features do not require replacing the core.

## What works now

- Rust 2024 project using Ratatui 0.30.2 + Crossterm 0.29.
- Cross-platform terminal/runtime design (Linux/macOS/Windows APIs only through Crossterm/std).
- Multiple accounts in core state/runtime.
- aerc-like typed worker action/event boundary with request correlation.
- Maildir backend:
  - discovers INBOX and common Maildir++ folders,
  - counts total/unread mail,
  - scans/sorts message headers,
  - lazy-loads and MIME-decodes selected bodies,
  - marks messages seen/unseen,
  - moves and deletes messages.
- Three-pane TUI: folders, message list, preview.
- Vim-style navigation: `j/k`, `g/G`, `Tab`, `Enter`, `u`, `r`, `q`; the preview pane scrolls independently.
- Message model already keeps `Message-ID`, `In-Reply-To`, and `References` for kernel-style threading/patch series.
- No periodic redraw loop. Terminal input and backend workers wake one bounded event queue; UI→worker requests are non-blocking so a busy backend cannot freeze key handling.
- `--headless-check` reports mailbox scan latency and throughput.
- Unit/integration tests plus a 10k-message fixture generator and repeated performance-smoke script.
- CI definition for formatting/Clippy, Linux/macOS/Windows tests, and Rust 1.98.1 MSRV checking.

## Run against an existing Maildir

```bash
cargo run --release -- --maildir ~/Mail/kernel
```

Or copy `config.example.toml` to `~/.config/aerc-rs/config.toml` and run:

```bash
cargo run --release
```

## Validate

```bash
./scripts/validate.sh
```

For a repeatable local scan benchmark:

```bash
cargo run --release --example generate_fixture -- /tmp/aerc-rs-maildir 10000
cargo run --release -- --maildir /tmp/aerc-rs-maildir --headless-check
# or repeat the scan five times:
./scripts/perf-smoke.sh /tmp/aerc-rs-maildir 10000 5
```

The important metric at this stage is **header-scan latency**, because opening a mailbox must not read every attachment/body. Body parsing happens only after selection.

The Maildir backend accepts both the normal `:2,` info suffix and the Windows-safe `!2,` spelling, and writes the latter on Windows. This keeps local-Maildir tests portable even though `:2,` itself is not a valid Windows filename.

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

## Next milestones

The next major step should be **real IMAP read support**, not more UI polish: TLS connection, LIST/STATUS, UID-based header fetch, lazy body fetch, STORE flags, MOVE/delete, and IDLE. After that, SMTP/composer and OAuth can be layered in. The core UI should not need to know which backend is in use.

After protocol parity gets closer to aerc, the direction changes from copying proven architecture to improving it: first-class patch-series rendering, lazygitrs-quality diffs, applying emailed patches to a selected repo/branch, review/reply flows, and tight lazygitrs integration.

## Acknowledgement

The architecture is intentionally informed by aerc, whose current source separates backend workers, backend-neutral models, account/message-store state, and UI. This project is a clean Rust implementation rather than a line-for-line port.
