# Validation status — milestone 2

Normal validation is one command:

```bash
./scripts/validate.sh
```

It runs formatting checks, Clippy with warnings denied, and all targets/tests. A 10,000-message Maildir fixture plus `--headless-check` provides a repeatable local header-scan benchmark. For an IMAP account, `--headless-check` also measures TLS/authentication, folder listing, and the bounded initial header scan.

## New milestone-2 tests/checks

The source includes tests for:

- parsing aerc-style percent-encoded password IMAP sources,
- parsing OAuth refresh-token source parameters,
- secret-safe source/debug redaction,
- URI-credential precedence over `source_cred_cmd`,
- Gmail XOAUTH2 payload shape,
- RFC 7628 OAUTHBEARER payload shape and authzid escaping,
- one-shot SASL secret response behavior,
- mapping RFC 6154 special-use attributes exposed by `imap` 2.4.1 as custom attributes,
- all milestone-1 mail parsing, Maildir, navigation, worker, and integration behavior.

## Validation performed in this build environment

The artifact is statically checked here for:

- valid `Cargo.toml` and example TOML configuration,
- valid shell syntax,
- valid GitHub Actions YAML structure,
- balanced Rust delimiters,
- no `unsafe` blocks,
- no copied user credentials/secrets in the source tree,
- API shapes cross-checked against the current documentation for `imap` 2.4.1, `rustls-connector` 0.23, and `ureq` 3.x.

A Rust toolchain is not installed in this execution container, so the artifact is **not claimed to be compiler-tested here**. Run `./scripts/validate.sh` on a Rust 1.98+ machine. Real IMAP authentication also necessarily requires testing against an account outside this sandbox.

## CI included

`.github/workflows/ci.yml` runs:

- `cargo fmt -- --check` and Clippy with warnings denied on Linux,
- the full test suite on Linux, macOS, and Windows,
- `cargo check` with Rust 1.98.1 to guard the declared MSRV.
