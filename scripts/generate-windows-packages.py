#!/usr/bin/env python3
"""Create Scoop, WinGet, and Chocolatey metadata from one Windows release ZIP.

No network access or non-standard Python dependencies are required. Build the
Chocolatey .nupkg with `choco pack` from the generated choco-stage directory.
"""

import argparse
import hashlib
import json
import re
import zipfile
from pathlib import Path
from xml.etree import ElementTree as ET

REPOSITORY = "https://github.com/HiFiveJazz/jerc"
TARGET = "x86_64-pc-windows-msvc"
DESCRIPTION = "Keyboard-driven terminal email client written in Rust"
TAG_PATTERN = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
MANIFEST_VERSION = "1.9.0"


def validate_tag(tag: str) -> str:
    match = TAG_PATTERN.fullmatch(tag)
    if not match:
        raise ValueError(f"Expected stable version tag vMAJOR.MINOR.PATCH, got {tag!r}")
    return tag[1:]


def archive_name(tag: str) -> str:
    return f"jerc-{tag}-{TARGET}.zip"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def release_url(tag: str) -> str:
    return f"{REPOSITORY}/releases/download/{tag}/{archive_name(tag)}"


def expected_bin(tag: str) -> str:
    return f"jerc-{tag}-{TARGET}/jerc.exe"


def inspect_archive(archive: Path, tag: str) -> bytes:
    if archive.name != archive_name(tag) or not archive.is_file():
        raise ValueError(f"Expected release archive named {archive_name(tag)}: {archive}")
    expected = expected_bin(tag)
    with zipfile.ZipFile(archive) as zf:
        members = zf.namelist()
        if members.count(expected) != 1:
            raise ValueError(f"Expected exactly one {expected!r} in the ZIP")
        for name in members:
            if name.startswith("/") or ".." in Path(name).parts or "\\" in name:
                raise ValueError(f"Unsafe archive member: {name!r}")
        binary = zf.read(expected)
        if not binary:
            raise ValueError("Empty Windows executable")
        if not binary.startswith(b"MZ"):
            raise ValueError("The archive does not contain a Windows PE executable")
    return binary


def scoop_manifest(tag: str, digest: str) -> dict:
    version = validate_tag(tag)
    return {
        "version": version,
        "description": DESCRIPTION,
        "homepage": REPOSITORY,
        "license": "GPL-3.0-only",
        "architecture": {"64bit": {"url": release_url(tag), "hash": digest}},
        "bin": expected_bin(tag),
        "checkver": {"github": REPOSITORY, "regex": r"v([0-9]+\.[0-9]+\.[0-9]+)"},
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": f"{REPOSITORY}/releases/download/v$version/jerc-v$version-{TARGET}.zip"
                }
            },
            "bin": f"jerc-v$version-{TARGET}/jerc.exe",
        },
    }


def winget_manifests(tag: str, digest: str) -> dict[str, str]:
    version = validate_tag(tag)
    common = f"PackageIdentifier: HiFiveJazz.Jerc\nPackageVersion: {version}\n"
    return {
        "HiFiveJazz.Jerc.yaml": (
            common + "DefaultLocale: en-US\nManifestType: version\n"
            f"ManifestVersion: {MANIFEST_VERSION}\n"
        ),
        "HiFiveJazz.Jerc.installer.yaml": (
            common + "InstallerType: zip\nNestedInstallerType: portable\n"
            "Installers:\n"
            "- Architecture: x64\n"
            f"  InstallerUrl: {release_url(tag)}\n"
            f"  InstallerSha256: {digest.upper()}\n"
            "  NestedInstallerFiles:\n"
            f"  - RelativeFilePath: {expected_bin(tag)}\n"
            "    PortableCommandAlias: jerc\n"
            "ManifestType: installer\n"
            f"ManifestVersion: {MANIFEST_VERSION}\n"
        ),
        "HiFiveJazz.Jerc.locale.en-US.yaml": (
            common + "PackageLocale: en-US\nPublisher: HiFiveJazz\n"
            "PackageName: Jerc\nLicense: GPL-3.0-only\n"
            f"ShortDescription: {DESCRIPTION}\n"
            f"PackageUrl: {REPOSITORY}\n"
            "Moniker: jerc\n"
            "ManifestType: defaultLocale\n"
            f"ManifestVersion: {MANIFEST_VERSION}\n"
        ),
    }


