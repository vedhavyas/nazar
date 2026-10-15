# One image: the built SPA embedded in the Rust server, plus go2rtc and
# ffmpeg for the camera pipeline. Nothing else is needed at runtime beyond
# a writable /data.

# --- the SPA -----------------------------------------------------------------
FROM node:24-bookworm-slim AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# --- the server --------------------------------------------------------------
FROM rust:bookworm AS rust
# rust-toolchain.toml pins the nightly; rustup installs it here.
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY --from=web /src/web/dist ./web/dist
RUN cargo build --release -p server

# --- the runtime -------------------------------------------------------------
FROM debian:bookworm-slim
# Bump deliberately: the spike proved this exact go2rtc against the cameras.
ARG GO2RTC_VERSION=v1.9.14
# Provenance, so a pulled image can be traced to its release: the tag run
# passes its ref name and sha in.
ARG VERSION=dev
ARG REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/vedhavyas/nazar" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg util-linux \
 && rm -rf /var/lib/apt/lists/*
RUN case "$(dpkg --print-architecture)" in \
      amd64) GO2RTC_ARCH=amd64 ;; \
      arm64) GO2RTC_ARCH=arm64 ;; \
      *) echo "unsupported architecture" >&2; exit 1 ;; \
    esac \
 && curl -fsSL "https://github.com/AlexxIT/go2rtc/releases/download/${GO2RTC_VERSION}/go2rtc_linux_${GO2RTC_ARCH}" -o /usr/local/bin/go2rtc \
 && chmod 0755 /usr/local/bin/go2rtc
COPY --from=rust /src/target/release/nazar-server /usr/local/bin/nazar-server
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/entrypoint.sh \
 && mkdir -p /data \
 && chown 101:101 /data

ENV DATA_DIR=/data \
    GO2RTC_BIN=/usr/local/bin/go2rtc \
    FFMPEG_BIN=/usr/bin/ffmpeg \
    RUST_LOG=info

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
