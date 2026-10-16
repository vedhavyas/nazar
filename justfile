# Run the full check suite: formatting, lints, tests.
check: fmt-check lint test test-scripts

# The release scripts' own tests: `update_manifest.py` writes the one file
# every installed desktop reads for updates, and the release job runs it
# only at tag time - so its shape is pinned here, where a change meets a
# test before a tag does.
test-scripts:
    python3 scripts/test_update_manifest.py

# Fail on unformatted Rust.
fmt-check:
    cargo fmt --all -- --check

# Lint the workspace with clippy, warnings denied.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Run every test suite; builds the SPA first, since the server embeds it.
test: build-web test-rust test-web

# Run the Rust test suites.
test-rust:
    cargo nextest run --workspace --no-tests=pass

# Run the web test suites.
test-web:
    npm --prefix web test

# Build the SPA into web/dist.
build-web:
    npm --prefix web install
    npm --prefix web run build

# Build the release server binary and the SPA.
build: build-web
    cargo build --release -p server

# Run the server locally; state lands in ./.data (DATA_DIR and the binary
# paths all take environment overrides; just checks PATH otherwise).
run: build-web
    mkdir -p {{justfile_directory()}}/.data
    DATA_DIR={{justfile_directory()}}/.data cargo run -p server

# Build the container image and prove it healthy end to end.
image:
    ./tests/image_test.sh

# Run the shell against the Vite dev server: a frontend edit reloads into
# the open window.
desktop-dev:
    npm --prefix web run tauri -- dev

# Compile the shell the way it ships, without a bundle: the routine gate for
# anything under web/src-tauri/. The shell crate is its own workspace root,
# so the workspace's cargo steps never reach it, and this is minutes - not a
# step in `check`, which every Rust-only change would then pay for.
shell-check:
    npm --prefix web run tauri -- build --no-bundle --ci -- --locked
    cargo nextest run --manifest-path web/src-tauri/Cargo.toml --no-tests=pass

# Build the app and the dmg, under web/src-tauri/target/release/bundle. Not
# the routine gate: the dmg step mounts the image and opens a Finder window.
# The app alone is what `smoke-desktop` and a release use.
desktop-build:
    npm --prefix web run tauri -- build --ci -- --locked

# Build the debug APK for the phone: debug-signed, installable by hand with
# adb, Arm64 alone to match the release, and the Kotlin unit tests run once
# it is built. Needs the SDK, a JDK and the aarch64-linux-android Rust
# target; scripts/android-env.sh finds the rest.
android-build-debug:
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/android-env.sh
    npm --prefix web run tauri -- android build --debug --apk --ci --target aarch64
    # The build compiles the Kotlin but not its unit tests, and its success
    # prints no test count; the CLI wrote the gradle glue, so gradle runs
    # alone here. Universal is the variant `--target aarch64` builds.
    (cd web/src-tauri/gen/android && ./gradlew --console=plain :app:testUniversalDebugUnitTest)

