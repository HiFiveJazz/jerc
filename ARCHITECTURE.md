# Architecture

Jerc separates interactive UI state from blocking mail protocol work, with modal keyboard interaction and a bounded worker queue.

```text
Crossterm input thread ───────────────────────────┐
                                                  │ bounded RuntimeEvent queue
Account worker 0 ── Maildir/IMAP + SMTP sender ──┤
Account worker 1 ── Maildir/IMAP + SMTP sender ──┤
Remote image worker ── bounded fetch/decode ─────┤
                                                  ▼
                                           UI/state loop
                                                  │
                                      event / active-animation redraw
                                                  ▼
                                               Ratatui
```

## Finding code by feature

The renderer is divided by surface (`src/ui/render/{mail_list,reader,compose,contacts,attachments,dialogs,help,status,setup}.rs`) with shared formatting in `common.rs`. The root `src/ui/render.rs` owns layout and overlay ordering. These modules use the same app state and theme; the split is organizational and does not change rendering behavior.

`src/app/state.rs` defines the single UI-owned `App`/`AccountState` data model, configuration-independent limits, and state types. Methods are grouped by responsibility under `src/app/state/`: `account_state.rs` holds per-account selection/prefetch behavior; `app_core.rs`, `app_recovery.rs`, and `app_notices.rs` handle common state, connection recovery, and warnings; `app_setup.rs`, `app_animation.rs`, `app_navigation.rs`, `app_reader.rs`, and `app_controls.rs` implement the corresponding app interactions. Reusable MIME, clipboard, attachment, and viewport utilities have dedicated `*_util.rs` modules. Existing feature modules (`initialization.rs`, `input_dispatch.rs`, `mouse.rs`, `reader_text.rs`, Compose, Contacts, cache and worker events) remain separate. **All of these files implement or support the same state types**, not separate state stores. State regression tests live beside the implementation.

The shared behavior layer is:

- `src/app/vim.rs`: UTF-8 character offsets, word motions, text objects, visual ranges, persistent marked selection, the unnamed yank register, a single pending-command/count state machine (`VimSequence`), and bounded undo/redo history (`VimHistory`). Compose and Contacts share these primitives while retaining their distinct field validation, editing actions, and mode-specific UI; the reader shares motions and the register. Editor history and pending keys are ephemeral, not serialized in drafts.
- `src/app/navigation.rs`: bounded/wrapping row navigation, filtered selection, smooth-scroll steps, and scrolloff in display rows. Mailbox lists, file pickers, contacts, and the reader reuse it.
- `src/app/help_palette.rs`: one searchable-help key controller for app, contacts, composer, file picker, and attachment destination. It operates on their existing query/cursor fields rather than introducing another state store.
- `src/ui/render/common.rs`: shared popup shells, centered list windows, borrowed filtered slices, display-width truncation, and rendering helpers. Feature renderers keep ownership of their content while using a common themed overlay frame. All searchable keybinding popups use `draw_help_palette` in `help.rs`.

Mailbox cache snapshots are constructed in one place (`AccountState::current_cache_snapshot`) for both in-session restoration and durable persistence. `forget_cached_folder` invalidates the memory snapshot, its LRU entry, and the on-disk snapshot together, without clearing the active folder. `invalidate_remote_folder` additionally marks STATUS counts unknown after sends and draft uploads. Optimistic Seen updates and correlated worker acknowledgements both use `apply_seen_flag`; the second application is idempotent so unread counts cannot drift from duplicate transitions.

Mailbox synchronization uses `src/app/state/account_sync.rs`: initial mailbox counts, periodic Inbox polls and IMAP IDLE notifications share one deduplicated, request-ID-correlated STATUS queue. The queue retains at most one waiting request per known mailbox, even during an IDLE notification burst, and never applies an older completion over a newer pending request. Foreground SELECT/FETCH requests still use their existing correlation IDs. A failed mutation/Seen request removes only its own pending record, not those of unrelated operations.

Worker-event routing is centralized in `src/app/state/worker_events.rs`, whose focused handlers live in `worker_events/{mailbox,content,outgoing,errors}.rs`. The UI thread remains the single owner of `App`; handlers are organized by event ownership rather than creating extra state stores. Account identities are parsed only for event categories that need contact/recipient learning.

The IMAP implementation and its capability-gated native threading parser live
in `src/backend/imap/`; the JMAP implementation lives in `src/backend/jmap/`.
Its `session.rs` module validates discovered capabilities and endpoint origins;
`push.rs` parses and coalesces SSE state notifications but does not
start a new worker or perform network I/O on its own. `body.rs`, `views.rs`,
and `submission.rs` remain focused on the existing reader, search/conversation,
and outgoing-message paths. Test modules sit alongside these implementations;
the main protocol modules are not a dumping ground for regression tests.

## Worker and cache boundaries

`src/worker.rs` defines public actions, events, queue handles and worker ownership. The implementation is split into `src/worker/handle.rs` (bounded queue lifecycle, startup/shutdown, and recovery), `foreground.rs` (interactive execution and queue compaction), `background.rs` (low-priority fetch and STATUS reconciliation), `idle.rs` (separate IMAP IDLE watchers), and `helpers.rs` (raw MIME and attachment jobs). These are **the same worker threads, queues, scheduling rules, and memory bounds** as before the split; the files do not create additional workers.

`src/cache.rs` retains its snapshot models, cache format, identities, limits, initialization, and offline journal. `src/cache/writer.rs` owns the existing async batched writer and index change application; `body_store.rs` and `raw_store.rs` own the existing lazy, bounded disk-backed stores. None of the modules performs disk I/O from a renderer; cache JSON and on-disk keys are unchanged.

The protocol backends and the UI worker remain separate; renderers must never start filesystem or network I/O. Prefer adding behavior to the existing shared primitives over introducing new duplicate implementations.

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for a file map and local validation commands.

