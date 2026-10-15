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
    DATA_DIR={{justfile_directory()}}/.data NAZAR_INSECURE_COOKIE=1 cargo run -p server

# Build the container image and prove it healthy end to end.
image:
    ./tests/image_test.sh
