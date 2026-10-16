# FINDINGS - what is certain, what is not, and the live experiments that settle it

Companion to PROTOCOL.md. Every uncertainty below has exactly one cheap experiment (the lead
runs these with a live session). Ordered by how much they matter for getting a Python client
to a live P2P session.

## Certain (verified twice where possible)

- MQTT broker, clientId shape, username/password, QoS, TLS trust store (PROTOCOL.md 1).
  Cross-checked: decompiled code + the Java config literals + the app's own log strings,
  **and now on the wire**: username=userUUID / password=accessToken, CONNECT Success, 16:05 IST
  2026-10-14 (mqtt_probe.log, PROTOCOL.md 3.5).
- Device-access-token request: URL, method, `{"src": ..., "dest": ...}` body, and the live
  token burst: `dest` = each target device's **controllerUUID** (camera d0000000, purifier
  d0000000), pushed back as MQTT `action:71` on `/config/<unit>` with
  `metadata.deviceAccessToken` (PROTOCOL.md 3.5).
- `mqttPassword = base64(PBKDF2-HMAC-SHA256(token, salt=base64(rand12), 901, 24))` - full
  decompile of `generatePasswordHash`.
- Command topic anatomy `/control/<unit>/<device>/<service>` - native builder + Java call
  sites + the live start command agree.
- Command envelope key names and P2P service/action names - decoded by emulating the
  initializer functions of both libraries, and the live payload matches every one.
- The full startP2pCall wire shape incl. `offerData` (stringified candidate array),
  `aesKey`/`aesIV` (hex), `useTls`/`playTimestamp`, top-level `timestamp`/`deviceUUID`/
  `srcDeviceId`/`msgSequenceId`/`clientId` (PROTOCOL.md 4.3).
- The camera's reply: `/monitor/<unit>/<camera>/p2pStreaming`, event `p2pCallStarted` under
  `devices.services.p2pStreaming.events.parameters` with `offerResponse` + `sessionId`
  (PROTOCOL.md 5.4) - settles the previously [OPEN] envelope nesting.
- Correction banked: the `{"username":"<uuid>:<digits>","password":"<b64>"}` JSON the app
  saves as `saveP2PCredentials` is the **P2P allocation credential** (from the
  pp2p/credentials response), NOT broker credentials - the broker rejects it ("Not
  authorized"). Broker creds are userUUID + accessToken throughout.
- The v3 `/p2p` allocation request/response shape and the full event vocabulary
  (`p2pCallStarted`, `p2pCallCancelled`, `p2pStateUpdated`, `streamStateChange`,
  `callAccepted`, `incomingCall`, `doorbell`, `storageMemoryFull`, `offerResponse`,
  `offerData`, `sessionId`, `serverIP`, `serverPort`, `aesKey`, `aesIV`, ...) - event names
  from the decoded constant table + the live answer.
- The start command is the "missing link": `startP2pCall` on
  `/control/<unit>/<camera>/p2pStreaming` carrying the session keys + candidate, with a REST
  `/p2pCall` fallback.

## Uncertain - with the one experiment that settles each

1. **Device-access-token response key.** Structurally [HIGH] `deviceAccessToken` (17-char
   global, explicit `n` last char, key literal exists in the lib at 0x1537e9, no other
   candidate fits), but the 16 middle bytes are written through a register/stack path that
   even emulation could not reconstruct.
   *Settle:* fire the request live (PROTOCOL.md 2) and read the response body - it names the
   key directly.

2. **mqttPassword verifiability.** The salt is a fresh `RAND_bytes(12)` per call and is not
   persisted, so the stored hash can never be re-derived or matched against a capture. If the
   camera/broker verifies MQTT credentials, the accepted value must come from a live exchange.
   *Settle:* capture what the app actually sends/uses: (a) the simple MQTT connect for the
   P2P path if it is visible in logcat, or (b) replay a device-access-token call and see
   whether the relay/camera accepts anything derived from the returned token directly
   (password = token) - if the camera accepts the token itself, `mqttPassword` is local-only.

3. ~~The precise event envelope nesting~~ **SETTLED by the live capture**: the camera replies
   on `/monitor/<unit>/<camera>/p2pStreaming` with
   `devices.services.p2pStreaming.events.p2pCallStarted.parameters.{offerResponse, sessionId}`
   (mqtt_probe.log 16:06:03 IST, PROTOCOL.md 5.4).

4. ~~SimpleMQTT connect credentials~~ **SETTLED** (libcore-rest decompile, 2026-10-14):
   broker = same cloud URL, username = userUUID, password = access token
   (`ServerInfo` fields anchored via `ServerInfoJni::initServerInfo`), clientId =
   `CLIENT_<srcDeviceId/Android ID>_SimpleMQTTClient_<unitUUID>`, publish QoS 2 / retain 0.
   See PROTOCOL.md 1.6.

5. ~~Which subscribe topic(s) deliver the P2P events~~ **SETTLED**: the SimpleMQTT client
   subscribes `/monitor/<unitUUID>/<cameraUUID>/heartbeat` at QoS 2
   (`SimpleMQTTClient` constructor + `sendSubscriptionRequests`, libcore-rest). The `watch`
   mode still runs a wildcard subscription so a live call shows everything on the wire.

6. **LWT/heartbeat keys `hb1..hb3`** (10-char keys, first is `unitUUID`): the remaining two
   end in `ID` (candidate set: `deviceUUID`). Only affects the app's will message, not the
   P2P flow.
   *Settle:* logcat prints the heartbeat JSON (`getHeartbeatMsg`).

7. ~~`offerData` content~~ **SETTLED** by the live runs: the client builds it capture-exact
   (stringified candidate array, relay advertised as `srflx_1`) and the camera accepts it on
   every live run on 2026-10-14 without exception - it answers `p2pCallStarted` with its own
   candidate list each time. What stopped the flow was further on - see "The live wall".

## Social (Google) login - extraction status (PROTOCOL.md 7)

- Certain: endpoint, method, query params (`system=GM&type=RELEASE`), body fields and the
  three strings' semantics (system / Google ID token / build flavour), the no-auth
  connection, the response keys (`uuid`, `accessToken`, `refreshToken`, `newUser`, `name`),
  and that the refresh token is persisted.
- Soft spots: (1) the request must carry a Google ID token whose audience is the app's
  OAuth client - a token from any other Google app will be rejected; use the one the app
  prints on its own sign-in ("Token ID : ..." in logcat). (2) `deviceAttribute` is
  client-built (`MANUFACTURER|MODEL|PRODUCT`) and the server does not appear to validate it.
  (3) If the response ever comes back `newUser: true`, the app adds a `verifyUserByUUID`
  step for brand-new cloud users - Ved's existing account should never hit that path.
- Settling run: `python sso_login.py --id-token <logcat token>` and read the raw body it
  prints.

## The live wall - 2026-10-14 16:32-17:01 IST (the lead's GO run, 7 attempts)

**FINAL VERDICT (2026-10-15, close of the chase):** the relay pairs peers with
RELAY-INITIATED forwarding - it forwards a caller's SYN train to the peer's session port -
and that forwarding is **armed server-side per session, only for sessions created by the
genuine app session**. Our client's REST-allocated sessions are never armed: held alive
200 seconds with byte-exact trains, the relay forwarded nothing to the camera, while the
same instrument watched it forward the app's train (which the camera then refused because
its listener was already down - the corrected W4b read). **The arming gate is cloud-side:
the standalone client's ceiling.** Everything client-side is extracted, implemented, and
live-verified up to that gate - protocol, packets, ordering, auth, headers, identity,
attachment, MQTT presence, STUN operator and network each matched the app or were closed
by evidence. The genuine-session path belongs to the app build. Details: the W4b/W5
resolution sections below, chronologically after window 3's own-camera capture.

