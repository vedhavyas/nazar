#!/usr/bin/env python3
"""Check a built update tarball's signature against the pinned public key.

Usage: check_updater_signature.py <repo-root> <version>

The Tauri CLI signs with whatever key `TAURI_SIGNING_PRIVATE_KEY` names, and
only WARNS when that key is not the one `tauri.conf.json` pins - a mismatched
key signs happily, the recipe would print its OK line, and every installed
desktop would then refuse the artifact at install time (fail-closed, so the
release is dead on arrival rather than dangerous). The pair is read back and
compared here instead.

Reads the pinned `pubkey` from `<root>/web/src-tauri/tauri.conf.json` and the
signature from the app bundle's `nazar.app.tar.gz.sig`. Both are base64 of a
minisign text file whose second line decodes to a blob carrying the
eight-byte key id at offset 2; the signature's trusted comment carries the
version the artifact was signed for.
"""

import base64
import json
import re
import sys
from pathlib import Path


def fail(message: str) -> None:
    print(f"[ERROR] {message}", file=sys.stderr)
    sys.exit(1)


def lines_of(base64_text: str, what: str) -> list[str]:
    """The minisign text behind a base64 blob, as its lines."""
    try:
        return base64.b64decode(base64_text).decode().splitlines()
    except (ValueError, UnicodeDecodeError) as err:
        fail(f"{what} is not base64 minisign text: {err}")


def key_id(lines: list[str], what: str) -> bytes:
    """The eight key-id bytes every minisign blob carries at offset 2."""
    if len(lines) < 2:
        fail(f"{what} carries no minisign blob")
    try:
        blob = base64.b64decode(lines[1])
    except ValueError as err:
        fail(f"{what}'s blob does not decode: {err}")
    if len(blob) < 10:
        fail(f"{what}'s blob is too short to carry a key id")
    return blob[2:10]


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: check_updater_signature.py <repo-root> <version>")
    root = Path(sys.argv[1])
    version = sys.argv[2]

    conf_path = root / "web/src-tauri/tauri.conf.json"
    try:
        conf = json.loads(conf_path.read_text())
        pinned_text = conf["plugins"]["updater"]["pubkey"]
    except OSError as err:
        fail(f"{conf_path} could not be read: {err}")
    except (json.JSONDecodeError, KeyError, TypeError):
        fail(f"{conf_path} carries no plugins.updater.pubkey")

    bundle = root / "web/src-tauri/target/release/bundle/macos"
    tarball = bundle / "nazar.app.tar.gz"
    signature = bundle / "nazar.app.tar.gz.sig"
    if not tarball.exists():
        fail(f"no updater tarball at {tarball} - the build did not produce one")
    if not signature.exists():
        fail(f"no signature at {signature} - the tarball was not signed")

    try:
        sig_text = signature.read_text()
    except OSError as err:
        fail(f"{signature} could not be read: {err}")

    pinned_id = key_id(lines_of(pinned_text, "the pinned pubkey"), "the pinned pubkey")
    sig_lines = lines_of(sig_text, "the update signature")
    signed_id = key_id(sig_lines, "the update signature")
    if signed_id != pinned_id:
        fail(
            "the update signature is not from the pinned key "
            f"({signed_id.hex()} signed, {pinned_id.hex()} pinned) - "
            "every installed desktop would refuse this artifact"
        )

    comment = sig_lines[2] if len(sig_lines) > 2 else ""
    match = re.search(r"version:(\S+)", comment)
    signed_version = match.group(1) if match else None
    if signed_version != version:
        fail(f"the signature is for version {signed_version or 'unknown'}, expected {version}")

    print(f"[OK] the update tarball is signed by the pinned key, for version {signed_version}")


if __name__ == "__main__":
    main()
