#!/usr/bin/env python3
"""Independent IMAP timing probe (Python stdlib, not io-imap).

With no authentication, times TLS/greeting, CAPABILITY and NOOP.
With --auth login or xoauth2, also times LIST, SELECT, FETCH headers,
and optionally one BODY.PEEK[]. Nothing from message responses or
credentials is logged; only stage durations.

An XOAUTH2 login needs a current *access* token, not a refresh token.
"""

import argparse
import getpass
import imaplib
import statistics
import sys
import time


def timed(rows, stage, op):
    start = time.perf_counter()
    try:
        result = op()
    except Exception:
        rows.append((stage, (time.perf_counter() - start) * 1000, False))
        raise
    rows.append((stage, (time.perf_counter() - start) * 1000, True))
    return result


def require_ok(result, operation):
    status, data = result
    if status != "OK":
        raise RuntimeError(f"{operation} returned {status!r} (response omitted)")
    return data


def run_one(args, secret):
    rows = []
    client = timed(rows, "connect_tls_greeting", lambda: imaplib.IMAP4_SSL(
        args.host, args.port, timeout=args.timeout))
    try:
        require_ok(timed(rows, "capability", client.capability), "CAPABILITY")
        require_ok(timed(rows, "noop", client.noop), "NOOP")

        if args.auth == "none":
            return rows

        if args.auth == "login":
            require_ok(timed(rows, "login", lambda: client.login(args.user, secret)), "LOGIN")
        else:
            # imaplib takes care of SASL base64 encoding; no token goes to logs.
            auth_line = f"user={args.user}\x01auth=Bearer {secret}\x01\x01".encode()
            require_ok(timed(rows, "xoauth2", lambda: client.authenticate(
                "XOAUTH2", lambda _: auth_line)), "AUTHENTICATE XOAUTH2")

        require_ok(timed(rows, "list", client.list), "LIST")
        data = require_ok(timed(rows, "select", lambda: client.select(
            args.mailbox, readonly=False)), "SELECT")
        exists = int(data[0]) if data and data[0] is not None else 0
        if exists and args.headers:
            start = max(1, exists - args.headers + 1)
            query = "(UID FLAGS RFC822.SIZE RFC822.HEADER BODYSTRUCTURE)"
            require_ok(timed(rows, "fetch_headers", lambda: client.fetch(
                f"{start}:{exists}", query)), "FETCH headers")
        if exists and args.body:
            # Read one message without setting \\Seen; do not print the body.
            require_ok(timed(rows, "fetch_last_body", lambda: client.fetch(
                str(exists), "(BODY.PEEK[])")), "FETCH body")
        return rows
    finally:
        try:
            client.logout()
        except Exception:
            pass


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", default="imap.gmail.com")
    p.add_argument("--port", type=int, default=993)
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--timeout", type=float, default=30.0)
    p.add_argument("--auth", choices=("none", "login", "xoauth2"), default="none")
    p.add_argument("--user", help="Required only when authenticating")
    p.add_argument("--mailbox", default="INBOX", help='e.g. "[Gmail]/All Mail"')
    p.add_argument("--headers", type=int, default=100)
    p.add_argument("--body", action="store_true", help="Fetch one complete body via BODY.PEEK[]")
    args = p.parse_args()
    if args.runs < 1 or args.runs > 20 or args.headers < 0 or args.headers > 500:
        p.error("runs must be 1..20 and headers 0..500")
    if args.auth != "none" and not args.user:
        p.error("--user is required with --auth login/xoauth2")
    secret = None
    if args.auth != "none":
        prompt = "Gmail app password (hidden): " if args.auth == "login" else "Current OAuth ACCESS token (hidden): "
        secret = getpass.getpass(prompt)
        if not secret:
            p.error("empty credential")

    print(f"Independent IMAP probe — {args.host}:{args.port} — {args.runs} runs, {args.auth} authentication")
    print("Only timings and stage status are printed; no message contents or credentials.")
    all_rows = []
    for i in range(args.runs):
        print(f"\nRun {i + 1}/{args.runs}", flush=True)
        try:
            rows = run_one(args, secret)
        except Exception as exc:
            # Some library exception strings contain server responses; do not emit them.
            print(f"  ERROR: {type(exc).__name__} (details withheld)")
            # This script deliberately avoids logging sensitive response details.
            continue
        for stage, duration, ok in rows:
            print(f"  {stage:22s} {duration:8.1f} ms  {'ok' if ok else 'failed'}")
        all_rows.extend(rows)
    if not all_rows:
        print("No successful runs. Check network or authentication; do not share credentials.")
        return 1
    print("\nStage summaries (successful runs only):")
    for stage in dict.fromkeys(stage for stage, _, _ in all_rows):
        times = [ms for name, ms, ok in all_rows if name == stage and ok]
        if times:
            print(f"  {stage:22s} median={statistics.median(times):7.1f} ms, worst={max(times):7.1f} ms")
    return 0


if __name__ == "__main__":
    sys.exit(main())
