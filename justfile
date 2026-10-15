# Run the full check suite: formatting, lints, tests.
check: fmt-check lint test

# Fail on unformatted Rust.
fmt-check:
    cargo fmt --all -- --check

# Lint the workspace with clippy, warnings denied.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Run the Rust test suites.
test:
    cargo nextest run --workspace --no-tests=pass

# Build the SPA into web/dist.
build-web:
    npm --prefix web ci
    npm --prefix web run build

# Build the release server binary and the SPA.
build: build-web
    cargo build --release -p server

# Run the server locally.
run: build-web
    cargo run -p server

# Build the container image.
image:
    docker build -t nazar:dev .
