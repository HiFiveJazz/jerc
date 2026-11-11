#!/usr/bin/env bash
# Package the existing static musl release archive for Alpine (x86_64).
# Usage: ./scripts/package-linux-apk.sh v0.6.3 [archive.tar.gz]
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TAG="${1:-}"
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo 'Usage: package-linux-apk.sh vMAJOR.MINOR.PATCH [archive.tar.gz]' >&2
  exit 2
}
for command in nfpm tar; do
  command -v "$command" >/dev/null || { echo "Missing tool: $command" >&2; exit 1; }
done

VERSION="${TAG#v}"
BUNDLE="jerc-${TAG}-x86_64-unknown-linux-musl"
ARCHIVE="${2:-$ROOT/dist/$BUNDLE.tar.gz}"
[[ -s "$ARCHIVE" ]] || { echo "Missing release archive: $ARCHIVE" >&2; exit 1; }
STAGE="$ROOT/dist/apk-stage"
rm -rf -- "$STAGE"
mkdir -p -- "$STAGE"
for filename in jerc LICENSE README.md config.example.toml; do
  tar -xOzf "$ARCHIVE" "$BUNDLE/$filename" > "$STAGE/$filename"
  [[ -s "$STAGE/$filename" ]] || { echo "Empty: $filename" >&2; exit 1; }
done
chmod 0755 "$STAGE/jerc"
chmod 0644 "$STAGE/LICENSE" "$STAGE/README.md" "$STAGE/config.example.toml"

export JERC_VERSION="$VERSION"
OUTPUT="$ROOT/dist/jerc-${VERSION}-r0.apk"
cd "$ROOT"
nfpm package --packager apk --config packaging/linux/alpine/nfpm.yaml --target "$OUTPUT"
[[ -s "$OUTPUT" ]] || { echo "APK not generated: $OUTPUT" >&2; exit 1; }
echo "Created $OUTPUT"
