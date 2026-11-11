#!/usr/bin/env python3
"""Create a reproducible, offline-buildable Jerc source archive from a Git tag.

The Git-tracked tree is archived from the requested tag, not main.  Cargo
vendors every locked registry dependency and the resulting .cargo/config.toml
points at the vendored directory. No network access is needed by downstream
package builds. Cargo vendor itself requires network access the first time.
"""

from __future__ import annotations

import argparse
import gzip
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def run(command: list[str], *, cwd: Path, stdout=None) -> subprocess.CompletedProcess:
    return subprocess.run(command, cwd=cwd, stdout=stdout, check=True,
                          text=stdout is None)


def archive_source(tag: str, output: Path) -> Path:
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise ValueError(f"Invalid tag: {tag}")
    version = tag[1:]
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"jerc-{tag}-vendored.tar.gz"
    with tempfile.TemporaryDirectory(prefix="jerc-vendor-") as temp:
        work = Path(temp) / f"jerc-{version}"
        work.mkdir()
        with tempfile.TemporaryFile() as git_tar:
            run(["git", "archive", "--format=tar", tag], cwd=ROOT, stdout=git_tar)
            git_tar.seek(0)
            with tarfile.open(fileobj=git_tar, mode="r:") as tar:
                # Git archive names are Git-controlled; avoid links and escapes.
                for m in tar.getmembers():
                    target = (work / m.name).resolve()
                    if not target.is_relative_to(work.resolve()) or not (m.isfile() or m.isdir()):
                        raise ValueError(f"Unsafe git archive member: {m.name}")
                tar.extractall(work, filter="data")
        package = tomllib.loads((work / "Cargo.toml").read_text())["package"]
        if package["name"] != "jerc" or package["version"] != version:
            raise ValueError("Tag version differs from Cargo.toml")
        lock = tomllib.loads((work / "Cargo.lock").read_text())
        if not any(x["name"] == "jerc" and x["version"] == version for x in lock["package"]):
            raise ValueError("Tag version differs from Cargo.lock")
        # Make cargo vendor output deterministic enough for normal packaging.
        config_dir = work / ".cargo"
        config_dir.mkdir(exist_ok=True)
        result = subprocess.run(["cargo", "vendor", "--locked", "--versioned-dirs", "vendor"],
                                cwd=work, capture_output=True, text=True, check=True)
        config = result.stdout
        if '[source.crates-io]' not in config or 'directory = "vendor"' not in config:
            raise ValueError("cargo vendor did not return a local-source configuration")
        config_file = config_dir / "config.toml"
        existing = config_file.read_text() if config_file.exists() else ""
        # Preserve tracked settings (e.g. Jerc's OAuth provider IDs). Cargo's
        # generated source replacement is appended, never replaces the file.
        config_file.write_text(existing.rstrip() + "\n\n" + config)
        # Deterministic file order, timestamps, ids, gzip header and metadata.
        with destination.open("wb") as raw:
            with gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0, compresslevel=9) as gz:
                with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as tar:
                    for path in [work, *sorted(work.rglob("*"))]:
                        if path.is_symlink():
                            raise ValueError(f"Unexpected symlink: {path}")
                        info = tar.gettarinfo(str(path), arcname=str(path.relative_to(work.parent)))
                        info.uid = 0
                        info.gid = 0
                        info.uname = ""
                        info.gname = ""
                        info.mtime = 0
                        if path.is_file():
                            with path.open("rb") as f:
                                tar.addfile(info, fileobj=f)
                        elif path.is_dir():
                            tar.addfile(info)
    if not destination.is_file() or destination.stat().st_size == 0:
        raise RuntimeError("Missing source archive")
    print(f"Created {destination}")
    return destination


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("tag")
    ap.add_argument("--dist", type=Path, default=ROOT / "dist")
    args = ap.parse_args()
    archive_source(args.tag, args.dist)


if __name__ == "__main__":
    main()
