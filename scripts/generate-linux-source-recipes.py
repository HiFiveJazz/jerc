#!/usr/bin/env python3
"""Render source-build APKBUILD and xbps-src template from a vendored tarball.

Produced recipes are complete (pinned checksums) and can be copied to Alpine
aports/Void void-packages for review; official acceptance is separate.
"""
from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import re
import tarfile

ROOT = Path(__file__).resolve().parents[1]


def digest(path: Path, name: str) -> str:
    h = hashlib.new(name)
    with path.open("rb") as file:
        for part in iter(lambda: file.read(1024 * 1024), b""):
            h.update(part)
    return h.hexdigest()


def generate(tag: str, dist: Path) -> tuple[Path, Path]:
    if re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag) is None:
        raise ValueError("Invalid release tag")
    version = tag[1:]
    source = dist / f"jerc-{tag}-vendored.tar.gz"
    if not source.is_file() or source.stat().st_size == 0:
        raise FileNotFoundError(source)
    with tarfile.open(source, "r:gz") as tar:
        files = tar.getnames()
        for name in ("Cargo.toml", "Cargo.lock", ".cargo/config.toml", "LICENSE", "vendor"):
            if not any(x == f"jerc-{version}/{name}" or x.startswith(f"jerc-{version}/{name}/") for x in files):
                raise ValueError(f"Vendored source archive lacks {name}")
    apk = dist / "source-recipes" / "alpine" / "APKBUILD"
    xbps = dist / "source-recipes" / "void" / "srcpkgs" / "jerc" / "template"
    apk.parent.mkdir(parents=True, exist_ok=True)
    xbps.parent.mkdir(parents=True, exist_ok=True)
    apk.write_text((ROOT / "packaging/linux/alpine/source/APKBUILD.in").read_text()
                   .replace("@VERSION@", version).replace("@SHA512@", digest(source, "sha512")))
    xbps.write_text((ROOT / "packaging/linux/void/source/template.in").read_text()
                    .replace("@VERSION@", version).replace("@SHA256@", digest(source, "sha256")))
    print(f"Generated Alpine source recipe: {apk}")
    print(f"Generated Void source recipe: {xbps}")
    return apk, xbps


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("tag")
    p.add_argument("--dist", type=Path, default=ROOT / "dist")
    args = p.parse_args()
    generate(args.tag, args.dist)


if __name__ == "__main__":
    main()
