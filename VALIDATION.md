# Validation status — milestone 3.1

Normal local validation:

```bash
./scripts/validate.sh
```

It runs formatting, Clippy with warnings denied, and all targets/tests. `--headless-check` now measures four network stages independently: connect/auth, mailbox LIST, selected-mailbox header scan, and first selected-body load.

## Milestone-3.1 test coverage added/updated

The source now includes tests for:

- Vim/lazygitrs-style key mapping including numeric prefixes, smooth Ctrl-j/Ctrl-k commands, theme picker and raw export;
- Vim-style relative line-number calculation;
- built-in theme lookup/uniqueness;
- HTML-only message rendering without an external browser/filter;
- body plain/HTML fallback behavior;
- outgoing SMTP source parsing and secret redaction;
- outgoing account configuration redaction;
- the updated worker protocol, including the new mailbox-status and outgoing seams;
- all previous Maildir, threading, IMAP-source/auth, redaction, and model tests.

## Validation performed in this build environment

This environment still does not contain a Rust toolchain, and its package repository update path timed out. Therefore this artifact is **not claimed to be compiler-tested here**.

Static checks performed before packaging include:

- TOML parsing for `Cargo.toml` and `config.example.toml`;
- GitHub Actions YAML parsing;
- shell syntax checks;
- Rust delimiter/string/comment structural scan;
- search for stale milestone-2 field/signature uses;
- no `unsafe` blocks;
- no copied user credentials/tokens in the artifact;
- source-level review of the new browser/message state split, theme persistence, and raw-message export paths.

The previous HTML fix uses `html2text`; `august` is no longer a dependency.

Run `./scripts/validate.sh` on the user's Rust machine. Any compiler/API error from that run should be treated as the next test result rather than hidden or assumed away.