### ROOT-CAUSE CANDIDATE found before the retry: the VPN (lead's finding, 17:10)

This machine is ON the camera's LAN (en1 192.0.2.140, gateway 192.0.2.1, camera
192.0.2.21 one hop away) - but a VPN (utun0 198.51.100.15, holding `0/1` + `128/1` routes)
was carrying essentially all UDP egress. So the offerData we published advertised host
`198.51.100.15:<port>` (the utun address) and a srflx of `203.0.113.189` (the VPN exit) -
while the camera's own WAN is 203.0.113.219. The peer had no candidate of ours it could
reach the way we were actually talking to it, and the relay saw the same wrong path. That
fits every observation: echo never came, relay never answered, while the app (no VPN in the
path, host 192.0.2.x) worked.

Fix (built into p2p_client.py, verified offline): `--bind <ip>` / `--iface <name>` bind
every UDP socket (STUN included), and the default host candidate is now the default-route
interface's address - `local_ip()` returns 192.0.2.140 again instead of the utun
address. The retry, single-shot:

    venv/bin/python p2p/p2p_client.py natcheck --bind 192.0.2.140
    venv/bin/python p2p/p2p_client.py flow --allocate --heartbeat --bind 192.0.2.140 --seconds 40

Watch the echo first ([udp] SYN echo), then [udp] extended SYN acked, then [rtp]/[decode].
With the socket on the LAN, the host candidate (192.0.2.140:port) is one hop from the
camera; the srflx lookup may come back empty (the bound LAN socket cannot route to the
internet past the VPN) - then the offer is host-only, which matches how the camera reaches
us on-LAN. natcheck reports exactly what the bound path returns.

### Round 3 - the LAN bind, and what the captures say about the direct leg (18:30-18:38)

Results, in the order the lead's plan ran them:

- `natcheck --bind 192.0.2.140` -> **no reply from all three destinations** (google,
  Qubo coturn, cloudflare): a LAN-bound socket cannot reach the internet while the VPN
  holds 0/1+128/1. So the bound run's offer has no real srflx (the relay took the srflx_0
  slot).
- `natcheck` unbound -> **203.0.113.189:59059 for all three**, endpoint-independent and
  port-preserving (local port 59059 -> public 59059). Consequence: in every pre-bind run
  the SYN source on the camera path WAS the advertised srflx_0 - the "packets left from an
  unadvertised address" explanation is dead for the WAN path.
- Run I (session e1000001, 18:36), bound to 192.0.2.140: allocation 200, STUN failed as
  predicted, offer = host 192.0.2.140:60558 + srflx_0 = relay. Camera answered with its
  candidates, and its probe arrived **from its LAN address 192.0.2.21:59661** (all
  earlier probes came from its WAN srflx) - we acked 0101/0018. Then 60s of simple (0-1s)
  and extended (3-60s) SYN trains to both camera candidates from 192.0.2.140:60558.
  **Unanswered: the 0101/0018 echo of our simple SYN, and the 0101/0020 ack of our extended
  SYN.** No RTP, no decrypt path.

Re-reading both captures for the direct leg:

- ~~The camera never answers the app's LAN-path packets~~ **CORRECTED by the 20:52 capture
  (below): the camera DOES serve the LAN path - it carried the app's full video tonight.
  What the morning captures showed was one session each where the LAN-path packets went
  unanswered while the hairpin leg carried the exchange - a per-session outcome (path
  selection / listener state), wrongly generalized here into a rule.**
- In both morning captures the camera-direct flow starts 25-36 seconds after the relay
  flows (09:27: 39.7s vs 3.8s; 09:53: 26.2s vs 1.6s) - but see the 20:52 capture: there the
  direct SYN came 0.12s after the relay's probe, 5.4s into the session. The invariant is
  not a timer: **the direct attempt happens only after the relay has engaged that socket**.

### The 20:52 capture - the app working on the home WiFi (the ground truth)

Phone 192.0.2.19 on the home WiFi, camera 192.0.2.21, full video over the LAN
(5.5K frames). The sequence, byte for byte:

    t=6.8028  relay 57104 -> app  0001/0018 probe txid QXowagnR1335
    t=6.8033  app -> relay        0101/0018 ack (0.5ms)
    t=6.8157  app -> cam LAN      0001/0020 SYN txid A20XAoHV1009 (one SYN, fresh txid)
    t=6.8177  app -> cam WAN      0001/0020 SYN txid 76njl9Pf1010 (different txid, 2ms later)
    t=6.8288  app -> cam          0101/0018 ack of a camera probe (txid 1333 - packet not
                                  captured, a PCAPdroid gap)
    t=6.9121  cam -> app          0101/0018 ECHO of A20XAoHV1009 (96ms after the SYN)
    t=6.9139  cam -> app          0001/0018 its own probe txid ERD7Un7W1336
    t=6.9170  app -> cam          0101/0018 ack
    t=6.9179  app -> cam          0001/0028 extended SYN txid gGo9oYWi1011 (attrs 0025/7002)
    t=6.9365  cam -> app          0101/0020 ext-ack (19ms)
    t=6.9772  cam -> app          RTP (seq 0x16e7 first, then 1227B packets) - full video

Three findings, in order of worth:

1. **Byte shapes are identical to ours; the pattern is not.** The app sends ONE simple SYN
   per path, each with a FRESH txid, and never repeats it; we send the SAME txid to both
   paths, repeated ~4x every 250-300ms. The camera echoed the app's fresh LAN txid within
   96ms and has never echoed ours. Also: the app's LAN SYN came 5.4s into the session and
   0.12s after the relay's probe on that socket; ours fire from t~0 with no relay
   engagement.
