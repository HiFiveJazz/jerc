#!/usr/bin/env bash
# Install a generated formula through an ephemeral Homebrew tap on a macOS runner.
# Usage: smoke-homebrew-tap.sh <local-formula-path> <expected-version>
set -euo pipefail

if [[ $# -ne 2 || ! -f "$1" || ! "$2" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Usage: $0 <local-formula-path> <expected-version>" >&2
  exit 2
fi

formula_source="$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")"
expected_version="$2"
tap="jerc-ci/local"
formula="$tap/jerc"
created_tap=false

# Do not clobber an existing tap, even if a runner is reused.
if brew tap | grep -Fxq "$tap"; then
  echo "Refusing to replace existing Homebrew tap: $tap" >&2
  exit 1
fi

cleanup() {
  result=$?
  trap - EXIT
  if [[ "$created_tap" == true ]]; then
    brew uninstall --force "$formula" || true
    brew untap --force "$tap" || true
  fi
  exit "$result"
}
trap cleanup EXIT

brew tap-new --no-git "$tap"
created_tap=true
tap_directory="$(brew --repository "$tap")"
mkdir -p "$tap_directory/Formula"
cp "$formula_source" "$tap_directory/Formula/jerc.rb"

# Homebrew rejects installation from arbitrary local formula paths. Using a
# fully-qualified tap formula also exercises current Homebrew trust semantics.
brew install "$formula"
brew test "$formula"
"$(brew --prefix "$formula")/bin/jerc" --version | grep -F "$expected_version"
