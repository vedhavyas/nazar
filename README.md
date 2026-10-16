# Nazar

Home cameras on our own terms - a private, cross-platform client for the family's Qubo cameras, built on the reverse-engineered Qubo cloud API. Desktop, Android, and wherever else is decided, from one Rust + Tauri codebase.

Private repo, personal use. The canonical API research lives in `docs/`.

## The Android app

Sideloaded, no store listing. The build needs the Android SDK, a JDK 17+, and
the `aarch64-linux-android` Rust target; `scripts/android-env.sh` finds the
SDK wherever it is installed (Homebrew's `android-commandlinetools` counts).

- Debug: `just android-build-debug` writes
  `web/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk`;
  install it with `adb install -r <apk>`.
- Release: `just android-release <version>` builds, checks and stages
  `web/src-tauri/target/release/bundle/android/nazar-<version>-arm64.apk`,
  signed with the release keystore. The keystore lives at
  `~/.android/nazar-release.keystore` and is minted once with
  `just android-keystore`; its fingerprint is committed so a regenerated key
  is caught before a release rather than by every installed phone.
- A phone carrying a debug build takes one uninstall before the first
  release-signed install: the signers differ, and Android refuses an upgrade
  across them. After that, releases upgrade in place (and in app).
