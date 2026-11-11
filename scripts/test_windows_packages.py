#!/usr/bin/env python3
"""Offline regression tests for Scoop, WinGet, and Chocolatey release metadata."""

import hashlib
import importlib.util
import json
import tempfile
import unittest
import zipfile
from pathlib import Path
from xml.etree import ElementTree as ET

SCRIPT = Path(__file__).with_name("generate-windows-packages.py")
spec = importlib.util.spec_from_file_location("jerc_windows_packages", SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class WindowsPackageTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.tag = "v0.6.2"
        self.archive = self.root / module.archive_name(self.tag)
        self.exe = b"MZ" + b"test-windows-binary" * 4
        with zipfile.ZipFile(self.archive, "w") as zf:
            zf.writestr(module.expected_bin(self.tag), self.exe)
            zf.writestr(f"jerc-{self.tag}-{module.TARGET}/LICENSE", "GPL-3.0-only")
        self.hash = hashlib.sha256(self.archive.read_bytes()).hexdigest()

    def render(self):
        module.generate(
            self.tag, self.archive, self.root / "meta", self.root / "stage",
            self.root / "jerc-windows-packages-v0.6.2.zip",
        )

    def test_generated_metadata_references_exact_release_archive(self):
        self.render()
        manifest = json.loads((self.root / "meta/scoop/jerc.json").read_text())
        self.assertEqual("0.6.2", manifest["version"])
        self.assertEqual(self.hash, manifest["architecture"]["64bit"]["hash"])
        self.assertEqual(module.release_url(self.tag), manifest["architecture"]["64bit"]["url"])
        self.assertEqual(module.expected_bin(self.tag), manifest["bin"])
        self.assertIn("v$version", manifest["autoupdate"]["architecture"]["64bit"]["url"])
        winget = self.root / "meta/winget/manifests/h/HiFiveJazz/Jerc/0.6.2"
        self.assertEqual(3, len(list(winget.glob("*.yaml"))))
        installer = (winget / "HiFiveJazz.Jerc.installer.yaml").read_text()
        self.assertIn(module.release_url(self.tag), installer)
        self.assertIn(self.hash.upper(), installer)
        self.assertIn(module.expected_bin(self.tag), installer)
        self.assertIn("NestedInstallerType: portable", installer)
        self.assertIn("PortableCommandAlias: jerc", installer)

    def test_chocolatey_stage_contains_exact_executable_and_nuspec(self):
        self.render()
        self.assertEqual(self.exe, (self.root / "stage/tools/jerc.exe").read_bytes())
        xml = ET.parse(self.root / "stage/jerc.nuspec").getroot()
        fields = {x.tag: x.text for x in xml.find("metadata")}
        self.assertEqual("jerc", fields["id"])
        self.assertEqual("0.6.2", fields["version"])
        self.assertEqual("false", fields["requireLicenseAcceptance"])
        self.assertEqual("tools\\**", xml.find("files/file").attrib["src"])
        self.assertEqual("GPL-3.0-only", (self.root / "stage/tools/LICENSE.txt").read_text())

    def test_bundle_is_only_metadata_not_repacked_executable(self):
        self.render()
        with zipfile.ZipFile(self.root / "jerc-windows-packages-v0.6.2.zip") as zf:
            names = zf.namelist()
            self.assertIn("scoop/jerc.json", names)
            self.assertIn("chocolatey/jerc.nuspec", names)
            self.assertEqual(6, len(names))  # README + Scoop + three WinGet + nuspec
            self.assertNotIn("tools/jerc.exe", names)

    def test_reject_invalid_tag_path_and_zip(self):
        for tag in ["v0.6.2-rc1", "v0.6", "../0.6.2", "v01.2.3"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                module.validate_tag(tag)
        renamed = self.root / "something.zip"
        renamed.write_bytes(self.archive.read_bytes())
        with self.assertRaises(ValueError):
            module.inspect_archive(renamed, self.tag)
        self.archive.unlink()
        with zipfile.ZipFile(self.archive, "w") as zf:
            zf.writestr("../jerc.exe", self.exe)
            zf.writestr(module.expected_bin(self.tag), self.exe)
        with self.assertRaises(ValueError):
            module.inspect_archive(self.archive, self.tag)


if __name__ == "__main__":
    unittest.main()
