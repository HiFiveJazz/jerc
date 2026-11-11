#!/usr/bin/env bash
# Print the pkgrel required to publish a staged PKGBUILD against the AUR's
# currently published PKGBUILD. Does not modify either file.
# Usage: next-aur-pkgrel.sh DESIRED_PKGBUILD [AUR_PKGBUILD]
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 || ! -f "$1" ]]; then
  echo 'Usage: next-aur-pkgrel.sh DESIRED_PKGBUILD [AUR_PKGBUILD]' >&2
  exit 2
fi

desired="$1"
published="${2:-}"

read_field() {
  local file="$1" key="$2" value
  value="$(sed -n -E "s/^${key}=([^[:space:]]+)$/\\1/p" "$file")"
  if [[ -z "$value" ]]; then
    echo "Missing or malformed $key in $file" >&2
    exit 1
  fi
  printf '%s\n' "$value"
}

desired_ver="$(read_field "$desired" pkgver)"
[[ "$desired_ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "Unexpected desired pkgver: $desired_ver" >&2
  exit 1
}

# The first submission of an empty package repository starts at 1.
if [[ -z "$published" || ! -f "$published" ]]; then
  echo 1
  exit 0
fi

published_ver="$(read_field "$published" pkgver)"
published_rel="$(read_field "$published" pkgrel)"
[[ "$published_rel" =~ ^[1-9][0-9]*$ ]] || {
  echo "Unexpected AUR pkgrel: $published_rel" >&2
  exit 1
}
command -v vercmp >/dev/null || {
  echo 'Missing vercmp (provided by pacman)' >&2
  exit 1
}

version_order="$(vercmp "$desired_ver" "$published_ver")"
if (( version_order < 0 )); then
  echo "Refusing to downgrade from AUR ${published_ver} to ${desired_ver}" >&2
  exit 1
fi
if (( version_order > 0 )); then
  echo 1
  exit 0
fi

# A rerun of identical packaging must not inflate pkgrel. Ignore comments,
# blank lines, and pkgrel itself, which is derived rather than authored.
canonical() {
  sed -E \
    -e '/^[[:space:]]*(#|$)/d' \
    -e 's/^pkgrel=.*/pkgrel=IGNORED/' \
    "$1"
}

if cmp -s <(canonical "$desired") <(canonical "$published"); then
  echo "$published_rel"
else
  echo "$((published_rel + 1))"
fi