2. **The register is the relay's trigger (proven on a third session).** Only the direct
   socket's relay flow ever registered (2004 at t=6.011, txid wKkLYCmc1008, attr 7005);
   0.79s later the relay probed it, the app acked, and 0.12s later the relay echoed the
   flow's SYN txid (vWrBTamP1001). The three never-registered flows got nothing but two
   2003 byes at session end. 2003 = per-flow close (the SDK closed the relay flow at
   t=13.9 once the direct LAN leg won) as well as session end.
3. **The invariant across all three captures: the direct SYN only happens after the relay
   has engaged that socket** (morning: relay engaged 6.5s -> direct 39.7s; tonight: relay
   probed 6.803 -> direct 6.816). Our client fires direct SYNs with no relay engagement -
   a pattern the app never produces. Client change queued: gate the camera leg on relay
   engagement, one fresh txid per path, no repeats.

The constant anomaly stays the **relay leg**: our registers are byte-exact and correctly
sourced and never draw a probe.

**Resolved (offline, 21:3x): the profile-0x2d lead was a red herring.** `StreamRestClient::createConnection(int)`
(libcore-rest decompile, 0x10c10) shows the int is the connection **timeout** (`0x2d` = 45
seconds), and the connection is the standard user-authenticated one: headers `User-UUID`,
`Subscriber-Key`, `Source-Device-Id`, `Login-Device-Name`, `Token-Type` + the libcurl
User-Agent (`createConnectionWithoutAuth` is its no-auth sibling, the social-login path).
No M2M, no machine auth; the APK ships no client cert or key (`assets/cacert.pem` is the
TLS trust store only). One real difference found and fixed: the SDK sends
`Login-Device-Name` and we did not - `p2p_client.py` now mirrors it
(`--login-device-name`, default the phone's unit device name).

**Client change landed (the surgical one):** the direct (camera) leg is now gated on relay
engagement - it fires only after the first packet from the relay on our socket, per the
app's invariant in all three captures - with one fresh txid per path, no repeat inside a
full second, the extended SYN on the echo (or 0.5s after a camera probe), repeated every
200ms up to 4x until the 0101/0020 ack. `--no-relay-gate` restores the old behaviour;
compile + selftest green.

### Verify run (run O) - the app's sequence, clean fail at the relay

New default sequence, app's own network (VPN down, real host + home-WAN srflx), token
preflight, Login-Device-Name header: allocation 200 (session e1000002, relay
13.235.185.216:59606), register byte-exact (txid eilHqtb31045) at +2.1s - and the relay
stayed silent for the whole window. The client's own framing: "[udp] relay never engaged in
10.0s - the direct leg was held for the whole window, as the app holds it". Zero camera
SYNs left the client (its probe arrived, txid BgpeWVYu1350, and was acked - the probe-ack
path is ungated, as in the app). 

**Every client-side hypothesis is now closed**: request shape, headers (including the last
one, Login-Device-Name), packets, ordering, auth and network all match the app's working
session of the same evening - and the relay engaged the app 0.8s after its register on
this same network (20:52 capture) while it has never engaged us across ~18 runs. The
remaining difference is session-side.

### Node identity - borrowed, and the registration gate (21:35-21:45)

- **Our client borrows the phone everywhere**: allocation `userAppID`, startP2pCall
  `mobileUUID`, device-access-token `src`, heartbeat `deviceUUID` = `d0000000-b1a4-...`;
  envelope `srcDeviceId` = `<mobile-id>` (the Android ID). Nothing invented.
- **How the app mints that identity**: it registers itself as a device of the unit -
  `POST /unit-entity-management/api/v1/sp/<sp>/units/<unit>/entities/<entity>/devices/self`
  (`ObotApi.REQ_BLEHUB_AS_DEVICE`, Y4.java:7032-7044), name `MobileApp_<androidId>`
  assembled natively (string in libcore-main/rest/jni-main). The result is a real,
  VISIBLE unit device entry (deviceType mobile) - exactly what d0000000 is today; no
  agent-level/invisible minting path exists.
- **MUTATION GATE fired**: registering our own app-device would add a visible entry to
  Ved's unit/account - nothing was registered; that is his call.
- **User-Agent fix**: the SDK sends exactly `libcurl-agent` (libcore-rest 0xa6c1,
  NUL-terminated); ours was aiohttp's default. Now mirrored on all REST calls.
- **Presence variant run (run P) - same wall, and it closes the family.** With the app's
  exact SimpleMQTT clientId pattern (`CLIENT_<mobile-id>_SimpleMQTTClient_<unit>`,
  connect rc=Success), the User-Agent and Login-Device-Name headers mirrored, and every
  identity field borrowed: allocation 200 (relay 13.203.165.201:33764), register
  byte-exact (txid YxsHIAH41097) at +2.1s - **relay never engaged**, direct leg held, the
  camera's probe (txid 1C6wLo3t1361) acked. 
- **CONCLUSION (final, per the lead): the register-not-armed conclusion stands.** With
  request shape, headers, packets, ordering, auth, network, UA and MQTT presence all
  matching the app's, the relay still will not arm our session. The only remaining
  difference is the **visible device registration** - the app's unit device entry
  d0000000 exists as a unit device; we borrow it instead of owning it.

### Self-registration attempted (Ved-approved) - the endpoint cannot mint an app-device

`POST .../units/<unit>/entities/<entity>/devices/self` with the app's exact headers,
four attempts:
1. minimal body -> 400 `Invalid model No, Invalid manufacturerId` (both are validated).
2. + modelNo/manufacturerId/aModelNo -> 404 `Entity does not exist in the unit`
   (entities must pre-exist - they come with physical-device provisioning).
3. entity = the phone's (d0000000, hyphenated) -> 400 `ERR_DV_003 Device doesn't exist in
   the system`.
4. same with Source-Device-Id = the phone's existing androidId -> same `ERR_DV_003`.

Reading: `devices/self` is the **commissioning** endpoint (its only APK callers are the
BLE-hub/appliance commissioning screens): it requires a provisioned entity plus a catalog
model, and it mints APPLIANCE devices - not app instances. The app's own "mobile" device
is minted on the login/sms side (the login calls carry `deviceAttribute =
MANUFACTURER|MODEL|PRODUCT`; the login-device list is `/sms/api/<v>/sp/<sp>/devices`),
which needs Ved's login and could not be exercised tonight.

**Nothing was created** (verified: the unit device list is unchanged - 7 devices, the only
"mobile" entry is the original phone).

### Our own device identity exists - the login minted it (run Q)

- Our access token's claims carry **`"mob": "d0000000-0000-4000-8000-000000000018"`** -
  the mobile device our own OTP login created. (The session file stores no device fields;
  every field in every earlier run borrowed the phone's d0000000 instead.)
- `GET /sms/api/v1/sp/<sp>/devices` with our auth: **200**; it lists two records, both
  `{srcDeviceId: <mobile-id>, deviceAttribute: samsung|SM-F966B|q7qxins}` - the
  PHONE's logins. Our own device is absent from that list.
- Run Q (our identity in every field - allocation userAppId, startP2pCall mobileUUID,
  MQTT clientId `CLIENT_d000000000004000_SimpleMQTTClient_<unit>`): allocation 200, the
  camera answered `p2pCallStarted`; **device-access-token 404 `ERR_DC_026 "Device not
  found"`** (our device is not a UNIT member; non-fatal); relay never engaged in 20s, the
  direct leg held by the gate, camera probe acked. Same wall.
- **Verdict**: our own identity exists cloud-side but is not attached to the unit - and
  the relay does not arm either way. The irreducibly-remaining unknown is unit membership
  for a mobile device (needs Ved's login from another phone, or next-session work on the
  call that attaches a mobile device to a unit).

### STUN operator thread - our STUN target was wrong (fixed 2026-10-15)

The app STUNs **Qubo's own coturn** (`3.6.42.244:5349`) once per media socket right before
its SYN trains (both pcaps, t=1.4-1.5s). Our flow STUNned `stun.l.google.com:19302`
(`--stun-server` default, used by natcheck and the flow's media socket). If their relay
pairs peers using its own STUN's view of a peer's mapping, a mapping learned from any
other server leaves the register silently unpaired - a failure mode that survives every
identity change. Fixed: the default is now `3.6.42.244:5349`; a live run tests it.

### Unit attach dig - no client-callable attach; claim path needs provisioning tokens

- The unit-claim path is `POST /commissioning/api/<v>/sp/<sp>/units/<unit>/HOME/gw` with
  `{provisionToken, macAddress, type:0}` (QR/hotspot commissioning,
  `M_GwCommissioningManager` in libcore-mobile) - that is how PHYSICAL devices join.
- No endpoint in ObotApi's device/unit constants, nor in the Java write surface, adds a
  MOBILE device to a unit; `mobile` appears only on the read side. The app learns its
  per-unit self device via `hero::SelfDeviceHelper::getSelfDeviceUUIDByUnitUUID`
  (libcore-mobile, local DB) - i.e. the server told the app its self device and the app
  cached it: the attach happened server-side.
- To close: with a fresh session, fetch the unit/user space as the app does at login and
  look for a per-user `selfDevice` field. If the server mints one for our identity, no
  attach is needed; if not, the attach is app-install-only and a second-phone login is the
  confirming test.
- Blockers from 2026-10-15: `/tmp/lc_live.txt` was wiped by a machine restart (the
  Ghidra install/project went too; the dump .txt files and jadx survive), and the saved
  session is expired - all live probes wait on a fresh OTP.

### STUN-operator verify run (run R, 2026-10-15) - coturn in the path, relay still silent

Fresh session, VPN UP (egress 203.0.113.189; utun4 owns 0/1+128/1 - logged for context).
The new STUN default works: **their coturn answered our binding from the media socket**
(srflx_0 = 203.0.113.189:57214), so the binding and the register left the same socket and
port - the consistency the operator hypothesis requires. Allocation 200 (relay
13.235.185.216:50158), register byte-exact (txid etqKrK1Q1101) at +2.1s; **relay never
engaged in 10s**; direct leg held; camera probe acked. With their own STUN in the path and
every other client-visible layer mirrored, the relay still does not arm - **the last
client-side layer is closed**. One context difference remains (our mapping is the VPN's,
not the home WAN's - the tunnel-down Ved declined).

### Attach-closing probes (read-only, same session)

- `/sms/api/.../devices/<mob>` and `/devices/<mob>/units`: 404 (no such routes);
  `/sms/api/users/<user>/units`: 404.
- `/unit-entity-management/.../units/<unit>/entities`: 200 - the entity tree: **"Root
  Entity" d0000000, type 10**, with rooms under it ("Bedroom", image
  `entityDefault_MOBILE`). Correction: d0000000 is the unit's ROOT entity, not "the
  phone's entity"; the phone's device row sits under the root.
- **`/unit-entity-management/.../units/<unit>/devices/<deviceUUID>`: 405 "GET not
  supported"** - the path exists with other methods: the one candidate attach route found
  anywhere. OPTIONS returns 500 (no discovery); no ObotApi constant uses that bare path,
  so it is a server route the app itself never calls.
- No per-user `selfDevice` field surfaced anywhere readable.
- Calling POST/PUT on that route is a mutation (adds a device to Ved's unit) - held for
  approval; body would mirror the phone's record shape (mobile / root entity /
  MobileApp_<id>).

### Attach route closed by evidence (2026-10-15, Ved-approved sweep)

Full verb sweep on `/unit-entity-management/.../units/<unit>/devices/<mob>`, errors
verbatim:

| verb | result |
|---|---|
| GET | 405 `Request method 'GET' not supported` |
| POST (minimal phone-shaped body) | 405 `Request method 'POST' not supported` |
| POST (+ modelNo/manufacturerId/aModelNo) | 405 POST |
| PUT | 405 `Request method 'PUT' not supported` |
| PATCH | 405 `Request method 'PATCH' not supported` |
| DELETE | 404 `ERR_UM_013 "Gateway doesn't exist"` |

The only supported verb is DELETE, and its domain is **gateways** - the route is
gateway-removal, not attach. With `devices/self` = commissioning (provisioned entity +
catalog model required) and the `/sms/api` device members returning 404, **no
client-callable route attaches a mobile device to the unit**. Nothing was created (unit
device list verified: 7 devices, only the phone's mobile entry).

Consistent with run P: attachment was already weakened as a gate (a perfect mimic of the
attached phone failed). The client-side surface is now exhausted end to end - protocol,
headers, auth, identity, presence, STUN operator, ordering, attachment each mirrored or
closed. The camera-side capture is the remaining lever.

### The four relay ports - SOLVED, and the protocol is STUN-framed (offline, 18:45-19:20)

The relay/session machinery lives in its own library family, never decompiled until now:
`libP2pClient.so`, `libP2pNet.so`, `libP2pMedia.so`, `libP2pCommon.so`, `libP2pUtils.so`.
Decompiling `Session::init/create`, `P2pClient::createSession/setTurnDetails`,
`Session::startConnectivityWithTurn`, `UDPTransport::init/sendTurnConnectMsg` settles both
open questions:

- **The four relay ports are four SESSIONS.** `P2pClient::setTurnDetails(sessionId, ip, port)`
  -> `Session::startConnectivityWithTurn(ip, port)`: the endpoint is stored on the session,
  a `ConnectivityCandidate(type=3 /*TURN*/, state=1, UDP)` is appended to the session's
  candidate list, and a detached connectivity thread starts. One TURN endpoint per session,
  one socket per transport, `Session::init` logging "UDP single/multiple Transport Session
  enabled" by a SessionParams flag. The app's four (socket->relay-port) flows are four
  sessions, each with its own allocation - consistent with the several parallel API calls
  at app startup. Only the primary session's endpoint is advertised as `srflx_1`. Our
  single-session flow is structurally valid; the app's PRIMARY session completed the relay
  handshake and carried media on its own.
- **Every packet is standard STUN framing** with the Qubo magic `244a2112`
  (type/len/magic/txid(12)/attrs). What read as a "13-byte peer name" is a 12-byte
  **transaction id** (8 alnum + a global 4-digit counter) plus the first attribute byte;
  replies echo the txid - which is exactly the echo/ack behaviour we chased. The "register"
  (`2004` + attr `7005` len 0) is `UDPTransport::sendTurnConnectMsg`, the TURN-path connect
  message. Session end: the relay sends `2003` + attr `7004` to every live flow. Keepalive
  = `UDPTransport::startKaTimer` (the 0.3-0.7s SYN train through the whole session).

Both corrections are folded into PROTOCOL.md (6.1 grammar, 6.4 transaction ids, new 6.8
relay-client anatomy) and the client's docstrings; wire bytes are unchanged and the
selftest is still byte-exact.

### VPN-down round (18:52-19:02) - the app's twin, and the device-access-token gap

Both prepped runs, plus a find in the broker log and two follow-ups (J-M):

- **Run J** (plain, VPN down): allocation e1000003, relay 13.203.165.201:50567. Offer now
  exactly the app's shape - host 192.0.2.140:49821, **srflx_0 203.0.113.219:49821 (the
  real home WAN, the camera's own public)**, srflx_1 the relay. Camera answered, its probe
  arrived and was acked; relay silent; no echo; no media. (Prepped command = default 10s
  UDP window.)
- **Run K** (relay-first 20, UDP window extended to 75s - the prepped 10s default would
  clip the relay-only phase): relay 13.235.140.180:45450. The relay leg ran its full
  sequence alone for 20s -> no reply. Direct candidates joined -> probe acked, no echo,
  no extended-ack, 0 RTP.
- **The device-access-token gap (from the broker log):** the app fetches one before EVERY
  session - the two /config action:71 pushes at 16:05:59 precede its 16:06:02 start, and
  the 16:47:58 burst precedes its 16:48:01 start; `GatewayManager::isDeviceTokenRequired`
  gates it. Now built into the flow (default on): `POST units/<unit>/device/<self>/
  device-access-token` with `{"src": self, "dest": <camera controllerUUID>}` before
  startP2pCall; `--no-device-token` skips.
- **Runs L and M (with the token, 45s window on M):** token acquired 200 on both. The
  camera now probes on BOTH paths (LAN 192.0.2.21:port and WAN 203.0.113.219:port, two
  different txids) - we ack both. Still: no 0101 success to our simple SYN's txid, no
  extended-ack, relay never answers the register, 0 RTP.

So the token was a genuine missing step, now closed, but not the gate. Every observable
wire artifact now matches the app's captures byte for byte, on the app's own network (same
NAT, camera one hop away), with the app's MQTT preflight - and the camera still will not
send the two replies it gave the app, while sending its own requests at us on both paths.

### Two-session variant - run, eliminated (19:05)

One bounded run (`--sessions 2 --relay-first 10`, VPN down, token on): two allocations
(relay ports 38103/47010), two sockets, two staggered start calls - and the camera
answered BOTH, opening a separate per-session UDP listener for each (192.0.2.21:60524,
:52863) with its probe txids advancing 1311 -> 1318 across the two legs. Both legs ran
the full sequence; both relays silent; both probes acked; neither session ever got its
0101 echo or extended-ack; 0 RTP; both torn down cleanly. The app's four-session layout is
fully explained (four sessions - two served media in the captures, two were spare/never
registered) and ruled out as the gate.

### Observing the camera/relay side - ranked options (read-only throughout)

**UPDATE 2026-10-15: the LAN-interception route is BUILT** as `p2p/mitm_capture.py` -
scoped ARP interception for the camera only (two unicast poisons, automatic restore,
forwarding toggled for the window, pcap written). Selftest green; not run, pending Ved's
blessing. It is now the practical first option (no router shell, no cable, no monitor
mode); one window captures the app's session AND ours for a single diff.

**First window run (11:34-11:39, run T at 11:33:41 and run U at 11:35:06 inside it):**
the pcap holds 290 ARP frames - all of them the tool's own poisons (145 to the camera,
145 to the gateway, every ~2s for the full window) - and **zero frames from the camera**:
no UDP, no TCP, no conversations. The poison did not take (static ARP on the camera, or
its stack discards unsolicited replies). Run U also got NO `p2pCallStarted` for the first
time in ~20 runs - consistent with the window briefly disrupting the camera (inferred,
not proven: nothing from the camera was captured). Camera IP note: it moved .21 -> .22
during the day's first window; the tool now resolves the camera by MAC (`--target auto`,
default) so lease moves cannot point it at a dead IP.

**Tool v2** (built after that window): solicited poisoning - it answers the camera's own
broadcast who-has-gateway requests immediately (wins the race; IoT stacks often accept
only solicited replies), capture includes broadcast, MAC-based auto target, and an
exit acceptance check that shouts when zero camera frames arrived.

**Second window (11:42:14-11:47:14, run V inside) - MISCONFIGURED, not closed (corrected
11:5x after the MAC discovery).** 415 frames: 353 ARP poisons plus LAN noise now visible
through the broadcast filter. Zero frames from the camera MAC again - and the root cause
of BOTH windows' zero-frames AND both mutes turned out to be one bug: **the Studio's WiFi
uses a randomized private MAC on the wire - `02:00:00:00:00:02` (the ARP cache's
self-entry for 192.0.2.140, and the same MAC on the capture's own broadcasts) - while
the tool used the HARDWARE MAC from networksetup (`02:00:00:00:00:03`) in every poison and
in the capture filter.** Every poison therefore claimed ".22/gateway is at
1c:1d:d3..." - a MAC nothing receives on: the gateway (and the camera) redirected to a
dead address, the AP dropped the traffic (the camera's downlink starved = the mute,
bounded exactly by the windows; in window 1 the camera-side poison alone explains its mute
despite the dead .21 target - its UPLINK died), and nothing reached our NIC (zero frames;
our sniffer also watched the wrong MAC). The camera DID accept the redirect - the mutes
are the proof; the "static ARP" reading is withdrawn.

**Fixed:** `our_mac()` now takes the ARP cache's self-entry - the LAN's view - and the
selftest shows `02:00:00:00:00:02` in every packet. The tool is ready for a third window.
Even if same-interface forwarding still cannot re-emit (the one remaining unknown, which
is why the camera may mute again), the CAPTURE gets the camera's uplink - its relay
register included - before any forwarding question matters; capture-only alone answers
the brief. Remaining session-side fallbacks: AP-side capture or Wi-Fi monitor mode.

### Window 3 (12:0x) - THE ANSWER: the camera never joins the relay for our sessions

With the wire MAC the interception worked completely. The camera's entire conversation
transited the Studio - 499 camera-involving frames, uplink AND downlink (its TLS to AWS
3.108.97.1 / 15.252.210.196 / 15.206.60.180:443, its MQTT to 65.1.150.122:8883, DNS, NTP,
ICMP) - and **macOS forwarding re-emitted both directions fine** (re-emission frames
present in the capture), so the camera stayed healthy the whole window: both "walls" from
the earlier closure are dead (it takes the poison; same-interface forwarding works).

**The headline: the camera sent NOTHING to any relay for our entire session.** Zero UDP
to our session's relay (13.204.183.153:41744) or to any relay endpoint; the only 13.x
contact is NTP (13.126.27.131) for clock sync. Its cloud TLS/MQTT were active; it
answered our `p2pCallStarted`; and it never opened the relay leg. **The relay wall is
camera-side, proven by the camera's own bytes: for our sessions the other peer simply
never joins the relay, so there is nothing to pair.** (Its LAN probe to us missed this
capture by timing - it fired before the sniffer started; the MQTT answer is present.)

The completing diff: in the app's 20:52 session the camera DID use the relay (the relay
carried its media to the phone), so the camera relay-registers for the app and not for
us. One window with the phone running a real session under this same interception now
captures the camera's side of a WORKING session - which relay, when, and what precedes
its join - and the instrument is proven.

### W4a RESULT (12:05, window 4): NO - identity is not the trigger

The borrowed-identity run (d0000000 in all five fields, app closed) with the interception
live: **zero relay-bound UDP from the camera** - not one frame to 13.x. Its uplink was the
window-3 shape exactly (AWS TLS 443, MQTT 65.1.150.122:8883, DNS, ICMP). So the camera
does not dial the relay for the nearest-to-app identity we can present; **the trigger is
not peer identity -> a cloud-side push, and W4b (one real app view) decides it.**

Bonus, all from the camera's own bytes this window: its probe train is fully visible - it
repeats `0001/0018` probes (txid `wjdcj8GG1052`, matching the run's own log) every ~500ms
while our session runs, with a second concurrent txid (`34iHyl2bb1053`); forwarding
re-emitted everything both ways correctly. The failed half of the handshake is now on
tape in full duplex. Broker-side during the run: the camera published only its
`p2pCallStarted` answer; 23s after our teardown its heartbeat blipped offline->online
(an MQTT reconnect) with the cloud's `Camera stream terminated` notice for our session in
between.

### W4b checklist (needs one real app view; phone on <ssid>, interception live, broker log on)

Read-and-done list for the window; T0 = the app's `startP2pCall` timestamp (from the
broker log):
1. Camera uplink, new flows around T0: any UDP to a 13.x relay (IP:port), timed T0+delta.
   Compare with window 3's zero and with the app's own advertised relay (`srflx_1`).
2. The camera's register if present: `2004 0004` frame bytes (txid, attr 7005) - capture
   verbatim; note the relay IP:port it targets.
3. Any NEW TCP/TLS connection from the camera at the same moment (a push channel?).
4. The camera's MQTT publishes (broker log + our subscription) around T0: anything naming
   relay/session.
5. Note: the PHONE's relay flows do NOT transit the interception (only the camera's ARP
   entries are poisoned; the phone goes AP->gateway directly) - its side comes from the
   broker log and the archived phone pcaps. The camera's side is what this window adds.

