# Qubo VOD verification (recording playback)

Attempted 2026-10-15, ~18:42-18:50 IST (13:12-13:20 UTC). Status: blocked before any live call. Nothing in this file is verified against the API yet; the decompile-derived shapes stay unverified until a re-run lands.

## Blocker chain

1. Phone-OTP login is the only usable auth path on this machine. The stored session (`~/.qubopy_session.json`) was minted today at 15:28 IST and expired at 16:28 IST, and the phone route issues no refreshToken.
2. Server-side confirmation that the stored token is dead, not just past its own exp claim. Read-only `GET /unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units` returned:

```
HTTP 401
{"code":"ERR_AUTH_002","message":["User Token expired, Access denied"]}
```

3. A fresh OTP needs the phone's SMS, and no adb device is attached:
   - `adb devices -l` lists nothing, before and after `adb kill-server` / `start-server`.
   - The USB bus (via `ioreg -p IOUSB`) carries only known peripherals: webcam, hubs, keyboard, trackpad, audio, and one unknown "AIRHUG 28" (vendor 0x2F9D, product 0x1014, USB 1.1 full speed) with no adb interface. No Android vendor ID (0x18D1/0x04E8/0x2717/0x2A70/0x05C6/0x0E8D) is present. `system_profiler SPUSBDataType` printed an empty tree here, which is wrong for this machine - do not trust it on its own.
   - No wireless adb target exists: no address recorded in the spike tree or history, and the PCAPdroid captures show the phone on mobile data (100.64.0.x), never on the home LAN (192.0.2.x).
4. No alternative login exists: no `QUBO_PASSWORD` in the environment or shell rc, no other `.qubopy_session.json` on the machine, `qubopy login` needs the account password (not held), and qubopy has no Google sign-in tooling.

## Steps 5, 7, 8

Not executed, so no statuses or bodies to record. Unchanged, still decompile-derived:

- Calendar: `GET /content-manager/api/v1/sp/{sp}/unit/{unit}/device/{device}/calendar?fromTimeStamp&toTimeStamp&timezone` (timezone format and response schema unverified).
- VOD mint: `POST/PUT /stream-manager/api/v2/sp/{sp}/stream/unit/{unit}/device/{device}?timestamp=<clip-start-epoch-ms>&type=high&streamSourceCamera=primary&singleRecord=0`, body `{"userAppID": "<~/.qubopy_device_id>"}` (method, streaming-path v2, and streamUrl/appSessionId key spellings unverified).
- ffprobe on the returned URL: unverified. The verified live mint (2026-10-13) produces `rtsps://` URLs; whether VOD does the same is exactly what this probe was to settle.

## Unblock

Re-run with the phone attached over USB and adb debugging on. Phone sessions last one hour, so request the OTP and run the probe in the same sitting. Everything else (calendar, mint, ffprobe) is then a single pass.
