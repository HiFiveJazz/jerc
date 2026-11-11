#!/usr/bin/env bash
# Create a DEB from the same verified static musl release archive as jerc-bin.
# Usage: ./scripts/package-linux-deb.sh v0.6.3 [archive.tar.gz]
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TAG="${1:-}"
if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo 'Usage: package-linux-deb.sh vMAJOR.MINOR.PATCH [archive.tar.gz]' >&2
  exit 2
fi
for command in nfpm tar; do
  command -v "$command" >/dev/null || {
    echo "Required command not found: $command" >&2
    exit 1
  }
done

VERSION="${TAG#v}"
BUNDLE="jerc-${TAG}-x86_64-unknown-linux-musl"
ARCHIVE="${2:-$ROOT/dist/$BUNDLE.tar.gz}"
[[ -s "$ARCHIVE" ]] || { echo "Missing release archive: $ARCHIVE" >&2; exit 1; }

# Explicitly extract known paths; never unpack arbitrary archive members.
STAGE="$ROOT/dist/deb-stage"
rm -rf -- "$STAGE"
mkdir -p -- "$STAGE"
for filename in jerc LICENSE README.md config.example.toml; do
  tar -xOzf "$ARCHIVE" "$BUNDLE/$filename" > "$STAGE/$filename"
  test -s "$STAGE/$filename" || { echo "Empty file: $filename" >&2; exit 1; }
done
chmod 0755 "$STAGE/jerc"
chmod 0644 "$STAGE/LICENSE" "$STAGE/README.md" "$STAGE/config.example.toml"

export JERC_VERSION="$VERSION"
OUTPUT="$ROOT/dist/jerc_${VERSION}-1_amd64.deb"
cd "$ROOT"
nfpm package --packager deb --config packaging/linux/debian/nfpm.yaml --target "$OUTPUT"
[[ -s "$OUTPUT" ]] || { echo "DEB not generated: $OUTPUT" >&2; exit 1; }
echo "Created $OUTPUT"