### W4b RESOLUTION (12:15-12:20) - corrected: the relay forwards the APP's train; the camera refuses it

- Broker log: T0a 12:15:09 and T0b 12:16:05 (same sessionId reused), camera answered both;
  the app's offer advertised srflx_1 = `13.203.165.201:46243`.
- The capture: 12:15:57-12:16:02, eleven ICMP "Port unreachable" from the camera TO
  13.203.165.201. Each embeds the packet that caused it: **src relay:46243 -> dst
  camera:55427, carrying 52-byte SYNs with the APP's txids (1030-1036)**. CORRECTED
  READING (an interim report called this "the camera dials the relay" - wrong): the
  RELAY was forwarding the app's own SYN train to the camera's session port, and the
  camera refused each one - its listener for that session was already closed (the app's
  session was 48s old and failing on weak cellular).
- The camera itself dialed nothing: for the app's session too, zero UDP from it to any
  relay.

### W5 RESOLUTION (12:22-12:29): a 200-second hold - the relay never forwards OUR train

- Runs: e1000004 (relay 13.204.183.153:44494, held 200s) and e1000005 (relay
  13.235.140.180:41797, 60s) - trains and register live throughout, no teardown. The
  camera answered neither; its bytes show why: it was RE-ESTABLISHING ITS CLOUD through
  the window (fresh DNS for srvc.platform.quboworld.com at +8s and
  mqtt.platform.quboworld.com at +34s), the mute pattern again, now timestamped.
