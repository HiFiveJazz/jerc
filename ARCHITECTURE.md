# Architecture

This first milestone intentionally borrows **aerc's separation of concerns** without porting its Go implementation.

```text
Crossterm input thread ───────────────┐
                                     │ bounded RuntimeEvent queue
Account worker 0 ── backend ─────────┤
Account worker 1 ── backend ─────────┤
                                     ▼
                              single UI/state loop
                                     │
                                     ▼
                                  Ratatui
```

## Layers

- `model.rs`: backend-neutral mailbox/message/body types. Threading metadata (`Message-ID`, `In-Reply-To`, `References`) is in the core model from day one.
- `backend/`: synchronous protocol/storage implementations. The worker owns each backend, so backend latency cannot block terminal input/rendering.
- `worker.rs`: typed actions and typed events with request IDs, inspired by aerc's worker protocol. Channels are bounded so a runaway producer cannot grow memory without limit. Ordinary UI→worker actions use non-blocking `try_send`, so backend pressure cannot stall terminal input.
- `app/`: account state, selection state, message/body caches, and commands. This layer contains no filesystem or protocol code.
- `ui/`: pure-ish rendering of app state with Ratatui.
- `runtime.rs`: one blocking event loop. There is no animation tick and no redraw polling: input or backend work wakes the loop and causes a redraw.
- `mail.rs`: RFC 5322/MIME parsing. Header scanning reads only the header block; full messages are read only when a body is actually requested.

## Why no Tokio yet?

The initial Maildir backend is naturally blocking. Giving every account a dedicated worker thread keeps the UI responsive and mirrors the architecture that already works well in lazygitrs: no async runtime or per-keypress task churn on the hot path. IMAP/JMAP may justify an internal async adapter later, but that does not need to leak through the backend trait or into rendering.

## Performance invariants

1. Rendering never performs filesystem or network I/O.
2. Moving the selection never reparses a cached body.
3. Opening a Maildir reads only message headers, not attachments or bodies.
4. Channels are bounded, and the UI never blocks waiting to enqueue ordinary backend work.
5. There is no fixed redraw timer; idle CPU should approach zero.
6. Folder scans report their wall-clock duration in the UI and `--headless-check`.
7. Tests exercise backend behavior separately from the TUI.

## Near-term backend plan

1. Maildir feature completeness: watch filesystem changes, copy, flag/star, folder create/remove, robust Maildir++ nesting.
2. IMAP worker: TLS, LOGIN/OAUTHBEARER/XOAUTH2, LIST/STATUS, UID FETCH, STORE, MOVE, IDLE, SORT/THREAD capability negotiation.
3. SMTP/sendmail abstraction and composer.
4. Local thread builder from `Message-ID`/`References` plus provider/server thread IDs when available.
5. Patch-aware message model and diff renderer shared conceptually with lazygitrs.
