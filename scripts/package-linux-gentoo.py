#!/usr/bin/env python3
"""Build a versioned, self-contained Gentoo jerc-bin local overlay tarball.

This is a prebuilt-bin overlay, not an official Gentoo ebuild or a source build.
The ebuild fetches the same musl release archive, authenticated by Manifest.
"""

import argparse
import hashlib
from pathlib import Path
import re
import shutil
import tarfile

ROOT = Path(__file__).resolve().parents[1]
FILE_NAMES = ("jerc", "LICENSE", "README.md", "config.example.toml")


def make_overlay(tag: str, archive: Path, output_dir: Path) -> Path:
    if re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag) is None:
        raise ValueError(f"Invalid release tag: {tag}")
    version = tag[1:]
    bundle = f"jerc-{tag}-x86_64-unknown-linux-musl"
    archive = archive.resolve()
    if not archive.is_file() or archive.stat().st_size == 0:
        raise FileNotFoundError(f"Missing release archive: {archive}")
    if archive.name != f"{bundle}.tar.gz":
        raise ValueError("Archive filename does not match the release tag")
    # Verify the exact release file structure before producing a package recipe.
    with tarfile.open(archive, "r:gz") as package:
        for filename in FILE_NAMES:
            member = package.getmember(f"{bundle}/{filename}")
            if not member.isfile() or member.size == 0:
                raise ValueError(f"Invalid release archive member: {filename}")

    output_dir.mkdir(parents=True, exist_ok=True)
    stage = output_dir / "gentoo-overlay-stage"
    if stage.exists():
        shutil.rmtree(stage)
    overlay = stage / "jerc-overlay"
    ebuild_dir = overlay / "app-misc" / "jerc-bin"
    ebuild_dir.mkdir(parents=True, exist_ok=True)
    (overlay / "metadata").mkdir(exist_ok=True)
    (overlay / "profiles").mkdir(exist_ok=True)
    (overlay / "metadata" / "layout.conf").write_text(
        "masters = gentoo\nthin-manifests = true\n"
    )
    (overlay / "profiles" / "repo_name").write_text("jerc-local\n")
    ebuild = ebuild_dir / f"jerc-bin-{version}.ebuild"
    ebuild.write_text("""EAPI=8
DESCRIPTION="Keyboard-driven terminal email client written in Rust (binary release)"
HOMEPAGE="https://github.com/HiFiveJazz/jerc"
SRC_URI="https://github.com/HiFiveJazz/jerc/releases/download/v${PV}/jerc-v${PV}-x86_64-unknown-linux-musl.tar.gz"
S="${WORKDIR}/jerc-v${PV}-x86_64-unknown-linux-musl"
LICENSE="GPL-3"
SLOT="0"
KEYWORDS="~amd64"
RESTRICT="strip"
RDEPEND="app-misc/ca-certificates"

src_install() {
    dobin jerc
    einstalldocs
    insinto /usr/share/doc/${PF}
    doins config.example.toml
    insinto /usr/share/licenses/${PN}
    doins LICENSE
}
""")
    sha512 = hashlib.sha512()
    blake2b = hashlib.blake2b()
    with archive.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            sha512.update(chunk)
            blake2b.update(chunk)
    (ebuild_dir / "Manifest").write_text(
        f"DIST {archive.name} {archive.stat().st_size} "
        f"BLAKE2B {blake2b.hexdigest()} SHA512 {sha512.hexdigest()}\n"
    )
    output = output_dir / f"jerc-gentoo-overlay-{tag}.tar.gz"
    with tarfile.open(output, "w:gz") as tar:
        for path in sorted(overlay.rglob("*")):
            tar.add(path, arcname=path.relative_to(overlay.parent), recursive=False)
    if not output.is_file() or not output.stat().st_size:
        raise RuntimeError(f"Overlay tarball was not created: {output}")
    print(f"Created {output}")
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="Version tag, e.g. v0.6.3")
    parser.add_argument("archive", nargs="?", type=Path)
    args = parser.parse_args()
    bundle = f"jerc-{args.tag}-x86_64-unknown-linux-musl.tar.gz"
    archive = args.archive or ROOT / "dist" / bundle
    make_overlay(args.tag, archive, ROOT / "dist")


if __name__ == "__main__":
    main()
