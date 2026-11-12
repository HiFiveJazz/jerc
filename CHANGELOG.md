# Changelog

Significant changes are documented here. GitHub also generates per-release
notes from commit history when a version tag is pushed.

## 0.6.4 — 2026-11-12

### Fixed

- Prevent freezes when adding or removing accounts: account reload retires old mail
  workers in the background instead of joining network operations on the UI thread.
- Reject stale results from retired account workers after reloading accounts,
  without dropping the safety checks for pending sends and mailbox mutations.
- Eliminate spurious warning banners during ordinary online mail deletion.
- Make offline notices acknowledgeable immediately from `!`, with a yellow
  banner and clear descriptions of queued sends and deletions.
- Clear resolved offline queue notices as the server acknowledges work, while
  keeping uncertain delivery outcomes visible for manual review.

### Diagnostics and tests

- Record the beginning of IMAP TLS connection and authentication attempts so
  stalled operations can be diagnosed without modifying the working transport.
- Cover delayed connections, worker shutdown and reload, write acknowledgment,
  reconnection, and offline notice presentation with regression tests.

## 0.6.0 — 2026-11-10

### Added

- IMAP IDLE watcher with a reconnect/polling fallback for mailbox updates.
- Read-only cached mailbox rows while server-side UID verification completes.
- Unicode superscript ordinal rendering for explicit HTML `<sup>` elements.

### Improved

- Bounded inactive-mailbox and message caches and coalesced background cache writes.
- Mailbox synchronization and worker-event request correlation.
- Shared Vim primitives and editing history for Compose and Contacts.
- Shared help, navigation, scrolling, selection, and popup rendering components.
- Smaller central application state module, with focused worker-event handlers.

### Security and reliability

- Size limits for interactive message ingestion and streamed raw `.eml` exports.
- Private permissions for Maildir message creation and attachment exports.
- Restricted remote-image requests and automatic cleanup of temporary attachments.
- Additional regression tests for mailbox state, cache handling, and rendering.

The browser HTML preview remains an explicit user action; Jerc does not sandbox
external browsers. Native PDF preview uses the installed external converter.
