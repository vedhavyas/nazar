# Qubo VOD verification (recording playback)

**VERIFIED 2026-10-16, 07:07-07:20 IST** against a live session (the hub's own OTP login, 06:54 IST; token valid one hour). Probes ran from the Studio over the hub's session file, with the six qubopy headers (`Content-Type`, `Authorization: Bearer`, `Token-Type: USER`, `User-UUID`, `Subscriber-Key`, `Source-Device-Id`, `App-Id: com.hero.iot`, `Source: Android`) - without `User-UUID`/`Token-Type` every call answers `ERR_AUTH_001`.

Blocked attempt on 2026-10-15 is superseded; its blocker chain stays below for the record.

## Calendar (recorded days)

`GET /content-manager/api/v1/sp/{sp}/unit/{unit}/device/{dev}/calendar?fromTimeStamp=<ms>&toTimeStamp=<ms>&timezone=Asia/Kolkata` -> **200**.

Body is a bare JSON array of day strings in **DD-MM-YYYY**, **unordered**:

```json
["14-10-2026","13-10-2026","16-10-2026","15-10-2026"]
```

`timezone=Asia/Kolkata` works first try. The server must sort; camera c0000000 (Cam360) has 13-16 Oct recorded.

## VOD mint

`POST /stream-manager/api/v2/sp/{sp}/stream/unit/{unit}/device/{dev}?timestamp=<ms>&type=high&streamSourceCamera=primary&singleRecord=0` with body `{"userAppID": "<the session's device_id>"}` -> **200. POST works; no PUT fallback needed.**

```json
{"appSessionId":"d0000000-0000-4000-8000-000000000013","streamURL":"rtsps://wowza2.platform.quboworld.com:443/hero-iot-live/<device_id>?wowzatoken=...&sourceDevice=<camera>&sessionId=<appSessionId>&userUUID=<uuid>&unitUUID=<unit>&?vodContent=true","instanceId":"0"}
```

Key spellings: **`streamURL`** (capital URL), `appSessionId`, `instanceId`. Host is **wowza2** (live is wowza) - playback and live do not share a slot. The literal `&?vodContent=true` suffix is theirs, not a typo. A timestamp in any recorded stretch mints; 2h back from now played.

## Playback probe (ffprobe)

`ffprobe -rtsp_transport tcp` on the returned URL: **rc 0**, streams = **aac + h264 640x368**. Recordings are already H.264 at a low rendition - the live path's HEVC transcode is NOT needed for VOD; the pipeline's h264 output is a pass-through here.

## Clip list: no server endpoint exists

Ten path candidates under `/content-manager/api/v1/sp/{sp}/unit/{unit}/device/{dev}/` (recordings, recording, metadata, content, media, list, clips, timeline, events, vod) all **404**; the native libs' path strings carry the module bases only. The app's own timeline (`RecordingManager.getRecordedContentMetaData` is a native range query) is a client-side metadata cache fed by the MQTT event stream.

**Shape for M3:** days from the calendar, time-seek by re-minting at any timestamp, and clip segments from the server's own event store (the M3 notification store's motion events - `recordingControl` carries `record: start` + `imageFileName` per event). This matches the app's own timeline behaviour.

## Measured stream-session semantics (live path, same scripts)

- Mints are **last-mint-wins**: one stream slot per device path; minting low while high plays leaves the old high URL serving the NEW stream (old URL's old token still opens the path, content = latest mint).
- An idle stream dies fast: **404 within ~40s of its last consumer leaving** (measured with no second mint - not a collision).
- Consequence for M2: hero-high + tiles-low via two mints is impossible; one mint plus a go2rtc-side second rendition is the shape (folded into the M2 plan).

## Superseded 2026-10-15 blocker chain (for the record)

1. Phone-OTP login was the only auth path; the stored session expired hourly with no refresh.
2. `GET /units` with the expired token: `HTTP 401 {"code":"ERR_AUTH_002","message":["User Token expired, Access denied"]}`.
3. No adb device attached; no wireless adb target; no alternative login.
4. Unblock: a fresh session - which arrived 2026-10-16 06:54 IST via the hub's own login, no phone needed.
