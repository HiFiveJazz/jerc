# Architecture — milestone 3.1

The project borrows aerc's separation of concerns without porting its Go implementation.

```text
Crossterm input thread ───────────────────────────┐
                                                  │ bounded RuntimeEvent queue
Account worker 0 ── Maildir/IMAP + SMTP sender ──┤
Account worker 1 ── Maildir/IMAP + SMTP sender ──┤
                                                  ▼
                                           UI/state loop
                                                  │
                                      event / active-animation redraw
                                                  ▼
                                               Ratatui
```

## Layers

- `model.rs`: backend-neutral mailbox, summary, body, flags, capabilities, and outgoing-message types. Thread metadata (`Message-ID`, `In-Reply-To`, `References`) is core data.
- `backend/`: synchronous storage/protocol implementations behind `MailBackend`.
- `backend/imap.rs`: TLS/auth/OAuth, mailbox discovery, lazy mailbox status, bounded header scans, UID body/mutations.
- `outgoing/`: transport-neutral `MailSender`; milestone 3 implements SMTP using Lettre.
- `worker.rs`: per-account typed actions/events. UI posts are bounded/non-blocking. Queued body fetches coalesce to the newest selection.
- `app/`: browser/message mode, selection, count prefixes, smooth-scroll targets, loading flags, body cache, read-state policy, help/theme/composer state.
- `ui/`: Ratatui rendering only; no filesystem or network access. Theme colors are centralized in `theme.rs` rather than hard-coded into widgets.
- `theme.rs`: built-in palettes plus the tiny persisted active-theme state.
- `runtime.rs`: blocks when idle. A 10 ms timeout exists only while a smooth-scroll animation is active, so there is still no permanent polling/redraw timer.
- `mail.rs`: RFC 5322/MIME parsing plus safe terminal-oriented HTML conversion.

## Read-state rule

`BODY.PEEK[]` never implicitly sets IMAP `\\Seen`. Browsing never requests a body. After Enter opens the reader, the selected body is fetched; when it actually arrives while that message is still open, the app sends an explicit `SetSeen(true)`. Smooth list animation therefore performs zero body requests and cannot mark intermediate mail read.

Explicit `u` uses the same action and the UI updates when the server acknowledges the flag change.

## First-paint performance

Milestone 2 originally performed `LIST` and then one sequential `STATUS` round trip per mailbox. On hosted Gmail this made a small folder list take seconds. Milestone 3 changes the pipeline to:

```text
connect -> LIST -> render folders -> SELECT/FETCH selected mailbox -> browser
                            \\-> lazy STATUS requests when worker is otherwise free
```

The initial IMAP header fetch also asks only for `DATE`, `FROM`, `SUBJECT`, `MESSAGE-ID`, `IN-REPLY-TO`, and `REFERENCES`, plus UID/flags/size.

Full message data is still fetched when the user explicitly opens a message. Because the browser never prefetches bodies, scrolling no longer competes with mailbox/status work. A later optimization can use IMAP `BODYSTRUCTURE` and section fetches to download only the selected text/HTML MIME parts before attachments.

## Smooth navigation

Normal `j/k` changes selection immediately. Numeric prefixes apply to ordinary movement. `Ctrl-j/Ctrl-k` maintain a target 22 rows away and advance one visible row per animation frame. Repeated/opposite input adjusts the current target rather than queuing animations, so the motion is interruptible and reversible. No body fetch occurs at the target; mail content is loaded only after Enter opens the reader.

## Outgoing mail

`MailSender` is separate from `MailBackend`; receiving and sending therefore do not have to use the same protocol/provider forever.

SMTP supports implicit TLS and required STARTTLS, PLAIN/LOGIN/XOAUTH2, and the same refresh-token exchange model as IMAP OAuth. `from` is configurable; if absent, the SMTP username becomes the envelope/header From address.

The current SMTP sender shares its account worker with IMAP, which keeps the implementation simple and keeps network work off the UI. A dedicated outgoing worker/queue is an easy later optimization if large sends should never serialize with receive operations.

## Performance invariants

1. Rendering performs no filesystem/network I/O.
2. Idle UI has no redraw polling.
3. Ordinary UI→worker enqueue never blocks the terminal thread.
4. Maildir and IMAP list views do not parse/download message bodies.
5. IMAP startup does not wait for every mailbox unread count.
6. Header scans are bounded (`max_messages`, default 500).
7. Browser navigation never fetches bodies; only an explicitly opened message does.
8. Network operations have explicit timeouts.
9. Secrets are redacted from diagnostics/debug output.
10. Headless metrics separate connect, folder list, header scan, and body latency.
