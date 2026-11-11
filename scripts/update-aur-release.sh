#!/usr/bin/env bash
# Refresh AUR packaging from an existing, fully-uploaded GitHub release.
# Usage: ./scripts/update-aur-release.sh v0.6.1
set -euo pipefail

TAG="${1:-}"
if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo 'Usage: update-aur-release.sh vMAJOR.MINOR.PATCH' >&2
  exit 2
fi

VERSION="${TAG#v}"
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
UPSTREAM='https://github.com/HiFiveJazz/jerc'

for cmd in curl sha256sum makepkg sed grep git; do
  command -v "$cmd" >/dev/null 2>&1 || {
    echo "Required command not found: $cmd" >&2
    exit 1
  }
done

TMP="$(mktemp -d)"
trap 'rm -rf -- "$TMP"' EXIT

# Stage ALL metadata before copying anything into the working tree.
# A missing or incomplete release must not leave half-updated PKGBUILDs.
packages=()
for name in jerc-bin jerc; do
  pkgdir="$ROOT/packaging/linux/aur/$name"
  [[ -f "$pkgdir/PKGBUILD" ]] || continue

  mkdir -p "$TMP/$name"
  cp -- "$pkgdir/PKGBUILD" "$TMP/$name/PKGBUILD"

  if [[ "$name" == 'jerc-bin' ]]; then
    archive="jerc-v${VERSION}-x86_64-unknown-linux-musl.tar.gz"
    url="$UPSTREAM/releases/download/$TAG/$archive"
    sum_field='sha256sums_x86_64'
  else
    archive="jerc-v${VERSION}-source.tar.gz"
    url="$UPSTREAM/archive/refs/tags/$TAG.tar.gz"
    sum_field='sha256sums'
  fi

  echo "Checking $name release archive: $url"
  curl --fail --location --retry 5 --retry-delay 2 --silent --show-error \
    --output "$TMP/$archive" "$url"
  sum="$(sha256sum "$TMP/$archive" | cut -d' ' -f1)"
  [[ "$sum" =~ ^[a-f0-9]{64}$ ]] || {
    echo "Invalid SHA-256 checksum for $name" >&2
    exit 1
  }

  if ! grep -qE '^pkgver=' "$TMP/$name/PKGBUILD" ||
     ! grep -qE '^pkgrel=' "$TMP/$name/PKGBUILD" ||
     ! grep -qE "^${sum_field}=\\(" "$TMP/$name/PKGBUILD"; then
    echo "Unexpected PKGBUILD format: $pkgdir/PKGBUILD" >&2
    exit 1
  fi

  sed -i -E \
    -e "s/^pkgver=.*/pkgver=${VERSION}/" \
    -e "s/^${sum_field}=.*/${sum_field}=('${sum}')/" \
    "$TMP/$name/PKGBUILD"

  # Compare against the published AUR package, not our local pkgrel.
  # A new upstream release starts at 1; a packaging-only change increments.
  # Re-running unchanged metadata preserves the already-published pkgrel.
  published="$TMP/aur-$name"
  git clone --quiet --depth 1 "https://aur.archlinux.org/${name}.git" "$published" || {
    echo "Cannot inspect published AUR package $name; refusing an unsafe pkgrel update" >&2
    exit 1
  }
  pkgrel="$("$ROOT/scripts/next-aur-pkgrel.sh" \
    "$TMP/$name/PKGBUILD" "$published/PKGBUILD")"
  sed -i -E "s/^pkgrel=.*/pkgrel=${pkgrel}/" "$TMP/$name/PKGBUILD"

  (
    cd "$TMP/$name"
    makepkg --printsrcinfo > .SRCINFO
    grep -Fqx $'\tpkgver = '"$VERSION" .SRCINFO || {
      echo "Generated .SRCINFO has the wrong version for $name" >&2
      exit 1
    }
    grep -Fqx $'\t'"pkgrel = ${pkgrel}" .SRCINFO || {
      echo "Generated .SRCINFO has the wrong pkgrel for $name" >&2
      exit 1
    }
    grep -Fq $'\t'"${sum_field} = ${sum}" .SRCINFO || {
      echo "Generated .SRCINFO has the wrong checksum for $name" >&2
      exit 1
    }
  )

  packages+=("$name")
  echo "Staged $name $VERSION-$pkgrel ($sum)"
done

if (( ${#packages[@]} == 0 )); then
  echo 'No AUR PKGBUILDs found' >&2
  exit 1
fi

for name in "${packages[@]}"; do
  cp -- "$TMP/$name/PKGBUILD" "$ROOT/packaging/linux/aur/$name/PKGBUILD"
  cp -- "$TMP/$name/.SRCINFO" "$ROOT/packaging/linux/aur/$name/.SRCINFO"
done

echo 'AUR metadata updated. Review changes before publishing.'
