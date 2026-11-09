#!/usr/bin/env python3
"""Interactive warm-cache versus no-cache jerc comparison.

No dependency beyond Python 3.  Runs a real TUI, asks the operator to close
it normally, and compares *instrumented data readiness*, not actual paints.
Use the same account, network and sequence of actions in each run.
"""

import argparse
import csv
import math
import os
from pathlib import Path
import statistics
import subprocess
import sys
import time


def log_events(path):
    events = []
    if not path.is_file():
        return events
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        event = dict(chunk.split("=", 1) for chunk in line.split() if "=" in chunk)
        try:
            event["ts_ms"] = int(event["ts_ms"])
            event["duration_ms"] = float(event["duration_ms"])
            q = event.get("queue_ms", "-")
            event["queue_ms"] = 0.0 if q == "-" else float(q)
        except (KeyError, ValueError):
            continue
        events.append(event)
    return events


def metric(events, stage):
    return [event for event in events if event.get("stage") == stage]


def summary(events, start_ms, mode, iteration):
    cache = metric(events, "ui_cache_restore")
    applied = metric(events, "ui_mailbox_applied")
    opens = metric(events, "worker_open_mailbox")
    result = {
        "run": iteration,
        "mode": mode,
        "cache_restore_ms": round(cache[0]["ts_ms"] - start_ms, 1) if cache else "",
        "network_applied_ms": round(applied[0]["ts_ms"] - start_ms, 1) if applied else "",
        "cache_read_ms": round(metric(events, "cache_read")[0]["duration_ms"], 2)
            if metric(events, "cache_read") else "",
        "cache_valid": metric(events, "cache_read")[0].get("ok", "")
            if metric(events, "cache_read") else "",
        "auth_ms": round(sum(e["duration_ms"] for e in metric(events, "imap_auth") if e.get("worker") == "mail-worker-0"), 1),
        "list_ms": round(sum(e["duration_ms"] for e in metric(events, "imap_list")), 1),
        "open_max_ms": round(max((e["duration_ms"] + e["queue_ms"] for e in opens), default=0), 1),
        "queue_max_ms": round(max((e["queue_ms"] for e in opens), default=0), 1),
        "opens": len(opens),
        "ui_stale": len(metric(events, "ui_mailbox_stale")),
    }
    return result


def median(rows, mode, key):
    data = [float(r[key]) for r in rows if r["mode"] == mode and r[key] != ""]
    return f"{statistics.median(data):.0f} ms" if data else "N/A"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=5, help="number of runs per mode")
    parser.add_argument("--scenario", choices=("startup", "switch", "message"), default="startup")
    parser.add_argument("--binary", default="./target/release/jerc")
    parser.add_argument("--out", default="/tmp/jerc-cache-bench")
    parser.add_argument("--skip-warmup", action="store_true")
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")
    binary = Path(args.binary).expanduser().resolve()
    if not binary.is_file():
        parser.error(f"No executable at {binary}. Run cargo build --release first.")
    out = Path(args.out).expanduser().resolve()
    out.mkdir(parents=True, exist_ok=True)

    print("This runs the real TUI. Exit with q after each trial.")
    print("startup: wait for INBOX to populate and refresh; switch: toggle INBOX and All Mail 5 times;")
    print("message: open the same 3 messages, including one previously opened HTML message.")
    print("Tests use your existing cache; neither mode deletes cache data.")

    def launch(label, index, disable):
        path = out / f"{index:02d}-{label}.log"
        env = os.environ.copy()
        env["JERC_PERF_LOG"] = str(path)
        if disable:
            env["JERC_DISABLE_CACHE"] = "1"
        else:
            env.pop("JERC_DISABLE_CACHE", None)
        try:
            input(f"\n{index}: {label} — Enter to launch (Ctrl+C to end): ")
        except (KeyboardInterrupt, EOFError):
            print("\nStopped.")
            return None
        start_ms = time.time_ns() / 1_000_000
        rc = subprocess.call([str(binary)], env=env)
        events = log_events(path)
        result = summary(events, start_ms, label, index)
        result["exit_code"] = rc
        result["log"] = path.name
        print(f"  cache-ready: {result['cache_restore_ms'] or 'N/A'} ms; "
              f"network applied: {result['network_applied_ms'] or 'N/A'} ms; "
              f"worst mailbox open: {result['open_max_ms']} ms")
        if label == "warm-cache" and result["cache_restore_ms"] == "":
            print("  WARNING: no cache-restored event (no cache yet, or wrong build).")
        return result

    if not args.skip_warmup:
        print("\nWarmup: visit Inbox AND All Mail, wait for their network refreshes, then exit normally.")
        if launch("warmup", 0, False) is None:
            return 130

    rows = []
    for i in range(args.runs):
        # Alternating order reduces bias if Gmail slows down during the test.
        modes = ("warm-cache", "no-cache") if i % 2 == 0 else ("no-cache", "warm-cache")
        for mode in modes:
            result = launch(mode, len(rows) + 1, mode == "no-cache")
            if result is None:
                break
            rows.append(result)
        else:
            continue
        break

    if not rows:
        return 130
    csv_path = out / "results.csv"
    with csv_path.open("w", newline="", encoding="utf-8") as file:
        writer = csv.DictWriter(file, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    print("\nMedians across completed trials:")
    print("  Cache-ready marker:   warm", median(rows, "warm-cache", "cache_restore_ms"))
    print("  Network-refreshed:    warm", median(rows, "warm-cache", "network_applied_ms"),
          "/ uncached", median(rows, "no-cache", "network_applied_ms"))
    print("  Worst mailbox open:  warm", median(rows, "warm-cache", "open_max_ms"),
          "/ uncached", median(rows, "no-cache", "open_max_ms"))
    print(f"\nResults: {csv_path}")
    print("Cache-ready is before the first terminal paint, not measured paint latency.")
    print("Compare perceived paint time manually; network-refreshed times depend on Gmail conditions.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
