# Run the full check suite: formatting, lints, tests.
check: fmt-check lint test

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
