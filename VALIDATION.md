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
- Add two `[[accounts]]` blocks and rotate with `Alt-p` / `Alt-n`; the active tab and status should change without restarting.

## Mail correctness regression suite (tests-first milestone)

Run `cargo test --all-targets` (and `cargo fmt` first). The new
`tests/mail_correctness.rs`, `tests/drafts.rs`, and worker protocol regression
cover generated/unique Message-ID, RFC 5322 thread headers, Bcc envelope versus
serialized headers, non-ASCII subjects and attachment names, multipart nesting,
raw `message/rfc822` forwards, Reply-To / Reply All / List-Post recipient routing,
subject-prefix preservation, header injection rejection, postponement round trips,
account-scoped draft isolation, and failure to append a Sent copy after successful
SMTP acceptance. The worker must **never retry SMTP** merely because saving a
Sent copy failed.

Current local postponed drafts are per-account JSON in the user data directory
(`~/.local/share/aerc-rs/postponed` on typical Linux systems). They are
confidential and are written with 0600 permissions on Unix systems. They are
not yet synchronized with server-side Drafts. No signing or encryption was added.

This artifact was reconstructed from the complete local patch stack and checked
with `git apply --check` and `git diff --check`. This build environment has no
Rust toolchain, so the Cargo tests cannot be run until applied on the user host.

## Inline forward, attachments and triage polish

`tests/polish_regressions.rs` covers inline HTML/plain forwarding with MIME
attachments, decoded PDF bytes and mailbox-list attachment hints. Rust unit
tests cover Gmail archive route classification, current-line visibility for
Default/Monochrome themes, and syntax colors for quoted Git diffs.

Manually verify with a disposable Gmail email: `a` from INBOX, `U` from All
Mail, and refresh both views. Gmail auto-expunge settings affect how the IMAP
`\Deleted` flag is interpreted; do not test first on irreplaceable mail.

In the reader or browser, press `z` for the MIME attachment picker, `p` to
preview PNG/JPEG/GIF/WebP, Enter to open a PDF or other file with the system
handler, and `m` to show or hide Message-ID. Fwd a multipart HTML email with
a PDF and confirm the forwarded message is inline with the PDF but without
a `forwarded.eml` MIME part. Inline CID references need future dedicated
`multipart/related` handling to reproduce all original images exactly.

This environment has no Rust compiler; `cargo fmt` and `cargo test --all-targets`
must be run on the target machine.
