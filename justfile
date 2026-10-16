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
