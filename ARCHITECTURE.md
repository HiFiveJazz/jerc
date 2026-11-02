# Architecture — milestone 5

The project keeps aerc's separation of UI state from blocking mail protocols while using a modal, lazygitrs-like terminal interaction model.

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

`P` toggles a vertical preview beside the message list. Browser movement still loads only headers until a preview or full reader needs a body. Cached bodies are reused by both surfaces. `b` can hide the mailbox sidebar so the browser/preview can use the full terminal width.

## HTML mail

MIME parsing remains in `mail-parser`. HTML uses `html2text`'s rich line API, which preserves table box drawing and annotations such as links, images, emphasis, strong text, strikeout and code. Those annotations are converted into model-level spans and styled by Ratatui using the active aerc-rs theme. This produces text-browser-like visual structure without spawning an external process for every preview.

The parser also strips common zero-width preheader padding and collapses useless blank runs. The reader can select rendered links with Tab/Shift-Tab and launch one with `o`.

## Modal composer

The composer has NORMAL, INSERT and VISUAL modes. Recipient/subject fields and the body share the same modal editing core, while Tab/Shift-Tab changes fields. Labels are rendered separately from editable text, so active fields do not lose their To/Cc/Subject identity.

Esc follows modal semantics: VISUAL → NORMAL, INSERT → NORMAL, and only Esc from NORMAL closes the composer. Enter from NORMAL opens a send confirmation; sending is never bound to Ctrl-S. Reply supports quoted and unquoted variants.

## Performance invariants

1. Rendering performs no filesystem/network I/O.
2. Idle UI has no redraw polling.
3. Ordinary UI→worker enqueue never blocks the terminal thread.
4. Browser navigation never fetches bodies unless split preview is enabled.
5. Initial IMAP mailbox paint is bounded by `page_size`, not the full folder.
6. Loaded mailbox pages/bodies are reused when revisiting a folder.
7. Rapid body/mailbox requests coalesce when older queued work is stale.
8. Successful mutations do not refresh the mailbox tree.
9. Search reuses cached headers and pages older mail progressively.
10. Network operations retain explicit timeouts and credential diagnostics remain redacted.