# Mint the release identity once: ~/.android/nazar-release.keystore, the
# gitignored key.properties beside the android project, and the key's
# fingerprint recorded in the repo. The fingerprint is the tripwire - a
# regenerated keystore signs happily, and only the installed phones would
# find out.
android-keystore:
    #!/usr/bin/env bash
    set -euo pipefail

    if ! command -v java >/dev/null 2>&1; then
        echo "[ERROR] no java on PATH - keytool needs a JDK 17 or newer" >&2
        exit 1
    fi

    keystore="$HOME/.android/nazar-release.keystore"
    props=web/src-tauri/gen/android/app/key.properties
    fingerprint=web/src-tauri/gen/android/release-keystore.sha256
    if [ -e "$keystore" ] || [ -e "$props" ]; then
        echo "[ERROR] the release identity already exists ($keystore or $props) - change it deliberately, not by accident" >&2
        exit 1
    fi
    if [ -e "$fingerprint" ]; then
        echo "[ERROR] $fingerprint already records a keystore - that keystore is the release, not a new one" >&2
        exit 1
    fi

    password=$(openssl rand -base64 24)
    mkdir -p "$(dirname "$keystore")"
    keytool -genkeypair -keystore "$keystore" -alias nazar -keyalg RSA -keysize 4096 \
        -validity 10000 -storepass "$password" -keypass "$password" -dname "CN=nazar release"
    chmod 600 "$keystore"
    printf 'storeFile=%s\nstorePassword=%s\nkeyAlias=nazar\nkeyPassword=%s\n' \
        "$keystore" "$password" "$password" > "$props"
    chmod 600 "$props"
    keytool -list -v -keystore "$keystore" -alias nazar -storepass "$password" \
        | sed -n 's/.*SHA256: //p' | head -1 | tr -d ':' | tr 'A-Z' 'a-z' > "$fingerprint"
    echo "[OK] minted $keystore; wrote $props; recorded the fingerprint in $fingerprint"
    echo "     commit $fingerprint - the release checks against it"

