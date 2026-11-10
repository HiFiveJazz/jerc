#!/usr/bin/env bash
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
PKG_NAME="${1:-jerc-bin}"

case "$PKG_NAME" in
  jerc|jerc-bin) ;;
  *)
    echo "Usage: $0 [jerc|jerc-bin]" >&2
    exit 1
    ;;
esac

PKG="$ROOT/packaging/aur/$PKG_NAME"

for file in PKGBUILD .SRCINFO; do
  if [[ ! -f "$PKG/$file" ]]; then
    echo "Missing $PKG/$file" >&2
    exit 1
  fi
done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

git -c init.defaultBranch=master clone \
  "ssh://aur@aur.archlinux.org/${PKG_NAME}.git" \
  "$TMP/$PKG_NAME"

cp "$PKG/PKGBUILD" "$TMP/$PKG_NAME/"
cp "$PKG/.SRCINFO" "$TMP/$PKG_NAME/"

cd "$TMP/$PKG_NAME"

git add PKGBUILD .SRCINFO

if git diff --cached --quiet; then
  echo "$PKG_NAME is already up to date."
  exit 0
fi

git commit -m "Update $PKG_NAME packaging"
git push origin HEAD:master

echo "Successfully published $PKG_NAME to AUR."