## Browser model

The browser has one logical message cursor and one mailbox cursor. `j/k` moves messages; `J/K` changes mailboxes. Smooth mailbox motion delays opening the destination until animation finishes, so rapidly traversing folders does not serialize expensive intermediate mailbox loads.

The message renderer draws only the visible filtered slice and clears that region before painting. The state layer owns the viewport offset and maintains a four-row scrolloff. This avoids `TableState` changing the viewport behind the application and prevents stale characters in the line-number gutter.

`v` stores a visual anchor. Actions operate on the inclusive anchor-to-cursor range; `Esc` clears that anchor before it performs any other escape action.

## Pagination and mailbox cache

`AccountState` keeps an in-memory cache per opened mailbox: loaded summaries, bodies already fetched, cursor/viewport state, and pagination progress. `page_size` controls each interactive header request (default 100). The first page contains newest mail; reaching the bottom requests the next older page and merges it into the existing list.

Switching to a cached mailbox restores it synchronously. A first-page refresh performs a real IMAP `SELECT` so new arrivals are visible; subsequent pages reuse the selected mailbox count where possible. The worker coalesces stale queued mailbox opens and body fetches.

Search is a presentation filter over the currently selected mailbox cache. When a filter is active and too few matches are loaded to fill the viewport, pagination continues progressively until enough matches are available or the server-side mailbox window is exhausted.

## Mutation policy

`d` asks for confirmation; `D` deliberately bypasses confirmation. Outside Trash the action prefers the advertised Trash role; inside Trash it becomes permanent deletion. `a` prefers the advertised Archive/All Mail role.

Bulk IMAP mutations build one UID sequence for the whole selection. Successful mutations update the current list/cache optimistically and never re-LIST the account or jump to the configured default mailbox. A failed mutation reloads only the current mailbox to reconcile state.

## Split preview and body loading

`p` toggles a vertical preview beside the message list. Browser movement still loads only headers until a preview or full reader needs a body. Cached bodies are reused by both surfaces. `b` can hide the mailbox sidebar so the browser/preview can use the full terminal width.

## HTML mail

MIME parsing remains in `mail-parser`. A real HTML part is the default reader view and `html2text`'s rich line API preserves table structure plus annotations such as links, images, emphasis, strong text, strikeout and code. Those annotations are converted into model-level spans, but sender foreground/background CSS is deliberately not painted into the TUI: semantic emphasis survives while the active theme owns readability. `t` switches to the supplied text/plain part or, for HTML-only mail, a generated plain fallback.

Before rich conversion, HTML tables explicitly marked `role="presentation"` are linearized so newsletter/grid scaffolding does not become data-table chrome. Post-processing then removes zero-width/preheader padding, Markdown-like heading markers emitted by the conversion layer, generic image labels and repeated layout-table chrome. Dense nested layout blocks are flattened as a fallback; remaining semantic tables have their outer edges closed instead of ending in dangling junctions.

`ratatui-image` provides Kitty/Sixel/iTerm2 rendering with a half-block fallback. Remote image HTTP/decode work runs on its own bounded worker, has an 8 MiB body cap and a global timeout, and filters obvious/decoded tracking pixels. Remote loading defaults off because direct image requests can disclose IP address and read time; `i` toggles it for the session and `ui.remote_images` can opt in at startup.

## Modal composer

The composer has NORMAL, INSERT and VISUAL modes. Recipient/subject fields and the body share the same modal editing core, while Tab/Shift-Tab changes fields. Labels are rendered separately from editable text, so active fields do not lose their To/Cc/Subject identity.

Esc follows modal semantics: VISUAL → NORMAL, INSERT → NORMAL, and only Esc from NORMAL closes the composer. Enter from NORMAL opens a send confirmation; sending is never bound to Ctrl-S. Reply supports quoted and unquoted variants.

## Refactor boundaries

- **Runtime:** `src/runtime.rs` owns the terminal event loop and the bounded
  event channel; it does not own mailbox data or the on-disk cache.
- **App state:** `src/app/state.rs` and its feature modules are the sole owners
  of the interactive account view, selection, and navigation state.
- **Workers:** `src/worker.rs` schedules and executes blocking operations;
  `src/app/state/worker_events/` applies their results on the UI thread.
- **Backends:** `src/backend/mod.rs` defines the shared contract; `imap/`,
  `jmap/`, `pop3.rs`, and `maildir.rs` implement it. The transport's internal
  mailbox/thread identity must not leak into the presentation layer.
- **Persistence:** `src/cache.rs` and `src/outgoing/` own cache and queued
  mutation persistence. Refactoring their modules must not alter disk formats,
  write batching, or startup recovery behavior.
- **Presentation:** `src/ui/render.rs` orchestrates draw order; individual
  renderers only borrow current state and format it for Ratatui.

Large tests for a module live in its adjacent `tests.rs`, while smaller focused
regressions may remain in named sibling test modules. Moving a test does not
change its `#[cfg(test)]` build conditions, module path, or access permissions.

## Performance invariants

1. Rendering performs no filesystem/network I/O; remote image fetch/decode runs on its own worker.
2. Idle UI has no redraw polling.
3. Ordinary UI→worker enqueue never blocks the terminal thread.
4. Browser navigation never fetches bodies unless split preview is enabled.
5. Initial IMAP mailbox paint is bounded by `page_size`, not the full folder.
6. Loaded mailbox pages/bodies are reused when revisiting a folder.
7. Rapid body/mailbox requests coalesce when older queued work is stale.
8. Successful mutations do not refresh the mailbox tree.
9. Search reuses cached headers and pages older mail progressively.
10. Network operations retain explicit timeouts and credential diagnostics remain redacted.
