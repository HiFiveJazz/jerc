#!/usr/bin/env python3
"""Verify each Gentoo overlay Manifest against exactly the release distfile."""
import argparse
import hashlib
from pathlib import Path
import tarfile


def verify(tag: str, directory: Path) -> None:
    version = tag.removeprefix("v")
    overlay = directory / f"jerc-gentoo-overlay-{tag}.tar.gz"
    entries = {
        "jerc-bin": directory / f"jerc-{tag}-x86_64-unknown-linux-musl.tar.gz",
        "jerc": directory / f"jerc-{tag}-vendored.tar.gz",
    }
    with tarfile.open(overlay, "r:gz") as tf:
        assert tf.extractfile("jerc-overlay/profiles/repo_name").read().strip() == b"jerc-local"
        for pkg, dist in entries.items():
            root = f"jerc-overlay/mail-client/{pkg}/"
            ebuild = tf.extractfile(f"{root}{pkg}-{version}.ebuild").read().decode()
            assert f"jerc-v${{PV}}" in ebuild
            manifest = tf.extractfile(root + "Manifest").read().decode().strip().split()
            assert manifest[:3] == ["DIST", dist.name, str(dist.stat().st_size)]
            assert manifest[3] == "BLAKE2B" and manifest[5] == "SHA512"
            hashes = {"blake2b": hashlib.blake2b(), "sha512": hashlib.sha512()}
            with dist.open("rb") as src:
                for part in iter(lambda: src.read(1024 * 1024), b""):
                    for h in hashes.values():
                        h.update(part)
            assert manifest[4] == hashes["blake2b"].hexdigest()
            assert manifest[6] == hashes["sha512"].hexdigest()
            if pkg == "jerc":
                assert "inherit cargo" in ebuild
                assert "cargo_src_configure --locked --offline" in ebuild
        print(f"Verified Gentoo source and binary Manifests for {tag}")


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("tag")
    p.add_argument("--dist", type=Path, default=Path("dist"))
    args = p.parse_args()
    verify(args.tag, args.dist)
