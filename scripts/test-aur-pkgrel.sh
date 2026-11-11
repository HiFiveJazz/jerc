#!/usr/bin/env bash
# Deterministic tests of the AUR pkgrel decision logic; no network or releases.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf -- "$TMP"' EXIT

cat > "$TMP/published" <<'EOF'
pkgname=jerc
pkgver=0.6.1
pkgrel=1
options=('!lto')
sha256sums=('abc')
EOF
cp "$TMP/published" "$TMP/desired"

assert_release() {
  local expected="$1" description="$2" actual
  actual="$("$ROOT/scripts/next-aur-pkgrel.sh" "$TMP/desired" "$TMP/published")"
  if [[ "$actual" != "$expected" ]]; then
    echo "FAIL: $description (wanted $expected, got $actual)" >&2
    exit 1
  fi
  echo "PASS: $description ($actual)"
}

assert_release 1 'no change is idempotent'

sed -i "s/options=('!lto')/options=('!lto' '!debug')/" "$TMP/desired"
assert_release 2 'packaging fix increments pkgrel'

cp "$TMP/desired" "$TMP/published"
sed -i 's/^pkgrel=.*/pkgrel=2/' "$TMP/published"
assert_release 2 'rerun after publishing keeps pkgrel'

sed -i "s/options=('!lto' '!debug')/options=('!lto' '!debug' '!strip')/" "$TMP/desired"
assert_release 3 'second packaging fix increments again'

cp "$TMP/published" "$TMP/desired"
sed -i '1i# A comment-only adjustment' "$TMP/desired"
assert_release 2 'comment-only changes do not bump'

sed -i 's/pkgver=0.6.1/pkgver=0.6.2/' "$TMP/desired"
assert_release 1 'new upstream version resets pkgrel'

sed -i 's/pkgver=0.6.2/pkgver=0.6.0/' "$TMP/desired"
if "$ROOT/scripts/next-aur-pkgrel.sh" "$TMP/desired" "$TMP/published" >/dev/null 2>&1; then
  echo 'FAIL: old version should be rejected' >&2
  exit 1
fi
echo 'PASS: older version rejected'

[[ "$("$ROOT/scripts/next-aur-pkgrel.sh" "$TMP/desired" "$TMP/absent")" == 1 ]]
echo 'PASS: initial package submission starts at 1'
