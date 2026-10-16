#!/usr/bin/env python3
"""Tests for `update_manifest.py`, the file every installed desktop reads
for updates. Run: python3 scripts/test_update_manifest.py

Stdlib only, like the server: python3 is already a check dependency.
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("update_manifest.py")
RELEASE = "https://github.com/vedhavyas/nazar-releases/releases/download/v9.9.9"
SIGNATURE = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkK\n"


class UpdateManifestTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.bundle = self.root / "web/src-tauri/target/release/bundle"
        (self.bundle / "macos").mkdir(parents=True)
        (self.bundle / "android").mkdir(parents=True)
        # Stand-ins: the contents only have to exist, because what the test
        # pins is which file each name resolves to.
        (self.bundle / "macos/nazar.app.tar.gz").write_bytes(b"the app tarball")
        (self.bundle / "macos/nazar.app.tar.gz.sig").write_text(SIGNATURE)
        (self.bundle / "android/nazar-9.9.9-arm64.apk").write_bytes(b"the apk")

    def run_generator(self) -> "subprocess.CompletedProcess[str]":
        return subprocess.run(
            [sys.executable, str(SCRIPT), str(self.root), "9.9.9"],
            capture_output=True,
            text=True,
        )

    def manifest(self) -> dict:
        return json.loads((self.bundle / "latest.json").read_text())

    def test_writes_the_platform_block_and_the_release_url(self) -> None:
        result = self.run_generator()
        self.assertEqual(result.returncode, 0, result.stderr)

        manifest = self.manifest()
        self.assertEqual(manifest["version"], "9.9.9")
        self.assertEqual(
            manifest["platforms"]["darwin-aarch64"]["url"],
            f"{RELEASE}/nazar.app.tar.gz",
        )

        # The operator's line must not claim a block the file does not carry.
        self.assertIn("platforms", result.stdout)

    def test_trims_only_the_trailing_newline_off_the_signature(self) -> None:
        self.run_generator()

        # The plugin base64-decodes this value whole: one trailing newline
        # is an InvalidByte and every desktop install fails.
        self.assertEqual(
            self.manifest()["platforms"]["darwin-aarch64"]["signature"],
            SIGNATURE.rstrip(),
        )

    def test_refuses_a_missing_artifact_by_name(self) -> None:
        (self.bundle / "macos/nazar.app.tar.gz.sig").unlink()

        result = self.run_generator()

        self.assertEqual(result.returncode, 1)
        self.assertIn("nazar.app.tar.gz.sig", result.stderr)
        self.assertFalse((self.bundle / "latest.json").exists())

    def test_writes_the_android_block(self) -> None:
        result = self.run_generator()
        self.assertEqual(result.returncode, 0, result.stderr)

        # Beside `platforms`, not inside it: a platforms entry must carry a
        # signature, and the APK has none.
        self.assertEqual(
            self.manifest()["android"]["url"],
            f"{RELEASE}/nazar-9.9.9-arm64.apk",
        )
        self.assertIn("android", result.stdout)

    def test_refuses_a_missing_apk_by_name(self) -> None:
        (self.bundle / "android/nazar-9.9.9-arm64.apk").unlink()

        result = self.run_generator()

        self.assertEqual(result.returncode, 1)
        self.assertIn("nazar-9.9.9-arm64.apk", result.stderr)
        self.assertFalse((self.bundle / "latest.json").exists())


if __name__ == "__main__":
    unittest.main()
