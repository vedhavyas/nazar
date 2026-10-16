# Qubo cloud API - spec derived from the qubopy library

Date: 2026-10-13
Status: draft, reviewed, PARTIALLY LIVE-VERIFIED (updated 2026-10-14). Input to the Rust + Tauri desktop client build (one app per person, connecting straight to Qubo's cloud). Sections 3.3, 4 (E10/E12/E16), 5, 6 and Appendix B carry measured corrections from live sessions; the rest is still source-derived. Section 9 tracks what remains.

## 1. Purpose, method, and honesty statement

### Purpose

This spec describes the Qubo (Hero Electronix, India) smart-home cloud API as far as it can be reconstructed from `qubopy`, a community reverse-engineering library. It is the input for a Rust + Tauri desktop client that should list devices, watch live camera feeds, and reach as much of what the official app does as the API allows. Every claim below carries an evidence status so the build knows what to trust and what to verify first.

### Method

- Source read in full: `qubopy` version 0.1.2 (PyPI wheel, MIT, author Selva Kumaran K / @selvakk2k), installed at `/tmp/qubopy-proof/venv/lib/python3.14/site-packages/qubopy/`. All 17 Python files were read end to end (3,063 lines): `__init__.py`, `__main__.py`, `auth.py`, `client.py`, `cli.py`, `const.py`, `exceptions.py`, `models.py`, and `devices/{__init__,base,camera,dashcam,light,lock,plug,purifier,sensor}.py`.
- The README is the wheel's `METADATA` file (`qubopy-0.1.2.dist-info/METADATA`). All README quotes below are from it.
- The wheel contains no tests, fixtures, or captured traffic (confirmed against `RECORD`): the library code and README are the whole artifact.
- No network calls were made to Qubo. No account session exists yet. Nothing here is verified against the live service.

### What this is NOT

- Not a decompilation of the Qubo mobile app. `qubopy` is a third-party artifact; where its comments claim APK provenance ("modeled from APK application metadata", "extracted from native C++ binaries (libcore-rest.so)"), that provenance is the library author's claim, unverifiable from the wheel.
- The README's "hardware verified" labels describe the author's own testing. No evidence of that testing ships in the wheel, so those labels are treated here as README claims, cited as such, not as independently confirmed facts. Where a README claim describes something the code does not implement (LAN P2P decryption, BLE control, MQTT), the disagreement is flagged in section 8.

### Evidence statuses

- VERIFIED-HW: the README marks this flow or family as hardware verified, and the code path implementing it exists in the wheel. The basis is named for each assignment.
- MODELED: present and callable in code, but no README verification claim. The README itself labels several families "API Modeled / Untested", "modeled from APK application metadata".
- INFERRED: structure derived from code alone (a path constant with no call site, response field aliases guessed by defensive parsing). Never presented as fact.

Modifier: UNUSED-IN-CODE means the library defines it but nothing calls it (verified by grep over the package: no call site outside its own definition).

### Citation convention

`qubopy/<file>:<line>` refers to the files under the package root above. Line numbers are from qubopy 0.1.2 exactly as shipped.

## 2. Infrastructure

### REST gateway

| Item | Value | Source | Status |
|---|---|---|---|
| Base URL | `https://srvcapp.platform.quboworld.com:443` | qubopy/const.py:4 | VERIFIED-HW (all login and discovery flows run through it per README) |
| Service-provider UUID (`{sp}` in every path) | `d10e4bfb0153496e8e8bb955f7ebe413` | qubopy/const.py:7 | VERIFIED-HW (hardcoded in all working flows). The "extracted from libcore-rest.so" comment is unverifiable provenance. |
| App-Id header value | `com.hero.iot` (the official Android package id) | qubopy/const.py:10 | VERIFIED-HW (sent on login and every authenticated call) |
| Device attribute default | `Qubopy` (label shown in the official app's "Logged In Devices") | qubopy/const.py:13 | VERIFIED-HW (README: session visible in the app under this label) |

### TLS and transport

- HTTPS with default certificate verification. No custom SSL context anywhere; `aiohttp` defaults apply (qubopy/auth.py:152, qubopy/client.py:56). No certificate pinning.
- Total request timeout 30 seconds on both the auth and client sessions (qubopy/auth.py:153, qubopy/client.py:57).
- Base-URL hardening in code: userinfo in the URL is rejected; `http://` is only allowed for `localhost`, `127.0.0.1`, `::1`; other schemes rejected (qubopy/auth.py:87-101).
- Request host pinning: `QuboClient.request` refuses absolute URLs whose scheme or host differs from `base_url` (qubopy/client.py:71-86).

### Header requirements

Three distinct header sets exist. Exact sets in section 3.5. Summary:

- Unauthenticated login calls: `Content-Type`, `App-Id`, `Source-Device-Id`, `Source: USER` (email login additionally `Token-Type: USER`).
- Authenticated calls and refresh: `Authorization: Bearer <accessToken>`, `Subscriber-Key: <accessToken>`, `User-UUID`, `Token-Type: USER`, `Source-Device-Id`, `App-Id`, `Source: Android`.

### MQTT broker

| Item | Value | Source | Status |
|---|---|---|---|
| Host | `mqtt.platform.quboworld.com` | qubopy/const.py:16 + README | INFERRED: constant exists; no MQTT client code, constant has no call site |
| Port | `8883` (TLS, `ssl://`) | qubopy/const.py:17 + README | same |

### STUN server

`stun6.platform.quboworld.com:5349` (qubopy/const.py:18-19). Not mentioned in the README, never used in code. INFERRED: likely related to the camera P2P path (section 6), but nothing in the artifact says so.

### Streaming provider

Wowza. The only mentions are the docstring "Mint a live single-consumer Wowza RTSPS stream URL from Qubo Cloud" (qubopy/client.py:400) and the CLI label "Live RTSPS Stream URL" (qubopy/cli.py:578). The stream-manager paths in section 4 are the interface. The URL scheme (`rtsps://`) is never parsed in code; README says RTSPS. Treat the exact URL format as unconfirmed until the mint response is captured (checklist item 9).

## 3. Authentication

All flows live in `qubopy/auth.py` (class `QuboAuth`) plus the CLI persistence in `qubopy/cli.py`.

### 3.1 Email + password login

- Call: `POST https://srvcapp.platform.quboworld.com:443/sms/api/v1/sp/{sp}/user/login?system=CS` (qubopy/const.py:22, qubopy/auth.py:165). The `system=CS` query value's meaning is unknown; it is hardcoded.
- Request body (qubopy/auth.py:166-171):
  ```json
  {"username": "<email>", "email": "<email>", "password": "<password>", "deviceAttribute": "<label, default Qubopy>"}
  ```
  `username` and `email` both carry the email address (belt-and-braces guess, see status note).
- Headers (qubopy/auth.py:172-178): `Content-Type: application/json`, `App-Id`, `Source-Device-Id`, `Source: USER`, `Token-Type: USER`.
- Response: JSON object, or a JSON array whose first element is used (qubopy/auth.py:190-191). Fields the code reads (qubopy/auth.py:201-206): `accessToken` (or `token`), `uuid` (or `userUUID` or `userId`), optional `refreshToken`. Anything else in the response is not parsed.
- Token expiry: decoded from the JWT `exp` claim without signature verification (qubopy/auth.py:59-73, 207). If absent, defaults to now + 24h (qubopy/auth.py:139, 207).
- Errors: HTTP != 200 raises `QuboAuthError` with only the status (body is logged at debug, truncated to 200 chars); network errors raise `QuboConnectionError`; non-JSON body raises `QuboAuthError` (qubopy/auth.py:184-199).
- Status: VERIFIED-HW (README auth row; quick start 1). The exact accepted key set of the request body is INFERRED: the code sends four keys and the server evidently accepts them, but which are required is not observable from here.

### 3.2 Token refresh and lifecycle

- Refresh call: `POST /sms/api/v1/sp/{sp}/users/{user_uuid}/auth/refresh` (qubopy/const.py:25, qubopy/auth.py:404-407).
- Request body: `{"accessToken": "<current>", "refreshToken": "<current>"}` (qubopy/auth.py:418-421).
- Headers: the authenticated set, including both `Authorization: Bearer` and `Subscriber-Key` carrying the same access token, plus `Source: Android` (qubopy/auth.py:408-417).
- Response fields read: `accessToken` (required), `refreshToken` (optional; the old one is kept if absent) (qubopy/auth.py:444-449).
- Lifecycle rules in code:
  - Proactive: `get_valid_access_token` refreshes when fewer than 300 seconds remain and a refresh token exists (qubopy/auth.py:466-468). If the refresh fails but the token is still valid, it warns and continues; if expired, it raises (qubopy/auth.py:471-478).
  - Expired with no refresh token raises "Session expired; please log in again" (qubopy/auth.py:480-485).
  - Refresh is serialized by an `asyncio.Lock`; a coroutine that waited on the lock returns the fresh token instead of refreshing again (qubopy/auth.py:391-399).
  - `QuboClient.request` reacts to HTTP 401 by refreshing once (with the failed token, to defeat the lock's double-check) and retrying the request exactly once; a second 401 raises `QuboAuthError` (qubopy/client.py:102-149).
- Status: VERIFIED-HW (README auth section: "proactively refreshes the access token before expiration using POST /auth/refresh"). The exact token TTL for email sessions is unknown (derived from the JWT `exp` at runtime; README implies a long-lived continuous session).

### 3.3 Phone + SMS OTP (1-hour wire session; the app mints a durable pair on profile select)

Two steps. The wire session is 1 hour and the verify call itself returns no refresh token - but the app's own OTP login does NOT stay on this route: after profile selection it mints a durable refresh token (180 days). See the measured-in-app bullet below and `docs/qubo-findings.md` ("the app-side answer").

Request OTP:

- Call: `POST /sms/api/v1/sp/{sp}/user/phone/login` (qubopy/const.py:23, qubopy/auth.py:224).
- Body: `{"phone": "<number>", "isdCode": "+91", "deviceAttribute": "<label>"}` (qubopy/auth.py:226-230). The ISD code is normalized to a leading `+`.
- Headers: `Content-Type`, `App-Id`, `Source-Device-Id`, `Source: USER` (no `Token-Type`; qubopy/auth.py:231-236).
- Response: JSON object; the code reads `uuid` (the login session UUID) and stores it as `_login_uuid` (qubopy/auth.py:247-249). The CLI writes it to the pending-session file (section 3.6).

Verify OTP:

- Call: `POST /sms/api/v1/sp/{sp}/user/phone/verify` (qubopy/const.py:24, qubopy/auth.py:262).
- Body: `{"phone", "isdCode", "otp", "deviceAttribute", "uuid": "<session uuid from step 1>"}` (qubopy/auth.py:270-276).
- Headers: same four as the request step (qubopy/auth.py:277-282).
- Response: a single JSON object (wrapped into a one-element list) or a JSON list of account profiles (qubopy/auth.py:293-298). Each profile's fields read: `accessToken` (or `token`), `uuid` (or `userUUID` or `userId`), optional `refreshToken` (qubopy/auth.py:341-350). The CLI also reads `name` and `email` per profile for display (qubopy/cli.py:276-279).
- The 1-hour limit: the README states phone sessions live exactly 1 hour (`exp: 3600s`) and the phone route issues no `refreshToken` (README, "Phone Number and SMS OTP"). The code does not hardcode 3600; it derives expiry from the JWT `exp` claim, and the CLI's error handler prints "Phone OTP tokens are valid for 1 hour" (qubopy/cli.py:169). Treat "1 hour" as a server-side property confirmed by README + observed error text, not by code logic.
- **Measured 2026-10-13:** the session JWT carried `iat` 11:42:33 and `exp` 12:42:33 - exactly 3600s - and the session then failed with "Session expired; please log in again". No refresh token exists on this route, so the hour is hard; a durable client needs the email/password route (section 3.1), and no email-code login exists on the gateway (eight candidate endpoints probed, all 404; `/user/login` rejects OTP-shaped bodies with `ERR_SM_004 Invalid username or password`).
- **Measured 2026-10-16 (in-app, rootable emulator):** the raw verify response (profile list, one profile per account, each with a 1h `token`) is NOT the whole app flow. The app's profile-select step runs a native `MobileUserManager.userPhoneLogin` call whose response it stores; read back from the app's own `hero_iot.db` (user table) and `shared_prefs` minutes after an OTP login, that response carried a 1h `accessToken` (sub `USER`) **plus a `refreshToken` JWT with `sub: USER_REFRESH` and `exp - iat = 180 days`**, an `mqttPassword` (PBKDF2$sha256$...), `accountType` `GM` and `gmLoginId` set. So "no refresh token on this route" is true of the raw verify call only; the app's OTP path is durable for 180 days. The exact mint call (URL, headers, body) is the open item - candidates probed and ruled out are in `docs/qubo-findings.md` ("the app-side answer").
- Errors: HTTP != 200 -> `QuboAuthError` (status only); network -> `QuboConnectionError`; unexpected body type -> `QuboAuthError` (qubopy/auth.py:286-302). Inconsistency: unlike email login, `request_phone_otp` does not catch `json.JSONDecodeError`, so a malformed body escapes as a raw `JSONDecodeError` (qubopy/auth.py:240-251).
- Status: VERIFIED-HW (README auth row; quick start method B).

### 3.4 Profile / account selection

A phone number can map to multiple Qubo accounts. After verify, the returned profile list is resolved to one:

- `find_profile_index(profiles, profile_index=0, account_uuid=None)` (qubopy/auth.py:304-327): with `account_uuid`, exact match against each profile's `uuid`/`userUUID`/`userId`; otherwise the index, default 0. Errors: empty list, uuid not found, index out of range all raise `QuboAuthError`.
- `select_profile` adopts the chosen profile's tokens (qubopy/auth.py:329-359).
- CLI behavior: interactive picker when more than one profile and a TTY is present (qubopy/cli.py:273-283, 370-380); non-interactive selection via `--profile-index` or `--account-uuid` (qubopy/cli.py:670-679). When the selected profile has an `email`, it is stored in the session file (qubopy/cli.py:297-302).
- Status: VERIFIED-HW (README: "detects multiple profiles and allows interactive selection or targeted selection via --profile-index or --account-uuid").
- In the app the selection is not purely local: picking a profile triggers a native `MobileUserManager.userPhoneLogin(UserDto)` call and the picked profile's tokens (including the 180-day `refreshToken`) are committed only at that point - measured 2026-10-16, see section 3.3's in-app bullet. Ved's number <phone-number> maps to two accounts (uuid `10000000-...`, <family-email>; uuid `10000000-...`, <family-email>), and the app's picker shows exactly those two.

### 3.5 What carries the token

Authenticated request headers, generated by `QuboAuth.get_headers` (qubopy/auth.py:489-501) and duplicated in `refresh_session` (qubopy/auth.py:408-417):

| Header | Value |
|---|---|
| `Content-Type` | `application/json` |
| `Authorization` | `Bearer <accessToken>` |
| `Subscriber-Key` | `<accessToken>` (same token, second header) |
| `User-UUID` | account uuid |
| `Token-Type` | `USER` |
| `Source-Device-Id` | persistent device UUID (section 3.6) |
| `App-Id` | `com.hero.iot` |
| `Source` | `Android` |

Note the split: login calls send `Source: USER` (and email login also `Token-Type: USER`), while authenticated calls and refresh send `Source: Android`. Both are reproduced verbatim above; whether the server cares is unknown.

### 3.6 Session files, device id, and paths

| File | Path | Format | Written by | Source |
|---|---|---|---|---|
| Session | `~/.qubopy_session.json` (Windows: `%USERPROFILE%\.qubopy_session.json`) | JSON: `{"accessToken", "refreshToken" (null for phone), "uuid", "email" (phone only, if profile had one)}` | `login`, `login-phone`, `verify-otp` | qubopy/cli.py:23, 40-45, 203-209, 292-303, 391-402 |
| Pending OTP | `~/.qubopy_otp_pending.json` | JSON: `{"phone", "isd", "sessionUuid", "deviceId", "deviceName"}`; deleted after successful verify | `login-phone` | qubopy/cli.py:218, 239-249, 305, 404 |
| Device id | `~/.qubopy_device_id` | one-line UUID4, created once, reused forever ("matching Android uniqueAndroidID behavior") | any `QuboAuth()` construction | qubopy/auth.py:30, 33-56 |

- All three are created with POSIX mode 0o600 (qubopy/auth.py:41, qubopy/cli.py:28). If the device-id file cannot be written, an ephemeral UUID is used for that run (qubopy/auth.py:52-56).
- The session file does not store expiry; on load, expiry is re-derived from the JWT (qubopy/auth.py:135-139). If the JWT has no parsable `exp`, `set_tokens` assumes 24h validity, which can make an already-dead token display as "Active" in `qubopy tokens`.
- README confirms these paths and the 0o600 claim (README, "Operating System File Permissions" and "Session & Token Management").
- Status: VERIFIED-HW for the email path; the phone path's session file content is exercised in the same CLI flows (same README verification section).

### 3.7 Error shapes

Classes in `qubopy/exceptions.py` (all inherit `QuboError`):

| Class | Raised when | Example sites |
|---|---|---|
| `QuboAuthError` | login/OTP/refresh failures, missing fields, 401/403 from any authenticated call, expired session | qubopy/auth.py:188, 204, 246, 292, 433, 446, 462, 475, 485; qubopy/client.py:115, 139 |
| `QuboConnectionError` | aiohttp network errors | qubopy/auth.py:197, 251, 300, 438; qubopy/client.py:130 |
| `QuboCommandError` | non-2xx (other than 401/403) on API calls; unparsable JSON response; lock unlock with no gateway | qubopy/client.py:122, 132; qubopy/devices/lock.py:138 |
| `QuboKeyExtractionError` | lock BLE fields absent from metadata | qubopy/devices/lock.py:103 |
| `QuboDeviceNotFoundError` | never raised anywhere in the package (defined and exported only) | qubopy/exceptions.py:16, qubopy/__init__.py:21 |

HTTP mapping inside `QuboClient.request` (qubopy/client.py:102-132): 401 with a refresh token -> refresh and retry once; 401/403 otherwise -> `QuboAuthError`; any other non-200/201/204 -> `QuboCommandError`; 204 or empty body -> `{}`.

Server error body shapes: the code only logs them (debug, first 200 chars); the envelope itself was captured live 2026-10-15: `{"code": "...", "message": ["..."]}` - `message` is an ARRAY of strings (confirmed in three independent sites: a live 401, and the app's own OTP error handling twice). Named codes: `ERR_AUTH_002` ("Invalid/Expired Token"; also matched in qubopy/cli.py:161-166), `ERR_OTP_001` (wrong OTP; decompile `Y4.java:1850`), `ERR_SM_057` (OTP resend rate limit; captured live 2026-10-15 with the verbatim text "Resend OTP limit has been exhausted. Please try after 60 minutes"). The full code catalogue remains open.

## 4. REST endpoint catalog

Every endpoint referenced anywhere in the package. `{sp}` is always `d10e4bfb0153496e8e8bb955f7ebe413`. "Auth" = requires the authenticated header set from 3.5.

Statuses: VERIFIED-HW (12), MODELED (2), INFERRED (2). Basis for each VERIFIED-HW is the specific README claim or quick start that exercises the path.

### Auth endpoints

**E1. Email login** - VERIFIED-HW
- `POST /sms/api/v1/sp/{sp}/user/login?system=CS`
- Purpose: authenticate with email + password; returns access and refresh tokens.
- Request: see 3.1. Response: see 3.1 (fields read: `accessToken`/`token`, `uuid`/`userUUID`/`userId`, optional `refreshToken`; other fields unknown).
- Auth: none (headers carry `App-Id`, `Source-Device-Id`, `Source: USER`, `Token-Type: USER`).
- Source: qubopy/const.py:22, qubopy/auth.py:158-215.
- Basis: README auth row hardware verified; quick start 1.

**E2. Phone OTP request** - VERIFIED-HW
- `POST /sms/api/v1/sp/{sp}/user/phone/login`
- Purpose: send SMS OTP; returns login session `uuid`.
- Request/response: see 3.3. Auth: none. Source: qubopy/const.py:23, qubopy/auth.py:217-251.
- Basis: README auth row; quick start method B.

**E3. Phone OTP verify** - VERIFIED-HW
- `POST /sms/api/v1/sp/{sp}/user/phone/verify`
- Purpose: exchange OTP for session tokens; returns one or more account profiles.
- Request/response: see 3.3. Auth: none. Source: qubopy/const.py:24, qubopy/auth.py:253-302.
- Basis: README auth row; quick start method B.

**E4. Token refresh** - VERIFIED-HW
- `POST /sms/api/v1/sp/{sp}/users/{user_uuid}/auth/refresh`
- Purpose: renew access token (and optionally rotate refresh token).
- Request/response: see 3.2. Auth: authenticated headers (old token in both `Authorization` and `Subscriber-Key`).
- Source: qubopy/const.py:25, qubopy/auth.py:389-457.
- Basis: README "Automatic Renewal".

### Discovery and shadow endpoints

**E5. List units** - VERIFIED-HW
- `GET /unit-entity-management/api/v1/sp/{sp}/units`
- Purpose: list the account's homes/sites (units).
- Response: JSON list, or an object with `data` or `units` list (qubopy/client.py:323-330). Unit fields read elsewhere: `unitUUID` / `uuid` / `unitId` (qubopy/client.py:489). Exact full unit schema unknown.
- Auth: yes. Source: qubopy/const.py:28, qubopy/client.py:312-330.
- Basis: exercised by `get_all_devices()` in README quick start 1 (hardware verified).

**E6. List devices in a unit** - VERIFIED-HW
- `GET /unit-entity-management/api/v1/sp/{sp}/units/{unit_uuid}/devices`
- Purpose: all devices registered under a home.
- Response: JSON list, or object with `data` or `devices` list (qubopy/client.py:343-350). Device descriptor keys read by `get_all_devices` (qubopy/client.py:489-560): `deviceUUID`/`deviceId`/`uuid`, `deviceType`/`type`, `deviceModel`/`model`/`aModelNo`, `deviceName`/`name`, `isOnline`/`online`, `extraData`, `entityUUID`/`entityId`, and an optional embedded `state`/`shadow`/`deviceShadow`. These aliases are INFERRED (defensive guesses); the real key set must be confirmed from a live response.
- Auth: yes. Source: qubopy/const.py:29-31, qubopy/client.py:332-350.
- Basis: same as E5.

**E7. Entity device listing** - MODELED, UNUSED-IN-CODE
- `GET /unit-entity-management/api/v1/sp/{sp}/units/{unit_uuid}/entities/{entity_uuid}/devices/self`
- Purpose (per method name `get_devices_for_entity`): device descriptors for one unit + entity (room). Nothing calls it.
- Response parsing mirrors E6 (qubopy/client.py:352-369); unit/entity UUIDs are injected into each descriptor if absent.
- Auth: yes. Source: qubopy/const.py:32-34, qubopy/client.py:352-369.
- Note: no README claim and no call site. Its existence suggests the app queries per-entity, but the path's role is unverified.

**E8. Bulk shadows** - MODELED, UNUSED-IN-CODE
- `GET /unit-entity-management/api/v1/sp/{sp}/devices/shadow`
- Purpose (per method name `get_all_shadows`): all device shadow states in one call.
- Response parsing (qubopy/client.py:162-192): a list of items with `deviceUUID`/`uuid`/`deviceId` and `state.reported`/`state.desired` (or top-level `reported`/`desired`) plus `timestamp`; or a dict keyed by device id. Aliases INFERRED.
- Auth: yes. Source: qubopy/const.py:35, qubopy/client.py:151-192.
- Note: never called by the CLI or any device class. Useful if the Rust client wants one bulk poll, but verify the shape first (checklist item 6).

**E9. Single device shadow** - VERIFIED-HW
- `GET /unit-entity-management/api/v1/sp/{sp}/units/{unit_uuid}/entities/{entity_uuid}/devices/{device_uuid}/shadow`
- Purpose: live shadow state (telemetry + attributes) for one device. This is the read path for every device family.
- Path placeholders: `unit_uuid` and `entity_uuid` fall back to the literal string `default` when unknown (qubopy/client.py:303-308). Whether the server accepts `default` is unverified.
- Response: parsed by `_parse_shadow_payload` (qubopy/client.py:194-294). Two shapes supported:
  1. Services-array format (comment: "primary Qubo IoT hardware format"): `{"services": [{"service": "<name>", "attributes": {"<attr>": {"value": ...} or scalar}}, ...]}`. The parser flattens each attribute into `reported["<service>_<attr>"]`, stores the whole service as `reported["<service>"] = {attr: value, ...}`, and derives the convenience fields listed in section 7.
  2. Legacy format: `{"state": {"reported": {...}, "desired": {...}}}` or top-level `reported`/`desired` dicts (qubopy/client.py:278-286).
- Auth: yes. Source: qubopy/const.py:36-38, qubopy/client.py:296-310.
- Basis: exercised by `dashcam.refresh()` in README quick start 6 (hardware verified) and by lock key extraction. The service names and derived fields are the code's model; verify against a live capture (checklist item 7).

### Control endpoints

**E10. Send command** - VERIFIED-HW (endpoint); payload shapes vary by family
- `POST /unit-entity-management/api/v1/sp/{sp}/units/{unit_uuid}/entities/{entity_uuid}/devices/{device_uuid}/command`
- **Live correction (2026-10-13): POST returns 405. The working method is PUT.** The executing body shape is the lock-style envelope carrying an attributes map (proven on a camera: panTiltControl physically moved it, shadow-verified):
  ```json
  {"topic": "/control/{unit_uuid}/{device_uuid}/{service}",
   "payload": {"devices": {"deviceUUID": "{device_uuid}",
     "services": {"{service}": {"attributes": {"<attribute>": "<value>"}, "instanceId": 0}}}}}
  ```
  TRAP: the server returns `200 SUCCESS_UM_006` for every shape tried, including commands-nested variants that do nothing - only the attributes+instanceId shape executes. Never trust the ack; read the shadow back and confirm the change.
- Purpose: dispatch a control command to a device. `unit_uuid`/`entity_uuid` fall back to `default` (qubopy/client.py:425-430).
- Request body: a bare JSON object, family-specific (section 7). The only payload on a README-verified flow is the camera siren `{"siren": <bool>}` (qubopy/devices/camera.py:150-152). Plug/purifier/light payloads are MODELED guesses.
- Response: whatever the server returns; the code passes it through. Unverified.
- Auth: yes. Source: qubopy/const.py:41-43, qubopy/client.py:417-431.
- Basis: README camera row lists Siren as verified. Caveat: the siren payload is a bare dict unlike every other camera control (which uses the attribute envelope, E12); its exact shape is unconfirmed from code.

**E11. Gateway unlock (lock via in-home bridge)** - VERIFIED-HW
- `PUT /unit-entity-management/api/v1/sp/{sp}/units/{unit_uuid}/entities/{entity_uuid}/devices/{gateway_uuid}/command`
- Purpose: unlock a BLE door lock through a paired in-home gateway (camera or doorbell). Same path as E10 but method PUT and a different envelope.
- Request body (qubopy/client.py:446-463):
  ```json
  {
    "topic": "/control/{unit_uuid}/{gateway_uuid}/lockControl",
    "payload": {
      "devices": {
        "deviceUUID": "{gateway_uuid}",
        "services": {
          "lockControl": {
            "commands": {
              "lockUnlock": {"instanceId": 0, "parameters": {"control": "unlock"}}
            }
          }
        }
      }
    }
  }
  ```
- Response: passed through; unverified shape.
- Auth: yes. Source: qubopy/client.py:433-464; caller qubopy/devices/lock.py:129-148.
- Basis: README lock section: "Verified on physical hardware with live bolt movement". The `topic` string is a topic name carried inside a REST body, the strongest hint in the artifact about the MQTT topic naming scheme (section 5).

**E12. Update shadow attribute** - VERIFIED-HW (endpoint); per-family shapes vary
- **Live correction (2026-10-13):** POST here returns 405; PUT is accepted but every attributes shape tried is rejected (`ERR_BAD_REQUEST invalid attributes`). The working camera write is the E10 PUT with the attributes envelope above; do not build on E12.
- `POST /unit-entity-management/api/v1/sp/{sp}/units/{unit_uuid}/entities/{entity_uuid}/devices/{device_uuid}/shadow/attribute`
- Purpose: write a desired attribute to the device shadow. Used by every camera/dashcam control except siren.
- Request body, generated by `set_service_attribute` (qubopy/devices/base.py:125-136):
  ```json
  {"<service>": {"attributes": {"<attribute>": "<value as string>"}}}
  ```
  Note the value is always stringified (`str(value)`), even booleans and numbers.
- Response: passed through; unverified.
- Auth: yes. Source: qubopy/const.py:44-46, qubopy/client.py:466-480.
- Basis: camera privacy mask is in the README's verified camera list; the envelope above is the code path for it. Which (service, attribute) pairs the server accepts beyond the camera ones is MODELED.

### Media endpoints

**E13. RTSP support query** - VERIFIED-HW
- `GET /unit-entity-management/api/v1/sp/{sp}/units/{unit_uuid}/devices/rtsp/support?deviceId={device_uuid}`
- Purpose: ask whether a camera supports cloud RTSP and, if so, its stream URL and credentials.
- Response fields read (qubopy/client.py:378-386): `supported` or `isSupported`; `rtspUrl` or `url` or `streamUrl`; `authRequired`; `username`; `password`. Aliases INFERRED; capture the real shape (checklist item 8).
- Failure behavior: any exception (including auth errors) is swallowed and reported as `is_supported=False` (qubopy/client.py:388-390). Do not copy that behavior in the Rust client; distinguish 401 from "not supported".
- Auth: yes. Source: qubopy/const.py:49-51, qubopy/client.py:371-390.
- Basis: README camera row "RTSP Stream Query" hardware verified.

**E14. Mint live stream URL** - VERIFIED-HW
- `POST /stream-manager/api/v1/sp/{sp}/stream/unit/{unit_uuid}/device/{device_uuid}`
- Query params: `timestamp=NOW`, `type={quality}`, `streamSourceCamera=primary`, `deviceType={device_type or "ptzeCamera"}` (qubopy/client.py:405-410).
- Body: `{"userAppID": "<the client's device id>"}` (qubopy/client.py:411-413). The value sent is `QuboAuth.device_id` (the persistent UUID from `~/.qubopy_device_id`).
- Response field read: `streamURL` (capital URL; qubopy/client.py:414). Returns `""` if absent. Full response schema unknown.
- Auth: yes. Source: qubopy/const.py:55-57, qubopy/client.py:392-415.
- Basis: README "Cloud RTSP URL minting ... verified on physical hardware"; CLI `stream` command.

**E15. P2P credentials** - INFERRED, UNUSED-IN-CODE
- `GET /stream-manager/api/v1/sp/{sp}/unit/{unit_uuid}/devices/pp2p/credentials`
- Only a path constant exists (qubopy/const.py:52-54). No method, no call site, no response shape, no README mention. Its role (likely the credential half of the app's P2P path) is speculation; nothing further is claimed here.

**E16. MQTT credentials** - INFERRED, UNUSED-IN-CODE
- `GET /unit-entity-management/api/v1/sp/{sp}/agent/device/mqtt/unit/{unit_uuid}/device/{device_uuid}`
- Path constant only (qubopy/const.py:60-62); no call site. The README documents it as a GET returning the dynamic broker credentials (section 5). The response shape below is README-only, not from code.
- **Measured 2026-10-13:** GET returns device MQTT status only (`mqttConnectionStatus`, `messageQueueSize`, `deviceConnectionStatus`, `queueBlocked`) - not credentials. The README's documented response does not match the server.

## 5. MQTT

Status 2026-10-14: **LIVE-VERIFIED**. The broker connection, credentials, real topics and payload shapes are captured from live sessions (the official app's and our own replay client). The library contains no MQTT client; the README-only subsections below are retained as provenance, but the app's actual wire behavior differs from them in material ways - the topic families especially.

### Live verification (2026-10-14, captured wire traffic)

- **Broker**: `mqtt.platform.quboworld.com:8883`, TLS, MQTTv5 accepted. Raw capture log: `~/Projects/qubo-spike/mqtt_probe.log` (16:05-16:52 IST, 2026-10-14).
- **Credentials (verified)**: `username = <account userUUID>`, `password = <a live accessToken>` - any valid session of the account works (ours: phone-OTP session; the app's own login equally). Connected and held QoS-2 subscriptions for the full capture window.
- **Not the credentials**: the `{"username": "<uuid>:<expiry-epoch>", "password": "<20-byte base64>"}` pair the app logs into `ServerInfo.saveP2PCredentials` (native event 307) is for the native P2P layer - the broker refuses it (`Not authorized`). TURN-shaped (user:expiry).
- **clientId**: `CLIENT_<deviceName>_MainMqttClient_<unitUUID>` (main client) and `CLIENT_<srcDeviceId>_SimpleMQTTClient_<unitUUID>` (simple client); the broker accepts unknown clientIds.
- **Real topic families** (all observed live):
  - `/monitor/<unitUUID>/<deviceUUID>/<service>` - device events. Observed: `heartbeat` (action 140, `operationState: online|offline`), `p2pStreaming` (`p2pCallStarted` with `offerResponse`), `panTiltControl`, `recordingControl`, `mcuSWVersion`, `wifiSettings`, `aqiStatus`, `streamControl` (cloud-side session notices, `srcDeviceId: "cloud"`).
  - `/control/<unitUUID>/<deviceUUID>/<service>` - commands to devices, envelope `{command: {devices: {services: {<service>: {commands: {<command>: {instanceId, parameters}}}}}}}`. Observed: `p2pStreaming.startP2pCall` / `endP2pCall`, `aqiRefresh`.
  - `/config/<unitUUID>` - account/unit notices. Observed: action 71 `deviceAccessToken` issuance (per-dest JWT, `sub: DEVICE_ACCESS`, `src`/`dest` device UUIDs), action 90 acks.
- **Envelope fields**: every message carries `serviceProviderId`, `srcDeviceId` (device identity - cameras publish as `MPC_<mac>`, purifiers as `HPH_<mac>`, the app phone as its `srcDeviceId` hash), `timestamp` and `msgSequenceId` (epoch-ms), `clientId`.
- **P2P signaling, captured end to end** (raw in the log above; frame-level detail in `~/Projects/qubo-spike/p2p/PROTOCOL.md`): `startP2pCall` with `aesKey`/`aesIV`/`sessionId`/`offerData`/`mobileUUID`/`streamSourceQuality`; camera's `p2pCallStarted` with `offerResponse`; `endP2pCall` with `sessionId` on leaving the view (the app tears down when the viewer backs out, not on app kill).
- **Connectivity types** (native event 308, `P2P_STREAM_CONNECTIVITY_TYPE`): `2 = STUN_V2` (direct), `3 = TURN_V2` (relay), else `LOCAL_V2`. Working watched sessions were observed as both 2 and 3.

### Broker

- Host `mqtt.platform.quboworld.com`, port 8883, `ssl://` / TLS, TCP with standard server certificate verification (README, "Cloud MQTT Broker"; constants qubopy/const.py:16-17, unused).

### Credential acquisition

- README: call the authenticated REST endpoint `GET /unit-entity-management/api/v1/sp/{sp}/agent/device/mqtt/unit/{unit_uuid}/device/{device_uuid}` (path constant qubopy/const.py:60-62; the GET method is README-only).
- README-documented response:
  ```json
  {
    "username": "<user_uuid>",
    "password": "<dynamic_mqtt_session_secret>",
    "clientId": "<client_or_device_uuid>",
    "serverUrl": "ssl://mqtt.platform.quboworld.com:8883"
  }
  ```
  Status: README-documented; the code never calls this endpoint, so the shape is unverified.

### Topics - README table (SUPERSEDED 2026-10-14: these topic strings were never observed on the wire; the real families are in the live block above)

| Topic pattern | Purpose per README | Notes |
|---|---|---|
| `/users/{user_uuid}/events` | push events: doorbell presses, motion detection, security alarms | leading slash, unlike the other two |
| `units/{unit_uuid}/devices/{device_uuid}/status` | live telemetry: plug power (W), purifier PM2.5, sensor metrics | no leading slash |
| `units/{unit_uuid}/devices/{device_uuid}/command` | asynchronous command publication channel for cloud-relayed actuation | no leading slash |

The leading-slash inconsistency is the README's, reproduced verbatim. No topic string appears anywhere in the code.

### Payload shapes

- None are given in the code or README for MQTT.
- The only related shape in the artifact is the lockControl envelope (E11), sent over REST with `topic: "/control/{unit_uuid}/{gateway_uuid}/lockControl"` and a `{devices: {deviceUUID, services: {<service>: {commands: {<command>: {instanceId, parameters}}}}}}` payload. This is the best available model of the app's service/command envelope, and the topic string hints at a `/control/...` topic family. The 2026-10-14 live capture confirms exactly that family and envelope shape on the wire (see the live block above).

### What flows where (README only)

- Doorbell presses, motion, alarms -> `/users/{user_uuid}/events`.
- Telemetry -> `units/.../status`.
- Commands -> `units/.../command`.

## 6. Streaming

### The mint flow (exact)

1. Resolve the camera's `unit_uuid`, `device_uuid`, and `device_type` from discovery (E5/E6).
2. `QuboCamera.start_stream(quality)` -> `QuboClient.get_live_stream_url(unit_uuid, device_uuid, device_type=self.info.device_type, quality=quality)` (qubopy/devices/camera.py:105-112).
3. The client sends `POST /stream-manager/api/v1/sp/{sp}/stream/unit/{unit_uuid}/device/{device_uuid}` with params `timestamp=NOW`, `type={quality}`, `streamSourceCamera=primary`, `deviceType={device_type or "ptzeCamera"}`, body `{"userAppID": auth.device_id}` (qubopy/client.py:392-415).
4. The response's `streamURL` field is returned as a string (qubopy/client.py:414). CLI prints it for use in VLC/ffplay/go2rtc/Home Assistant (qubopy/cli.py:576-580).
5. Everything else about the response (expiry, session id, scheme) is unknown until captured.

### Quality options

- CLI accepts `high` or `low` only (qubopy/cli.py:717-719), passed through as the `type` query param. Server-side accepted values are otherwise unknown.
- `deviceType` defaults to the literal `ptzeCamera` when the device's type string is empty (qubopy/client.py:396, 409-410).

### The single-consumer caveat

README: "Streaming URLs minted by Qubo cloud are single-consumer sessions (each URL serves one active stream session)." The code echoes this in the docstring "single-consumer Wowza RTSPS stream URL" (qubopy/client.py:400). Practical consequence for the Rust client: one viewer per mint; a second concurrent viewer needs its own mint, and whether concurrent mints on one camera are allowed is unknown.

### Live measurements (2026-10-13)

- **The Wowza path is per login session.** All mints from one session share one path; mints from different sessions get different paths. Within a session the LAST mint wins - every URL of that session then serves that camera (proven byte-identical frames). Different sessions serve different cameras simultaneously (three sessions, three cameras, concurrent, verified with frames).
- **Unconsumed URLs expire fast.** A minted URL that nobody connects to dies within about a minute (404 on a later connect). Once connected, the session streams steadily (90 seconds held, 899 frames, no cut) and ends after roughly five minutes (observed EOF cadence ~5 min), so clients re-mint on reconnect.
- **Sharpened 2026-10-16: the URL is one-shot per attach.** A fresh mint attaches promptly and plays, but after its first consumer attaches and leaves, every later attach to the SAME URL 404s - measured at +7s and +9s after a +1s success, well inside the old five-minute claim. A wrong-token attach consumes that one slot too (wowza did not validate the token on this path). Multiple clients therefore share one producer (the earlier "multi-consumer" observation is go2rtc sharing a single attach), and any retry path must RE-MINT and swap the source rather than reconnect to a URL that has been attached once. The camera's push can also lag intermittently after a mint (one cold attempt 404'd for 52s, while later mints on the same device id attached in ~1s), so a retry loop needs both a fresh URL and patience. Found on the hub as the live-open defect (task f9861050).
- **The README's "single-consumer" claim did not reproduce:** one minted URL served three simultaneous consumers, all streaming, zero errors.

### No open LAN RTSP / ONVIF

README: "The mobile app does not rely on open LAN RTSP ports. Instead, it negotiates direct peer-to-peer (P2P) UDP connections encrypted with AES-128-GCM over RTP." The library implements none of the media path: it mints cloud URLs and reads RTSP-support metadata, nothing else. The P2P credentials path exists only as an unused constant (E15), and the STUN server constant (qubopy/const.py:18-19) is unused too. There is no RTSP server on the camera to point a player at; the Wowza RTSPS URL from E14 is the supported route, and any P2P implementation is out of scope until captured from the app.

## 7. Device capability matrix

### Shared mechanics

- Device class is chosen by model prefix first, then by type-string substring (qubopy/devices/__init__.py:72-114). Prefix map (qubopy/const.py:65-73): camera `HCP/HCO/HCM/HCI`, doorbell `HCD`, dashcam `HCA`, lock `HLM/HLW/HLR`, plug `HSP`, purifier `HPH`, light `HLB`, sensors `HDS/HGS/HSA`, climate `HDT`. Note: `HCM` and `HCI` appear in the code but not in the README matrix.
- Gateway detection (`is_gateway_device`, qubopy/devices/__init__.py:43-68): cameras and doorbells count as lock gateways; dashcams are explicitly excluded; type substrings `doorbell`/`camera`/`gateway` also qualify. During discovery, a lock with no gateway metadata in its own record is auto-bound to the first gateway found in its unit (qubopy/client.py:517-535); with multiple gateways the choice is arbitrary (first in list, logged).
- Shadow state model: all state reads go through `DeviceShadow.reported`. For services-array shadows, keys are either `"<service>_<attr>"` flattened or the nested `"<service>": {attr: value}` dict (qubopy/client.py:210-218). Device property reads that expect other flat keys only match legacy-format shadows. Several reads are unreachable for the services format; those are called out per family.
- Shared command helper `set_service_attribute(service, attribute, value)` stringifies the value and posts the envelope of E12 (qubopy/devices/base.py:125-136).

### Camera and doorbell - `QuboCamera` (HCP/HCO/HCD, plus code-only HCM/HCI)

State (all reads from shadow `reported`):

| Property | Wire key / derivation | Source | Status |
|---|---|---|---|
| `camera_enabled` / `privacy_mode` | derived: `cameraFunction.state == "enable"` / `== "disable"` | qubopy/client.py:221-224 | MODELED (derived read; the write path is verified) |
| `night_mode` | `nightModeControl.nightMode` | qubopy/client.py:226-228 | MODELED |
| `ptz_position` | `panTiltControl.horizontalVerticalPostion` (spelling as in code) | qubopy/client.py:230-232 | MODELED |
| `motion_tracking` | `motionTracking.enabled` (truthy for "true"/"enable") | qubopy/client.py:234-239 | MODELED |
| `ip_address` / `signal_strength` / `wifi_ssid` | `wifiSettings.ipAddress` / `.signalStrength` / `.SSIDName` | qubopy/client.py:241-247 | MODELED |
| `sd_card_status` / `sd_total_storage` / `sd_available_storage` | `systemDiagnosis.sdCardStatus` / `.totalExternalStorage` / `.availableExternalStorage` | qubopy/client.py:249-255 | MODELED |
| `is_siren_on` | keys `siren`, `alarm` | qubopy/devices/camera.py:23-27 | MODELED, and unreachable for services-array shadows (parser never produces these keys; a nested service dict would coerce to False) |
| `is_motion_detected` / `is_person_detected` | keys `motion`/`motion_detected`, `person`/`person_detected` | qubopy/devices/camera.py:29-39 | same unreachable caveat as siren |
| `is_privacy_mode_on` | prefers derived `camera_enabled`, else `privacy`/`privacy_mode`/`lens_mask` | qubopy/devices/camera.py:12-21 | MODELED |
| `is_online`, `name`, `model` | discovery descriptor fields | qubopy/client.py:552-562 | `name`/`model`/`device_id` VERIFIED-HW (printed by verified listing); `is_online` MODELED |

Commands:

| Method | Payload | Endpoint | Status |
|---|---|---|---|
| `start_stream(quality)` | E14 mint | E14 | VERIFIED-HW |
| `get_rtsp_info()` | E13 query | E13 | VERIFIED-HW |
| `set_privacy_mode(bool)` | E12: `cameraFunction.state` = `disable`/`enable` | E12 | VERIFIED-HW (README "Privacy Mask") |
| `set_siren(bool)` | E10: `{"siren": <bool>}` | E10 | VERIFIED-HW per README ("Siren"); payload shape unconfirmed, see E10 caveat |
| `set_night_mode(mode)` | E12: `nightModeControl.nightMode` = `"true"`/`"false"`/`"auto"` | E12 | MODELED |
| `set_motion_tracking(bool)` | E12: `motionTracking.enabled` | E12 | MODELED |
| `pan_tilt(x, y)` | E12: `panTiltControl.horizontalVerticalPostion` = `"x,y"` | E12 | MODELED |

Doorbell: no separate class; HCD routes to `QuboCamera`. The README matrix lists "Ring Event, Chime" for doorbells, but no code implements either (gap, section 8).

### Dashcam - `QuboDashcam` (HCA)

State:

| Property | Wire key / derivation | Source | Status |
|---|---|---|---|
| `serial_number` | `extraData.serialNumber` | qubopy/devices/dashcam.py:11-14 | VERIFIED-HW (printed by quick start 6 / `dashcam-wifi`) |
| `wifi_ap_ssid` / `wifi_ap_password` | `dashcamWifiSettings.wifiName` / `.wifiPassword` | qubopy/client.py:258-262, qubopy/devices/dashcam.py:34-42 | VERIFIED-HW (README dashcam quick start; `dashcam-wifi` CLI) |
| `is_parking_monitoring_on` | `parking_mode` derived: `parkingConfig.state == "enable"`; fallback key `parkingMonitoring` | qubopy/client.py:264-266, qubopy/devices/dashcam.py:28-32 | VERIFIED-HW (printed by quick start 6) |
| `overspeed_limit` / `speed_unit` | `dashcamGpsConfig.overSpeedLimits` / `.speedUnit` | qubopy/client.py:268-272, qubopy/devices/dashcam.py:44-58 | VERIFIED-HW (printed by quick start 6) |
| `is_audio_recording_on` | `audio_recording` derived: `dashcamRecordingConfig.audioTrack == "enable"` | qubopy/client.py:274-276, qubopy/devices/dashcam.py:60-64 | MODELED |
| `is_recording` | keys `recording`, `isRecording`, `loop_record` | qubopy/devices/dashcam.py:16-20 | MODELED, and unreachable for services-array shadows (parser never produces these keys) |
| `sd_card_status` | keys `sdcard_status`, `sdStatus`, `sd_card` | qubopy/devices/dashcam.py:22-26 | MODELED, and unreachable: the parser stores the value under `sd_card_status`, which this property does not check |

Commands (all E12 shadow-attribute writes; none claimed verified in the README):

| Method | Payload | Status |
|---|---|---|
| `set_parking_monitoring(bool)` | `parkingConfig.state` = `enable`/`disable` | MODELED |
| `set_overspeed_limit(kmh)` | `dashcamGpsConfig.overSpeedLimits` = `"<int>"` | MODELED |
| `set_audio_recording(bool)` | `dashcamRecordingConfig.audioTrack` = `enable`/`disable` | MODELED |

### Plug - `QuboPlug` (HSP)

Everything MODELED (README: "Telemetry fields and power controls are modeled from APK application metadata", "API Modeled / Untested"). Note: none of the state keys below are produced by the services-array parser; they only match legacy-format shadows. The real wire service/attribute names are unknown.

- State: `is_on` (`state`/`switch`/`power_state`), `power_watts` (`power`/`active_power`/`watts`), `voltage` (`voltage`/`volts`), `current` (`current`/`amps`), `energy_kwh` (`energy`/`total_energy`/`kwh`) (qubopy/devices/plug.py:11-51).
- Commands (E10): `turn_on` / `turn_off` -> `{"state": "ON"|"OFF", "switch": true|false}` (qubopy/devices/plug.py:53-59).

### Air purifier - `QuboAirPurifier` (HPH)

All MODELED; same parser caveat as the plug.

- State: `is_on` (`state`/`power`/`power_state`), `fan_speed` 1-3 (`speed`/`fan_speed`/`fanSpeed`), `mode` (`mode`/`preset_mode`), `pm25` (`pm2_5`/`pm25`/`aqi`), `filter_life_percent` (`filter_life`/`filter_life_percent`), `is_child_lock_on` (`child_lock`/`childLock`) (qubopy/devices/purifier.py:11-54).
- Commands (E10): `turn_on`/`turn_off` (`{"state", "power"}`), `set_fan_speed(1-3)` -> `{"fan_speed": n, "speed": n}` (two keys for one value; a guess), `set_mode("auto"|"sleep"|"manual")` -> `{"mode": "<lowercased>"}`, `set_child_lock(bool)` -> `{"child_lock": bool}` (qubopy/devices/purifier.py:56-74).

### Light - `QuboLight` (HLB)

All MODELED; same parser caveat.

- State: `is_on` (`state`/`switch`/`power`), `brightness` 0-100 (`brightness`/`dimmer`), `color_temp` (`color_temp`/`cct`/`temperature`), `rgb_color` (`rgb` or `color` as a 3-list) (qubopy/devices/light.py:11-45).
- Commands (E10): `turn_on(brightness, rgb, color_temp)` -> `{"state": "ON", "switch": true, ...}` (sends both state forms; a guess), `turn_off`, `set_brightness` (clamped 0-100), `set_rgb`, `set_color_temp` (qubopy/devices/light.py:47-78).

### Sensors - `QuboSensor` (HDS/HGS/HSA/HDT)

All MODELED; read-only, no commands; same parser caveat.

- `is_contact_open` (`contact`/`door_state`/`open`), `is_gas_alarm` (`gas_alarm`/`gas_detected`), `is_smoke_alarm` (`smoke_alarm`/`smoke_detected`), `temperature_celsius` (`temperature`/`temp`), `humidity_percent` (`humidity`/`rh`), `battery_percent` (`battery`/`power`) (qubopy/devices/sensor.py:9-52).

### Lock - `QuboLock` (HLM/HLW/HLR)

State:

| Property | Wire key / derivation | Source | Status |
|---|---|---|---|
| `gateway_id` / `is_gateway_accessible` | `extraData.gatewayUUID`/`gatewayId`, else shadow `reported` same keys; also auto-bound during discovery | qubopy/devices/lock.py:16-26, qubopy/client.py:517-535 | MODELED |
| `is_locked` | `isOpen`/`open` (string "open"/"closed" handled) or `locked`/`isLocked` | qubopy/devices/lock.py:29-45 | MODELED |
| `battery_percent` | `dpower`/`battery` in shadow, else in extraData | qubopy/devices/lock.py:47-56 | MODELED |

Commands:

- `unlock(gateway_uuid=None)` -> E11 PUT envelope; raises `QuboCommandError` when no gateway is known (qubopy/devices/lock.py:129-148). VERIFIED-HW (README: verified with live bolt movement).

BLE credential extraction (`get_ble_lock_keys`, qubopy/devices/lock.py:78-127):

- Searches `info.extra_data`, `shadow.reported`, and `shadow.raw`, including one level into nested keys `P01DeviceBean`, `deviceBean`, `bean`, `bleInfo` (qubopy/devices/lock.py:58-76).
- Extracted fields and their alternate key names: `dmac` (mac/deviceMac/bleMac), `dbtauthkey` (authKey/btauthkey), `dbtoffkey` (offKey/btoffkey), `dbtoffkeyflag` (offKeyFlag), `dadminpass` (adminPass/adminPin), `dadminflag` (adminFlag), `dbtname` (btName/advName), `dpower`, `isOpen` (qubopy/devices/lock.py:88-113). Model fields: qubopy/models.py:83-102.
- Missing `dmac` or `dbtauthkey` raises `QuboKeyExtractionError` after one retry refresh (qubopy/devices/lock.py:92-106).
- Status: MODELED (experimental). The README marks the whole BLE path "Experimental / Untested", "derived from reverse-engineering the official mobile application... not been tested or verified on physical smart lock hardware". The library only extracts keys; it contains no BLE code.
- GATT UUIDs (README + unused constants qubopy/const.py:76-78): service `0000fee0-0000-1000-8000-00805f9b34fb`, write characteristic `...fee1...`, notify characteristic `...fee2...`. INFERRED (constants only; README labels the whole thing experimental).

## 8. Gaps and unknowns

### README claims the code does not implement (disagreements, both sides cited)

1. BLE: the README title and sections describe a "local BLE enabler" with GATT services and an ESPHome config. The code has no BLE transport at all (dependencies are only aiohttp + pydantic, per METADATA); it extracts keys from cloud metadata (qubopy/devices/lock.py:78-127) and defines unused GATT UUID constants (qubopy/const.py:76-78). The README itself labels the BLE path experimental.
2. MQTT: the README documents a broker, dynamic credentials, and three topic families. The code defines the host/port and the credential path (qubopy/const.py:16-17, 60-62) and implements nothing; no topic string appears in code.
3. P2P media: the README says cloud RTSP minting and "LAN P2P H.264 stream decryption are verified on physical hardware" (camera quick start). The library implements no media path, P2P or otherwise; that sentence describes RE findings, not a library capability.
4. Doorbell functions: the README matrix lists "Ring Event, Chime" for HCD. No code implements ring events or chime.
5. Camera example bug: README quick start prints `stream_info.rtsp_url`, but the model field is `stream_url` (qubopy/models.py:112). The example as printed would raise AttributeError.
6. Camera models: the README matrix lists HCP/HCO; the code also routes HCM and HCI to the camera class (qubopy/const.py:69).
7. Phone session length: README states "exp: 3600s"; the code never hardcodes 3600 (derives from JWT, falls back to 24h if the JWT is unparsable; qubopy/auth.py:207). Not a contradiction, but do not rely on code-side expiry math for the 1-hour limit.

### Things the official app likely does that this library does NOT cover (all UNKNOWN)

No endpoints are speculated for any of these; the artifact simply has nothing.

1. MQTT real-time: telemetry streaming, doorbell/motion/alarm push events. Broker details are README-only; wire payloads unknown.
2. P2P local media: the app's UDP/AES-128-GCM/RTP path, and the P2P credentials endpoint (E15) whose semantics are unknown.
3. Cloud recording playback / SD-card footage browsing and download.
4. Push notifications (the app's own FCM/APNs channel).
5. Subscription-gated features: which features are gated and how the API marks them is unknown.
6. Doorbell ring events and chime control.
7. Camera two-way audio / talkback.
8. Device onboarding: pairing, Wi-Fi provisioning, QR/AP flows, firmware update.
9. Account and topology management: unit/entity create/rename, room assignment, invitations.
10. Automations, scenes, schedules, geofencing.
11. Event/activity history feed and thumbnails.
12. Full response schemas: only the fields the code reads are known for any endpoint. The complete server responses (extra fields, pagination, metadata) are unknown.
13. Auth details: the meaning of `system=CS` on login; full login/refresh response fields; server error envelope (only `ERR_AUTH_002` is named, qubopy/cli.py:161-166); whether the server accepts the `default` placeholder in shadow paths (qubopy/client.py:305-307); real TTLs for email sessions.
14. Real wire service/attribute names for plug, purifier, light, and sensor telemetry. The property keys in those classes are guesses that the services-array parser never produces; the true names must come from a live capture.
15. Whether MQTT and P2P paths need the same `userAppID`/device-id material as the REST calls (unknown; no code).

### Code observations worth knowing before porting

1. Dead constants (defined, zero call sites): `PATH_P2P_CREDENTIALS`, `PATH_MQTT_CREDENTIALS`, `MQTT_BROKER_HOST/PORT`, `STUN_SERVER_HOST/PORT`, `GATT_SERVICE_LOCK`, `GATT_CHAR_LOCK_WRITE`, `GATT_CHAR_LOCK_NOTIFY`.
2. Dead exception: `QuboDeviceNotFoundError` is exported but never raised.
3. Dead API surface: `get_all_shadows` (E8) and `get_devices_for_entity` (E7) are never called by the CLI or device classes.
4. Camera siren payload `{"siren": bool}` (qubopy/devices/camera.py:150-152) is the odd one out among camera controls, which all use the E12 envelope.
5. Several state reads can never match services-array shadows (camera siren/motion/person; dashcam recording/sd_card_status; all plug/purifier/light/sensor properties). See section 7 per-family notes.
6. `get_camera_rtsp_support` swallows every exception, including auth failures, and reports `is_supported=False` (qubopy/client.py:388-390). An expired token looks like "camera does not support RTSP".
7. `request_phone_otp` misses the `json.JSONDecodeError` catch that email login has (qubopy/auth.py:240-251).
8. The misspelling `horizontalVerticalPostion` (qubopy/client.py:231, qubopy/devices/camera.py:146) is likely faithful to the server's field name; confirm in capture, do not "fix" it in the Rust client before verifying.
9. CLI `verify-otp` ISD fallback to the pending-session file is dead code (argparse default `+91` is always truthy; qubopy/cli.py:334).
10. `set_tokens` assumes 24h validity when the JWT lacks `exp` (qubopy/auth.py:139), which can mislabel a dead token as Active.

## 9. Live-verification checklist

Run against a real account session, capturing raw request/response pairs. Each item names what to record and what it promotes. Do not run these before the session exists; order matters (1 before everything).

**Verified so far (2026-10-13, live account, 4x HCP01 cameras + 2 purifiers):** phone OTP login and session TTL (item 2, exactly 3600s, no refresh token); units/devices listing (items 4-5, 7 devices); single-device shadow read (item 7, services-array format confirmed); stream mint high quality and concurrency semantics (item 9, per-session paths, ~1-minute unconsumed expiry, ~5-minute session life, multi-consumer works); camera write via E10-PUT (item 10, PTZ proven end to end; privacy/siren/night-mode still to verify). Email login remains blocked: no email-code endpoints exist on the gateway and the password route is untested. **Update 2026-10-14:** item 14 (MQTT) is now verified - broker connect with `uuid + accessToken`, real topics and payloads captured live; P2P signaling captured end to end (`startP2pCall` -> `p2pCallStarted` -> `endP2pCall`); privacy write + read-back verified; Google v4 sign-in returns a refreshToken (system=GM) while the phone route stays 1h/no-refresh. Siren and night-mode writes remain open.

1. Email login (E1). Capture the full raw response. Promotes: the response schema beyond `accessToken`/`refreshToken`/`uuid`; email token TTL from the JWT.
2. Phone OTP request + verify (E2/E3). Capture both raw responses, including the profile list. Confirms: `exp: 3600s`, absence of `refreshToken`, the full profile schema.
3. Token refresh (E4). Capture the raw response and note whether `refreshToken` rotates. Confirms the refresh response schema.
4. `GET units` (E5). Capture raw JSON. Confirms: list vs `{data}` vs `{units}` and the real unit key set.
5. `GET units/{uuid}/devices` (E6). Capture raw JSON for each unit. Confirms: the real device descriptor keys (is it `deviceUUID`? `aModelNo`? `extraData`?) and the full device schema.
6. `GET devices/shadow` (E8). One call. Confirms whether the bulk endpoint works and its shape; decides whether the Rust client polls bulk or per-device.
7. `GET device shadow` (E9) for one camera and one dashcam. Capture raw. Confirms: the services-array format, the real service names (`cameraFunction`, `nightModeControl`, `panTiltControl`, `motionTracking`, `wifiSettings`, `systemDiagnosis`, `dashcamWifiSettings`, `parkingConfig`, `dashcamGpsConfig`, `dashcamRecordingConfig`), the attribute names (including the `horizontalVerticalPostion` spelling), and which convenience fields actually populate.
8. RTSP support query (E13) for a camera. Capture raw. Confirms the real field names (`supported` vs `isSupported`, URL key) and the URL format.
9. Stream mint (E14) for a camera, both qualities. Capture raw. Confirms: the URL scheme and full format, extra response fields, and the single-consumer behavior (mint twice, try both; then try two concurrent plays of one URL).
10. Camera writes (E12/E10): privacy on/off, night mode, PTZ, motion tracking, siren. Capture responses and re-read the shadow after each. Confirms which (service, attribute) pairs the server accepts and the siren payload shape.
11. Dashcam writes (E12): parking monitoring, overspeed limit, audio recording. Same method. Promotes those from MODELED.
12. Lock: `GET device shadow` on a lock (E9) and key extraction. Confirms where the BLE fields live (extraData? nested `P01DeviceBean`?) and their exact names. Then unlock via gateway (E11), with the owner watching the bolt.
13. Lock + gateway topology: with a lock and its camera in one unit, confirm the auto-binding assumption (first gateway in the unit) matches the app's pairing.
14. MQTT (E16): call the credentials endpoint, connect with the returned values, subscribe to the three topics, and capture real payloads. This is the only way the MQTT layer moves from README-only to usable.
15. Plug / purifier / light / sensor: capture one full shadow each. This is where the real telemetry service and attribute names come from; every state key in those classes is currently a guess.
16. Error shapes: force a 401 with an expired phone token and capture the raw error body (confirm `ERR_AUTH_002` and the envelope); attempt a shadow read with `default` placeholders to see if the server accepts them.

### What to do with the results

Promote each item's status in this document (VERIFIED-HW / MODELED / INFERRED -> confirmed), and feed the captured schemas into the Rust client's types. Until then, the Rust client should treat every MODELED payload shape as a hypothesis with a test against the live API before it is relied on.

---

## Appendix A - Complete endpoint catalogue from the official app

Extracted 2026-10-14 from the installed APK (`com.hero.iot`, pulled via adb from a Samsung SM-F966B) - `ObotApi.java` in the jadx decompile, plus native-library strings from `libcore-rest.so` and `libcore-stream.so`. Paths use `%1$s` for the service-provider UUID and `%2$s`+ for resource UUIDs. This is the app's own catalogue; the qubopy-derived sections above are a subset of it.

**Auth, account, devices (sms family)**
- `/sms/api/v1/sp/{sp}/user/login?system=CS` - email+password login (section 3.1)
- `/sms/api/v1/sp/{sp}/user/emailprofile/login` - email-profile login (different route from the above; bodies unverified)
- `/sms/api/v1/sp/{sp}/user/phone/login` + `/user/phone/verify` - phone OTP (section 3.3)
- `/sms/api/v1/sp/{sp}/users/{uuid}/auth/refresh` - token refresh (section 3.2)
- `/sms/api/v1/sp/{sp}/user/changePassword` - set/change password (the route to a durable email login)
- `/sms/api/v1/sp/{sp}/user/{uuid}/account` - connect/remove account (PUT/DELETE)
- `/sms/api/v1/sp/{sp}/device/logout` - force logout a device
- `/sms/api/v1/sp/{sp}/devices` - list logged-in app devices (GET; measured live: returns srcDeviceId + deviceAttribute per device)
- `/sms/api/v1/sp/{sp}/device/{device}/user` - lock users (create/get/update/delete)
- `/sms/api/v1/sp/{sp}/temp/users/uuid/{uuid}` - temporary (guest) users
- `/sms/api/v1/countries`, `/sms/api/v1/timezones` - static lists

**Units, entities, devices, shadows (unit-entity-management family)**
- `/unit-entity-management/api/v1/sp/{sp}/units` + `/units/{unit}` + `/units/{unit}/entities` - homes and rooms (measured live)
- `/unit-entity-management/api/v1/sp/{sp}/units/{unit}/devices` - devices per unit (section 4 E6)
- `/unit-entity-management/api/v1/sp/{sp}/devices` - flat device list (measured live)
- `/unit-entity-management/api/v1/sp/{sp}/units/{unit}/entities/{entity}/devices/self` - self devices (E7)
- `/unit-entity-management/api/v1/sp/{sp}/devices/shadow` - bulk shadow (GET per the app; measured: GET 405, POST 400 "Invalid Request Body" - method/body still open)
- `/unit-entity-management/api/v1/sp/{sp}/devices/{device}/deviceshadow` - per-device shadow via the flat path (note the name: `deviceshadow`, not `shadow`; the flat `/shadow` variant requires the api_key header)
- `/unit-entity-management/api/v1/sp/{sp}/units/{unit}/entities/{entity}/devices/{device}/shadow` - per-device shadow (section 4 E9)
- `/unit-entity-management/api/v1/sp/{sp}/units/{unit}/entities/{entity}/devices/{device}/shadow/attribute` - single attribute (E12; measured 405/400 - superseded by the command route)
- `/unit-entity-management/api/v1/sp/{sp}/units/{unit}/entities/{entity}/devices/{device}/shadow` (PUT) - MULTIPLE attributes in one call
- `/unit-entity-management/api/v1/sp/{sp}/unit/{unit}/generic/device/shadow` - generic multi-attribute write
- `/unit-entity-management/api/v1/sp/{sp}/units/{unit}/entities/{entity}/devices/{device}/command` - the working control write (section 4 E10, PUT, attributes envelope)
- `/unit-entity-management/api/v1/sp/{sp}/units/{unit}/devices/rtsp/support` - RTSP support query (E13)
- `/unit-entity-management/api/v1/sp/{sp}/units/{unit}/device/{device}/device-access-token` - device access token
- `/unit-entity-management/api/v1/sp/{sp}/units/{unit}/entities/{entity}/devices/{device}/wifi/config` - Wi-Fi lock config short URL
- `/unit-entity-management/api/v1/sp/{sp}/agent/device/mqtt/unit/{unit}/device/{device}` - MQTT status (E16; measured: status only, no credentials) and REMOVE_MQTT_CONNECTION (DELETE)
- `/unit-entity-management/api/v1/sp/{sp}/units/mpc/sync` - MPC sync
- `/unit-entity-management/api/v1/sp/{sp}/bulb/{id}/scene`, `/bulb/{id}/rule`, `/bulb/{id}/rule/state` - bulbs
- `/unit-entity-management/api/v1/sp/{sp}/smartglasses` - QuboGo smart glasses (CRUD)

**Streaming (stream-manager family)**
- `/stream-manager/api/v1/sp/{sp}/stream/unit/{unit}/device/{device}` + query `timestamp`, `type`, `streamSourceCamera`, `deviceType`, optional `singleRecord` - the live mint (E14); the app also has v2 and v3 variants of the family in native code
- `/stream-manager/api/v1/sp/{sp}/stream/audiocall/unit/{unit}/device/{device}` - the AUDIO (talkback) stream
- `/stream-manager/api/v1/sp/{sp}/unit/{unit}/devices/pp2p/credentials` - P2P credentials (the local fast path; native `updateP2PStream`)
- `/stream-manager/api/v1/sp/{sp}/stream/unit/{unit}/device/{device}/s3Files` - playable recorded files
- `/stream-manager/api/v1/sp/{sp}/stream/unit/{unit}/device/{device}/media/asset` - time-lapse HTTP URL
- `/stun/credentials` (native) - STUN credentials for P2P

**Events, recordings, VMS, content**
- `/control-monitor/api/v1/sp/{sp}/unit/{unit}/ooi-events` - events feed (device, recoUUID, from/to timestamp, objectType, records params)
- `/control-monitor/api/v1/sp/{sp}/lastEvent`, `/device/{device}/event` (POST/GET/DELETE)
- `/content-manager/api/.../unit/{unit}/device/{device}/calendar` - recording calendar; `.../units/{unit}/devices/{device}/events` (DELETE)
- `/vms/api/v1/sp/{sp}/units/{unit}/devices/{device}` - VMS events; `.../transactions/{id}/snapshot`, `.../transactions/{id}/contents/{cid}` - snapshots
- `/media-assets/audios` (+`/{id}/url`, `/upload`), `/media-assets/videos` (+`/{id}/url`, `/delete-many`), `/media-assets/clients/versions` - lullabies, time-lapse, app version
- `/push-pull/request` + `/push-pull/transaction/{id}` - SD-card footage download

**Energy, purifier, weather (control-monitor)**
- `/control-monitor/api/v1/sp/{sp}/device/{device}/energyMeasurement{Day,Hour,Month}Events`, `/energyMeasurementOverallData`, `/energy/metering` - energy
- `/control-monitor/api/v1/sp/{sp}/unit/{unit}/device/{device}/airpurifier/pm25` (+`/dayInsight`), `/airpurifier/outdoor/location`, `/weather/location` - purifier and weather
- `/control-monitor/api/v1/sp/{sp}/devices/cloud/dvr/device/{device}/setting` - cloud DVR subscription

**Rules, zones, ROI (rule-manager / roi-manager)**
- `/rule-manager/api/.../units/{unit}/devices/{device}` - schedules (create/get/delete)
- `/rule-manager/api/.../units/{unit}/devices/{device}/intruder/schedules` - intruder schedules (CRUD)
- `/rule-manager/api/.../units/{unit}/entities/{entity}/devices/{device}/proguard` (+`/state`, `/sync`, DELETE) - privacy zones (proguard)
- `/roi-manager/api/.../units/{unit}/roi`, `/units/{unit}/zones`, `/units/{unit}/detect/object` - regions of interest and object detection
- `/roi-manager/api/.../device/{device}/lock/pin` (+otp, refresh), `/fingerPrint`, `/rfid`, `/lock/users`, `/lock/count` - smart-lock management

**Locks, commissioning, OTA, subscriptions**
- `/commissioning/api/...` - device onboarding (photoframe, gw provision token, mac, recommissioning)
- `/ota-service/default/controller/v1/{id}` (+`/deploymentBase/{id}/feedback`), `/lc-ota-service/api/.../distribution/device/ota` - firmware OTA
- `/inventory/api/.../mac/{mac}/sku`, `/device/definition`, `/service/declaration/services`, `/gatewayIdentifier` - inventory
- `/plan-sub-management/zoho/...` - subscriptions, plans, coupons (Zoho-backed)
- `/qrcode-service/api/.../generateqr` - QR codes
- `/inapp-campaign/users/banners`, `/campaigns/action` - in-app campaigns
- `/storage-access-manager/api/.../genericUpload`, `/updateFile`, `/device/gallery/uploadFile`, `/device/gallery/file` - uploads
- `/server-status/api/{v}/available`, `/sms/api/.../user/{uuid}/server-status/token` - cloud health

**Known gaps after this extraction**
- The `api_key` header value for the `/users` and flat-shadow routes: not in the app's Java; likely in native code or unused by the app (those routes may be web-dashboard-only).
- Request BODIES for most writes: the constants give paths; bodies live in the app's repositories/viewmodels and are the next decompile pass. Entry points in the decompiled tree: `ui/modes/model/ModeCommandSceneModel.java` (command envelope builder), `ui/routine/selectAction/adapter/service/a.java` (textToSpeech action), `ui/recording/RecordingCameraActivity.java` and `ui/dashboard/fragment/vdb_v2/VDBViewFragmentV2.java` (recordingControl call sites).
- Per-device service/attribute schemas are extracted for every model (`assets/app/services-configuration/*_Services.json`); the HCP01 camera's full writable set is recorded in the live-notes below.

**Camera (HCP01) writable services, from the app's own config asset (2026-10-14)**
- `recordingConfig / triggerEventType` - recording settings
- `recordingControl / record` - recording on/off
- `cameraFunction / state` - camera on/off (privacy; verified live)
- `playAudio / playAudio` (audio) - play an audio asset on the camera
- `playAudio / playAudio` (siren) - the siren
- `textToSpeech / speak` (message) - speak a text message through the camera
- `playCmsAudio / cmsContentUuid` (lullaby) - play a lullaby
- `textsms / command` (message) - send a text notification to the phone

---

## Appendix B - The P2P protocol (measured 2026-10-14, on-device capture)

Everything below is read off a real capture (PCAPdroid on the phone) plus live probing. This is the app-speed path.

**Session allocation (cloud)**
- `POST /stream-manager/api/v2/sp/{sp}/unit/{unit}/devices/pp2p/credentials` - note **v2**, POST; GET returns 405. Body: `{"userAppID": "<registered app-device uuid>", "devices": ["<camera uuid>"]}` (a list of device UUID strings; object shapes are rejected).
- Response (per device): `[{appSessionId, aesKey, iv, serverIP, serverPort, protocol}]` - a dedicated relay endpoint **and the AES-128 key + IV the media is encrypted with**. The relay pool rotates per call (observed: 13.235.185.216, 13.203.165.201 - AWS Mumbai); each session gets its own port.

**Handshake (UDP to serverIP:serverPort)**
- The client sends a 52-byte SYN every ~300ms until answered:
  `0001 0020 244a2112 <13-byte peer name> 02 0004 00000000 0008 0014 <20-byte random nonce>`
- The full exchange is six packets per peer: simple SYN (52B) -> peer echo (`0101/0018`) -> extended SYN (60B, `0001 0028`, fresh 12B name = simple name's counter + 1, retransmitted every 200ms) -> peer's own probe (`0001/0018`, we ack) -> peer extended-ack (`0101/0020`) -> RTP. Both legs (relay and camera) follow the same grammar. Frame tables: `p2p/PROTOCOL.md` section 6.
- Peer names look like `<8 random alphanumerics><4-digit counter>p` and are regenerated by the app per run (observed `zTkp28PX1001p` in one capture, `6C23ENJU1001p` in the next) - they are not stable device identities.
- A standard STUN binding request (magic cookie 2112a442) also goes to the STUN server on port 5349.

**Media**
- Plain **RTP**, payload type 96, ~1219-byte datagrams (MTU-sized), with the payload AES-encrypted using the session's `aesKey`/`iv`.
- After the handshake the media can arrive **directly from the camera's public address** (observed incoming from a home-ISP address, not the relay) - hole punching works.

**The start-call link (RESOLVED 2026-10-14, captured live)**
- The missing link was the MQTT start call: the app publishes `startP2pCall` to `/control/<unit>/<camera>/p2pStreaming` right after allocation, and the camera answers `p2pCallStarted` on `/monitor/<unit>/<camera>/p2pStreaming`. Both directions captured live; our own replay client can arm the camera the same way (the camera answered every one of our calls).
- Calling the credentials endpoint alone does NOT arm the camera; the MQTT start call is required.
- **Resolution (2026-10-15)**: the wall is a cloud-side arming gate, proven from the camera's own bytes. Pairing is RELAY-INITIATED: the relay forwards a caller's SYN train to the peer's advertised session port, and the peer (if listening) answers. Capture evidence: the relay forwarded the genuine app's train to the camera (`13.203.165.201:46243 -> camera:55427`; refused only because that app session's camera-side listener had already torn down). For our REST-allocated sessions the relay never forwarded anything - not at 60s, not at 200s of held-open session, with byte-exact trains and every identity/header/network variable mirrored (windows 3-5 with full camera-side interception capture). Conclusion: the forwarding is armed server-side, tied to sessions created by the genuine app SDK, and unreachable from a standalone client. The standalone protocol work is complete and verified (`~/Projects/qubo-spike/p2p/PROTOCOL.md` + `p2p_client.py` + `FINDINGS.md`); whether the real app's genuine sessions inherit the arming is to be verified with the app build, and v1 does not depend on it - the cloud-streaming path already works.

**Decompiled (2026-10-14, Ghidra on the native libraries - dumps preserved in `~/Projects/qubo-spike/ghidra/`, plus a working arm64 decompiler binary)**
- The MQTT path is fully readable in the decompiled C now. Found so far: `GatewayManager::getDeviceAccessTokenUrlPath` builds `https://<server>/unit-entity-management/api/v1/sp/<sp>/units/<unit>/device/<device>/device-access-token`; `GatewayManager::createDeviceAccessTokenInDB` / `updateDeviceAccessTokenInDB` derive the stored `mqttPassword` via `hero::generatePasswordHash(<device access token>)` - so the MQTT password is computed locally from the token the cloud returns.
- The P2P signaling vocabulary is visible in the function set: `getP2pTokenFromCloud`, `getP2pSignallingUrl`, `sendP2pStartCallCommand`, `sendStartP2pStreamCommand`, `setAnswer`, `handleP2PEvents`, `handle_mqtt_message`, `getControlTopicForService`, `p2pDataMessage`, `sendPeerStatus`, `sendSessionSocketStatus`, `sendP2pPttHeartbeat` (talkback), plus JSON key strings (`appSessionId`, `cameraSessionId`, `offerData`, `offerResponse`, `callAccepted`, `mediaStreams`, `connectionProtocol`, ...).
- The remaining extraction (exact topics, payload schemas, src/dest semantics, the password-hash algorithm) is a bounded engineering task on these dumps; a dedicated chase worker is on it, and the live replay runs from the lead with real sessions.

**State observed on the camera during sessions**
- `streamControl`: the cloud-writable service that drives everything - `streamDestinationUrl` (the camera PUSHES rtsps to `wowza2.platform.quboworld.com/hero-iot-live/<device>_CAMERA_VOD0` when recording/live), `streamState`, `cameraSessionId`, `playlist` (SD-card recording paths: `/tmp/mnt/sdcard0/HERO_IOT_<sp>/Content/Recordings/<unit>/<date>/<device>_primary_<ts>.mp4`), quality, and `singleRecord`.
- `p2pStreaming` - the P2P session slot on the camera (empty unless the app's start-call runs).
- `supportedFeatures.p2pSupport = true`.
