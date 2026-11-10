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
3. Packages both GNU/glibc and statically linked musl Linux `.tar.gz` archives,
   macOS `.tar.gz` archives, and a Windows `.zip`, each with a binary,
   `LICENSE`, `README.md`, and `config.example.toml`. The release checks that
   the musl executable has no ELF interpreter or shared-library dependencies.
4. Produces `SHA256SUMS` and a `LINUX-SIZES.md` comparison of actual binary
   and compressed archive sizes. Publishes all five binaries in a GitHub Release
   with automatically generated notes **only if all jobs succeed**.

Watch the workflow under **GitHub → Actions → Release**. Its artifacts are
published at <https://github.com/HiFiveJazz/jerc/releases>.

Archives are built on GitHub-hosted runners. The GNU/Linux archive requires a
compatible glibc, whereas the musl variant is statically linked and does not
require glibc. Neither variant embeds trusted CA certificates or optional
programs such as `pdftoppm` and a browser. macOS artifacts are
unsigned and unnotarized. No crate is published to crates.io (`publish = false`
in `Cargo.toml`).

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
CC_x86_64_unknown_linux_musl=musl-gcc \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
  cargo build --locked --release --target x86_64-unknown-linux-musl

# No program interpreter or NEEDED shared-library entries should be present.
readelf -l target/x86_64-unknown-linux-musl/release/jerc | grep INTERP || true
readelf -d target/x86_64-unknown-linux-musl/release/jerc | grep NEEDED || true
ls -lh target/x86_64-unknown-linux-{gnu,musl}/release/jerc
```

If you use Arch's system Rust instead, install the matching musl standard
library package (`rust-musl`); do not mix the system Rust target libraries
with a different rustup toolchain. Preserve both release variants until the
size and behavior comparisons justify changing the default recommendation.
