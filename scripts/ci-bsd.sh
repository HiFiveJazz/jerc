#!/bin/sh
# Compile and smoke-test Jerc in a native FreeBSD/OpenBSD/NetBSD GitHub Actions VM.
set -eu

case "$(uname -s)" in
    FreeBSD) target=x86_64-unknown-freebsd ;;
    OpenBSD) target=x86_64-unknown-openbsd ;;
    NetBSD) target=x86_64-unknown-netbsd ;;
    *) echo 'This script must run on FreeBSD, OpenBSD, or NetBSD' >&2; exit 1 ;;
esac

# Keep the large Rust build directory inside the guest instead of rsyncing it
# back to the Linux host; only the final archive is copied back by vmactions.
export CARGO_TARGET_DIR="${TMPDIR:-/tmp}/jerc-bsd-target"

rustc --version
cargo --version
rustc -vV | grep -F "host: $target"

cargo build --locked --release
"$CARGO_TARGET_DIR/release/jerc" --version
cargo test --locked --all-targets --all-features

mkdir -p bsd-dist
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT HUP INT TERM
cp "$CARGO_TARGET_DIR/release/jerc" "$staging/jerc"
cp LICENSE config.example.toml "$staging/"
archive="bsd-dist/jerc-${GITHUB_SHA:-local}-$target.tar.gz"
tar -czf "$archive" -C "$staging" jerc LICENSE config.example.toml
printf 'Experimental BSD binary: %s\n' "$archive"
