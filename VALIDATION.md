# Validation status — milestone 5

Normal local validation:

```bash
./scripts/validate.sh
```

It runs formatting, Clippy with warnings denied, and all targets/tests. `--headless-check` continues to report connect/auth, mailbox LIST, selected-mailbox header scan, and first-body latency separately.

## Milestone-5 coverage added/updated

The source includes tests for:

- lowercase message movement, uppercase mailbox movement, and Ctrl-Shift mailbox smooth-scroll bindings;
- visual selection and immediate/confirmed delete bindings;
- pagination metadata and page merging for Maildir/worker protocol paths;
- Neovim-style viewport scrolloff and modal compose line/visual editing helpers;
- modal compose field/mode editing helpers;
- HTML rich rendering with theme-owned colours, cleaned heading markers, closed semantic tables, presentation-table compaction, annotated links, and zero-width preheader cleanup;
- rich-HTML-default/plain-fallback behavior and remote-image key/config plumbing;
- existing IMAP source/auth parsing, Maildir behavior, themes, outgoing mail, and worker protocol behavior.

## Validation performed in the patch build environment

This environment does not contain a Rust toolchain, so milestone 5 is **not claimed to be compiler-tested here**. The patch is only ready to hand off after source/static checks and a clean `git apply --check` + `git apply` against a fresh reconstruction of the exact user baseline (the supplied archive plus the verified milestone-4 patch).

Run `./scripts/validate.sh` after applying it. Any compiler or API error from that run should be treated as the next concrete test result.

## Polish regression checks

- Forward a rich HTML message and confirm the outgoing MIME contains the untouched original as a `message/rfc822` attachment rather than the terminal HTML/text projection.
- Put the reader cursor on two different links and press `o`; each press must open the URL under the cursor. Tab/Shift-Tab must no longer cycle reader links, and browser helper stderr/stdout must not leak into the TUI.
- Toggle read/unread with `u`; the list marker and mailbox unread count should update immediately, before the server round trip completes.
- Start `/` search and verify the editable query appears once (in the search row), not again in the status bar.
- Open a quoted reply with `R`; the attribution should read `On <weekday> <month> <day>, <year> at <time> <zone>, <author> wrote:` and quoted `>` lines should be gray in both reader and composer.
- Add two `[[accounts]]` blocks and rotate with `[` / `]`; the active tab and status should change without restarting.
