#!/usr/bin/env python3
"""Look up Jerc versions in Cargo's public sparse registry index.

Uses the static index rather than the crates.io application API, which may
reject generic CI HTTP clients. A failed lookup is never treated as a 404.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

INDEX_URL = "https://index.crates.io/je/rc/jerc"
USER_AGENT = "jerc-release-ci/1.0 (+https://github.com/HiFiveJazz/jerc)"


def version_exists(index: str, version: str) -> bool:
    records = [json.loads(line) for line in index.splitlines() if line.strip()]
    if not records:
        raise ValueError("crates.io index returned an empty response")
    if any(record.get("name") != "jerc" for record in records):
        raise ValueError("crates.io index returned an unexpected crate")
    return any(record["vers"] == version for record in records)


def fetch_index() -> str:
    request = Request(
        INDEX_URL,
        headers={"User-Agent": USER_AGENT, "Accept": "application/json"},
    )
    for attempt in range(3):
        try:
            with urlopen(request, timeout=30) as response:
                return response.read().decode("utf-8")
        except HTTPError as exc:
            if exc.code not in (429, 500, 502, 503, 504) or attempt == 2:
                raise
        except (URLError, TimeoutError):
            if attempt == 2:
                raise
        time.sleep(2 ** attempt)
    raise RuntimeError("crates.io index could not be fetched")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    if not args.version or args.version.startswith("v"):
        parser.error("--version must be a bare Cargo version (e.g. 0.6.2)")

    exists = version_exists(fetch_index(), args.version)
    publish = "false" if exists else "true"
    if os.getenv("GITHUB_OUTPUT"):
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
            print(f"publish={publish}", file=output)
    print(
        f"jerc {args.version} already exists on crates.io; no-op"
        if exists else f"jerc {args.version} is not published; proceeding"
    )


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print(f"crates.io index lookup failed: {exc}", file=sys.stderr)
        raise SystemExit(1) from exc
