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
