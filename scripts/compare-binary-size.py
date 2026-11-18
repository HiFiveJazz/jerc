#!/usr/bin/env python3
"""Compare stripped jerc executable sizes with identical Cargo features.

Usage:
    python3 scripts/compare-binary-size.py
    python3 scripts/compare-binary-size.py --target x86_64-unknown-linux-musl
    python3 scripts/compare-binary-size.py --no-build

Builds the production size-optimized release configuration and two opt-in
speed-oriented variants, then reports the *uncompressed executable size*.
All three profiles retain the same features, strip symbols, and abort panics.
"""

import argparse
from pathlib import Path
import subprocess
import sys


PROFILES = (
    ("Production (Fat LTO + size)", "release"),
    ("Fat LTO + opt-level=3", "release-fat"),
    ("Thin LTO + opt-level=3", "release-thin"),
)


def format_size(size: int) -> str:
    return f"{size / (1024 * 1024):.3f} MiB"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Build all variants for this Rust target")
    parser.add_argument(
        "--no-build", action="store_true", help="Report sizes of existing executables"
    )
    args = parser.parse_args()

    root = Path(__file__).resolve().parents[1]
    target_dir = root / "target"
    suffix = ".exe" if args.target and "windows" in args.target else ""
    sizes = []
    for label, profile in PROFILES:
        if not args.no_build:
            command = ["cargo", "build", "--locked", "--profile", profile]
            if args.target:
                command.extend(("--target", args.target))
            print(f"\nBuilding {label}: {' '.join(command)}", flush=True)
            subprocess.run(command, cwd=root, check=True)
        artifact = target_dir
        if args.target:
            artifact /= args.target
        artifact = artifact / profile / ("jerc" + suffix)
        if not artifact.is_file():
            parser.error(f"missing executable: {artifact}")
        sizes.append((label, artifact.stat().st_size, artifact))

    baseline = sizes[0][1]
    print("\nUncompressed executable size (all features unchanged):")
    for label, size, path in sizes:
        difference = (size / baseline - 1) * 100
        print(f"  {label:28} {format_size(size):>11}  {difference:+6.1f}%  {path}")
    print("\n`cargo build --release` uses the production size-optimized profile.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, subprocess.CalledProcessError) as exc:
        print(f"size comparison failed: {exc}", file=sys.stderr)
        sys.exit(1)
