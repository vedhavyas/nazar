# Nazar on the hub - the module recipe

The deployment contract for hosting nazar on Ved's hub, written from the hub's own facts (`hub-modules` repo: the docker-module system, caddy labels, env handling, the forge-web precedent). Task 10 of the M1 plan hands this to the hub-modules project's agent.

## Compose file (in hub-modules: `static/system/docker/docker-compose-nazar.yml`)

```yaml
services:
  nazar:
    image: ghcr.io/vedhavyas/nazar:latest
    pull_policy: always
    user: "101:101"
    restart: unless-stopped
    environment:
      DATA_DIR: /data
      RUST_LOG: info
      QUBO_SP: d10e4bfb0153496e8e8bb955f7ebe413
    volumes:
      - $DATA_DIR/nazar:/data
    networks:
      - docker-direct
    labels:
      caddy: nazar.hub
      caddy.tls.issuer: internal
      caddy.reverse_proxy: "{{upstreams 8080}}"
      hub.type: nazar
      hub.group: tools
      hub.exposed: "false"
      com.centurylinklabs.watchtower.enable: "false"

networks:
  docker-direct:
    external: true
```

Notes:
- `user` uid/gid must match whatever the image's entrypoint expects for `/data` (the image declares it; adjust to its uid 101 or the image's own).
- Bind mount only (hub convention), directory auto-provisioned by the pre-start hook.
- `docker-direct` for public egress (Qubo cloud + Wowza).
- `pull_policy: always` + watchtower off: the restart is the upgrade gate, per hub convention; image floats on `:latest`.
- No `[envs]` in config.toml needed beyond what the compose inlines; the SP id is public (it ships in every APK).

## Where secrets live

Only inside the container's volume: the Qubo session file that the server writes after login. Nothing secret in the compose or the image.

## Access

- Browser/extensions on the mesh: `https://nazar.hub` (caddy internal CA; root CA install recipe is in the hub README).
- WireGuard is the perimeter; nazar adds its own PIN gate (first-run setup in the app).

## Deploy sequence (hub-modules owns it)

1. nazar CI publishes `ghcr.io/vedhavyas/nazar:latest` (public GHCR visibility so the hub pulls without credentials).
2. hub-modules: add the compose file, run `just --yes hub-sync` then `ssh <hub-ext> "/sbin/hub sync"` on a branch, confirm `hub-services@nazar` active, merge after it works (their deploy skill).
3. Acceptance from the Studio over WG: `curl --cacert <hub root CA> https://nazar.hub/api/health`, then log in and watch one camera live.
