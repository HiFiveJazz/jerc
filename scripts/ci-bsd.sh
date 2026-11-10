#!/bin/sh
# Compile and smoke-test Jerc in a native FreeBSD/OpenBSD/NetBSD GitHub Actions VM.
set -eu

case "$(uname -s)" in
    FreeBSD) target=x86_64-unknown-freebsd ;;
    OpenBSD) target=x86_64-unknown-openbsd ;;
    NetBSD)
        target=x86_64-unknown-netbsd
        # NetBSD VM storage is constrained. Avoid duplicate debug/release
        # dependency trees, and limit concurrent compiler scratch files.
        export CARGO_BUILD_JOBS=2
        export CARGO_INCREMENTAL=0
        export CARGO_TARGET_DIR="$HOME/jerc-bsd-target"
        ;;
    *) echo 'This script must run on FreeBSD, OpenBSD, or NetBSD' >&2; exit 1 ;;
esac

# Keep the large Rust build directory inside the guest instead of rsyncing it
# back to the Linux host; only the final archive is copied back by vmactions.
# Use the guest filesystem, not the synchronized GitHub workspace.
# NetBSD uses $HOME instead of /tmp in case /tmp has a smaller quota.
: "${CARGO_TARGET_DIR:=${TMPDIR:-/tmp}/jerc-bsd-target}"
export CARGO_TARGET_DIR
mkdir -p "$CARGO_TARGET_DIR"
df -h "$CARGO_TARGET_DIR"

rustc --version
cargo --version
rustc -vV | grep -F "host: $target"

cargo build --locked --release
"$CARGO_TARGET_DIR/release/jerc" --version
# NetBSD can compile the release binary but runs out of disk when Cargo
# subsequently creates a second, debug-profile dependency tree for tests.
# --release reuses the already-built release-profile dependencies.
if [ "$target" = x86_64-unknown-netbsd ]; then
    cargo test --locked --release --all-targets --all-features
else
    cargo test --locked --all-targets --all-features
fi

df -h "$CARGO_TARGET_DIR"

mkdir -p bsd-dist
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT HUP INT TERM
cp "$CARGO_TARGET_DIR/release/jerc" "$staging/jerc"
cp LICENSE config.example.toml "$staging/"
archive="bsd-dist/jerc-${GITHUB_SHA:-local}-$target.tar.gz"
tar -czf "$archive" -C "$staging" jerc LICENSE config.example.toml
printf 'Experimental BSD binary: %s\n' "$archive"
