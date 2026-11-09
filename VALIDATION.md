# Validation

[← Back to README](README.md) · [Contributing](CONTRIBUTING.md)

Run the same checks locally that are used in CI:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo +1.98.1 check --all-targets --all-features
```

The final command requires the Rust 1.98.1 toolchain installed through rustup.

For a quick local run without real credentials, create a fixture and launch the Maildir backend:

```sh
cargo run --release --example generate_fixture -- /tmp/jerc-maildir 10000
cargo run --release -- --maildir /tmp/jerc-maildir
```

`./scripts/validate.sh` runs the formatting, lint, and test checks. `./scripts/perf-smoke.sh` and `./scripts/profile-jerc.sh` support performance investigations; the latter requires `strace` on Linux.

Manual checks before a release should include: startup with multiple accounts, mailbox pagination, HTML/plain switching, reply/reply-all/forward, attachment preview and export, OAuth login/probe against a test account, drafts after restart, and resize/selection behavior on narrow terminals. Avoid tests with irreplaceable messages.

This document describes checks to perform; it does not claim the current revision has passed them.

## Pass 4 synchronization regression checks

In addition to the automated tests, use a test Gmail or Microsoft 365 account to verify:

1. Start with a warm mailbox cache, then verify live STATUS/SELECT results replace cached rows without blocking navigation.
2. Receive multiple messages while an unread-count check is in progress. Repeated IDLE wakeups should not create an unbounded queue, and no wakeup should be forgotten.
3. Disconnect during a STATUS check, reconnect, and confirm count reconciliation resumes; large mailboxes should retain polling fallback.
4. Trigger two independent bulk operations (using test messages), then fail one. A failure must not clear the other operation's in-flight state.
5. Open and mark a message Seen while another background operation runs. An old response must not overwrite a newer Seen request.

The STATUS queue and request-correlation unit tests do not replace these real-account checks.
