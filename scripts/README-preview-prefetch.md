# Priority previews and adaptive neoscroll (phase 3)

This patch layers on the aggressive preview cache patch. It restores ~100 Hz
Ctrl-j/k animation (10 ms frame deadline), and keeps speculative network work
bounded and out of the way of interactive message reads. No extra dependencies
or Gmail-specific mechanisms are introduced.

- **Selected-message priority:** With a separate background session present,
  explicit `FetchBody` requests go to that worker ahead of speculative PEEK
  downloads and STATUS checks. The foreground IMAP session may be busy with a
  slow SELECT/header refresh; the selected body can load independently. The
  fallback without a background session remains the foreground worker. A
  single already-in-flight background IMAP operation cannot be interrupted.
- **Adaptive prefetch:** Ordinary navigation warms 12 ahead and 6 behind;
  Ctrl-j/k travel prioritizes the next 3 rows and the destination, then uses a
  22-ahead/8-behind window near the destination. Old windows are coalesced
  between message fetches. Per-window raw size budget is 8 MiB and per-message
  speculative limit is 1 MiB (unknown sizes skipped). Explicit reads have no
  speculative size cap.
- **RAM bound:** Up to 80 decoded message bodies / ~48 MiB of decoded preview
  text, down from 128 / ~96 MiB. Current-message content is protected from
  eviction. Existing on-disk metadata/body cache policies remain unchanged.
- **Diagnostics:** `ui_animation_tick` measures state advance plus paint;
  `ui_render_frame` retains paint alone. `ui_preview_cache_hit/miss`,
  `ui_prefetch_applied/duplicate/discarded`, `ui_prefetch_window_submitted`, and
  `worker_priority_fetch_body` help evaluate cache usefulness and priority.
- **Safety:** Prefetch still uses PEEK and does not mark unread mail read.
  Different account/folder generations never share bodies. Interactive reads
  still update read status only when the user views the selected email.

Validation on a host with Rust:

    cargo test -- --test-threads=1
    JERC_PERF_LOG=/tmp/jerc-scroll-priority.log cargo run --release

Compare `worker_priority_fetch_body queue_ms` and preview hit/miss rates during
Ctrl-j/k, and inspect `ui_animation_tick` p95. Existing renderer timings under
~10 ms leave headroom at 100 Hz; terminal output time and animation steps can
still vary depending on message layout and terminal implementation.