- **Zero frames between our relay IPs and the camera, in either direction - no forward,
  no refusal.** Contrast the app, where the relay forwarded its train within the same
  instrument's view. **The forwarding is armed server-side per session and is never
  armed for REST-allocated client sessions, even held 200 seconds with byte-exact
  trains. That is the standalone client's ceiling.** Pairing is relay-initiated; the
  arming gate is cloud-side, beyond anything the client can present - protocol, packets,
  identity, attachment, timing and network are all matched or closed.

1. **Router-side capture - the decisive view, needs Ved's router** (gateway 192.0.2.1,
   MAC 02:00:00:00:00:04; camera 02:00:00:00:00:01, matching the MPC_ srcDeviceId). If the
   firmware has a shell: `ssh root@192.0.2.1 "tcpdump -i br-lan -U -w - host
   192.0.2.21" > /tmp/cam.pcap`, run during one of our flow runs. Answers: does the
   camera register with the relay at all (2004 to 13.x), does the relay answer it, which
   relay IP:port it uses vs our advertised srflx_1, its own SYN train, and whether it ever
   sends toward our advertised candidates. One capture, app session vs ours, is the whole
   unknown as a byte diff. No shell on the firmware -> option 4.
2. **Wi-Fi monitor-mode capture** - passive fallback for 1; needs the camera on Wi-Fi,
   Ved's passphrase, a second Mac in monitor mode and WPA decryption; flakier.
