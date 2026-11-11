# Jerc Homebrew formula

`jerc` ships native macOS x86_64 and arm64 release archives. The release CI
checksums both archives and generates a ready-to-copy `Formula/jerc.rb`, then
installs it with Homebrew and runs `brew test` on **both** native macOS runners.
No signing credentials or brew tap are required for the CI tests.

## Test an existing GitHub Release

```sh
gh workflow run test-homebrew.yml -f tag=v0.6.2
gh run list --workflow test-homebrew.yml --limit 3
```

That workflow downloads the **published** archives, verifies their SHA256SUMS
entries, generates a release-URL formula and a local-file-URL variant, then
installs/tests the latter. It does not modify the release or install a tap.

## What a new tagged release produces

`release.yml` publishes `jerc-homebrew-vVERSION.tar.gz`, containing
`Formula/jerc.rb` with the hashes of the exact macOS archives from the release.
Both native Homebrew installation tests must pass *before* GitHub Release
publication. The CI test uses local archives because release URLs do not exist
yet. The published formula uses the proper GitHub Release URLs.

To distribute install/update commands such as `brew install HiFiveJazz/jerc/jerc`
without committing generated files to `main`, create a separate GitHub
`HiFiveJazz/homebrew-jerc` **tap** (recommended), add its `Formula/jerc.rb` from
the release bundle, and keep it updated after each release. With that tap in
place, the installation command would be:

```sh
brew install HiFiveJazz/jerc/jerc
```

**That command won't work until the tap actually exists.** Homebrew/core
inclusion is a separate upstream submission, generally requiring a
source-build formula rather than a wrapper around upstream binaries.
