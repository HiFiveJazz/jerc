#!/usr/bin/env python3
"""Fail CI if cargo-binstall metadata drifts from package_release.py assets.

No network, Cargo, or third-party Python dependencies are required.
"""
from __future__ import annotations

import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
MANIFEST = tomllib.loads((REPO / "Cargo.toml").read_text())
PKG = MANIFEST["package"]
META = PKG["metadata"]["binstall"]
BASE = "https://github.com/HiFiveJazz/jerc"
TARGETS = {
    "x86_64-unknown-linux-musl": ("tar.gz", "tgz", "x86_64-unknown-linux-musl", "jerc"),
    "x86_64-unknown-linux-gnu": ("tar.gz", "tgz", "x86_64-unknown-linux-musl", "jerc"),
    "aarch64-apple-darwin": ("tar.gz", "tgz", "aarch64-apple-darwin", "jerc"),
    "x86_64-apple-darwin": ("tar.gz", "tgz", "x86_64-apple-darwin", "jerc"),
    "x86_64-pc-windows-msvc": ("zip", "zip", "x86_64-pc-windows-msvc", "jerc.exe"),
}


def expand(template: str, target: str) -> str:
    mapping = {
        "repo": PKG["repository"],
        "name": PKG["name"],
        "version": PKG["version"],
        "target": target,
        "bin": "jerc",
        "binary-ext": ".exe" if "windows" in target else "",
    }
    for key, value in mapping.items():
        template = template.replace("{ " + key + " }", value)
    assert "{" not in template, template
    return template


def main() -> None:
    assert PKG["name"] == "jerc"
    assert PKG["repository"] == BASE
    assert PKG["license"] == "GPL-3.0-only"
    assert PKG.get("publish") is not False
    for target, (extension, archive_format, artifact_target, executable) in TARGETS.items():
        override = META.get("overrides", {}).get(target, {})
        url = expand(override.get("pkg-url", META["pkg-url"]), target)
        path = expand(override.get("bin-dir", META["bin-dir"]), target)
        fmt = override.get("pkg-fmt", META["pkg-fmt"])
        bundle = f"jerc-v{PKG['version']}-{artifact_target}"
        expected_url = f"{BASE}/releases/download/v{PKG['version']}/{bundle}.{extension}"
        assert url == expected_url, (target, url, expected_url)
        assert path == f"{bundle}/{executable}", (target, path)
        assert fmt == archive_format, (target, fmt)
        print(f"OK {target}: {bundle}.{extension} -> {path}")
    print("cargo-binstall metadata matches Jerc release artifact naming")


if __name__ == "__main__":
    main()
