# Contributing to Jerc

Thanks for improving Jerc. This is a terminal email client, so changes can
involve untrusted MIME content and private account information. Keep changes
small, testable, and focused on one subsystem.

Contributions intended for inclusion in Jerc are submitted under the project's
`GPL-3.0-only` license. Keep third-party copyright, license, and attribution
notices intact; bundled third-party assets may have separate licenses.

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
| IMAP implementation and native threading | `src/backend/imap/mod.rs`, `src/backend/imap/threads.rs` |
| JMAP backend and push events | `src/backend/jmap/mod.rs`, `src/backend/jmap/{session,push}.rs` |
| JMAP body, search, and submission | `src/backend/jmap/{body,views,submission}.rs` |
| Maildir and POP3 | `src/backend/{maildir,pop3}.rs` |
| Cache index, bodies, offline journal | `src/cache.rs` |
| MIME parsing and HTML conversion | `src/mail.rs` |
| Worker actions and events | `src/worker.rs` |
| Worker scheduling, execution and attachment jobs | `src/worker/{handle,foreground,background,idle,helpers}.rs` |
| Authentication, onboarding | `src/oauth.rs`, `src/setup.rs` |
| SMTP, MIME output, and drafts | `src/outgoing/` |
| Account settings | `src/config.rs`, `config.example.toml` |

State structs remain in `src/app/state.rs` to preserve the existing public API
and draft serialization. Read `src/app/state/account_state.rs` for mailbox and
prefetch behavior; `app_core.rs`, `app_recovery.rs`, `app_notices.rs`,
`app_setup.rs`, `app_animation.rs`, `app_navigation.rs`, `app_reader.rs`, and
`app_controls.rs` for application orchestration. Small `*_util.rs` files
contain shared, non-UI helpers. These are ordinary Rust modules with `impl`
blocks on the **same** state types.
Rendering helpers are separated by surface; `src/ui/render.rs` still defines
overlay precedence and the top-level `draw` function.

## How the code fits together

New contributors can follow one user action from keyboard input to its result:

1. `src/app/input.rs` interprets the key; the relevant `src/app/state/` module
   updates the single UI-owned application state.
2. `src/worker.rs` declares requests, events and worker types.
   `src/worker/handle.rs` manages bounded queues and lifecycle;
   `foreground.rs` handles interactive work, `background.rs` handles STATUS
   and speculative fetches, `idle.rs` owns IDLE listeners, and
   `helpers.rs` handles raw MIME and attachment jobs. None renders UI.
3. `src/backend/mod.rs` selects an IMAP, JMAP, POP3, or Maildir backend.
   JMAP wire/SSE details live beside that backend; IMAP threading extensions
   live beside IMAP rather than in the shared backend factory.
4. A `WorkerEvent` returns to `src/app/state/worker_events/`, which updates
   UI state and requests a redraw.
5. `src/ui/render.rs` composes views from the `src/ui/render/` feature modules;
   it must not perform network or disk I/O.

The cache model and index loading stay in `src/cache.rs`; its writer loop is
in `src/cache/writer.rs`, and disk body/raw-MIME stores are in
`src/cache/{body_store,raw_store}.rs`. All run independently of rendering.
Keep index serialization, disk write batching, cache identity, offline journal
behavior and cache-size limits intact when reorganizing them.

## Where tests belong

Large unit and regression suites live in `tests.rs` or named `*_tests.rs`
files **beside their production module**, for example:

- `src/backend/jmap/tests.rs` exercises JMAP parsing and synchronization.
- `src/backend/imap/tests.rs` exercises IMAP behavior.
- `src/worker/tests.rs` and `src/worker/foreground_recovery_tests.rs` test
  worker scheduling, failure recovery, and acknowledgment ordering.
- `src/cache/tests.rs` and `src/cache/offline_mutation_journal_tests.rs`
  cover persistence, cache compatibility, and offline mutations.
- `src/mail/tests.rs`, `src/ui/render/tests.rs`, and `src/setup/tests.rs`
  cover MIME/HTML, rendering, and onboarding respectively.

These files are declared as ordinary `#[cfg(test)] mod ...;` child modules.
Tests retain access to the immediate parent's private helpers through
`super::`, without making production APIs public just for testing.
Keep a small local test next to a helper when it clarifies the implementation;
move a large suite out when it obscures the code newcomers need to read.
Avoid real provider credentials in tests, and use deterministic fixtures.

## Local checks

Requires Rust 1.98 or newer (the minimum version checked by CI).

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
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
  behavior was preserved. For a file-only move, prefer `git mv`; ensure the
  test list, worker queue behavior, and cache format are unchanged.
- If a refactor needs to change thread ownership, extra copies of message data,
  write cadence, or cached mailbox state, make it a separate measured change;
  don't hide it among module moves.

For an architecture overview and performance invariants, see
[`ARCHITECTURE.md`](ARCHITECTURE.md).
