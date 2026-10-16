# Qubo (Hero IoT) P2P protocol - reference

Extracted 2026-10-14 by the `qubo-p2p-chase` worker from the decompiled native libraries
(`libcore-main.so`, `libcore-stream.so`, `libcore-rest.so`, plus the jadx Java decompile of
`com.hero.iot`). Every claim carries its evidence: `ghidra_main.txt` / `ghidra_stream2.txt`
are the Ghidra C dumps in `~/Projects/qubo-spike/ghidra/` (line numbers are into those
dumps), `jadx_out` is at `~/Projects/qubo-spike/apk/jadx_out/`, and byte-level decodes cite
the file address of the constant. "decoded from immediates" means the value was assembled
from the instruction stream where the compiler inlined it (some string constants never
appear as plain literals).

Confidence marks: **[VERIFIED]** = read directly off decompiled code or a byte-accurate
decode. **[HIGH]** = strong structural evidence, one live check away. **[OPEN]** = named
uncertainty, settling experiment listed in FINDINGS.md.

---

## 1. MQTT - the cloud broker connection

### 1.1 Broker URL and TLS

- Server URI: `ssl://mqtt.platform.quboworld.com:8883`
  - host:port literal `mqtt.platform.quboworld.com:8883` - jadx
    `apk/jadx_out/sources/U5/a.java:60` (`public static String f9571y = ...`), copied into
    the initializer by `com/hero/iot/controller/StartupHelper.java` (`initializer.mqttUrl = a.f9571y`)
    and passed into native init via `initNativeModules(Initializer)`.
  - the `ssl://` scheme prefix is added natively: `setupMqttConnection` builds
    `std::string("ssl://") + <mdns/cloud address>` before handing it to the client helper
    (`ghidra_main.txt:15145-15156`, the 6-char short string decoded as `ssl://`).
  - `NetworkDiscoveryService::getMqttBrokerAddress` just returns `Constants::getCloudMqttUrl()`
    (`ghidra_main.txt:12605-12628`), i.e. the cloud URL; there is no LAN-broker fallback in
    this build.
- TLS: Paho C++ `mqtt::async_client` with `mqtt::ssl_options` whose trust store is
  `/data/data/com.hero.iot/files/cacert.pem` (`ghidra_main.txt:20231-20249`; the path string
  read as a C string at file address 0x15754b). The CA file is copied out of APK assets by
  `BaseStartupHelper` on first run.
- Protocol details: `mqtt::async_client(serverURI, clientId, maxBufferedMessages=200, persistType-string)`
  (`ghidra_main.txt:20200-20201`, decompiled signature
  `_ZN4mqtt12async_clientC1E...S9_iS9_`); connect timeout 10 s (`ghidra_main.txt:20207`,
  log line 21149 "Default connect timeout : %d").

### 1.2 clientId

```
clientId = "CLIENT_" + <deviceName> + "_" + "MainMqttClient" + "_" + <unitUUID>
```