# Build and stage the Android release: nazar-<version>-arm64.apk, signed
# with the release keystore. What the build produced is read back rather
# than assumed - the version off the APK, its ABI, and its signer against
# both the keystore and the fingerprint the repo records - because a stale
# APK, a wrong-ABI APK and a differently-signed APK all install exactly as
# quietly as a fresh one.
android-release version:
    #!/usr/bin/env bash
    set -euo pipefail
    source scripts/android-env.sh

    keystore="$HOME/.android/nazar-release.keystore"
    keyprops=web/src-tauri/gen/android/app/key.properties
    fingerprint=web/src-tauri/gen/android/release-keystore.sha256
    if [ ! -e "$keystore" ] || [ ! -e "$keyprops" ]; then
        echo "[ERROR] the release keystore is not set up - mint it once with 'just android-keystore'" >&2
        exit 1
    fi

    # Existence is not enough: a field missing from the file mislabels the
    # failure as a missing APK later, so each is required by name.
    keyfile=$(sed -n 's/^storeFile=//p' "$keyprops" || true)
    password=$(sed -n 's/^storePassword=//p' "$keyprops" || true)
    keyalias=$(sed -n 's/^keyAlias=//p' "$keyprops" || true)
    keypassword=$(sed -n 's/^keyPassword=//p' "$keyprops" || true)
    missing=""
    [ -n "$keyfile" ] || missing="$missing storeFile"
    [ -n "$password" ] || missing="$missing storePassword"
    [ -n "$keyalias" ] || missing="$missing keyAlias"
    [ -n "$keypassword" ] || missing="$missing keyPassword"
    if [ -n "$missing" ]; then
        echo "[ERROR] $keyprops is missing:$missing" >&2
        exit 1
    fi

    # A keystore readable past its owner is worth one line, not a refusal.
    # GNU stat first (the common case on this machine's PATH), BSD second.
    mode=$(stat -c '%a' "$keystore" 2>/dev/null || stat -f '%Lp' "$keystore" 2>/dev/null || true)
    if [ "$mode" != "600" ]; then
        echo "[WARN] $keystore is readable beyond its owner - chmod 600 it" >&2
    fi

    # What the build may not do is CHANGE the tree - a Cargo.lock or gen/
    # rewrite riding out of a release. Compared before and after rather than
    # against clean, so a standalone retry works from whatever the tree
    # holds.
    before=$(git status --porcelain || true)
    npm --prefix web run tauri -- android build --apk --ci --target aarch64

    built=web/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release.apk
    if [ ! -e "$built" ]; then
        echo "[ERROR] the build produced no APK at $built" >&2
        exit 1
    fi

    aapt2=$(find "$ANDROID_HOME/build-tools" -maxdepth 2 -name aapt2 -type f 2>/dev/null | sort -V | tail -1 || true)
    apksigner=$(find "$ANDROID_HOME/build-tools" -maxdepth 2 -name apksigner -type f 2>/dev/null | sort -V | tail -1 || true)
    if [ -z "$aapt2" ] || [ -z "$apksigner" ]; then
        echo "[ERROR] no aapt2/apksigner under $ANDROID_HOME/build-tools - the SDK is incomplete" >&2
        exit 1
    fi

    got=$("$aapt2" dump badging "$built" | sed -n "s/^package:.*versionName='\([^']*\)'.*/\1/p" || true)
    if [ "$got" != "{{version}}" ]; then
        echo "[ERROR] the built APK is version $got, expected {{version}}" >&2
        exit 1
    fi

    # The whole field, not its first token: a multi-ABI APK lists several and
    # a first-token read would stage it under the arm64 name.
    abi=$("$aapt2" dump badging "$built" | sed -n 's/^native-code: //p' | tr -d "'" || true)
    if [ "$abi" != "arm64-v8a" ]; then
        echo "[ERROR] the built APK's native code is '$abi', expected arm64-v8a" >&2
        exit 1
    fi

    # keytool prints the digest upper-case with colons and apksigner
    # lower-case without, so both sides are normalised before they are
    # compared. The signer label depends on the apksigner version ("Signer
    # #1" one build-tools release, "V2 Signer" another); the digest line is
    # what counts. The password rides argv here, which the threat model
    # allows: one trusted user on their own machine.
    want=$(keytool -list -v -keystore "$keystore" -alias "$keyalias" -storepass "$password" 2>/dev/null | sed -n 's/.*SHA256: //p' | head -1 | tr -d ':' | tr 'A-Z' 'a-z' || true)
    if [ -z "$want" ]; then
        echo "[ERROR] the release keystore could not be read - check key.properties' alias and password" >&2
        exit 1
    fi
    recorded=$(tr -d '[:space:]' < "$fingerprint" || true)
    if [ "$recorded" != "$want" ]; then
        echo "[ERROR] the keystore's fingerprint is ${want:-none}, the repo records ${recorded:-none}" >&2
        echo "        a regenerated keystore signs happily and every installed phone refuses it" >&2
        exit 1
    fi
    signed=$("$apksigner" verify --print-certs "$built" | sed -n 's/^.*certificate SHA-256 digest: //p' | head -1 | tr -d ':' | tr 'A-Z' 'a-z' || true)
    if [ "$signed" != "$want" ]; then
        echo "[ERROR] the APK is not signed by the release keystore (signer ${signed:-none}, expected $want)" >&2
        exit 1
    fi

    if [ "$(git status --porcelain)" != "$before" ]; then
        echo "[ERROR] the Android build changed the tree - commit or undo before releasing" >&2
        git status --short >&2
        exit 1
    fi

    out=web/src-tauri/target/release/bundle/android
    mkdir -p "$out"
    cp "$built" "$out/nazar-{{version}}-arm64.apk"
    echo "[OK] staged the Android release: $out/nazar-{{version}}-arm64.apk (arm64, release-signed)"

# What to check by hand on the desktop shell, with the server reachable.
smoke-desktop:
    @echo "1. just run   (or point the shells at a running server; the"
    @echo "   server address is set in the app once the setup screen lands)"
    @echo "2. open web/src-tauri/target/release/bundle/macos/nazar.app"
    @echo "3. the window opens at 1200x800 and the app renders inside it"
    @echo "   (with no server reachable: the banner and the unlock screen,"
    @echo "   never a blank window)"