3. **LAN port survey** - `nmap -sT -Pn -p 80,443,554,8000,8080,8443,10554 192.0.2.21`,
   plain connect scans only, Ved's OK; low expected payoff for this question.
4. **Managed switch mirror** - definitive, needs hardware and re-cabling; when convenient.
5. **This Mac's interface** - sees only traffic to/from this host: our trains out, the
   camera's probes back, any relay reply to US; cannot see the camera's relay leg, its
   registration or the relay's traffic to it. A formality now.

Checked and dropped: `systemDiagnosis` (jadx) is storage/SD diagnostics
(`availableExternalStorage` / `notSupported` / `readOnly` / `noSpaceAvailable`) - no
network or P2P state. The camera's own MQTT broadcasts are already fully observed via
`/monitor/<unit>/#`; even in the app's working sessions it published nothing beyond its
`p2pCallStarted` answer.

### What works, every single run

- Session load, v3 `/p2p` allocation (v2 too), STUN srflx discovery, online heartbeat,
  MQTT connect (rc=Success), startP2pCall publish - all clean.
- The camera answers `p2pCallStarted` on every run with its candidate list, e.g.
  `[{'host': '192.0.2.21:47881', 'srflx_0': '203.0.113.219:47881'}]`.
- UDP: the camera's own probe packet (below, step 4) reached us in 3 of 7 runs and we ack
  it exactly as the app does (`[udp] probe from 203.0.113.219:x peer=b'...' -> 0101 ack`).
  Two-way UDP with the camera from this network is proven.

