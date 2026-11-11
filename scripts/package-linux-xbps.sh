#!/bin/sh
# Create native Void XBPS packages for both x86_64 glibc and x86_64 musl.
# Run inside ghcr.io/void-linux/void-musl-full:latest (xbps-create is supplied there).
# Usage: ./scripts/package-linux-xbps.sh v0.6.3 [archive.tar.gz]
set -eu

ROOT="$(CDPATH= cd "$(dirname "$0")/.." && pwd)"
TAG="${1:-}"
printf "%s\n" "$TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || {
  echo 'Usage: package-linux-xbps.sh vMAJOR.MINOR.PATCH [archive.tar.gz]' >&2
  exit 2
}
for command in xbps-create tar; do
  command -v "$command" >/dev/null || { echo "Missing tool: $command" >&2; exit 1; }
done

VERSION="${TAG#v}"
BUNDLE="jerc-${TAG}-x86_64-unknown-linux-musl"
ARCHIVE="${2:-$ROOT/dist/$BUNDLE.tar.gz}"
[ -s "$ARCHIVE" ] || { echo "Missing release archive: $ARCHIVE" >&2; exit 1; }
STAGE="$ROOT/dist/xbps-stage"
rm -rf -- "$STAGE"
mkdir -p -- "$STAGE/usr/bin" "$STAGE/usr/share/licenses/jerc" "$STAGE/usr/share/doc/jerc"
for filename in jerc LICENSE README.md config.example.toml; do
  case "$filename" in
    jerc) destination="$STAGE/usr/bin/jerc" ;;
    LICENSE) destination="$STAGE/usr/share/licenses/jerc/LICENSE" ;;
    *) destination="$STAGE/usr/share/doc/jerc/$filename" ;;
  esac
  tar -xOzf "$ARCHIVE" "$BUNDLE/$filename" > "$destination"
  [ -s "$destination" ] || { echo "Empty: $filename" >&2; exit 1; }
done
chmod 0755 "$STAGE/usr/bin/jerc"
chmod 0644 "$STAGE/usr/share/licenses/jerc/LICENSE" "$STAGE/usr/share/doc/jerc/"*

# A fully static musl binary is usable on either Void libc variant, but XBPS
# architecture identifiers must match the user's chosen distribution.
cd "$ROOT/dist"
for arch in x86_64 x86_64-musl; do
  xbps-create \
    --architecture "$arch" \
    --pkgver "jerc-${VERSION}_1" \
    --desc 'Keyboard-driven terminal email client written in Rust' \
    --license 'GPL-3.0-only' \
    --homepage 'https://github.com/HiFiveJazz/jerc' \
    --maintainer 'HiFiveJazz' \
    --dependencies 'ca-certificates>=0' \
    "$STAGE"
  output="$ROOT/dist/jerc-${VERSION}_1.${arch}.xbps"
  [ -s "$output" ] || { echo "XBPS not generated: $output" >&2; exit 1; }
  echo "Created $output"
done