def chocolatey_nuspec(tag: str) -> str:
    version = validate_tag(tag)
    # ElementTree protects the XML from accidental unescaped characters.
    root = ET.Element("package")
    metadata = ET.SubElement(root, "metadata")
    fields = {
        "id": "jerc", "version": version, "title": "Jerc", "authors": "HiFiveJazz",
        "owners": "HiFiveJazz", "projectUrl": REPOSITORY,
        "licenseUrl": f"{REPOSITORY}/blob/{tag}/LICENSE",
        "requireLicenseAcceptance": "false", "description": DESCRIPTION,
        "tags": "jerc terminal email rust cli imap smtp",
    }
    for name, value in fields.items():
        ET.SubElement(metadata, name).text = value
    files = ET.SubElement(root, "files")
    ET.SubElement(files, "file", {"src": r"tools\**", "target": "tools"})
    ET.indent(root, space="  ")
    return '<?xml version="1.0" encoding="utf-8"?>\n' + ET.tostring(root, encoding="unicode") + "\n"


def generate(tag: str, archive: Path, output: Path, choco_stage: Path, bundle: Path) -> None:
    version = validate_tag(tag)
    binary = inspect_archive(archive, tag)
    digest = sha256(archive)
    metadata = output
    scoop = metadata / "scoop" / "jerc.json"
    winget = metadata / "winget" / "manifests" / "h" / "HiFiveJazz" / "Jerc" / version
    choco = metadata / "chocolatey" / "jerc.nuspec"
    scoop.parent.mkdir(parents=True, exist_ok=True)
    winget.mkdir(parents=True, exist_ok=True)
    choco.parent.mkdir(parents=True, exist_ok=True)
    scoop.write_text(json.dumps(scoop_manifest(tag, digest), indent=2) + "\n", encoding="utf-8")
    for filename, text in winget_manifests(tag, digest).items():
        (winget / filename).write_text(text, encoding="utf-8")
    nuspec = chocolatey_nuspec(tag)
    choco.write_text(nuspec, encoding="utf-8")
    (metadata / "README.md").write_text(
        "Generated for Jerc " + tag + ".\n"
        "Scoop: submit scoop/jerc.json to a bucket.\n"
        "WinGet: submit winget/manifests/ to microsoft/winget-pkgs.\n"
        "Chocolatey: the separate jerc.VERSION.nupkg is self-contained.\n"
        "These files are not automatically listed by the package manager repositories.\n",
        encoding="utf-8",
    )
    choco_stage.mkdir(parents=True, exist_ok=True)
    (choco_stage / "jerc.nuspec").write_text(nuspec, encoding="utf-8")
    tools = choco_stage / "tools"
    tools.mkdir(parents=True, exist_ok=True)
    (tools / "jerc.exe").write_bytes(binary)
    with zipfile.ZipFile(archive) as zf:
        license_entry = f"jerc-{tag}-{TARGET}/LICENSE"
        if license_entry in zf.namelist():
            (tools / "LICENSE.txt").write_bytes(zf.read(license_entry))
    bundle.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(bundle, "w", zipfile.ZIP_DEFLATED) as zf:
        for file in sorted(metadata.rglob("*")):
            if file.is_file():
                zf.write(file, file.relative_to(metadata).as_posix())
    print(f"Created {bundle} and Chocolatey staging at {choco_stage}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--choco-stage", required=True, type=Path)
    parser.add_argument("--bundle", required=True, type=Path)
    args = parser.parse_args()
    try:
        generate(args.tag, args.archive, args.output, args.choco_stage, args.bundle)
    except (ValueError, OSError, zipfile.BadZipFile) as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    main()
