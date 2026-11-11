#!/usr/bin/env python3
"""Verify an internalized Chocolatey package contains the tagged Jerc binary."""

import argparse
import hashlib
import importlib.util
import zipfile
from pathlib import Path
from xml.etree import ElementTree as ET

MODULE_PATH = Path(__file__).with_name("generate-windows-packages.py")
spec = importlib.util.spec_from_file_location("jerc_windows", MODULE_PATH)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def verify(tag: str, archive: Path, nupkg: Path) -> None:
    version = module.validate_tag(tag)
    expected = module.inspect_archive(archive, tag)
    if nupkg.name != f"jerc.{version}.nupkg":
        raise ValueError(f"Unexpected Chocolatey package filename: {nupkg.name}")
    with zipfile.ZipFile(nupkg) as bundle:
        entries = bundle.namelist()
        if entries.count("tools/jerc.exe") != 1:
            raise ValueError("Chocolatey package is missing tools/jerc.exe")
        actual = bundle.read("tools/jerc.exe")
        if hashlib.sha256(actual).digest() != hashlib.sha256(expected).digest():
            raise ValueError("Chocolatey executable differs from GitHub release ZIP")
        nuspecs = [name for name in entries if name.lower().endswith(".nuspec")]
        if len(nuspecs) != 1:
            raise ValueError("Expected exactly one Chocolatey .nuspec")
        xml = ET.fromstring(bundle.read(nuspecs[0]))
        # NuGet may add an XML namespace when packing the input .nuspec.
        metadata = next((node for node in xml if node.tag.rsplit("}", 1)[-1] == "metadata"), None)
        if metadata is None:
            raise ValueError("Chocolatey metadata is missing")
        fields = {node.tag.rsplit("}", 1)[-1]: node.text for node in metadata}
        if fields.get("id") != "jerc" or fields.get("version") != version:
            raise ValueError("Chocolatey name or version does not match the release tag")
    print(f"Verified {nupkg.name}: version {version}, exact release executable")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--package", required=True, type=Path)
    args = parser.parse_args()
    try:
        verify(args.tag, args.archive, args.package)
    except (ValueError, OSError, zipfile.BadZipFile, ET.ParseError) as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    main()
