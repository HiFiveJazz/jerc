#!/usr/bin/env bash
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
PKG="$ROOT/packaging/aur/jerc-bin"
TMP="$(mktemp -d)"

trap 'rm -rf "$TMP"' EXIT

# Clone the existing AUR history
git clone \
  ssh://aur@aur.archlinux.org/jerc-bin.git \
  "$TMP/jerc-bin"

# Publish only packaging metadata
cp "$PKG/PKGBUILD" "$TMP/jerc-bin/"
cp "$PKG/.SRCINFO" "$TMP/jerc-bin/"

cd "$TMP/jerc-bin"

git add PKGBUILD .SRCINFO

if git diff --cached --quiet; then
  echo "AUR is already up to date."
  exit 0
fi

git commit -m "Update jerc-bin packaging"
git push origin master

echo "Successfully published jerc-bin to AUR."
