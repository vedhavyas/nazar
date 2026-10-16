# Nazar - design

2026-10-15. Approved in the design conversation; this document is the contract the implementation plan builds against.

## 1. What it is

A private, self-hosted replacement for the Qubo camera app, for Ved and family. One server owns the whole camera ecosystem; the family watches from a browser, a desktop app, and an Android app - all views of the same server. "Full version now": nothing is parked for later except what is genuinely unavailable or out of scope (section 10).

Success: open `nazar.hub` (or the desktop/Android app) from anywhere on the mesh, see the house live with app-speed latency, move cameras, get meaningful alerts, browse recordings - without the Qubo app.

## 2. Architecture

**Server-centric, the Forge Client model.**

- `nazar-server` (Rust) - the single owner of everything Qubo:
  - The one family Qubo account session (durable login; section 4).
  - MQTT subscription to the unit (`/monitor`, `/control`, `/config`) - the live event source.
  - Stream machinery per camera: the proven pipeline - per-session mint (5-minute sessions, automatic re-mint) feeding go2rtc, which transcodes HEVC (browsers cannot play it) to H.264 and passes audio (AAC 16 kHz mono) through.
  - Delivery to clients: **WebRTC primary**, carried over direct candidates - the hub deployment advertises the container's pinned static-network address (`NAZAR_WEBRTC_CANDIDATES`, verified peer-reachable over WG; host-port binds are not, measured 2026-10-15). The hub's coturn (`webrtc.ice_servers`) stands as the documented fallback if a client cannot reach the direct candidate. HLS remains the compatibility fallback, owned by M2's player task.
  - Recording playback: cloud-allocated VOD sessions (section 6).
  - Notification fan-out to connected clients over the client protocol.
  - The P2P engine (the extracted reference client), running in parallel and flag-gated; it becomes the fast lane and carries talkback the moment it proves itself in our own app (section 7).
- The web UI is built from the same Svelte codebase as the native shells and is served by `nazar-server` itself - **one Docker image, one container**.
- `nazar-web`/desktop/Android clients speak one small protocol to the server: HTTP for state (cameras, recordings, settings) and WebSocket for live events and stream coordination. WebRTC negotiation is proxied through the server to go2rtc.

Interfaces are deliberately transport-thin: any client that can speak the protocol (including a future CLI) is a first-class client.

## 3. Deployment (the hub)

