#!/usr/bin/env bash
set -euo pipefail

maildir="${1:-/tmp/aerc-rs-maildir}"
count="${2:-10000}"
runs="${3:-5}"

cargo run --release --example generate_fixture -- "$maildir" "$count"

echo
echo "Running $runs headless scans over $count synthetic messages"
for ((i = 1; i <= runs; i++)); do
  printf 'run %d: ' "$i"
  cargo run --quiet --release -- --maildir "$maildir" --headless-check
done
