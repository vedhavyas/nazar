#!/usr/bin/env python3
"""Write the update manifest a release publishes.

Usage: update_manifest.py <repo-root> <version>

The one file both halves read: the desktop updater (Tauri's static manifest
- a `platforms` entry carries the `.sig`'s content with its trailing newline
trimmed, and the asset URL) and the phone (a top-level `android` block).

The trim is load-bearing: the plugin base64-decodes the manifest's value
whole, so a trailing newline is an InvalidByte and every desktop install
fails.

The `android` block sits BESIDE `platforms` rather than inside it: every
`platforms` entry must carry both `url` and `signature` or the whole file
fails to parse, and the APK has no minisign signature.

Every artifact is required to exist in the staged bundle directory - this
runs at release time, where a manifest naming an asset that was never built
is a release no client can update from.
"""

import json
import sys
from datetime import datetime, timezone
from pathlib import Path

REPOSITORY = "https://github.com/vedhavyas/nazar-releases"


def fail(message: str) -> None:
    print(f"[ERROR] {message}", file=sys.stderr)
    sys.exit(1)


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: update_manifest.py <repo-root> <version>")
    root = Path(sys.argv[1])
    version = sys.argv[2]

    bundle = root / "web/src-tauri/target/release/bundle"
    tarball = bundle / "macos/nazar.app.tar.gz"
    signature = bundle / "macos/nazar.app.tar.gz.sig"
    apk = bundle / f"android/nazar-{version}-arm64.apk"
    for path in (tarball, signature, apk):
        if not path.exists():
            fail(f"the manifest names {path.name}, but nothing staged it at {path}")

    release = f"{REPOSITORY}/releases/download/v{version}"
    try:
        sig = signature.read_text().rstrip()
    except OSError as err:
        fail(f"{signature} could not be read: {err}")

    manifest = {
        "version": version,
        "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": {
            "darwin-aarch64": {
                "signature": sig,
                "url": f"{release}/nazar.app.tar.gz",
            }
        },
        "android": {
            "url": f"{release}/nazar-{version}-arm64.apk",
        },
    }

    out = bundle / "latest.json"
    out.write_text(json.dumps(manifest, indent=2) + "\n")
    # What it wrote, read back off the manifest itself: a line spelling a
    # fixed list claims blocks a mutation could have dropped.
    print(f"[OK] wrote {out} - version {version}, keys {' '.join(sorted(manifest))}")


if __name__ == "__main__":
    main()