# What to check by hand on the M2 house, with a real account and a phone
# for the OTP within reach.
smoke-m2:
    @echo "1. just run   (DATA_DIR defaults to ./.data; go2rtc and ffmpeg on PATH)"
    @echo "2. open http://localhost:8080 and log in with the OTP sent to the phone"
    @echo "3. the home shows one camera large and the rest as tiles, all live"
    @echo "4. walk in front of a tiled camera: it takes the hero within ~15s,"
    @echo "   and a second camera moving right after does not steal it back"
    @echo "5. turn away mode on: tiles show a still, replaced every 10 seconds"
    @echo "6. open a camera: drag the picture to pan (watch the request rate stay"
    @echo "   at or under four a second), flip privacy and see the shadow's answer,"
    @echo "   switch quality, press Listen for sound, and take a snapshot"

# Cut a release: bump both manifests (the workspace and the shell crate, so
# the tag names the version every check reads back), commit, tag, push. CI
# then builds, signs and publishes the desktop tarball, the APK and
# latest.json; `just publish <version>` verifies what landed once it has.
# nazar's cuts ride branches - main only ever moves deliberately - so the
# tag goes up with whatever branch HEAD is on.
release version:
    @if ! cargo set-version --help >/dev/null 2>&1; then \
        echo "[ERROR] cargo set-version not available - run: cargo install cargo-edit" >&2; \
        exit 1; \
    fi
    @if [ -n "$(git status --porcelain)" ]; then \
        echo "[ERROR] working tree dirty - commit or stash before releasing" >&2; \
        exit 1; \
    fi
    @if git rev-parse "v{{version}}" >/dev/null 2>&1; then \
        echo "[ERROR] tag v{{version}} already exists" >&2; \
        exit 1; \
    fi
    cargo set-version --workspace {{version}}
    cargo set-version --manifest-path web/src-tauri/Cargo.toml {{version}}
    cargo update --workspace
    git add Cargo.toml Cargo.lock web/src-tauri/Cargo.toml web/src-tauri/Cargo.lock
    git commit -m "release v{{version}}"
    # `-m` so the tag carries a message: tag.gpgSign is on, and signing
    # needs one.
    git tag -m "v{{version}}" "v{{version}}"
    git push --follow-tags origin HEAD
    @echo
    @echo "[OK] released v{{version}}: both manifests bumped, tagged and pushed"
    @echo "     CI is building and publishing now; verify with 'just publish {{version}}' once it lands"

# What a release owes after CI has published it: every asset answers on its
# own URL, and latest.json names this release for both shells. Run it after
# the release workflow turns green - before that, a 404 is CI still building.
publish version:
    #!/usr/bin/env bash
    set -euo pipefail

    if ! git ls-remote --exit-code --tags origin "v{{version}}" >/dev/null 2>&1; then
        echo "[ERROR] tag v{{version}} is not on origin - push it first" >&2
        exit 1
    fi

    release="https://github.com/vedhavyas/nazar-releases/releases/download/v{{version}}"
    for asset in latest.json nazar.app.tar.gz nazar.app.tar.gz.sig "nazar-{{version}}-arm64.apk"; do
        code=$(curl -sS -o /dev/null -w '%{http_code}' -L "$release/$asset")
        [ "$code" = "200" ] || {
            echo "[ERROR] $release/$asset answered $code - is the release still building?" >&2
            exit 1
        }
        echo "[OK] $asset answers 200"
    done

    tmp=$(mktemp)
    curl -fsSL "$release/latest.json" -o "$tmp"
    python3 - "$tmp" "{{version}}" <<'PY'
    import json
    import sys

    manifest = json.load(open(sys.argv[1]))
    version = sys.argv[2]
    if manifest["version"] != version:
        raise SystemExit(f"[ERROR] latest.json is {manifest['version']}, expected {version}")
    urls = [
        manifest["platforms"]["darwin-aarch64"]["url"],
        manifest["android"]["url"],
    ]
    for url in urls:
        if f"/download/v{version}/" not in url:
            raise SystemExit(f"[ERROR] latest.json names {url}, not this release")
    print(f"[OK] latest.json is {version} and names this release for both shells")
    PY
    rm -f "$tmp"
