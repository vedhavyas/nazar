#!/usr/bin/env bash
# The Android toolchain the shell's android recipes build with, resolved
# rather than required: a fresh login shell on this machine exports none of
# ANDROID_HOME / NDK_HOME / JAVA_HOME, and the CLI's own fallback lands on an
# empty ~/Library/Android/sdk (which the CLI creates while looking), so the
# recipes find what is installed and hand the CLI what it needs. Sourced by
# `android-build-debug` and `android-release`.
set -euo pipefail

# An SDK the CLI can use: one with platforms or an NDK inside it. The empty
# ~/Library/Android/sdk the CLI creates while looking must not count.
sdk=""
for candidate in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Library/Android/sdk" /opt/homebrew/share/android-commandlinetools; do
    [ -n "$candidate" ] || continue
    if [ -d "$candidate/platforms" ] || [ -d "$candidate/ndk" ]; then
        sdk="$candidate"
        break
    fi
done
if [ -z "$sdk" ]; then
    echo "[ERROR] no Android SDK found - set ANDROID_HOME, or install one (README, the Android app)" >&2
    exit 1
fi

# NDK_HOME wins when set, since that is what the CLI honours first.
ndk="${NDK_HOME:-$(find "$sdk/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort -V | tail -1 || true)}"
if [ -z "$ndk" ]; then
    echo "[ERROR] no Android NDK under $sdk/ndk - install one (README, the Android app)" >&2
    exit 1
fi

# Gradle takes java from PATH when JAVA_HOME is unset, which is the case in
# a fresh shell here; it only has to exist.
if ! command -v java >/dev/null 2>&1; then
    echo "[ERROR] no java on PATH - the Android build needs a JDK 17 or newer (README, the Android app)" >&2
    exit 1
fi

export ANDROID_HOME="$sdk"
export NDK_HOME="$ndk"
echo "[INFO] android build: SDK $sdk, NDK $ndk, java $(command -v java)"
