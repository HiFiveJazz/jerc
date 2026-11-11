#!/usr/bin/env bash
# Create an RPM from the same static musl archive used by jerc-bin.
# Usage: ./scripts/package-linux-rpm.sh v0.6.2 [archive.tar.gz]
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TAG="${1:-}"
if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo 'Usage: package-linux-rpm.sh vMAJOR.MINOR.PATCH [archive.tar.gz]' >&2
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

# Extract only explicitly named release files, not arbitrary archive paths.
STAGE="$ROOT/dist/rpm-stage"
rm -rf -- "$STAGE"
mkdir -p -- "$STAGE"
for filename in jerc LICENSE README.md config.example.toml; do
  tar -xOzf "$ARCHIVE" "$BUNDLE/$filename" > "$STAGE/$filename"
  test -s "$STAGE/$filename" || { echo "Empty file: $filename" >&2; exit 1; }
done
chmod 0755 "$STAGE/jerc"
chmod 0644 "$STAGE/LICENSE" "$STAGE/README.md" "$STAGE/config.example.toml"

export JERC_VERSION="$VERSION"
OUTPUT="$ROOT/dist/jerc-${VERSION}-1.x86_64.rpm"
cd "$ROOT"
nfpm package --packager rpm --config packaging/linux/fedora/nfpm.yaml --target "$OUTPUT"
[[ -s "$OUTPUT" ]] || { echo "RPM not generated: $OUTPUT" >&2; exit 1; }
echo "Created $OUTPUT"
