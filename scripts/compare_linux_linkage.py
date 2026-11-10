#!/usr/bin/env python3
"""Compare the raw executable and compressed archive sizes of Linux releases.

This runs after both GNU and musl builds complete; no size is estimated.
"""

import argparse
import re
import tarfile
from pathlib import Path


LINUX_TARGETS = (
    ("GNU/glibc (dynamic)", "x86_64-unknown-linux-gnu"),
    ("musl (static)", "x86_64-unknown-linux-musl"),
)


def archive_sizes(path: Path) -> tuple[int, int]:
    """Return (uncompressed executable bytes, compressed archive bytes)."""
    if not path.is_file():
        raise FileNotFoundError(path)
    with tarfile.open(path, mode="r:gz") as archive:
        executables = [
            member
            for member in archive.getmembers()
            if member.isfile() and Path(member.name).name == "jerc"
        ]
        if len(executables) != 1:
            raise ValueError(f"Expected exactly one jerc binary in {path}")
        return executables[0].size, path.stat().st_size


def size_label(size: int) -> str:
    return f"{size / (1024 * 1024):.2f} MiB ({size:,} bytes)"


def difference(size: int, baseline: int) -> str:
    if baseline == 0:
        return "n/a"
    amount = 100 * (size - baseline) / baseline
    return f"{amount:+.1f}%"


def report(tag: str, directory: Path) -> str:
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError(f"Invalid version tag: {tag!r}")

    sizes = [
        archive_sizes(directory / f"jerc-{tag}-{target}.tar.gz")
        for _, target in LINUX_TARGETS
    ]
    (gnu_binary, gnu_archive), (musl_binary, musl_archive) = sizes
    return "\n".join(
        [
            f"# Jerc {tag} — Linux executable size comparison",
            "",
            "| Variant | Executable (uncompressed) | Download (`.tar.gz`) |",
            "| --- | ---: | ---: |",
            f"| GNU/glibc (dynamic) | {size_label(gnu_binary)} | {size_label(gnu_archive)} |",
            f"| musl (static) | {size_label(musl_binary)} | {size_label(musl_archive)} |",
            "",
            "**Static vs dynamic:** "
            f"executable {difference(musl_binary, gnu_binary)}; "
            f"download archive {difference(musl_archive, gnu_archive)}.",
            "",
            "Both artifacts include the same documentation and sample configuration. "
            "The musl build does not require glibc or other dynamic libraries, "
            "but TLS still uses system CA certificates and certain optional "
            "features invoke external applications (for example pdftoppm).",
            "",
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        markdown = report(args.tag, args.directory)
    except (ValueError, OSError, tarfile.TarError) as exc:
        parser.error(str(exc))
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(markdown, encoding="utf-8")
    print(markdown)


if __name__ == "__main__":
    main()