- One image, published to GHCR from nazar's CI (public visibility - the hub pulls it without credentials; the WireGuard perimeter is the access control, and the image contains no secrets).
- Hosting: one compose file in `hub-modules` (`static/system/docker/docker-compose-nazar.yml`), following the forge-web precedent: image ref, `EXPOSE`ed port, caddy label giving it `nazar.hub` with the hub's internal TLS, a bind-mount volume `$DATA_DIR/nazar` (Qubo session, metadata cache, settings), env declared in `config.toml` `[envs]`, labels `hub.type`/`hub.group` and Watchtower disabled for our own image, network `docker-direct` (needs public egress to Qubo's cloud and Wowza).
- Access model: **WireGuard is the perimeter** (the hub's existing posture); each household device gets a WG profile. No app-level gate (see section 4).
- The same image can also run on the Studio later, unchanged, for home-LAN speed - the interface makes it a deployment choice, not an architecture change.

## 4. Sessions and login

- The server holds ONE Qubo account session for the family.
- **Durable login is the target**: the phone-OTP path is durable after all - the profile-select mint (`POST /user/emailprofile/login`, pinned 2026-10-16, `docs/qubo-findings.md`) returns a 180-day refresh token, so the server's login screen takes a phone OTP code and the session it stores refreshes itself (the earlier reading that OTP "expires hourly with no refresh" was true of the raw verify call only). The Google path (app-minted ID token, section 9) remains as built for when an app-token capture route exists.
- Client access: the WireGuard mesh IS the perimeter (the hub's model) - no app-level gate. Ved's ruling, 2026-10-15: the PIN was my defense-in-depth addition and he removed it; the perimeter, not an app lock, is the access control. Devices are identified for presence and per-device preferences.
- Login-route abuse is bounded by the perimeter, not a server throttle (decision 2026-10-16, after the gate removal took the rate limiter with it): anything that reaches the port is a mesh peer, so the OTP routes stay unthrottled by design, and that decision is recorded rather than silently inherited.

## 5. Live viewing

- Home screen: **adaptive hero** - the camera that most recently saw motion takes the large tile (full quality); the other cameras sit below it, live at low quality. Away from home the small tiles may drop to stills.
- Camera screen: live video; **drag on the video to pan/tilt** (direction overlay while dragging); privacy quick-toggle; quality selector; audio-listen toggle; client-side snapshot capture.
- Latency: WebRTC (direct WG candidates; coturn as the documented fallback) - sub-second. HLS fallback where WebRTC cannot negotiate, owned by M2's player task.
- **Warm streams, instant clients** (Ved, 2026-10-16, watching the hub): "the server is always up and getting all the data, and then passing it to the clients if there are any... as soon as a client connects to server, it should be snappy, instant." The server keeps each tracked stream's producer up between viewers, so a client attaching never waits on a fresh mint/dial. The cloud's minted URL is one-shot and its producer dies with the last consumer, so the warm state is a real consumer the SERVER holds per tracked stream, not a hope on go2rtc's entry (measured the same day).
- **Per-camera stream identity**: the cloud's wowza stream name is the requesting device id, so a server that presents one identity on every mint gets ONE stream slot. Sequential attaches can still route per-URL (which is why single-camera checks all look fine), but CONCURRENT attaches collapse: four simultaneous attaches on the shared name returned four identical frames, measured 2026-10-16 - the exact house-load pattern, and why Ved saw one camera on four tiles. Each camera's mints must carry a distinct, stable identity.
- One stream attachment, one consumer contract: the minted URL serves exactly one attach, a failed attach leaves no trace at go2rtc, and the camera's push lags seconds after a mint (measured 2026-10-16, 6-12s) - every retry is a fresh mint on a compounding backoff, never a re-dial of a URL that has been attached.
- Audio: listen works on the cloud path (AAC 16 kHz confirmed live). Talkback is in scope but gated on the fast lane (section 7).

## 6. Recordings

- Browsing: **per-camera clip lists**, grouped by day. Data: the recording calendar endpoint (`GET /content-manager/.../calendar`) plus a metadata cache the server keeps.
- Playback: a cloud-allocated VOD session - `POST/PUT /stream-manager/api/v2/sp/{sp}/stream/unit/{unit}/device/{device}?timestamp=<clip-start>&type=<quality>&singleRecord=0` returns an RTSPS URL; the same go2rtc pipeline transcodes it to the clients over WebRTC/HLS. Seeking re-mints at the new timestamp.
- Snapshot images for notifications come from the event feed's stored images where present; otherwise the server grabs a frame from the live pipeline. (There is no on-demand still endpoint on the camera - verified.)

## 7. The fast lane (P2P) and talkback

- The extracted P2P client ships inside the server as a second engine behind a flag. It is expected to inherit the relay arming once the server holds a genuine app login - the one thing the standalone client could not fake (see `docs/qubo-findings.md`, final verdict). Verification is a first-class task, not an assumption.
- When it works: live view rides the direct path (app-speed), and **talkback** (speaking through the camera) ships immediately after - it is in scope, blocked only by that proof.
- Mid-session controls (pan during a call) ride the fast lane's data channel when it is active, as the official app does.

## 8. Clients and delivery

- One Svelte 5 + Vite frontend; Tauri 2 shells for desktop (macOS first) and Android (arm64). Forge's exact delivery patterns: self-updating desktop app (Tauri updater + release manifest), self-updating sideloaded APK, web served from the server.
- Dark visual language consistent with the household's existing app and Forge's UI patterns.
- Web = full viewing, controls, recordings, notifications; no talkback (no microphone path on the web target). Desktop and Android = full parity including talkback when it lands.

## 9. Risks and open verifications (all have an owner task in the plan)

1. **Google durable login**: minting tokens with the app's extracted client id against Qubo's backend - the first experiment; OTP covers until then.
2. **VOD mint response shape**: decompile-derived, not yet live-captured - verify with one probe during the build.
3. **WebRTC through the hub's coturn**: the spike's go2rtc WebRTC config needs revisiting (its one failure was local config, not the approach).
4. **Fast-lane arming for genuine sessions**: verify once the server has a real login (section 7).
5. **Hub video route**: the hub sits between the cameras and viewers (extra hop). If live latency disappoints, the same image runs on the Studio for LAN viewers; measured, not assumed.

## 10. Out of scope for v1

- iOS (not needed).
- Our own NVR recording (the server records nothing in v1; playback is the camera's own SD content).
- Qubo's cloud AI clips (paid tier, unavailable to us) and the "save footage to phone" download feature.
- Multi-household/tenant support.

## 11. Milestones (build order)

1. **M1 - the first light**: server + web in one image, deployed to the hub; login (OTP) and ONE camera live in the browser over WebRTC.
2. **M2 - the house**: adaptive hero, all cameras, hero HQ + small-tile low quality, drag-PTZ, privacy, audio-listen.
3. **M3 - the memory**: recordings (calendar, clip lists, VOD playback) and notifications (motion, offline, AQI, snapshots).
4. **M4 - the desk**: desktop app (Tauri, self-updating).
5. **M5 - the pocket**: Android app (Tauri, self-updating).
6. **M6 - the fast lane**: P2P engine verification, then the direct path and talkback.