### The complete handshake, byte-exact from the 09:27 capture (camera-direct flow)

| frame | t (s) | dir | packet |
|---|---|---|---|
| 6293 | 39.733 | we→cam | 52B `0001 0020` simple SYN: 13B name `qWruqY1E1055p`, TLV `02 0004 00000000`, 20B nonce |
| 6300 | 39.780 | cam→us | 44B `0101 0018` **echo** of our 12B name + fresh nonce (47ms) |
| 6301 | 39.782 | we→cam | 60B `0001 0028` **extended SYN**: NEW 12B name `wsK7COXU1056` (counter +1, no trailing `p`), TLVs `0025 0004 0` + `7002 0004 0`, retransmitted byte-identical every 200ms |
| 6311 | 40.199 | cam→us | 44B `0001 0018` the camera's own probe, its own 12B name `T4inG06T1005` |
| 6312 | 40.200 | we→cam | 44B `0101 0018` ack echoing its name (1ms later) |
| 6317 | 40.420 | cam→us | 52B `0101 0020` **ack of the extended SYN** (echo name + TLV 0025) |
| 6318 | 40.457 | cam→us | RTP PT 96 starts - 37ms after |

Relay leg (socket 52233 ↔ 13.235.185.216:41473), same shape, different order:

| frame | t (s) | event |
|---|---|---|
| 585+ | 3.80+ | 52B simple SYN every 300ms, the SAME name every packet (`zTkp28PX1001p`) |
| 893 | 5.900 | 24B `2004 0004` register, name `t9UfMjSi1008p` (register goes to the **relay only**, never to the camera) |
| 932 | 6.507 | relay's probe `0001 0018` (own name `UGalw4sp1019`) |
| 935 | 6.507 | our ack `0101 0018` |
| 960 | 6.570 | relay's **echo** `0101 0018` of OUR name |
| 1010/1011 | 6.771/6.824 | our extended SYN → relay's `0101 0020` ack |
| 1014 | 6.865 | RTP starts |

Corrections to the earlier reading: the simple SYN name is 13B ending `p`; the extended SYN
name is 12B (no `p`) and its counter is the simple one +1 with a fresh prefix; the 44B
"answer" in Appendix B is the peer's **probe** (0001) and its **echo** (0101), two different
packets; the 0101/0020 is the extended-SYN ack. All now implemented in p2p_client.py
(selftest holds the extended-SYN packet against frame 6301 byte-for-byte).

### Where it stops

Run ledger (A-G; A predates the extended SYN):

| run | alloc | camera answer | cam probe→ack | cam echo `0101/0018` | ext-SYN ack `0101/0020` | relay reply | RTP |
|---|---|---|---|---|---|---|---|
| A 16:32 | v2 | yes | yes | no | (not built yet) | no | 0 |
| B/C 16:36-38 | v3 | yes | no | no | no | no | 0 |
| D 16:44 | v3 | yes | yes | no | no | no | 0 |
| E 16:47 | v3 | yes | no | no | no | no | 0 |
| F 16:49 | v3 | yes | yes | no | no | no | 0 |
| G 16:50 | v3 | yes | no | no | no | no | 0 |
| H 17:01* | v3 | yes | no | no | no | no | 0 |
| I 18:36** | v3 (LAN-bound) | yes | yes, from its LAN ip | no | no | no | 0 |

Times are IST, taken from each run's own heartbeat epoch-ms (e.g. run E's 1791976610622 =
16:46:49). The first pass at this ledger was stamped ~13-29 minutes late; corrected here.

\* H is the relay-focused attempt (round 2): the client's relay-leg bytes are now
faithful to the capture - the one deviation found was a STUN binding built with the Qubo
cookie `244a2112` instead of the real magic `2112a442` as the FIRST packet sent to the
relay; the captures send no STUN to the relay at all (the only STUN goes to the STUN server
3.6.42.244:5349). Removed. Run H: relay still silent.

\*\* I is the round-3 attempt, bound to 192.0.2.140: the camera engaged over the LAN for
the first time (its probe came from 192.0.2.21:59661), but the echo and the extended-ack
never came. See "Round 3" under the VPN section.

