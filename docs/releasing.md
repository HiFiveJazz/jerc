# Publishing Jerc releases

Jerc publishes GitHub Releases automatically from **pushed version tags**.
The release workflow is `.github/workflows/release.yml`. No personal access
token, release bot account, or manual binary upload is required: the publishing
job uses GitHub's short-lived `GITHUB_TOKEN` with `contents: write`.

## One-time setup

1. Enable GitHub Actions for the repository if it is not already enabled.
2. Allow the repository's workflows to run normally. Organization policies
   must allow the workflow's `contents: write` permission.
3. Keep the `Release` workflow and packaging script committed on the branch
   from which you create your version tag.

## Cut a release

From a clean, tested checkout on `main`:

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo audit

git add Cargo.toml Cargo.lock README.md CHANGELOG.md docs/releasing.md \
  .github/workflows/release.yml scripts/package_release.py
git commit -m "Release v0.6.0"
git push origin main

git tag -a v0.6.0 -m "Jerc v0.6.0"
git push origin v0.6.0
```

**Important:** Push the commit containing the workflow *before* pushing the
tag, and tag that same commit. Do not tag an earlier revision. Subsequent
versions follow the same pattern: update `Cargo.toml`, `Cargo.lock`, and the
changelog, then push `vX.Y.Z`.

## What happens automatically

When a version tag is pushed, the workflow:

1. Verifies that the tag matches the version in `Cargo.toml`, runs `cargo fmt`
   and Clippy, and refuses mismatches.
2. Runs the full test suite on Linux (including a musl-target test run), both
   macOS architectures, and Windows; builds a `--release --locked` binary for
   each target.
3. Packages one fully statically linked musl Linux `.tar.gz` archive, two macOS
   `.tar.gz` archives, and a Windows `.zip`, each with a binary, `LICENSE`,
   `README.md`, and `config.example.toml`. The workflow rejects a musl binary
   with an ELF interpreter or required shared libraries.
4. Creates one x86-64 RPM and one amd64 DEB from the verified Linux musl
   archive. RPM installation is checked on Fedora 44 and openSUSE Tumbleweed;
   DEB installation is checked on Debian 13 and Ubuntu 24.04 LTS. Distro smoke
   tests run concurrently where their package artifacts are ready.
5. Builds an Alpine APK, both Void XBPS variants and a Gentoo overlay with
   source and binary ebuilds. It also archives the tagged Rust source and
   locked vendor crates for offline builds, then generates checksummed Alpine
   and Void source recipes.
6. Generates a checksum-pinned Homebrew tap formula for native macOS Intel and
   Apple Silicon archives, then installs and tests it with Homebrew on both
   architectures before publishing. The formula ships in
   `jerc-homebrew-vVERSION.tar.gz` for later addition to a separate tap.
7. Produces `SHA256SUMS` for all thirteen release assets and publishes them
   **only if the package and runtime checks succeed**.

To test packaging of an **existing** release without a new tag or publication,
run `gh workflow run test-linux-packages.yml -f tag=v0.6.2`. The manual
workflow builds RPM and DEB from that release archive, then runs four Linux
installation tests in parallel. It does not modify GitHub Releases, AUR,
or crates.io.

Watch the workflow under **GitHub → Actions → Release**. Its artifacts are
published at <https://github.com/HiFiveJazz/jerc/releases>.

Archives are built on GitHub-hosted runners. The Linux release uses static musl
and does not require glibc. It still relies on host CA certificates and optional
programs such as `pdftoppm` and a browser. macOS artifacts are unsigned and
unnotarized. crates.io publication follows a successful GitHub Release
using Trusted Publishing after the first manual upload. GNU/glibc builds
remain available by building from source with
`cargo build --release --target x86_64-unknown-linux-gnu`.

## One-time crates.io setup

The first version of Jerc must be published manually before crates.io can
register a GitHub Trusted Publisher. First, commit the manifest changes on
`main`, confirm the `jerc` crate name is available, then run:

```sh
python scripts/test_binstall_metadata.py
cargo package --list
cargo publish --dry-run --locked
cargo login
cargo publish --locked
```

Use a crates.io API token for `cargo login`; never put it in the repository.
For 0.6.1 this is an initial, packaging-only publication from `main` after the
original GitHub tag was created. If exact tag-to-crate source identity is
required, instead bootstrap a new version with its own release tag.

In GitHub Actions, create an environment named `crates-io`. In the Jerc crate's
crates.io Settings > Trusted Publishing, register:

- Provider: GitHub Actions
- Owner: `HiFiveJazz`
- Repository: `jerc`
- Workflow filename: `release.yml`
- Environment: `crates-io`

The `publish_crate` job in `release.yml` runs **after** successful GitHub
Release creation on future version-tag pushes. It uses short-lived OIDC
credentials rather than a permanent token and skips versions already in the
registry. AUR publishing remains a separate workflow.

The `cargo-binstall` metadata reuses our GitHub Release binaries. Validate
its target-to-asset names with `python scripts/test_binstall_metadata.py`.

## If the workflow fails

Review the failing job before publishing. Correct the code on `main`, bump to
another version (for example `0.6.1`), and push a **new** release tag. Avoid
force-moving an existing published release tag. To retry a transient failure
without changing source, use **Actions → Release → Re-run jobs**.

GitHub also allows [manual release creation](https://docs.github.com/en/repositories/releasing-projects-on-github/managing-releases-in-a-repository),
but manually creating a release does not run the tag-push automation unless
its tag was pushed to the repository as part of that process.

## Compare dynamic and static Linux builds before tagging

Use **GitHub → Actions → Compare Linux linkage → Run workflow** on the branch
containing the workflow. This **never creates a release**. It compiles both
Linux binaries, runs the tests targeting musl, verifies static ELF linkage,
packages both variants, and uploads a `linux-gnu-vs-musl` artifact containing
both archives and `LINUX-SIZES.md`. The comparison is also printed in the job
summary, with exact byte counts and relative size changes.

Alternatively, on Arch Linux with Rust via rustup:

```sh
sudo pacman -S --needed musl gcc
rustup target add x86_64-unknown-linux-musl
cargo build --locked --release --target x86_64-unknown-linux-gnu
# Use musl-gcc to compile native C dependencies only. Do not set it as
# CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER: rustc's default link step
# is intended to produce a self-contained binary.
env -u CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER \
  -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS \
  CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --locked --release --target x86_64-unknown-linux-musl

