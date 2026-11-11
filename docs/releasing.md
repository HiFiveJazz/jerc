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
4. Creates an x86-64 RPM from the verified Linux musl archive and tests
   installation and execution in a Fedora 44 container. The RPM is an unsigned
   GitHub Release download, not a Copr or official Fedora repository package.
5. Produces `SHA256SUMS` and publishes the four archives plus the RPM in a
   GitHub Release with automatically generated notes **only if all jobs succeed**.

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