Round-2 context, corrected: the connectivity type is pinned (`streamNotification` event 308
-> `3 = TURN (relay)`, `2 = STUN (direct via srflx)`, else `LOCAL`;
LiveCameraViewFragment), but logcat shows working, watched sessions reporting BOTH values:
15:55:44 -> 3, **16:06:03 -> 2** (the session captured end-to-end in the pcaps - direct
cross-network from mobile data, nobody on the camera's LAN), 16:48:01 -> 3. So the direct
camera echo is NOT a LAN-only path: direct across networks works for the app, and the type
names the leg chosen rather than whether a session can work. Both legs stay open questions
for this client: the relay never replies to us, and the direct echo is the first thing we
cannot elicit. The anti-spoofing/egress-source check below is the top candidate for the
latter.

More from the captures this round: the relay carries mid-session control commands as
`2001` frames (PROTOCOL.md 6.7 - pan/tilt with the plaintext command envelope); the 307
credential pair in the app's logcat is TURN-shaped (`user:expiry` + b64) and is NOT used on
the wire in either capture - Qubo's STUN/TURN (3.6.42.244) answers a plain binding but
silently drops an unauthenticated TURN Allocate on 5349 and 3478, so the pair could not be
validated from outside; the cloud tracks sessions by appSessionId and sends
`streamControl.events.error "Camera stream terminated"` on /monitor/<unit>/<selfDevice>
(seen for the Python client's own session ~110s after it started, i.e. after teardown);
the two non-standard-port TCP flows in the 09:27 capture (203.0.113.129:10445,
203.0.113.197:10446) are TLS, not P2P.

The wall is step 2: **the peer's `0101 0018` echo of our simple SYN never arrives, and
without it nothing downstream moves** (in the capture every later step is causally after it).
The relay never replying at all - not to the register, not to 2+ seconds of SYN trains -
is the second anomaly. A-G rule out: allocation path (v2 == v3: same wall; the earlier
"v2 answered" reading was a red herring - those runs simply happened to catch the probe),
name content (G replayed the capture's literal `qWruqY1E1055p` name - silence), timing
variants (ext SYN from t=0, from +3s, and echo-gated all tried), and packet shapes
(byte-identical to the capture per selftest).

### What would settle it (re-aimed after the 16:06 STUN_V2 finding)

The top hypothesis for the missing echo is anti-spoofing: the peer answers only packets
that arrive from a source it was told about in `offerData`, and our advertised srflx may
not be the mapping our SYNs actually leave from (one NAT mapping was measured - against a
STUN server - while the SYNs go to the camera and relay; NAT mappings can differ per
destination). It fits every observation: the app's packets left from what it advertised,
ours never drew a reply. In order:

1. **Egress-source check.** `natcheck` (built into p2p_client.py, parked - not yet run)
   STUNs one socket against three destinations (google, Qubo's coturn 3.6.42.244:5349,
   cloudflare) and prints each mapped port; differing ports = endpoint-dependent NAT = the
   advertised srflx is wrong for the camera, which alone would explain the silence.
   `venv/bin/python p2p/p2p_client.py natcheck`. Alongside it, tcpdump the local leg during
   a `flow` run to confirm local egress matches the offered host candidate and that framing
   and cadence match the capture.
   Session-id continuity is already settled (offline): the allocation's appSessionId is
   what startP2pCall carries and what the camera echoes back - the answer parser requires
   that exact equality and every run matched.
2. **Run this client from the camera's own LAN** (queued with the lead): with a host
   candidate the camera can reach, the anti-spoofing hypothesis predicts the echo appears
   even if the NAT mapping question is unresolved.
3. **Capture the camera side during a working app session** (a capture on the camera's LAN
   - phone-side PCAPdroid sees only the phone): shows what address the camera answers FROM
   and whether it ever registers with our relay endpoint.

## p2p_client.py - readiness (2026-10-14 evening, post-live-runs)

- `selftest` ALL OK: simple SYN vs Appendix B, **extended SYN vs capture frame 6301
  byte-for-byte**, startP2pCall payload key-for-key vs the live capture, event parser on the
  captured camera answer, FU-A reassembly.
- The flow command now speaks the full capture handshake: simple SYN (13B name,
  monotonic counter, fresh prefix per run) → 0101 echo watch → extended SYN (12B name,
  counter+1) → probe ack (0101/0018) → extended ack (0101/0020) → media. The 0x2004
  register goes to the relay only, exactly as the capture does.
- Relay leg is now byte-faithful to the capture: SYN train (same name, ~250ms) → 0x2004 at
  +2.1s → extended SYN at +3.0s; **no STUN to the relay** - the earlier malformed STUN
  (Qubo cookie as the STUN magic) was the one wire-level deviation found in round 2 and is
  gone. The round-2 relay-focused run (H) stayed silent regardless, so the relay's silence
  is now a session-pairing question, not a packet-content one.
- Live-proven up to and including the camera's probe + our ack; everything past step 2 of
  the handshake has never been elicited from this network (see the wall section).
- `end` command sends endP2pCall for any session ids; `flow` tears its own session down
  every run (the camera starts refusing new calls otherwise - libcore-stream strings say so
  and Run C confirmed it).
- `natcheck`: one socket STUNned against three destinations (google, Qubo's coturn,
  cloudflare), printing the mapped port each returns. Round-3 results: unbound (VPN path)
  -> 203.0.113.189:59059 for all three (endpoint-independent, port-preserving); bound to
  192.0.2.140 -> no reply from all three (LAN source cannot traverse the routed VPN).

## Notes for the lead's live runs

- The client (p2p_client.py) takes the session + camera + unit from qubopy, connects MQTT,
  allocates via the v3 `/p2p` endpoint (v2 fallback available), publishes the start command,
  runs the full UDP handshake, then decrypts RTP with aesKey/iv (`--aes-mode ctr-cont`
  default; `ctr-reset`/`cfb`/`ofb`/`none` alternatives) and decodes with PyAV
  (`/tmp/qubo_first_frame.png`).
- Everything the client does is reversible/local except the MQTT publish and the two REST
  calls - all of which the app itself does on every call. Every run ends with an
  endP2pCall teardown.

## 2026-10-16 run: the hub's genuine login (M6 Task 2 pre-check)

Ran the reference flow against the hub's own OTP login (fresh that morning, from the server's session file) - the first arm attempt from a session minted by OUR server rather than the CLI's own login.

- Allocation v3: HTTP 200 (relay 13.203.165.201:40552), device-access-token 200, STUN via Qubo's coturn, candidates as the capture shapes them.
- The camera answered: `p2pCallStarted` in about a second; its direct probe reached the caller and was acked (`0101`) - the camera's own UDP path works live on this LAN.
- The relay never engaged in 10.0s despite a byte-faithful register; endP2pCall torn down cleanly.

Read: the relay gate is still shut for our session class, even freshly minted by the server. The camera-side half demonstrably works up to the relay wait. The kill-switch decision (M6 Task 2 Step 3) runs with the ported Rust engine and the server's own arm path before any conclusion - this run is a strong early signal, not the verdict.

### 2026-10-16, later: the ported engine's run - THE VERDICT

The ported Rust engine (byte-vector-reviewed) ran against a second fresh hub login, with the SDK's SYN beat held for the whole window (`--hold-beat`). Same shape one layer down: allocation 200 (relay 13.204.183.153:48532), token 200, candidates offered, `startP2pCall` published, the camera answered `p2pCallStarted` in ~3s with its candidates - then `Handshake { timeout: 10s, relay_engaged: false }`. The beat did not move it.

**Conclusion, three independent runs deep:** the relay pairs only sessions its own side recognises as the official SDK's - a server-side arming policy nothing reachable from outside the app reproduces, beating or no beating. The fast lane is closed for this client class; the cloud path stands as the design's primary and fully carries live view.
