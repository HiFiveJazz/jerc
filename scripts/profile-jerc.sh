#!/usr/bin/env bash
# Trace syscall costs during a real interactive jerc session.
# Usage: scripts/profile-jerc.sh [path/to/jerc] [app-args...]
# Exit jerc normally to flush the summary.
set -euo pipefail
if ! command -v strace >/dev/null; then
    echo 'Install strace first: sudo pacman -S strace' >&2
    exit 1
fi
binary="${1:-./target/release/jerc}"
if (( $# > 0 )); then shift; fi
if [[ ! -x "$binary" ]]; then
    echo "No executable at $binary; build with: cargo build --release" >&2
    exit 1
fi
out="${JERC_PROFILE_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/jerc-profile.XXXXXXXX")}"
mkdir -p -- "$out"
printf 'Recording in %s\nReproduce the slowdown; exit jerc normally to finish.\n' "$out"
# -c reports aggregate time and counts; it does not capture message bodies.
strace -f -c -S time -o "$out/syscalls.txt" -- "$binary" "$@"
printf '\nSyscall summary: %s\n' "$out/syscalls.txt"
sed -n '1,60p' "$out/syscalls.txt"
printf '\nOptional CPU profile (slower but helpful if syscalls are cheap):\n'
printf '  perf record -g --call-graph dwarf -o "%s/perf.data" -- "%s"\n' "$out" "$binary"
printf '  perf report -i "%s/perf.data"\n' "$out"
