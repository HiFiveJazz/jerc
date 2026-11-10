#!/usr/bin/env python3
"""Package a tagged Jerc release into a platform-specific archive.

Uses only Python's standard library, so release packaging is portable and
can be checked locally without installing an additional packaging tool.
"""

import argparse
import tarfile
import tomllib
import zipfile
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[1]
PACKAGED_FILES = ("LICENSE", "README.md", "config.example.toml")


def release_version(tag: str) -> str:
    with (REPOSITORY / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    if tag != f"v{version}":
        raise ValueError(f"Tag {tag!r} does not match Cargo.toml version {version!r}")
    return version


def package_release(tag: str, target: str, output_dir: Path) -> Path:
    release_version(tag)
    if target not in {
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
    }:
        raise ValueError(f"Unsupported release target: {target!r}")

    windows = target.endswith("windows-msvc")
    binary_name = "jerc.exe" if windows else "jerc"
    binary = REPOSITORY / "target" / target / "release" / binary_name
    if not binary.is_file():
        raise FileNotFoundError(f"Missing release binary: {binary}")

    bundle = f"jerc-{tag}-{target}"
    output_dir.mkdir(parents=True, exist_ok=True)
    entries = [(binary, f"{bundle}/{binary_name}")]
    for filename in PACKAGED_FILES:
        source = REPOSITORY / filename
        if not source.is_file():
            raise FileNotFoundError(f"Missing release file: {source}")
        entries.append((source, f"{bundle}/{filename}"))

    if windows:
        archive = output_dir / f"{bundle}.zip"
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as zf:
            for source, name in entries:
                zf.write(source, arcname=name)
    else:
        archive = output_dir / f"{bundle}.tar.gz"
        with tarfile.open(archive, "w:gz") as tf:
            for source, name in entries:
                tf.add(source, arcname=name, recursive=False)
    return archive


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True, help="Version tag, e.g. v0.6.0")
    parser.add_argument("--check-only", action="store_true")
    parser.add_argument("--target", help="Rust target triple")
    parser.add_argument("--output-dir", type=Path, default=REPOSITORY / "dist")
    args = parser.parse_args()

    try:
        if args.check_only:
            if args.target:
                parser.error("--target is not accepted with --check-only")
            release_version(args.tag)
            print(f"Verified release tag {args.tag}")
        else:
            if not args.target:
                parser.error("--target is required when packaging")
            print(package_release(args.tag, args.target, args.output_dir))
    except (ValueError, FileNotFoundError) as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    main()
