#!/usr/bin/env python3
"""Unit tests for generating Homebrew formulae from existing release archives."""

import importlib.util
import hashlib
import io
import os
import re
import tarfile
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("generate-homebrew-formula.py")
spec = importlib.util.spec_from_file_location("jerc_homebrew_formula", SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class HomebrewFormulaTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.archives = {}
        for target in module.TARGETS.values():
            root = f"jerc-v0.6.2-{target}"
            archive = self.root / f"{root}.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                for filename in ("jerc", "LICENSE", "README.md", "config.example.toml"):
                    payload = ("jerc 0.6.2\n" if filename == "jerc" else filename).encode()
                    entry = tarfile.TarInfo(f"{root}/{filename}")
                    entry.mode = 0o755 if filename == "jerc" else 0o644
                    entry.size = len(payload)
                    tar.addfile(entry, io.BytesIO(payload))
            self.archives[target] = archive

    def test_public_formula_has_both_release_urls_and_real_hashes(self):
        formula = module.generate("v0.6.2", self.root)
        self.assertIn('version "0.6.2"', formula)
        self.assertIn("on_arm do", formula)
        self.assertIn("on_intel do", formula)
        self.assertEqual(2, len(re.findall(r'^      sha256 "[a-f0-9]{64}"$', formula, re.M)))
        for archive in self.archives.values():
            self.assertIn(f"https://github.com/HiFiveJazz/jerc/releases/download/v0.6.2/{archive.name}", formula)
            self.assertIn(hashlib.sha256(archive.read_bytes()).hexdigest(), formula)

    def test_local_formula_only_changes_urls(self):
        published = module.generate("v0.6.2", self.root)
        local = module.generate("v0.6.2", self.root, local_assets=True)
        for archive in self.archives.values():
            self.assertIn(archive.resolve().as_uri(), local)
            self.assertIn(hashlib.sha256(archive.read_bytes()).hexdigest(), local)
        self.assertNotIn("releases/download", local)
        self.assertEqual(published.count("sha256"), local.count("sha256"))

    def test_local_formula_resolves_symlinked_archive_directory(self):
        # macOS aliases /var to /private/var. Path.resolve() should produce
        # the same canonical file URI for the formula and test expectations.
        alias = self.root / "archive-alias"
        alias.symlink_to(self.root, target_is_directory=True)
        local = module.generate("v0.6.2", alias, local_assets=True)
        for archive in self.archives.values():
            self.assertIn(archive.resolve().as_uri(), local)
            self.assertIn(hashlib.sha256(archive.read_bytes()).hexdigest(), local)

    def test_rejects_invalid_tag_and_missing_or_nonexecutable_binary(self):
        with self.assertRaises(ValueError):
            module.generate("v0.6.2-rc1", self.root)
        self.archives["aarch64-apple-darwin"].unlink()
        with self.assertRaises(FileNotFoundError):
            module.generate("v0.6.2", self.root)


if __name__ == "__main__":
    unittest.main()
