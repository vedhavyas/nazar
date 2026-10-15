# Nazar on the hub - the module recipe

The deployment contract for hosting nazar on Ved's hub, written from the hub's own facts (`hub-modules` repo: the docker-module system, caddy labels, env handling, the forge-web precedent). Task 10 of the M1 plan hands this to the hub-modules project's agent.

## Compose file (in hub-modules: `static/system/docker/docker-compose-nazar.yml`)

```yaml
services:
  nazar:
    container_name: nazar
    hostname: nazar
    image: ghcr.io/vedhavyas/nazar:latest
    restart: unless-stopped
    # The entrypoint chowns /data and drops to uid 101 itself; do NOT pin user:.
    pull_policy: always
    ports:
      # WebRTC media only, bound to the WireGuard address so it never lands on
      # the public interface; every viewer is a WG peer.
      - "198.51.100.1:8555:8555/tcp"
      - "198.51.100.1:8555:8555/udp"
    environment:
      DATA_DIR: /data
      RUST_LOG: info
      QUBO_SP: d10e4bfb0153496e8e8bb955f7ebe413
      NAZAR_WEBRTC_CANDIDATES: 198.51.100.1:8555
      # Caddy's exact address: with this set, unlock throttling keys on the real
      # client IP. Leave it empty and every viewer shares one bucket (any peer
      # could lock the household out of unlock with 5 tries/min). Never trust a
      # whole subnet here.
      NAZAR_TRUSTED_PROXY: "198.51.100.4"
    volumes:
      - $DATA_DIR/nazar:/data
    networks:
      - docker-direct
    healthcheck:
      test: curl -fsS http://127.0.0.1:8080/api/health || exit 1
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 20s
    labels:
      caddy: nazar.hub
      caddy.tls.issuer: internal
      caddy.tls.issuer.lifetime: 720h
      caddy.reverse_proxy: "{{upstreams 8080}}"
      hub.type: nazar
      hub.group: tools
      com.centurylinklabs.watchtower.enable: false

networks:
  docker-direct:
    external: true
```

Notes:
- WebRTC media does not flow through caddy: browsers connect directly to `198.51.100.1:8555` (the hub's WG address), which is why go2rtc's `webrtc.candidates` is templated from `NAZAR_WEBRTC_CANDIDATES`. The coturn route (`webrtc.ice_servers` -> the hub's coturn) stays documented as the fallback if a future client cannot reach the direct candidate.
- Bind mount only (hub convention); the directory is auto-provisioned, and the image's entrypoint fixes ownership itself (hence no `user:` pin, unlike forge-web).
- `docker-direct` for public egress (Qubo cloud + Wowza).
- `pull_policy: always` + watchtower off: the restart is the upgrade gate; image floats on `:latest`.
- GHCR package must be set public once (the hub pulls without credentials).

## Where secrets live

Only inside the container's volume: the Qubo session file that the server writes after login. Nothing secret in the compose or the image.

## Access

- Browser/extensions on the mesh: `https://nazar.hub` (caddy internal CA; root CA install recipe is in the hub README).
- WireGuard is the perimeter; nazar adds its own PIN gate (first-run setup in the app).

## Deploy sequence (hub-modules owns it)

1. nazar CI publishes `ghcr.io/vedhavyas/nazar:latest` (public GHCR visibility so the hub pulls without credentials).
2. hub-modules: add the compose file, run `just --yes hub-sync` then `ssh <hub-ext> "/sbin/hub sync"` on a branch, confirm `hub-services@nazar` active, merge after it works (their deploy skill).
3. Acceptance from the Studio over WG: `curl --cacert <hub root CA> https://nazar.hub/api/health`, then log in and watch one camera live.
