# Validation status

This milestone is structured so its normal validation is one command:

```bash
./scripts/validate.sh
```

That script runs formatting checks, Clippy with warnings denied, and all targets/tests. A 10,000-message Maildir fixture plus `--headless-check` provides a repeatable local header-scan benchmark.

## Validation performed in the build environment

The artifact was statically checked here for:

- balanced Rust delimiters across all `.rs` sources,
- valid `Cargo.toml` and example TOML configuration,
- valid shell syntax for `scripts/validate.sh` and `scripts/perf-smoke.sh`,
- no `unsafe` blocks,
- valid GitHub Actions YAML structure,
- source-tree consistency after the final edits.

A Rust toolchain was not available in this execution container, and network package installation was unavailable, so `cargo check`, Clippy, and `cargo test` could not honestly be claimed as executed here. Run `./scripts/validate.sh` on a Rust 1.98+ machine before treating this milestone as compiler-validated.

## CI included

`.github/workflows/ci.yml` runs:

- `cargo fmt -- --check` and Clippy with warnings denied on Linux,
- the full test suite on Linux, macOS, and Windows,
- `cargo check` with Rust 1.98.1 to guard the declared MSRV.