# Both checks must find no entries; then verify the binary actually starts.
binary=target/x86_64-unknown-linux-musl/release/jerc
if readelf -l "$binary" | grep -q INTERP; then echo 'unexpected dynamic loader' >&2; exit 1; fi
if readelf -d "$binary" | grep -q NEEDED; then echo 'unexpected shared dependency' >&2; exit 1; fi
"$binary" --version
ls -lh target/x86_64-unknown-linux-{gnu,musl}/release/jerc
```

If the musl binary still has a `/lib/ld-musl-x86_64.so.1` interpreter,
check `$CARGO_HOME/config.toml`, `~/.cargo/config.toml`, and parent `.cargo`
config files for a musl `linker = "musl-gcc"` override. Remove it for the
final Rust link. A binary with an `INTERP` entry must **not** be published as
statically linked, even if it has no `NEEDED` libraries. A startup crash is a
release blocker; do not work around it by changing the ELF interpreter.

If you use Arch's system Rust instead, install the matching musl standard
library package (`rust-musl`); do not mix the system Rust target libraries
with a different rustup toolchain.

The release workflow intentionally publishes only the musl Linux build.
The optional comparison workflow is retained for development and debugging.

## Additional Linux distributions

The release pipeline also generates and smoke-tests an Alpine x86-64 `.apk`,
Void x86-64 glibc and musl `.xbps` packages, and a versioned Gentoo overlay
with `mail-client/jerc` (source) and `mail-client/jerc-bin` (binary). All reuse the same verified static musl Linux release.
Alpine and Void are installed through their native package managers in CI.
The Gentoo job validates both ebuilds and Manifests and executes the binary on
Gentoo stage3; a real `emerge` source compilation is not yet covered. Alpine
and Void also get versioned native source-build recipes with checksum-pinned
`jerc-vVERSION-vendored.tar.gz` archives. These source recipes need native
abuild/xbps-src/Portage compilation testing before upstream submission.

To test these formats with existing v0.6.2 release assets without re-tagging:

```sh
gh workflow run test-linux-packages.yml -f tag=v0.6.2
```

None of these downloads are an entry in the distributions' official package
repositories, and downloaded packages/overlay do not provide unattended
updates. See `packaging/linux/README.md` for installation and submission paths.

## Homebrew macOS CI

The release workflow builds and tests Jerc natively on Intel and Apple Silicon
macOS, then runs a *second* check: Homebrew must install the release archives
on both native runner architectures and pass `brew test`. The generated formula
uses real GitHub Release URLs and the exact archives' SHA-256 digests; CI uses
a local `file://` URL variant because release assets are not public yet.

Test previously published macOS binaries without tagging again:

```sh
gh workflow run test-homebrew.yml -f tag=v0.6.2
gh run list --workflow test-homebrew.yml --limit 3
```

The generated formula does **not** create or publish a Homebrew tap. Refer to
`packaging/macos/homebrew/README.md` for creating a separate `homebrew-jerc` tap
and eventually submitting a separate source-build formula to Homebrew/core.

## Windows package managers (Scoop, WinGet, Chocolatey)

The Windows release ZIP is used to generate checksum-pinned Scoop and WinGet
manifests and a self-contained Chocolatey `.nupkg`. The normal Release workflow
checks those artifacts and installs the Chocolatey package before publication.
Two additional assets are uploaded with each successful release:
`jerc-windows-packages-vVERSION.zip` and `jerc.VERSION.nupkg`.

After publishing a release, validate native Windows installations on GitHub's
Windows runners without creating a new tag:

```sh
gh workflow run test-windows-packages.yml -f tag=v0.6.2
```

The post-release test runs Scoop and Chocolatey install/uninstall, and native
WinGet validation/install **if `winget` is available on the runner**. When not
available, WinGet's Python metadata checks still run, but its native install
remains unverified and the workflow reports the limitation explicitly.

The Scoop manifest requires a public bucket and the WinGet manifest requires a
PR to `microsoft/winget-pkgs`; Chocolatey's package needs an upstream submission.
Generating GitHub assets alone does not list Jerc in those package managers.
