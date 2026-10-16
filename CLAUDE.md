# Nazar - project instructions

Private replacement for the Qubo camera app, for Ved and family. Cross-platform by design (Rust + Tauri 2 intent; desktop first, Android next, web pending the design conversation). Built on the reverse-engineered Qubo cloud API documented in `docs/`.

Status: designed - the approved design is `docs/design.md` (the contract the implementation plan builds against). Nothing in `src/` exists yet; M1 is server + web in one image, deployed to the hub, one camera live in the browser.

## Layout

- `docs/qubo-api-spec.md` - canonical API spec (auth, REST catalogue, MQTT, streaming, controls, P2P). Verified sections are marked; evidence statuses are honest.
- `docs/qubo-protocol.md` - the P2P protocol reference (handshake, relay anatomy, identity model).
- `docs/qubo-findings.md` - the chase record and final verdict (relay pairing is cloud-armed for genuine app sessions only).
- `reference/p2p_client.py` - reference implementation of the P2P client, kept for its protocol vectors; not product code.

## Hard facts from the research (do not re-derive)

- Login: the raw phone-OTP verify gives a 1h session with no refresh token, BUT the app's own OTP login does not stay there - its profile-select step mints a refreshToken with a 180-day expiry (measured in-app 2026-10-16, docs/qubo-findings.md). Google sign-in also returns a refreshToken (durable path). Email is display-only on phone-created accounts.
- Streaming: per-session RTSPS mint (`/stream-manager/.../v1/live/...`); the URL is one-shot per attach - after its first consumer leaves (or after any failed attach) the same URL is dead, so retries must re-mint, and the camera's push can lag seconds after a mint (measured 2026-10-16, spec section on the mint). go2rtc + ffmpeg transcode works (see the spike).
- Controls: PUT command envelope `{topic, payload:{devices:{deviceUUID, services:{<svc>:{attributes...}}}}}`; the camera ACKs everything 200 regardless - always read the shadow back.
- MQTT: username = account userUUID, password = a live accessToken; topics `/monitor/<unit>/<device>/<svc>`, `/control/...`, `/config/<unit>`.
- P2P: pairing is relay-initiated and the relay's forwarding is armed server-side for genuine app-session SDK sessions; a standalone client cannot trigger it. The real app's own sessions may inherit this - verify when built.

## Conventions

- Every commit GPG-signed (`user.signingkey 9127C198B01C47DF`, email `7549475+vedhavyas@users.noreply.github.com`).
- Brainstorm -> plan -> approve -> build. Workers build; the lead reviews every PR substantively before merge.
- No hand-rolled infrastructure where a solid tool exists (go2rtc sidecar over custom streaming glue, and so on).
- Private repo on the personal account; no AI/process mentions in any commit, PR, or doc text.
- Release tags (`git tag` + push, and the CI publish they trigger) are routine in this repo - no per-action approval (Ved, 2026-10-15, when the first tag unblocked the hub module).
- A release tag bumps the workspace version first (bump -> commit -> tag): /api/health reports CARGO_PKG_VERSION, so a tag cut without the bump reports one version behind.