- Built in `MqttController::createMqttConnectionMap` (`ghidra_main.txt:12991-13045`):
  `getString("deviceName")` then insert `CLIENT_`, append `_`, `MainMqttClient`, `_`, and the
  connection argument (the unit UUID, logged "creating main mqtt client with this connection
  id %s" at 13111). The `_` separator is the 1-char literal at file address 0x162dc2.
- `deviceName` = Android `Build.MANUFACTURER + "_" + Build.MODEL`
  (`StartupHelper.java`, `getDeviceName()`), stored into native config under the key
  `deviceName` by the MqttController constructor
  (`ghidra_main.txt:12718-12725`; config key at 0x59fd58 decoded byte-accurately from the
  initializer at `ghidra_main.txt` region 0x3a89d0 as "deviceName").
  - Note: the constructor's log line calls this value "unique Android ID"
    (`ghidra_main.txt:12750-12752`), but the config slot it writes is the string decoded as
    `deviceName`, and the Java caller passes `deviceName` (Build.MANUFACTURER_MODEL) into the
    same field group. Treat the log text as a copy-paste artefact. Any stable per-phone
    string works for a reimplementation - the broker does not reject unknown clientIds.
- `unitUUID` is the unit the connection belongs to; one MainMqttClient exists per unit
  (`MqttController` keeps a map keyed by unitUUID, `ghidra_main.txt:12954-12968`).

### 1.3 username / password

Set in `MqttClientHelper::connect` (`ghidra_main.txt:20870-21310`):

- username = `UserManager::getUserUUID()` (`ghidra_main.txt:21358-21371`)
- password = `UserManager::getAuthToken()` (same block)
- the app skips the connect entirely if `UserManager::isUserLoggedIn()` is false
  (`ghidra_main.txt:15361-15363`), and refuses when username or password is empty
  (`ghidra_main.txt:21190-21198`).
- The login/auth-token acquisition is the /sms login flow already documented in the spec
  (section 3.x); the token here is the session `accessToken`-class value from the user record.

So the cloud MQTT connection is:

```
server   = ssl://mqtt.platform.quboworld.com:8883
clientId = CLIENT_<deviceName>_MainMqttClient_<unitUUID>
username = <user uuid>
password = <user auth token>
TLS trust store = /data/data/com.hero.iot/files/cacert.pem (app-local; a plain system CA
                  store works for a reimplementation)
```

### 1.4 Last-will and subscriptions

- When a device is found for the unit's gateway, the client sets a will on the monitor topic
  for that device: `TopicHelper::getMonitorTopicForDevice(device, "heartbeat")` with the
  payload from `HeartbeatService::getHeartbeatMsg(...)`, QoS 2, retained=1
  (`ghidra_main.txt:21548-21715`; the service-name constant at 0x5ae068 is a 9-char string,
  and the `heartbeat` literal is loaded in the same constant block - [HIGH]).
- Subscriptions go through `MainMqttClient::sendSubscriptionRequests` →
  `MqttClientHelper::subscribeToTopics` → Paho `async_client::subscribe(topic, qos=2, ...)`
  - every topic subscribes at QoS 2 (`ghidra_main.txt:22239-22552`). Topics come verbatim
  from the registering callers (no prefix is added at subscribe time).
- Unsubscribes mirror the same list (topics no longer registered are unsubscribed).

### 1.5 Publish wrapper: srcDeviceId + sequenceId injection

Every publish through `MqttController::sendPublishMessage(topic, jsonPayload, len, unitUUID, qos, retain)`
(`ghidra_main.txt:13784-14207`) parses the payload as JSON and injects, per the log at 13986
"Updating SrcDeviceId and Sequence ID":

- `json["devices"][...]["srcDeviceId"] = UserManager::getUniqueDeviceId()` (when the payload
  has a `devices` object) or `json["srcDeviceId"] = ...` at top level otherwise
  (`ghidra_main.txt:14012-14055`; the `devices` key constant is the exported
  `DEVICES_BASE_KEY` pointer table at 0x51fe70, pointing at the decoded string `devices`;
  the `srcDeviceId` key at 0x59eec8 decodes to an 11-char string ending `eId` and matches the
  log - [HIGH])
- the second injected field is a millisecond timestamp of `system_clock::now()`
  (`ghidra_main.txt:14023`); its key constant at 0x59eee0 is a 13-char string. The log names
  it "Sequence ID"; the exact spelling is [OPEN] (candidate list in FINDINGS.md).
- characters >= 0x7f and < 0x20 are stripped from the serialized JSON before sending
  (`ghidra_main.txt:14076-14101`).
- The message then goes to the MainMqttClient for that unitUUID (`ghidra_main.txt:14157-14186`).

This wrapper is the reason outgoing app MQTT payloads carry `srcDeviceId` + a timestamp
next to the payload's own fields.

### 1.6 The SimpleMQTT client (second connection, carries `/control/...` and the camera events)

P2P/camera-command traffic (`/control/...`, section 4) and the camera's event stream go
through a second client, `hero::SimpleMQTTClient` (implemented in `libcore-rest.so`,
created by `SimpleMqttClientHelper::createConnection`). Fully decompiled:

- Broker: same cloud URL via `Constants::getCloudMqttUrl()` with the `ssl://` prefix and the
  same `/data/data/com.hero.iot/files/cacert.pem` trust store
  (`SimpleMQTTClient::setupMqttConnection`, libcore-rest 0x120730).
- clientId: `CLIENT_` + `ServerInfo.srcDeviceId` (the Android ID) + `_SimpleMQTTClient_` +
  `<unitUUID>` (`createConnection`, libcore-rest 0x129858; the separator literal at
  0x10becc decodes to `_`).
- username = `ServerInfo.userUUID`, password = `ServerInfo.accessToken`
  (`SimpleMQTTClient::connect`, libcore-rest 0x120b84; the ServerInfo offsets 0xA8/0x90 are
  anchored by `ServerInfoJni::initServerInfo`'s jfield assignments - userUUID and
  accessToken). Same credential pair as the MainMqttClient.
- Subscribes (QoS 2, `subscribe(topic, 0, 2)` in `sendSubscriptionRequests`) to
  `/monitor/<unitUUID>/<deviceUUID>/heartbeat` - built in the constructor
  (`/monitor/` + unit + `/` + device + `/heartbeat`; the `/` separator at 0x10ade1) - and
  any topic added later through `subscribe()`. **This is the channel on which the camera's
  event envelopes (section 5.4) arrive.**
- Publishes with QoS 2, retain 0 (`SimpleMqttClientHelper::publishMessage` ->
  `SimpleMQTTClient::publish(topic, payload, len, 2, 0)`).

The Java side that drives it: `U5.a.initAsyncTaskSimpleMqtt` calls
`SimpleMqttClient.d().c(unitUUID, entityUUID, deviceUUID)` for each unit
(`jadx_out U5/a.java:99`), which lands in the native JNI
`SimpleMqttClientJni::setupMqttConnection(unit, entity, device)` ->
`SimpleMqttClientHelper::createConnection(unit, entity, device)`.

---

## 2. Device access token - the cloud request

Everything below is `GatewayManager::getDeviceAccessTokenFromCloud`
(`ghidra_main.txt:2955-3754`) and its callers.

### 2.1 URL

```
POST https://<Constants::getServerUrl(op=0x44)>/unit-entity-management/api/v1/sp/<spid>/units/<unitUUID>/device/<selfDeviceUUID>/device-access-token
```

- URL builder: `GatewayManager::getDeviceAccessTokenUrlPath` (`ghidra_main.txt:784-871`)
  appends, in order: `https://`, the server-url for operation type 0x44, the literal
  `/unit-entity-management/api/v1/sp/`, `Constants::getSPID()`, `/units/`, the unit UUID,
  `/device/`, the self device UUID, `/device-access-token`. [VERIFIED]
- `Constants::getServerUrl(op=0x44)` is the cloud REST host (`srvcapp.platform.quboworld.com`
  family; the exact host for op 0x44 lives in native Constants and was not decoded - [OPEN],
  same host is expected for the whole sms/unit-entity family).
- httpMethod enum value 1 is passed for this call (`ghidra_main.txt:3344-3345`). Given the
  route is a fetch/alloc POST, 1 = POST. (Cross-check: the credentials endpoint measured
  live in the spec is POST; the same enum value 1 appears for `sendP2pStartCallCommand`'s
  /p2pCall call.)

### 2.2 Request body

```json
{"src": "<selfDeviceUUID>", "dest": "<gwUUID>"}
```

- Verified byte-accurately: the two key strings are built from immediates in the initializer
  at `ghidra_main.txt` region 0x281a24-0x281a8c (no plain literals exist for them); the
  reconstruction is `src` (size byte 0x06, chars s,r,c) and `dest` (size byte 0x08, chars
  d,e,s,t) for the globals at 0x560e50 / 0x560e68, which are exactly the two keys assigned
  in the request at `ghidra_main.txt:3277-3289`. [VERIFIED]
- `src` = the phone's own device UUID inside the unit:
  `SelfDeviceHelper::getSelfDeviceUUIDByUnitUUID(unitUUID)` (`ghidra_main.txt:3174-3176`).
  Live capture: the phone is the unit's `mobile` device, deviceUUID `d0000000-...`.
- `dest` = the target device's **controllerUUID**, not its deviceUUID. Capture proof: the
  issued tokens carry `dest` = `d0000000-...` (Cam360's controllerUUID) and `d0000000-...`
  (the purifier's controllerUUID) - the token burst covers each target device in the unit
  (`ghidra_main.txt:3284-3288`; endpoint family in section 3.5).
- Request header: `Content-Type: application/json` (`ghidra_main.txt:3290-3303`, resolved
  strings "Content-Type" / "application/json"). [VERIFIED]
- Empty src or dest aborts with the error log "deviceUUID or GatewayUUID is empty"
  (`ghidra_main.txt:3224-3226`).

### 2.3 Response

- The token is read from the response JSON under a 17-character key
  (global at 0x560e80, size byte 0x22, last char 'n' stored explicitly at offset +17 in the
  initializer at `ghidra_main.txt:281a94-0x281aa0`). The literal `deviceAccessToken`
  (17 chars, ends in 'n') exists in the library at file address 0x1537e9. [HIGH - see
  FINDINGS.md for the one-step confirmation]
- Parsing/consumption: `ghidra_main.txt:3430-3522` - non-empty token →
  `GatewayManager::findGatewayInUnit(unitUUID, gwUUID)` → `updateDeviceAccessToken(gateway,
  selfDeviceUUID, token)` (writes the DB row and the gateway's in-memory map).
- Auth header: none beyond the standard session bearer used by the REST helper.

### 2.4 When it is called

`GatewayManager::syncDeviceAccessTokenFromCloud(unitUUID, forceRefresh)`
(`ghidra_main.txt:3805-4260`): for each gateway of the unit, compute the self-device UUID;
if the token is already in the DB (and not forceRefresh) skip, else fetch and store.
Called with an empty unitUUID it sweeps all units. The per-operation gate is
`UserManager::isDeviceTokenRequired(opType)`, a bitmask that returns true for operation
types {1, 5, 6, 7, 8, 33} (`ghidra_main.txt:5900-5932`, mask `0x4000002000100e2`).
Operation type 0x44 (the token fetch itself) is not in the set.

### 2.5 Storage

Table created in the native SQLite DB (`libcore-mobile.so` schema string):

```sql
CREATE TABLE if not exists deviceAccessToken(
  _id INTEGER PRIMARY KEY AUTOINCREMENT,
  gwuuid VARCHAR(16) NOT NULL,
  deviceuuid VARCHAR(16) NOT NULL,
  unit_id VARCHAR(16) NOT NULL,
  device_access_token TEXT DEFAULT NULL,
  mqttPassword TEXT NOT NULL,
  FOREIGN KEY(unit_id) REFERENCES unit(uuid) ON DELETE CASCADE)
```

Read back with `SELECT mqttPassword FROM deviceAccessToken WHERE deviceuuid = ?`.
One row per (gateway, self-device) pair.

### 2.6 The app-device identity (what `src` / `userAppID` / `mobileUUID` all are)

Every P2P call identifies the caller by ONE value - the app's self device UUID, a **unit
device** of `deviceType: "mobile"` (its unit record: `controllerUUID: null`, name
`MobileApp_<androidId>`, own `entityUUID`; the phone's is `d0000000-...` / entity
`d0000000-...`, PROTOCOL 3.5). It is used as: the allocation's `userAppID`, the start
command's `mobileUUID`, the device-access-token's `src`, and the heartbeat's `deviceUUID`;
the MQTT clientId patterns embed the device's Android ID (`srcDeviceId`) instead.

Where that identity comes from, per the 2026-10-14 investigation (FINDINGS.md carries the
full error chain):

- **Not** `devices/self` - `POST .../units/<unit>/entities/<entity>/devices/self`
  (`ObotApi.REQ_BLEHUB_AS_DEVICE`) is the **commissioning** endpoint (its only APK callers
  are the BLE-hub/appliance screens): it needs a provisioned entity plus a catalog
  `modelNo`, and mints appliance devices. A fresh entity 404s ("Entity does not exist in
  the unit"); a valid entity without a known device 400s `ERR_DV_003`.
- **The login mints a login device**: the access token carries a `mob` claim (ours:
  `d0000000-...`); `GET /sms/api/v1/sp/<sp>/devices` lists per-user login devices as
  `{srcDeviceId, deviceAttribute}` (the phone's: `samsung|SM-F966B|q7qxins`).
- **Unit membership is separate**: a login device that is not attached to the unit gets
  `404 ERR_DC_026 "Device not found"` from the device-access-token endpoint. Attaching a
  mobile device to a unit is the one step no extracted endpoint performs for a non-app
  install (untried/unfound as of 2026-10-14).

---

## 3. mqttPassword derivation

`GatewayManager::createDeviceAccessTokenInDB` / `updateDeviceAccessTokenInDB`
(`ghidra_main.txt:2098-2544` / `1546-2095`) call
`hero::generatePasswordHash(deviceAccessToken, out)` and store the result in the
`mqttPassword` column (log "passwordHash %s").

`hero::generatePasswordHash` decompiled in full (Ghidra re-run, `_ZN4hero20generatePasswordHashEPKcPc`
at 0x286414):

```
salt12      = RAND_bytes(12)                    // OpenSSL, random per call
salt_b64    = base64(salt12)                    // 16 chars
digest      = EVP_sha256()
dk          = PKCS5_PBKDF2_HMAC(token, strlen(token), salt_b64, strlen(salt_b64),
                                iterations=901, digest, dklen=24)
mqttPassword = base64(dk)                       // 32 chars
```

So:

```
mqttPassword = base64( PBKDF2-HMAC-SHA256( password = device_access_token,
                                           salt     = base64(12 random bytes),
                                           iterations = 901,
                                           dklen    = 24 ) )
```

**[HIGH]** for the algorithm itself (decompiled instruction by instruction); **[OPEN]** for
how a broker can verify it, since the salt is random per call and is not persisted: this
value cannot be re-derived later, so it only works if the app presents it as a credential
whose accepted value was provisioned to the other side out of band, or if the other side
does not check it. Consequence for the reference client: it can *generate* a valid-format
mqttPassword with the recipe above, but the value the server/camera will accept - if any
check happens - must come from a live session. Details and the settling experiments:
FINDINGS.md.

---

## 3.5 Live-capture confirmation (2026-10-14, mqtt_probe.log)

A fresh qubopy session (user 10000000-0000-4000-8000-000000000002) connected to the cloud
broker at 16:05:13 IST - CONNECT rc=Success - with **username = userUUID, password =
accessToken** (section 1.3/1.6 confirmed on the wire), QoS-2 subscriptions on
`/monitor/<unit>/#`, `/control/<unit>/#` and `/config/<unit>`.

Correction to an earlier reading: the `{"username":"<uuid>:<digits>","password":"<b64>"}`
pair the app logs as `saveP2PCredentials` is the **P2P allocation credential** (from the
`pp2p/credentials` response), not a broker credential - the broker answers "Not authorized"
to it. Broker creds are userUUID + accessToken, exactly as the native MainMqtt/SimpleMQTT
path builds them.

Unit in the capture: `20000000-0000-4000-8000-000000000001`; camera Cam360 3MP:
deviceUUID `c0000000-0000-4000-8000-000000000001`, controllerUUID `d0000000-0000-4000-8000-000000000001`;
the phone is registered in the unit as deviceType `mobile`, deviceUUID
`d0000000-0000-4000-8000-000000000002`, name `MobileApp_<androidId>`, and its heartbeat
carries `srcDeviceId` = the Android ID `<mobile-id>`.

- `POST`/push `/config/<unit>` `action:71` = deviceAccessToken issuance: the cloud pushes
  `metadata.deviceAccessToken` (the JWT, claims `sub DEVICE_ACCESS, unit, src, iss, dest,
  user, userRole, iat`) with `metadata.src` = the self device and `metadata.dest` = one
  target device's **controllerUUID** (two tokens in the burst: camera d0000000 + purifier
  d0000000). `srcDeviceId` in the envelope = the Android ID.
- `/monitor/<unit>/<deviceUUID>/heartbeat` `action:140` = online/offline, flat JSON
  (`devices.action/deviceUUID/entityUUID/operationState/...`). Every MQTT timestamp is
  **epoch milliseconds** (`1791974162628` = 16:06:02 IST).

### 4.1 Topic anatomy

```
/control/<unitUUID>/<deviceUUID>/<serviceName>
```

Confirmed twice: in the app's own Java (`/control/ + unitUUID + "/" + deviceUUID + "/playAudio"`,
`.../deviceReboot`, `.../lockControl` - `jadx_out` call sites of `AbstractC3694g.h/d`) and in
native `M_StreamingManager::getControlTopicForService` (`ghidra_stream2.txt:8396-8489`).
The monitor/event side uses the same shape with `/monitor/` as the prefix
(`/monitor/` prefix literal in `libcore-main.so`; heartbeat will topic built by
`TopicHelper::getMonitorTopicForDevice(device, "heartbeat")`).

### 4.2 The command envelope (native, `sendCommand`)

`M_StreamingManager::sendCommand(unitUUID, ???, serviceName, actionName, argsMap, deviceUUID)`
(`ghidra_stream2.txt:8026-8393`) builds this JSON and publishes it through the SimpleMQTT
client (`SimpleMqttClientHelper::publishMessage(unitUUID, topic, payload)`, implemented in
`libcore-rest.so`):

```json
{
  "<msgSequenceId-key>": <unix-millis>,
  "<ts-key>": <unix-millis>,
  "<deviceUUID-key>": "<deviceUUID>",
  "command": {
    "devices": {
      "deviceUUID": "<deviceUUID>",
      "services": {
        "<serviceName>": {
          "commands": {
            "<actionName>": {
              "instanceId": 0,
              "parameters": { ... }
            }
          }
        }
      }
    }
  },
  "srcDeviceId": "<the app device's uuid>"
}
```

All key names resolved byte-accurately by emulating the initializer (`_INIT_4`, entry
0x1895c0 in `libcore-stream.so`; same technique for `libcore-main.so`). The key table:

| address (stream) | value | used as |
|---|---|---|
| 0x206570 | `command` | top-level envelope key |
| 0x2065a0 | `devices` | |
| 0x205070 | `deviceUUID` | (see also Java `DBSchema.EventHistory.COLUMN_DEVICE_UUID`) |
| 0x205148 | `services` | |
| 0x206588 | `commands` | |
| 0x205280 | `parameters` | |
| 0x205250 / 0x205130 / 0x205770 | `instanceId` | int 0 |
| 0x206978 | `p2pStreaming` | the P2P service name |
| 0x2069d8 | `startP2pCall` | action: start |
| 0x2069c0 | `setStreamState` | action: update |
| 0x2069f0 | `endP2pCall` | action: end |
| 0x206990 | `call` | busy-message service |
| 0x206a08 | `busyMessage` | action: busy |
| 0x2067b0 | `aesKey` | |
| 0x2067c8 | `aesIV` | |
| 0x2067e0 | `iv` | (response field name of the v3 /p2p allocation) |
| 0x2067f8 | `protocols` | value `udp` / `tcp` / `both` |
| 0x206798 | `offerData` | the local candidate blob |
| 0x206780 | `offerResponse` | the remote answer blob |
| 0x206738 | `sessionId` | |
| 0x206900 | `mediaStreams` | value `videoOnly` / `audioOnly` / `both` / `none` (0x205568-0x2055b0) |
| 0x206918 | `pttEnabled` | value `true` / `false` (0x205878 / 0x205890) |
| 0x2068e8 | `streamSourceQuality` | value `low` / `medium` / `high` / `auto` (`getVideoType`: 0,1,2,3) |
| 0x206810 / 0x206828 | `serverIP` / `serverPort` | relay endpoint (v3 /p2p response) |
| 0x206840 | `sourceType` | `live` (0x206870) / `sdcard` (0x206888) |
| 0x205758 | `appSessionId` | |
| 0x205740 | `cameraSessionId` | |
| 0x205508 | `streamControl` | the camera event/service family name |
| 0x2066a8 / 0x2066c0 / 0x2066d8 / 0x2066f0 / 0x206708 | `faceRecognized` / `callAccepted` / `incomingCall` / `endCall` / `missedCall` | event names |
| 0x206648 / 0x206660 / 0x206678 / 0x206690 / 0x2068a0 | `p2pCallStarted` / `p2pCallCancelled` / `p2pStateUpdated` / `streamStateChange` / `p2pStreamState` | event names |
| 0x2068b8 / 0x2068d0 | `streamClosed` / `streamConnected` | p2pStreamState values |
| 0x205848 | `storageMemoryFull` | event name |
| 0x2069a8 | `doorbell` | event name |
| 0x2022d0 | `srcDeviceId` | |
| 0x2022e8 | `msgSequenceId` | |

(Also decoded in libcore-main, same family: `0x59eec8` `srcDeviceId`, `0x59eee0`
`msgSequenceId`, `0x55b0b8` `unitUUID`, `0x5ae068` `heartbeat`, `0x59fd58` `deviceName`.)

### 4.3 startP2pCall (the start command)

`M_StreamingManager::sendP2pStartCallCommand` builds the `parameters` map
(`ghidra_stream2.txt:9921-10047` calls `sendCommand(unitUUID, ..., "p2pStreaming", "startP2pCall",
map, deviceUUID)`). The live capture (mqtt_probe.log, 16:06:02 IST) shows the exact wire
payload:

```json
{"command": {"devices": {"services": {"p2pStreaming": {"commands": {"startP2pCall": {
    "parameters": {
      "aesIV": "AAE35AEC1773BC4F7BBE06FD8E146450",
      "aesKey": "538781C78D2BE3BADD9F7A5B70B14092",
      "mediaStreams": "both",
      "offerData": "[{\"host\":\"192.0.0.2:56902\",\"protocol\":\"UDP\",\"srflx_0\":\"203.0.113.19:56902\",\"srflx_1\":\"13.204.183.153:56329\"}]",
      "protocols": "udp",
      "pttEnabled": false,
      "sessionId": "d0000000-0000-4000-8000-000000000004",
      "useTls": null,
      "streamSourceQuality": "high",
      "sourceType": "live",
      "playTimestamp": "",
      "mobileUUID": "d0000000-0000-4000-8000-000000000002"
    }, "instanceId": 0}}}}},
  "deviceUUID": "<camera deviceUUID>"
 },
 "timestamp": 1791974162628,
 "deviceUUID": "<camera deviceUUID>",
 "srcDeviceId": "<mobile-id>",
 "msgSequenceId": 1791974162628,
 "clientId": "<uuid4 per message>"}
```

Facts the capture pins down (all in the live payload above):

- `aesKey`/`aesIV` are **hex strings** (16 bytes) taken from the P2P allocation; the camera
  receives them here, which is how it learns the session's media keys.
- `offerData` and the camera's `offerResponse` are **strings containing a JSON array** of
  candidate dicts, each `{"host": "ip:port", "protocol": "UDP", "srflx_0": "ip:port",
  "srflx_1": ...}` - `host` is the LAN address, `srflx_N` the STUN-reflexive (public)
  addresses. The SDK emits them jsoncpp-styled (indented); any valid JSON string parses.
- `pttEnabled` is a **JSON boolean** on the wire (`false`) - note the libcore-stream
  decompile builds a "true"/"false" string; the mobile variant the app actually runs sends a
  bool. Trust the capture.
- Extra parameters the decompile elided: `useTls: null` and `playTimestamp: ""`.
- `mobileUUID` = the app's self device UUID (`d0000000-...`, the unit's `mobile` device);
  top-level `srcDeviceId` = the Android ID (`<mobile-id>`); `timestamp` =
  `msgSequenceId` = epoch-ms; `clientId` = a fresh uuid4 per message.
- Topic: `/control/<unitUUID>/<cameraDeviceUUID>/p2pStreaming`.

This is the "missing link" from Appendix B: the camera is armed by this command.

### 4.4 Other commands on the same service

- update: service `p2pStreaming`, action `setStreamState`, parameters
  `{sessionId, pttEnabled, streamSourceQuality}` (`ghidra_stream2.txt:8926-9230`;
  sent only when the session does not report p2pSdcardSupport).
- end: service `p2pStreaming`, action `endP2pCall`, parameters `{sessionId}`
  (`ghidra_stream2.txt:10050-10082`).
- busy: service `call`, action `busyMessage`, parameters `{vmsTransactionId, responseContentId}`
  (`ghidra_stream2.txt:10085-10120`; key spellings from the same emulated table - treat as
  [HIGH]).
- There is also a **P2P data-channel** variant `sendCommandFromDataChannel`
  (`ghidra_stream2.txt:8493-8923`) that sends the identical envelope through the SDK's data
  channel socket (vtable call) instead of MQTT once media is up.

### 4.5 REST fallback for the start command

If the MQTT start command fails, `startP2PStream` falls back to
`sendP2pStartCallCommand` - `POST https://<cloudHttp>/stream-manager/api/v2/sp/<sp>/unit/<unitUUID>/device/<cameraUUID>/p2pCall`
(`getP2pSignallingUrl`, `ghidra_stream2.txt:6097-6146`) with body:

```json
{
  "srcDeviceId": "<app device id>",
  "aesIV": "<session iv>",
  "aesKey": "<session aes key>",
  "mediaStreams": "videoOnly|audioOnly|both|none",
  "offerData": "<candidate blob>",
  "protocols": "udp|tcp|both",
  "pttEnabled": "true|false",
  "sessionId": "<appSessionId>",
  "sourceType": "live",
  "mobileUUID": "<self device uuid>",
  "streamSourceQuality": "low|medium|high|auto"
}
```
(`ghidra_stream2.txt:6192-6649`; method enum value 1 = POST.)

## 5. P2P session lifecycle (app side, from the stream library)

1. **SDK session**: `generateOffer` (`ghidra_stream2.txt:11480-11770`) creates a P2P SDK
   session (protocol 1=udp/2=tcp/3=both via `convertProtocol`), retries getLocalCandidate
   once. The local candidate blob is what later travels as `offerData`.
2. **Allocation**: `getP2pTokenFromCloud` - `POST https://<cloudHttp>/stream-manager/api/v3/sp/<sp>/unit/<unitUUID>/device/<cameraUUID>/p2p`
   (`getP2pUrl`, `ghidra_stream2.txt:5501-5550`) with body
   `{"userAppID": <app device id>, "appSessionId": <uuid>, "isPTT": <bool>}` (method 1 = POST),
   or the GET variant with `?isPTT=1|0` and no body (`ghidra_stream2.txt:5160-5496`).
   Response fields read: `appSessionId`, `aesKey`, `iv`, and when present `serverIP` +
   `serverPort` (the relay endpoint). This is the same field set as the v2
   `/devices/pp2p/credentials` allocation in Appendix B. **Auth correction (libcore-rest
   decompile of `StreamRestClient::createConnection(int)`):** the int is the connection
   TIMEOUT (`0x2d` = 45s), not a profile, and the connection is the standard
   user-authenticated one - headers `User-UUID`, `Subscriber-Key`, `Source-Device-Id`,
   `Login-Device-Name`, `Token-Type` (+ the libcurl User-Agent). There is no M2M/machine
   authentication on this path, and the APK ships no client cert or key (`assets/cacert.pem`
   is only the TLS trust store). `p2p_client.py` now sends the `Login-Device-Name` header
   too.
3. **Device-access preflight** (observed in the broker log, 2026-10-14): right before every
   start the app fetches a device-access token for `[self -> camera controllerUUID]`
   (section 2); the cloud pushes it back over `/config/<unit>` `action:71`, and
   `GatewayManager::isDeviceTokenRequired` gates the call. `p2p_client.py` now does this by
   default (`--no-device-token` skips it).
4. **Start**: publish `startP2pCall` (section 4.3) with the allocation's `appSessionId` +
   `aesKey`/`aesIV` and the local candidate as `offerData`; if MQTT publish fails, use the
   REST fallback (4.5). On success the session state moves to 2 and a pending timer is armed
   (`startP2PStream`, `ghidra_stream2.txt:19367-20900`).
5. **Events** arrive over the SimpleMQTT connection as a services/events envelope.
   The live capture settles the exact nesting (mqtt_probe.log, 16:06:03 IST):

   ```json
   {"devices": {"Subscriber-Key": "", "deviceUUID": "<camera>", "entityUUID": "...",
                "firebase": "false", "handleName": "<camera controllerUUID>",
                "services": {"p2pStreaming": {"events": {
                    "p2pCallStarted": {"parameters": {
                        "offerResponse": "[{\"host\":\"192.0.2.21:56839\",\"protocol\":\"UDP\",\"srflx_0\":\"203.0.113.219:56839\"}]",
                        "sessionId": "d0000000-0000-4000-8000-000000000004"
                    }} }}} },
                "srcDeviceId": "MPC_02:00:00:00:00:01", "timestamp": 1791974162976,
                "unitUUID": "...", "userUUID": "..."}
   ```

   - **Topic**: `/monitor/<unitUUID>/<cameraDeviceUUID>/p2pStreaming` (QoS 2). The answer to
     `startP2pCall` comes back as event `p2pCallStarted` under
     `devices.services.p2pStreaming.events`, with `parameters.offerResponse` (the stringified
     candidate array) and `parameters.sessionId` = the `appSessionId` sent in the start
     command - the correlation key.
   - `handleName` in the reply = the camera's **controllerUUID** (`d0000000-...`), the same id
     the device-access-token request uses as `dest`; the camera's `srcDeviceId` is
     `MPC_<MAC>`; timestamps are epoch-ms.
   - The parser (`handle_mqtt_message`, `ghidra_stream2.txt:3680-4460`) walks
     `services` -> single member -> `events` -> event name and routes: `p2pStreaming`
     events to `handleP2PEvents` (`ghidra_stream2.txt:4839-5157` - applies the remote answer
     via `setAnswer`, tears down on `streamStateChange`/`p2pMaxLifetimeError`),
     `streamControl` to the stream-error surface, `call`/`doorbell`/`storageMemoryFull` to
     notifications.
6. **Media**: after nomination the app hands the session to the SDK: `setP2pEncryptionKeys`
   (AES key/IV of the session), `setP2pTurnDetails`, stun parameters, and the SDK runs the
   relay/ICE exchange. The wire behaviour is verified in section 6 (six-packet handshake,
   probe/echo/extended-ack, then RTP) - that is the part a Python client reproduces without
   the SDK.

### 5.6 Stream status events, and the cloud's streamControl

The native layer reports upward through `streamNotification(eventId, payload)`
(`M_StreamingManager.streamNotification`, jadx `M_StreamingManager.java`):

| eventId | meaning |
|---|---|
| 75 | user access token information (`ServerInfo.saveUserAccessTokenInformation`) |
| 301 | call data |
| 307 | the P2P credential pair, JSON `{"username":"<userUUID>:<epoch>","password":"<b64>"}` -> `ServerInfo.saveP2PCredentials(username, password)` |
| 308 | `P2P_STREAM_CONNECTIVITY_TYPE` -> `setCurrentStreamStatus(str)` |
| 1292 | P2P data (app data listeners) |

- The 307 pair is TURN-shaped (`user:expiry` username; the app's logcat 2026-10-14 showed
  `<userUUID>:1794563547`, ~30 days out). It is **not** the broker credential (the broker
  answers "Not authorized" to it). No capture shows it on the wire: Qubo's STUN/TURN at
  `3.6.42.244` answers a plain binding but silently drops an unauthenticated TURN Allocate on
  5349 and 3478, so it could not be validated from outside either way.
- 308's string is the leg that actually carries the session; the UI maps
  `"3" = TURN (relay)`, `"2" = STUN (direct via srflx)`, else `LOCAL (direct host path)`
  (`LiveCameraViewFragment.onP2PPlaybackType`). Logcat from 2026-10-14 shows working,
  watched sessions reporting BOTH values: 15:55:44 -> 3 (TURN_V2); 16:06:03 -> 2 (STUN_V2,
  the session captured end-to-end in the pcaps, streamed from mobile data with no device on
  the camera's LAN); 16:48:01 -> 3 (TURN_V2). So the value names the leg chosen for that
  session, not whether a session can work - direct cross-network is proven possible, and
  the type alone does not say which leg carried media; the captures remain the ground truth
  for that.
- The cloud drives the client over `/monitor/<unit>/<selfDevice>/streamControl`, e.g.
  `{"devices":{...,"srcDeviceId":"cloud","appSessionId":"<session>","services":
  {"streamControl":{"events":{"error":{"errorMessage":"Camera stream terminated"}}}}}}`
  (mqtt_probe.log, 16:48:41 IST, for the Python client's own session). The cloud tracks
  sessions by `appSessionId` and notifies the requesting device when the stream ends.

## 6. Relay handshake and media - complete, from the 09:27 capture

Appendix B's reading was partial: the full exchange is six packets per peer, and the peer
only starts RTP after the last of them. Every frame below is from
PCAPdroid_14_Oct_09_27_43.pcap (`frame.time_relative` seconds); the same sequence runs on
both legs - the relay and the camera-direct path differ only in packet order and the
register (relay only).

### 6.1 Packet grammar (payload lengths; `244a2112` is the cookie after the 4-byte header)

Every packet is **standard STUN framing with the Qubo magic**: type(2) length(2) magic(4)
txid(12) then attributes (type 2B, length 2B, value, padded to 4). What earlier read as a
"13-byte peer name" is a **12-byte transaction id** plus the first byte of the next
attribute (`70` = high byte of attr types `0x70xx`).

| packet | shape |
|---|---|
| simple SYN | 52B `0001 0020`: 12B txid, attr `7002` (len 4, value 0), attr `0008` (len 0x14 = 20B nonce) |
| extended SYN | 60B `0001 0028`: 12B txid, attrs `0025` (len 4, 0), `7002` (len 4, 0), `0008` + nonce |
| probe | 44B `0001 0018`: the sender's own txid, attr `0008` + nonce |
| echo | 44B `0101 0018`: the request's txid echoed back, fresh nonce |
| extended-ack | 52B `0101 0020`: the extended SYN's txid echoed, attr `0025` echoed, fresh nonce |
| register (TURN connect) | 24B `2004 0004`: 12B txid, attr `7005` (len 0) - **relay only, never sent to the camera**; built by `UDPTransport::sendTurnConnectMsg` |
| bye | 24B `2003 0004`: 12B txid (one shared txid to every live flow), attr `7004` (len 0) - sent by the relay at session end |
| RTP | 12B header (V=2, PT 96 video / 97 audio, SSRC 0) + AES-encrypted payload |

### 6.2 Sequence, camera-direct leg (socket 42070 <-> 203.0.113.219:55586)

| frame | t (s) | event |
|---|---|---|
| 6293 | 39.733 | us -> cam: simple SYN, name `qWruqY1E1055p` |
| 6300 | 39.780 | cam -> us: **echo** of `qWruqY1E1055` (47ms later) |
| 6301 | 39.782 | us -> cam: **extended SYN**, name `wsK7COXU1056` - counter = the simple name's +1, fresh 8-char prefix; retransmitted byte-identical every 200ms |
| 6311 | 40.199 | cam -> us: **probe**, its own name `T4inG06T1005` |
| 6312 | 40.200 | us -> cam: `0101 0018` ack echoing its name (1ms later) |
| 6317 | 40.420 | cam -> us: **extended-ack** (echo of `wsK7COXU1056`) |
| 6318 | 40.457 | cam -> us: RTP PT 96 - media begins 37ms after the ack |

### 6.3 Sequence, relay leg (socket 52233 <-> 13.235.185.216:41473)

The app ran four sockets in parallel, each against its own relay port (41473/46508/48055/
39793) on the AWS Mumbai relay pool (13.203.x/13.204.x/13.235.x).

| frame | t (s) | event |
|---|---|---|
| 585+ | 3.80+ | us -> relay: simple SYN every 300ms, the **same name** every packet (`zTkp28PX1001p`) |
| 893 | 5.900 | us -> relay: 24B register, name `t9UfMjSi1008p` |
| 931-932 | 6.505 | next SYN, then the relay's **probe** (`0001 0018`, its own name `UGalw4sp1019`) 2ms later |
| 935 | 6.507 | us -> relay: ack echoing its name |
| 960 | 6.570 | relay -> us: **echo** of our name `zTkp28PX1001` |
| 1010-1011 | 6.771-6.824 | our extended SYN (`T1ZW7w3y1011`) -> relay's **extended-ack** |
| 1014 | 6.865 | relay -> us: RTP starts - 3.06s from the first SYN |

The relay carried the app's main media (4.3MB in 57s); the camera-direct leg carried a
second stream. So the media does not require the relay - both legs end in RTP.

### 6.4 Transaction ids (what first looked like "peer names")

Each message gets a fresh txid: 8 random alnum + a global monotonic 4-digit counter
starting in the 1000s, with a fresh prefix per generation (the SDK's
`p2p::generateStunTransactionId`). Replies echo the request's txid, exactly like STUN.
Counters seen in the capture: 1001/1003/1005/1007 (the four relay sockets' simple SYNs),
1008 (a register), 1055/1056 (the camera flow's simple/extended pair), 1011 (a relay
extended SYN).

### 6.5 Media

RTP as Appendix B described, plus: PT 97 accompanies PT 96 on both legs (small ~300B
packets, timestamp stepping +1024 - audio; `mediaStreams "both"` was requested). PT 96
video packets run ~40B-1.2KB with marker bits on the first of each burst. Payloads are
AES-encrypted with the session `aesKey`/`aesIV` from the allocation; the mode is still not
proven (CTR assumed; `p2p_client.py --aes-mode` switches). H.264 depacketizes per RFC 6184
(single NAL / STAP-A / FU-A - all three seen).

### 6.6 What the peers did NOT do

No STUN binding request ever goes to the camera or to the relay: the only STUN in either
capture is a real binding request (magic `2112a442`, transaction id = the 12-byte peer name)
from each media socket to the STUN server `3.6.42.244:5349` (09:53 frame 600:
`000100002112a442` + `jaxOgosM1002`). No 0x2004 to the camera; no keepalive from the app to
the camera after the handshake - the peer streams and the app sends nothing. The app repeats
the simple SYN with the same name until the peer answers, then stops it.

### 6.7 The data channel - commands over the relay

Once a leg is up, control commands ride it as `2001` frames instead of MQTT. Captured 09:27
while the app panned the camera (frames 1886, 2024, 2218, 2442, 3012, 3166, 3354, 3356 -
11.2s to 21.4s into the session, over the relay socket 52233). Frame 1886, byte-exact
(the earlier reading of this frame was wrong and is corrected here - what looked like an
unpinned 1-byte field is the JSON attribute's own `7001` header and 2-byte length):

```
2001 01f8 244a2112 <12B txid> 7001 01dc {476B json command envelope} 0008 0014 <20B nonce>
```

`len16` counts everything after the 20-byte header: the JSON attribute (4-byte header + JSON
+ zero-padding to the 4-byte boundary) plus the nonce attribute. So 0x01f8 = 504 = 4 + 476 +
0 + 24; frame 2218's 477-byte JSON pads by 3 (0x01fc = 508 = 4 + 477 + 3 + 24), and frame
2024's 430-byte one by 2. The 12 bytes after the cookie are a fresh txid from the same
monotonic generator as the handshake's.

The JSON is the section 4.2 envelope in plaintext (unlike RTP), e.g. frame 1886:

```json
{"command": {"devices": {"deviceUUID": "c0000000-...", "handleName": "d0000000-...",
 "services": {"panTiltControl": {"commands": {"startPan": {"instanceId": 0,
 "parameters": {"direction": "LEFT"}}}}}}}, "deviceUUID": "c0000000-...",
 "timestamp": 1791950277327}
```

Actions seen: `startPan` (LEFT/RIGHT), `stopPan`, one command per user tap. This is
`sendCommandFromDataChannel` on the SDK side - the low-latency twin of the MQTT `/control`
path, used during a live session.

### 6.8 The relay client anatomy - from the SDK's own libraries

The relay/session machinery is not in libcore-stream: it ships as its own family
(`libP2pClient.so`, `libP2pNet.so`, `libP2pMedia.so`, `libP2pCommon.so`, `libP2pUtils.so`,
`libP2pCrypto.so`). Classes: `hero::iot::P2pClient` (session manager),
`hero::iot::Session`, `hero::iot::UDPTransport`, `hero::iot::UdpSocket`,
`hero::iot::UDPStunClient`, `hero::iot::StunMessage`, `hero::iot::ConnectivityCandidate`.

- **Where the four relay ports come from: four sessions.** `P2pClient::setTurnDetails(sessionId, ip, port)`
  looks the session up and calls `Session::startConnectivityWithTurn(ip, port)`, which
  stores the endpoint on the session, appends a `ConnectivityCandidate(type=3 /*TURN*/,
  state=1, protocol=1)` to the session's candidate list, and spawns a detached connectivity
  thread. One TURN endpoint **per session**. The app's four (socket->relay-port) flows in
  both captures are four sessions, each with its own allocation (`serverIP:serverPort`),
  its own socket, and its own handshake - the app's startup fires several parallel API
  calls to the service host, consistent with one allocation per session. Only the primary
  session's TURN endpoint is advertised in `offerData` as `srflx_1`.
- **Sockets**: `Session::init(SessionParams, StunConfig, vector<SockAddr>)` creates one or
  two `UDPTransport::create(...)` per session depending on a SessionParams flag (log
  strings: "UDP single Transport Session enabled" / "UDP multiple Transport Session
  enabled"); each transport owns a `UdpSocket` (bind address from SessionParams, else
  `getInetLocalAddr`). `UDPStunClient::create` receives the bind address, the STUN server
  vector and the StunConfig strings.
- **The register is `UDPTransport::sendTurnConnectMsg`**: it builds a `StunMessage` of type
  `0x2004`, magic `0x244a2112`, a generated txid, and one string attribute `0x7005` (empty
  on the wire), then sends it to the TURN endpoint.
- **Keepalive**: `UDPTransport::startKaTimer` keeps a 52-byte SYN running through the whole
  session (the captures show ~0.3-0.7s cadence, 83 SYNs on the primary flow over 57s) and
  the SDK re-registers a flow when it nominates it. The TransportConfig carries the timing
  (divided by 300 into tick counts in `init`).
- **Nomination**: ICE-shaped - `sendReNominateMsg`, `cleanupRemoteCandidatesExceptTurn`,
  logs "Holding for turn nomination", "Same transport was nominated again, just return",
  "closeAllTransportExcept tid_except[%d] turn_transid[%d]". The SDK picks one path per
  session (direct vs TURN) and drops the rest.
- **Teardown**: the relay sends `2003 0004` + attr `7004` (len 0, one shared txid to every
  live flow) at session end.
- Consequence for this client: a single session with one allocation is structurally valid -
  the app's primary session completed the relay handshake and carried media on its own; the
  other three are extra sessions the app creates for its own stream layout.

## 7. Social (Google) login - refresh token without OTP

Extracted 2026-10-14 from `Mobile_userManager::socialLogin` (libcore-mobile.so 0x158164),
`UserManager::parseUserJson` (libcore-main), `UserManager::onLoginSuccessfull`, and the Java
caller `GetStartedFragment` (`N0("GM", googleSignInAccount.getIdToken(), "RELEASE", dto)`).

### 7.1 Request

```
POST https://srvcapp.platform.quboworld.com:443/sms/api/v4/sp/<spid>/user/login?system=GM&type=RELEASE
Content-Type: application/json
App-Id: <Constants::getAppId()>        (= the Java applicationId constant, see 7.3)
Source: ANDROID

{"accessToken": "<Google ID token>",
 "username": "",
 "password": "",
 "deviceAttribute": "<MANUFACTURER>|<MODEL>|<PRODUCT>"}
```

- **No Authorization header** - the native path creates the REST connection with
  `createConnectionWithoutAuth` (libcore-mobile 0x158164), so a fresh login needs nothing
  but the Google ID token.
- The three strings the Java passes map, in native order, to
  `socialLogin(system, accessToken, type, UserDto&)`:
  - `system` = `GM` (the Google provider marker; `CS`/`FB`/`AM` are the other member
    markers seen in the profile parser),
  - `accessToken` (body field) = the **Google ID token**
    (`googleSignInAccount.getIdToken()`, logged by the app as "Token ID : ..."),
  - `type` = `RELEASE` (the build flavour).
- `deviceAttribute` = the native config value stored under the `deviceInfo` key, which Java
  sets to `Build.MANUFACTURER + "|" + Build.MODEL + "|" + Build.PRODUCT`
  (`UserManager.getDeviceInfo()`).
- Same route family serves email login: `userEmailLogin` POSTs the same
  `/user/login?system=<system>&type=<type>` (v4, op 0x32) with a filled `username`/`password`
  and an empty `accessToken`. Phone-OTP exchange uses the sibling route
  `/sms/api/v1/sp/<sp>/user/emailprofile/login` (headers `App-Type: HOME`, `Token-Type: USER`).

### 7.2 Response - and yes, it carries a refreshToken

`UserManager::parseUserJson` reads the response body as:

```json
{"uuid": "<user uuid>",
 "accessToken": "<session access token>",
 "refreshToken": "<session refresh token>",
 "newUser": true|false,
 "name": "<display name>"}
```

- `accessToken` and `refreshToken` are stored into the user record (dto offsets 0xE0/0xF8)
  and the app logs both ("access token Returned from cloud Social Login%s", "refresh token
  from cloud %s"). `newUser` routes into the extra `verifyUserByUUID` step; a null `uuid`
  is an error.
- `onLoginSuccessfull(unitUUID, userUUID, accessToken, refreshToken, status)` then persists
  the user (`saveUserInDB`), which is where the MainMqtt/SimpleMQTT credentials come from
  (`UserManager::getUserUUID` / `getAuthToken` -> the fields these clients connect with).
- Consequence for the OTP loop: one successful social login yields a durable
  `refreshToken`; refreshing the session then uses the standard
  `/sms/api/v1/sp/<sp>/users/<uuid>/auth/refresh` route from the spec, and the hourly OTP is
  no longer needed.

### 7.3 Constants

- `App-Id`/applicationId: `934488E68332E88B1E0F9AF552840184955629777525A195949C0BE97DEF6455`
  (jadx `U5/a.java` `f9570x`; the same value the native initializer receives as
  `applicationId`).
- spid: `d10e4bfb0153496e8e8bb955f7ebe413` (same constant used by the spike tooling).

`sso_login.py` in this directory implements the exchange and writes a qubopy session file.
