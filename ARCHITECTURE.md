# Architecture

The project borrows **aerc's separation of concerns** without porting its Go implementation.

```text
Crossterm input thread ──────────────────────┐
                                             │ bounded RuntimeEvent queue
Account worker 0 ── Maildir/IMAP backend ───┤
Account worker 1 ── Maildir/IMAP backend ───┤
                                             ▼
                                      single UI/state loop
                                             │
                                             ▼
                                          Ratatui
```

## Layers

- `model.rs`: backend-neutral mailbox/message/body types. Threading metadata (`Message-ID`, `In-Reply-To`, `References`) is core data from day one.
- `backend/`: synchronous protocol/storage implementations. The worker owns each backend, so filesystem/network latency cannot block terminal input/rendering.
- `backend/imap.rs`: TLS, auth, refresh-token exchange, mailbox discovery, UID-based fetch/mutations. Secrets are held only where needed and custom `Debug` output redacts them.
- `worker.rs`: typed actions/events with request IDs. Channels are bounded. Ordinary UI→worker actions use non-blocking `try_send`.
- `app/`: account state, selection state, message/body caches, and commands. No filesystem or protocol code.
- `ui/`: rendering of app state with Ratatui.
- `runtime.rs`: one blocking event loop. There is no animation tick or redraw polling: input or backend work wakes the loop.
- `mail.rs`: RFC 5322/MIME parsing. Header scans parse header blocks; full messages are parsed only when bodies are requested.

## Why synchronous IMAP instead of Tokio?

The runtime already gives every account its own worker thread. A blocking TLS handshake, IMAP `FETCH`, or OAuth HTTP exchange blocks only that account worker, never the terminal input/render loop. That gives us a simple, testable backend trait and avoids introducing an async runtime into the UI hot path.

If later requirements make async worthwhile—multiple simultaneous IMAP connections per account, JMAP request fan-out, or sophisticated IDLE/reconnect management—we can put an async adapter *inside* the backend. The UI/worker protocol does not need to change.

## IMAP identity rule

UI-visible message IDs are backend-opaque. Maildir uses paths; IMAP uses `imap-uid:<uid>`. Sequence numbers are used only to choose the initial newest-message range. Once fetched, all message operations are UID-based so expunges/inserts from another client do not retarget actions.

Future IMAP caching must pair UIDs with `UIDVALIDITY`; this milestone does not persist IMAP UIDs across sessions yet.

## Credential model

Supported source schemes:

- `imaps://` / `imaps+login://`: password or app-password login.
- `imaps+oauthbearer://`: SASL OAUTHBEARER.
- `imaps+xoauth2://`: SASL XOAUTH2.

The URI password component can carry a credential for aerc compatibility. If an OAuth source has `token_endpoint`, that credential is a refresh token; otherwise it is an access token. `source_cred_cmd` is preferred and supplies the credential from an external store. URI credentials take precedence, matching aerc's behavior.

No CLI secret flag exists on purpose: process listings and shell history are inappropriate places for reusable tokens.

## Performance invariants

1. Rendering never performs filesystem/network I/O.
2. Moving selection never reparses a cached body.
3. Maildir opening reads headers rather than attachments/bodies.
4. IMAP opening fetches a bounded newest-header window (`max_messages`, default 500).
5. Full IMAP message bodies are fetched lazily by UID.
6. Channels are bounded; UI never blocks to enqueue ordinary backend work.
7. There is no fixed redraw timer; idle CPU should approach zero.
8. Network operations have explicit timeouts.
9. Folder scans report wall-clock duration in the UI and `--headless-check`.
10. Tests exercise backend/auth/model behavior separately from the TUI.

## Near-term plan

1. Capability negotiation, IDLE/reconnect, and unsolicited response handling.
2. Header pagination and a cache keyed by account/mailbox/UIDVALIDITY/UID.
3. SMTP/sendmail abstraction and composer.
4. Thread builder and patch-series model.
5. Shared-quality diff renderer and Git patch application with lazygitrs integration.
