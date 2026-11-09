# Contributing to Jerc

Thanks for improving Jerc. This is a terminal email client, so changes can
involve untrusted MIME content and private account information. Keep changes
small, testable, and focused on one subsystem.

## Find the right code

| Area | Start here |
| --- | --- |
| Terminal event loop, notifications | `src/runtime.rs` |
| Keyboard commands and searchable help | `src/app/input.rs` |
| Application state and command routing | `src/app/state.rs` |
| Reader navigation and smooth scrolling | `src/app/state/reader_actions.rs` |
| Compose editing, forwarding, draft actions | `src/app/state/composer.rs`, `compose_actions.rs` |
| Compose attachment picker | `src/app/state/file_picker.rs` |
| Contacts interactions | `src/app/state/contacts.rs`, `src/contacts.rs` |
| Account/network worker events | `src/app/state/worker_events.rs`, `src/worker.rs` |
| Message mutations and confirmation logic | `src/app/state/mail_actions.rs` |
| Received attachment interactions | `src/app/state/attachment_actions.rs` |
| UI layout and overlay order | `src/ui/render.rs` |
| Mail list, reader, compose, contacts UI | `src/ui/render/{mail_list,reader,compose,contacts}.rs` |
| Modals, search, status, and attachments UI | `src/ui/render/{dialogs,help,status,attachments}.rs` |
| IMAP/Maildir | `src/backend/` |
| SMTP, MIME output, and drafts | `src/outgoing/` |
| Account settings | `src/config.rs`, `config.example.toml` |

State structs remain in `src/app/state.rs` to preserve the existing public API
and draft serialization. Feature-specific `impl` blocks live in adjacent modules.
Rendering helpers are separated by surface; `src/ui/render.rs` still defines
overlay precedence and the top-level `draw` function.

## Local checks

Requires Rust 1.98 or newer (the minimum version checked by CI).

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo +1.98.1 check --all-targets --all-features
```

Install the older toolchain first with `rustup toolchain install 1.98.1` if
needed. You can run most unit tests without configuring a real email account.
Run `cargo test <test_name>` when iterating, followed by the full suite before
opening a pull request. CI also checks Linux, macOS, and Windows.

## Code and review conventions

- Preserve UI keybindings and existing config defaults unless the change is
  explicitly about those behaviors. Update searchable help and README at the
  same time when bindings change.
- Avoid blocking disk or network work from the drawing path. Mailbox work runs
  through bounded workers; image fetching and decoding must remain off the UI
  thread.
- Put feature-specific renderers in their existing modules, not in the top-level
  renderer. Prefer a coherent feature module over many tiny files.
- Keep protocol types and MIME parsing independent of the terminal widgets.
- Add regression tests for navigation, Unicode width, caching, and email
  mutation changes. Don't require live provider credentials in CI tests.
- Use established Rust crates and avoid new dependencies for trivial helpers.
- Treat email messages, MIME filenames, and external URLs as untrusted input.
  Never add real access tokens, private messages, or personally identifying
  account configuration to fixtures, screenshots, test scripts, or logs.
- Separate pure refactoring from feature changes so reviewers can verify
  behavior was preserved.

For an architecture overview and performance invariants, see
[`ARCHITECTURE.md`](ARCHITECTURE.md).
