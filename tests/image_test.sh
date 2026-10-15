#!/usr/bin/env bash
# Builds the image and proves it healthy end to end: the container comes up
# on a root-owned data mount, the API answers, the SPA is served, and the
# bundled go2rtc runs.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# The docker CLI (and the credential helper it shells out to) is not always
# on PATH on this machine; both live in /usr/local/bin when the desktop
# install is present.
DOCKER="${DOCKER:-}"
if [ -z "$DOCKER" ]; then
  if command -v docker >/dev/null 2>&1; then
    DOCKER=docker
  elif [ -x /usr/local/bin/docker ]; then
    DOCKER=/usr/local/bin/docker
    PATH="/usr/local/bin:$PATH"
    export PATH
  else
    echo "docker CLI not found; set DOCKER=/path/to/docker" >&2
    exit 1
  fi
fi

IMAGE="${IMAGE:-nazar:test}"
CONTAINER="nazar-image-test"
PORT="${PORT:-18080}"
SCRATCH="$(mktemp -d)"

cleanup() {
  # Nothing here may fail the script: on Linux a bind mount maps the
  # container's chown to the host's root, so the scratch files are not
  # deletable (or up for grabs) from the runner's own uid - clear them from
  # a throwaway root shell running the image, then drop the directory.
  "$DOCKER" rm -f "$CONTAINER" >/dev/null 2>&1 || true
  if [ -d "$SCRATCH" ] && ! rm -rf "$SCRATCH" 2>/dev/null; then
    "$DOCKER" run --rm -v "$SCRATCH:/scratch" --entrypoint /bin/sh "$IMAGE" \
      -c 'rm -rf /scratch/* /scratch/.[!.]*' >/dev/null 2>&1 || true
    rm -rf "$SCRATCH" 2>/dev/null || true
  fi
  return 0
}
trap cleanup EXIT

echo "[..] building $IMAGE"
# Provenance labels come from the CI env when there is one.
"$DOCKER" build -t "$IMAGE" \
  --build-arg "VERSION=${VERSION:-${GITHUB_REF_NAME:-dev}}" \
  --build-arg "REVISION=${REVISION:-${GITHUB_SHA:-unknown}}" \
  "$ROOT"

echo "[..] starting the container on a root-owned data dir"
mkdir -p "$SCRATCH/data"
"$DOCKER" run -d --name "$CONTAINER" \
  -p "127.0.0.1:$PORT:8080" \
  -v "$SCRATCH/data:/data" \
  "$IMAGE" >/dev/null

health=""
for _ in $(seq 1 60); do
  if health="$(curl -fsS "http://127.0.0.1:$PORT/api/health" 2>/dev/null)"; then
    break
  fi
  sleep 1
done
if [ -z "$health" ]; then
  echo "the container never answered /api/health; logs:" >&2
  "$DOCKER" logs "$CONTAINER" >&2 || true
  exit 1
fi
case "$health" in
  *'"ok":true'*) echo "[ok] health: $health" ;;
  *) echo "unexpected health body: $health" >&2; exit 1 ;;
esac

if curl -fsS "http://127.0.0.1:$PORT/" | grep -q 'id="app"'; then
  echo "[ok] the SPA is served"
else
  echo "the SPA was not served" >&2
  exit 1
fi

if version="$("$DOCKER" exec "$CONTAINER" go2rtc -version 2>&1)"; then
  echo "[ok] go2rtc: $version"
else
  echo "go2rtc is not runnable in the image" >&2
  exit 1
fi

echo "[ok] image is healthy"
