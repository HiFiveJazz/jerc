# Incremental IMAP header refresh (first phase)

No new dependency or provider-specific configuration is needed. The first page
continues to use `SELECT` to validate the server's UIDVALIDITY. When the local
index holds headers for that UIDVALIDITY, the client now:

1. Fetches **UID + FLAGS** for the server's current first-page sequence range.
2. Reuses cached header metadata and BODYSTRUCTURE hints **only** for those UIDs.
3. Fetches full header metadata via **UID FETCH** only for new or missing UIDs.
4. Reconciles removals (UIDs not in the current page) and refreshes flags.
5. Falls back to the original full header fetch on changed UIDVALIDITY, a
   backwards UIDNEXT, unsupported/partial data, or an EXPUNGE race.

It does not skip the server's first-page check or silently trust cached flags.
It does not yet negotiate CONDSTORE/QRESYNC; those require capability checks,
mod-sequence persistence, and a dedicated reconciliation path for VANISHED and
changed flags. Servers without those extensions need this RFC 3501 fallback.

`uid_next` is optional in the on-disk cache, so caches written by older
releases continue to load. If an IMAP server omits UIDVALIDITY, no header
reuse is attempted. The Maildir backend remains unchanged.

## Verify locally

```sh
cargo test -- --test-threads=1
cargo clippy --all-targets -- -D warnings
```

To inspect which path runs, set the existing performance-log variable before
starting aerc-rs. A successful cached refresh records `imap_fetch_uid_flags`
and `imap_incremental_reuse`; a message arrival additionally records
`imap_fetch_new_headers`. A cold page records `imap_fetch_headers` instead.
Compare several warm folder switches against your prior baseline rather than
relying on one measurement (IMAP server latency varies).
