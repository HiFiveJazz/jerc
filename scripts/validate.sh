#!/usr/bin/env bash
set -euo pipefail

cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features

echo
echo "Validation passed. Optional local performance smoke test:"
echo "  cargo run --release --example generate_fixture -- /tmp/aerc-rs-maildir 10000"
echo "  cargo run --release -- --maildir /tmp/aerc-rs-maildir --headless-check"
