"""Qubo P2P reference signaling client - speaks the exact wire shape captured 2026-10-14.

Extracted from the decompiled native libraries (libcore-main/libcore-stream/libcore-mobile)
and validated against a live MQTT capture (mqtt_probe.log, 16:05-16:07 IST: the app's full
startP2pCall and the camera's p2pCallStarted answer). See PROTOCOL.md for every claim and
FINDINGS.md for what still needs the live run. Uncertainty is marked inline with
[UNCERTAIN ...] comments naming the evidence.

Commands:
  selftest   offline: validates the payload builder against the capture, the SYN against
             Appendix B, the event parser against the captured camera answer, and the FU-A
             reassembler - no network
  watch      MQTT only: subscribe like the app and print every message
  token      device-access-token call only
  flow       the real chain: allocate -> publish startP2pCall -> consume p2pCallStarted ->
             UDP handshake (camera candidates + relay) -> RTP -> AES decrypt -> H.264 decode
             --dry-run stops before the publish and diffs the payload against the capture

The Python client cannot produce a P2P-SDK-grade candidate (no STUN/ICE in this script): it
offers its own LAN host candidate, which is what a same-LAN run needs (the capture's app
offered a VPN host 192.0.0.2 and the camera answered from 192.0.2.21 - a client on the
camera's LAN should pair directly).
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import json
import os
import secrets
import socket
import sys
import time
import uuid as uuid_mod
from typing import Any

import aiohttp

from qubopy import QuboAuth, QuboClient

# From the app's own config (jadx_out U5/a.java) - PROTOCOL.md 1.1
HTTP_BASE = "https://srvcapp.platform.quboworld.com:443"
MQTT_BROKER = "ssl://mqtt.platform.quboworld.com:8883"

DEFAULT_SP = "d10e4bfb0153496e8e8bb955f7ebe413"
SESSION_PATH = os.path.expanduser("~/.qubopy_session.json")
CAPTURE_PATH = "/Users/vedhavyas/Projects/qubo-spike/mqtt_probe.log"

# The captured session's identities (mqtt_probe.log + the unit device list).
CAPTURE_UNIT = "20000000-0000-4000-8000-000000000001"
CAPTURE_CAMERA = "c0000000-0000-4000-8000-000000000001"      # Cam360 3MP
CAPTURE_CAMERA_CONTROLLER = "d0000000-0000-4000-8000-000000000001"  # token dest + handleName
CAPTURE_SELF_DEVICE = "d0000000-0000-4000-8000-000000000002"  # the phone, unit "mobile" device
CAPTURE_SRC_DEVICE_ID = "<mobile-id>"                    # the phone's Android ID


# --------------------------------------------------------------------------------------
# MQTT connect parameters (PROTOCOL.md 1.3/1.6 - confirmed on the wire 16:05 IST)
# --------------------------------------------------------------------------------------


def build_client_id(device_name: str, unit_uuid: str, role: str = "SimpleMQTTClient") -> str:
    """CLIENT_<name>_<role>_<unitUUID>.

    MainMqttClient's role string is "MainMqttClient", the P2P/control client's is
    "SimpleMQTTClient" (createConnection, libcore-rest 0x129858). Keep the clientId distinct
    from the app's own (same broker) so neither side kicks the other's session.
    """
    return f"CLIENT_{device_name}_{role}_{unit_uuid}"


def derive_mqtt_password(device_access_token: str) -> str:
    """mqttPassword = base64(PBKDF2-HMAC-SHA256(token, base64(rand12), 901, 24)).

    [UNCERTAIN mqttPassword] salt is random and not persisted - cannot be re-derived.
    """
    salt = base64.b64encode(secrets.token_bytes(12))
    dk = hashlib.pbkdf2_hmac("sha256", device_access_token.encode(), salt, 901, dklen=24)
    return base64.b64encode(dk).decode()


# --------------------------------------------------------------------------------------
# REST helpers
# --------------------------------------------------------------------------------------


async def fetch_device_access_token(
    session: aiohttp.ClientSession, headers: dict[str, str], sp: str, unit: str,
    self_device_uuid: str, dest_controller_uuid: str,
) -> str | None:
    """POST .../units/<unit>/device/<selfDeviceUUID>/device-access-token {"src","dest"}.

    src = the self device uuid, dest = the TARGET DEVICE'S controllerUUID (capture proof:
    the issued tokens carry dest = the camera's controllerUUID d0000000, PROTOCOL.md 2).
    The response key is "deviceAccessToken" (the MQTT push uses exactly that name; the REST
    body key is [OPEN] but structurally the same).
    """
    url = (f"{HTTP_BASE}/unit-entity-management/api/v1/sp/{sp}/units/{unit}"
           f"/device/{self_device_uuid}/device-access-token")
    body = {"src": self_device_uuid, "dest": dest_controller_uuid}
    async with session.post(url, headers={**headers, "Content-Type": "application/json"},
                            json=body, timeout=aiohttp.ClientTimeout(total=20)) as r:
        text = await r.text()
        print(f"[token] HTTP {r.status} body={text[:500]}")
        if r.status != 200:
            return None
        try:
            data = json.loads(text)
        except ValueError:
            return None
        if isinstance(data, dict):
            for key in ("deviceAccessToken", "device_access_token", "accessToken", "token"):
                if isinstance(data.get(key), str) and data[key]:
                    print(f"[token] using response key {key!r}")
                    return data[key]
        return None


async def allocate_credentials_v2(
    session: aiohttp.ClientSession, headers: dict[str, str], sp: str, unit: str,
    user_app_id: str, camera_uuid: str,
) -> dict[str, Any] | None:
    """POST /stream-manager/api/v2/sp/<sp>/unit/<unit>/devices/pp2p/credentials.

    Body {"userAppID": <the app self-device uuid>, "devices": [camera]} (Appendix B,
    measured live). Response per device: appSessionId, aesKey, iv, serverIP, serverPort,
    protocol - plus the P2P username/password pair the app saves (an allocation credential,
    NOT a broker credential - the broker rejects it).
    """
    url = f"{HTTP_BASE}/stream-manager/api/v2/sp/{sp}/unit/{unit}/devices/pp2p/credentials"
    body = {"userAppID": user_app_id, "devices": [camera_uuid]}
    async with session.post(url, headers={**headers, "Content-Type": "application/json"},
                            json=body, timeout=aiohttp.ClientTimeout(total=20)) as r:
        text = await r.text()
        print(f"[pp2p] HTTP {r.status} body={text[:700]}")
        if r.status != 200:
            return None
        try:
            data = json.loads(text)
        except ValueError:
            return None
        if isinstance(data, list) and data:
            return data[0]
        if isinstance(data, dict):
            inner = data.get("data")
            if isinstance(inner, list) and inner:
                return inner[0]
            return data
        return None


async def allocate_p2p_v3(
    session: aiohttp.ClientSession, headers: dict[str, str], sp: str, unit: str,
    camera_uuid: str, user_app_id: str, app_session_id: str, is_ptt: bool = False,
) -> dict[str, Any] | None:
    """POST /stream-manager/api/v3/sp/<sp>/unit/<unit>/device/<cam>/p2p

    The native streaming path's allocation (getP2pTokenFromCloud): body
    {"userAppID": <the app device>, "appSessionId": "", "isPTT": false} - the request sends
    an EMPTY appSessionId (the decompile initialises an empty string just before the call;
    the real id comes back in the response) - and the response carries appSessionId,
    aesKey, iv, serverIP, serverPort (PROTOCOL.md 5.2). This is the session the cloud links
    to the camera; the v2 pp2p/credentials route is the local/fast path whose sessions were
    measured never to draw a relay answer.
    """
    url = (f"{HTTP_BASE}/stream-manager/api/v3/sp/{sp}/unit/{unit}/device/{camera_uuid}/p2p"
           f"?isPTT={'true' if is_ptt else 'false'}")
    body = {"userAppID": user_app_id, "appSessionId": app_session_id, "isPTT": is_ptt}
    async with session.post(url, headers={**headers, "Content-Type": "application/json"},
                            json=body, timeout=aiohttp.ClientTimeout(total=20)) as r:
        text = await r.text()
        print(f"[p2p-v3] HTTP {r.status} body={text[:700]}")
        if r.status != 200:
            return None
        try:
            data = json.loads(text)
        except ValueError:
            return None
        return data if isinstance(data, dict) else None


# --------------------------------------------------------------------------------------
# The start command - exact wire shape (captured 16:06:02 IST, PROTOCOL.md 4.3)
# --------------------------------------------------------------------------------------


def build_candidate_list(lan_ip: str, port: int, srflx: list[str] | None = None) -> str:
    """offerData as the app sends it: a STRING containing a JSON array of candidate dicts.

    Each candidate: {"host": "ip:port", "protocol": "UDP", "srflx_N": "ip:port"} - host is
    the LAN address, srflx_N the STUN-reflexive (public) ones. No STUN here, so srflx
    entries appear only when passed via --srflx.
    """
    cand: dict[str, Any] = {"host": f"{lan_ip}:{port}", "protocol": "UDP"}
    for i, s in enumerate(srflx or []):
        cand[f"srflx_{i}"] = s
    return json.dumps([cand], indent="\t")


def build_start_call_payload(
    *, camera: str, self_device: str, src_device_id: str,
    session_id: str, offer_data: str, aes_key: str, aes_iv: str,
    protocols: str = "udp", quality: str = "high", media: str = "both",
    ptt: bool = False,
) -> dict[str, Any]:
    """The native sendCommand envelope, byte-for-byte per the capture.

    Key names come from the initializer table; the shape from mqtt_probe.log: parameters
    carries useTls(null) + playTimestamp(""), pttEnabled is a JSON boolean, and the top
    level has timestamp/deviceUUID/srcDeviceId/msgSequenceId/clientId.
    """
    ts = int(time.time() * 1000)
    params: dict[str, Any] = {
        "aesIV": aes_iv,
        "aesKey": aes_key,
        "mediaStreams": media,
        "offerData": offer_data,
        "protocols": protocols,
        "pttEnabled": ptt,
        "sessionId": session_id,
        "useTls": None,
        "streamSourceQuality": quality,
        "sourceType": "live",
        "playTimestamp": "",
        "mobileUUID": self_device,
    }
    return {
        "command": {
            "devices": {
                "services": {
                    "p2pStreaming": {
                        "commands": {
                            "startP2pCall": {"parameters": params, "instanceId": 0}
                        }
                    }
                },
                "deviceUUID": camera,
            }
        },
        "timestamp": ts,
        "deviceUUID": camera,
        "srcDeviceId": src_device_id,
        "msgSequenceId": ts,
        "clientId": str(uuid_mod.uuid4()),
    }


def build_end_call_payload(*, camera: str, src_device_id: str,
                           session_id: str) -> dict[str, Any]:
    """sendEndP2pCallCommand (ghidra_stream2.txt:10050-10082): service p2pStreaming, action
    endP2pCall, parameters {sessionId} - the app always tears its session down; abandoning
    one leaves it live on the camera until it expires ("p2p session limit reached" logs)."""
    ts = int(time.time() * 1000)
    return {
        "command": {
            "devices": {
                "services": {
                    "p2pStreaming": {
                        "commands": {
                            "endP2pCall": {"parameters": {"sessionId": session_id},
                                           "instanceId": 0}
                        }
                    }
                },
                "deviceUUID": camera,
            }
        },
        "timestamp": ts,
        "deviceUUID": camera,
        "srcDeviceId": src_device_id,
        "msgSequenceId": ts,
        "clientId": str(uuid_mod.uuid4()),
    }


def build_reboot_payload(*, camera: str, src_device_id: str) -> dict[str, Any]:
    """deviceReboot/reboot (DeviceSettingActivity.java:984/3974): the app's own reboot button,
    published on /control/<unit>/<camera>/deviceReboot with empty parameters - used to force
    the camera's WiFi reassociation for an air-side capture window."""
    ts = int(time.time() * 1000)
    return {
        "command": {
            "devices": {
                "services": {
                    "deviceReboot": {
                        "commands": {"reboot": {"parameters": {}, "instanceId": 0}}
                    }
                },
                "deviceUUID": camera,
            }
        },
        "timestamp": ts,
        "deviceUUID": camera,
        "srcDeviceId": src_device_id,
        "msgSequenceId": ts,
        "clientId": str(uuid_mod.uuid4()),
    }


def parse_capture_start_call(path: str) -> dict[str, Any] | None:
    """Pull the app's startP2pCall payload out of the captured log (for the selftest diff)."""
    with open(path) as f:
        for line in f:
            if "/p2pStreaming QoS" in line and "startP2pCall" in line:
                try:
                    return json.loads(line.split("| ", 1)[1].strip())
                except (ValueError, IndexError):
                    continue
    return None


# --------------------------------------------------------------------------------------
# The camera's answer - exact wire shape (captured 16:06:03 IST, PROTOCOL.md 5.4)
# --------------------------------------------------------------------------------------


def parse_p2p_callback(topic: str, payload: str, session_id: str) -> dict[str, Any] | None:
    """Parse a /monitor/<unit>/<cam>/p2pStreaming answer for OUR sessionId.

    Wire evidence: devices.services.p2pStreaming.events.p2pCallStarted.parameters
    {offerResponse (stringified candidate array), sessionId}; handleName = camera
    controllerUUID; timestamps epoch-ms.
    """
    if "/p2pStreaming" not in topic:
        return None
    try:
        data = json.loads(payload)
    except ValueError:
        return None
    devices = data.get("devices")
    if not isinstance(devices, dict):
        return None
    events = (devices.get("services", {}).get("p2pStreaming", {}) or {}).get("events", {})
    for event_name, event in (events or {}).items():
        params = (event or {}).get("parameters", {})
        if params.get("sessionId") != session_id:
            continue
        try:
            candidates = json.loads(params.get("offerResponse", "[]"))
        except ValueError:
            candidates = []
        return {
            "event": event_name,
            "session_id": params.get("sessionId"),
            "handle_name": devices.get("handleName"),
            "candidates": candidates,
            "raw": params,
        }
    return None


def candidate_endpoints(candidates: list[dict[str, Any]]) -> list[tuple[str, int]]:
    """Flatten a candidate list into (ip, port) tuples - host first, then srflx_N."""
    out: list[tuple[str, int]] = []
    for c in candidates:
        for key in ("host",) + tuple(sorted(k for k in c if k.startswith("srflx_"))):
            v = c.get(key)
            if isinstance(v, str) and ":" in v:
                ip, port = v.rsplit(":", 1)
                try:
                    out.append((ip, int(port)))
                except ValueError:
                    pass
    seen: set[tuple[str, int]] = set()
    return [e for e in out if not (e in seen or seen.add(e))]


# --------------------------------------------------------------------------------------
# Relay handshake (Appendix B, verified on the wire there)
# --------------------------------------------------------------------------------------

STUN_COOKIE = bytes.fromhex("244a2112")


async def discover_srflx(sock: socket.socket, server: str = "stun.l.google.com:19302",
                         timeout: float = 2.5) -> str | None:
    """One-shot STUN binding request from OUR media socket -> "ip:port" as the OS's NAT sees it.

    Sent from the same socket that carries our candidate port, so the mapped port is the one
    the camera should punch to (Appendix B: the app's own srflx candidates look exactly like
    this). Failure returns None - the run then leans on the relay leg.
    """
    import struct

    loop = asyncio.get_running_loop()
    host, port = server.rsplit(":", 1)
    try:
        infos = await loop.getaddrinfo(host, int(port), type=socket.SOCK_DGRAM)
        addr = infos[0][4][:2]
    except OSError:
        return None
    txid = secrets.token_bytes(12)
    req = struct.pack(">HHI", 0x0001, 0, 0x2112A442) + txid
    try:
        sock.sendto(req, addr)
        data, _ = await asyncio.wait_for(loop.sock_recvfrom(sock, 2048), timeout)
    except (asyncio.TimeoutError, OSError):
        return None
    if len(data) < 20 or data[0:2] != b"\x01\x01":  # binding success response
        return None
    i = 20
    while i + 4 <= len(data):
        atype, alen = struct.unpack_from(">HH", data, i)
        val = data[i + 4:i + 4 + alen]
        if atype == 0x0020 and len(val) >= 8:  # XOR-MAPPED-ADDRESS
            xport = struct.unpack(">H", val[2:4])[0] ^ 0x2112
            xip = bytes(b ^ c for b, c in zip(val[4:8], b"\x21\x12\xa4\x42"))
            return f"{socket.inet_ntoa(xip)}:{xport}"
        if atype == 0x0001 and len(val) >= 8:  # MAPPED-ADDRESS fallback
            port_n = struct.unpack(">H", val[2:4])[0]
            return f"{socket.inet_ntoa(val[4:8])}:{port_n}"
        i += 4 + alen + ((4 - alen % 4) % 4)
    return None


def build_syn(peer_name: str, nonce: bytes) -> bytes:
    """52-byte simple SYN, capture-exact. STUN framing (magic 244a2112) where what reads as
    a 13-byte "name" is a 12-byte txid (8 alnum + 4 digits) plus the first attribute byte:

    0001 0020 244a2112 <txid12> 70 02 0004 00000000 0008 0014 <20-byte nonce>
    """
    if len(peer_name) != 13:
        raise ValueError("txid is 8 random alnum + 4-digit counter (+ the attr byte 'p')")
    if len(nonce) != 20:
        raise ValueError("nonce is 20 bytes")
    return (b"\x00\x01" + b"\x00\x20" + STUN_COOKIE + peer_name.encode()
            + b"\x02" + b"\x00\x04" + b"\x00\x00\x00\x00"
            + b"\x00\x08" + b"\x00\x14" + nonce)


def build_extended_syn(peer_name: str, nonce: bytes) -> bytes:
    """60-byte session SYN - the packet the camera waits for before it starts RTP.

    0001 0028 244a2112 <12-byte name> 0025 0004 00000000 7002 0004 00000000 0008 0014 <nonce>

    Capture-exact (09:27 frame 6301): sent 2ms after the camera's 0101 echo and retransmitted
    byte-identical every 200ms; the camera's 0101/0020 ack of THIS packet (f6317) is followed
    by RTP 37ms later. The 12-byte field is a fresh txid (counter = the simple SYN's +1).
    """
    if len(peer_name) != 12:
        raise ValueError("extended-SYN txid is 8 alnum + 4 digits (12 bytes)")
    if len(nonce) != 20:
        raise ValueError("nonce is 20 bytes")
    return (b"\x00\x01" + b"\x00\x28" + STUN_COOKIE + peer_name.encode()
            + b"\x00\x25" + b"\x00\x04" + b"\x00\x00\x00\x00"
            + b"\x70\x02" + b"\x00\x04" + b"\x00\x00\x00\x00"
            + b"\x00\x08" + b"\x00\x14" + nonce)


_NAME_COUNTER = [secrets.randbelow(60) + 1040]  # live sessions run in the 1000s (pcap: 1055)


def random_peer_name() -> str:
    """Fresh 8-alnum prefix + the next 4-digit counter + 'p' (13 bytes = 12B txid + attr byte).

    The counter is global and monotonic, so two consecutive calls produce a simple/extended
    pair the way the SDK's txid generator does: pcap 09:27 camera flow counts 1055 (simple
    SYN) then 1056 (extended SYN), with a fresh random prefix each generation.
    """
    import random
    prefix = "".join(random.choice("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789") for _ in range(8))
    _NAME_COUNTER[0] += 1
    return f"{prefix}{_NAME_COUNTER[0]}p"


def build_register(name: str) -> bytes:
    """24-byte TURN-connect message (the "register"): `2004 0004 <cookie> <txid12> 7005 0000`.

    UDPTransport::sendTurnConnectMsg builds exactly this (type 0x2004 + one empty string
    attribute 0x7005) and sends it to the session's TURN endpoint - relay only, never the
    camera. Capture: sent 2.1s into the SYN train; the relay's first reply follows within
    ~0.6s (09:27 frame 893, 09:53 frame 1201).
    """
    return b"\x20\x04\x00\x04" + STUN_COOKIE + name.encode() + b"\x05\x00\x00"


def build_peer_ack(peer_name: bytes, nonce: bytes) -> bytes:
    """44-byte reply to the relay's answer: 0101 0018 <cookie> <peer name> 0008 0014 <nonce>.

    Capture proof (09:53 frame 1245): 0.3ms after the relay's 0001 answer the app sent this,
    echoing the peer's 12-byte name with a fresh 20-byte nonce of its own.
    """
    if len(peer_name) != 12 or len(nonce) != 20:
        raise ValueError("peer-ack: 12-byte name + 20-byte nonce")
    return (b"\x01\x01" + b"\x00\x18" + STUN_COOKIE + peer_name
            + b"\x00\x08" + b"\x00\x14" + nonce)


def parse_relay_reply(data: bytes) -> dict[str, Any] | None:
    """44-byte answer: 0001 0018 244a2112 <peer name> 0008 0014 <20-byte nonce>.

    [UNCERTAIN] Appendix B's field lengths add to 45 while the capture measured 44; the
    parser is lenient - cookie checked, trailing 20 bytes taken as the nonce. Tighten once a
    live reply is seen.
    """
    if len(data) < 40 or data[4:8] != STUN_COOKIE:
        return None
    return {"peer_name": data[8:-24], "nonce": data[-20:], "raw": data.hex()}


async def udp_exchange(sock: socket.socket, targets: list[tuple[str, int]], peer_name: str,
                       timeout: float = 10.0,
                       relay: tuple[str, int] | None = None,
                       relay_first: float = 0.0,
                       require_relay_engagement: bool = True) -> Any:
    """Run the app's handshake: relay leg first, the direct leg only once the relay engages.

    Capture-exact behaviour (three captures, the 20:52 one on this LAN):
    - Relay leg: plain SYN train (same txid every ~300ms) from t0, 0x2004 register at +2.1s,
      extended SYN after the relay's echo (or +3s). The relay answers a flow only after
      that flow registered: probe ~0.6-0.8s later, then an echo of the train's txid.
    - Direct (camera) leg: fires ONLY after the relay has engaged this socket - the app
      never sends a direct SYN before that (20:52: relay probe 6.803s -> direct SYN 6.816s;
      09:27: relay engaged 6.5s -> direct 39.7s). Each path gets ONE simple SYN with a
      FRESH txid and no repeat inside a full second; the camera echoed in 47-96ms.
    - The extended SYN goes once that SYN's echo lands (or 0.5s after a camera probe), to
      the same path, repeated every ~200ms up to 4x until the 0101/0020 ack. Session-up is
      that ack (or RTP); probes and echoes alone are not.
    """
    loop = asyncio.get_running_loop()
    t0 = loop.time()
    # No STUN is sent to the relay: the captures' relay legs open with the plain SYN, and
    # the only STUN is a real binding (magic 2112a442) to the STUN server 3.6.42.244:5349.
    relay_syn = build_syn(peer_name, secrets.token_bytes(20))
    relay_ext = build_extended_syn(random_peer_name()[:12], secrets.token_bytes(20))
    register = build_register(random_peer_name())
    reg_at = t0 + 2.1
    reg_sent = False
    deadline = t0 + timeout
    printed = set()
    relay_engaged = relay is None or not require_relay_engagement
    relay_echo = False
    paths: dict[tuple[str, int], dict] = {}
    print(f"[udp] handshake: relay txid={peer_name}"
          + ("" if relay_engaged else " (direct leg waits for relay engagement)"))
    while loop.time() < deadline:
        now = loop.time()
        if relay:
            # The capture's relay leg runs the plain SYN train (same txid every packet)
            # through the whole handshake - the relay answered the first SYN after the
            # register.
            try:
                sock.sendto(relay_syn, relay)
                if relay_echo or now - t0 >= 3.0:
                    sock.sendto(relay_ext, relay)
            except OSError:
                pass
        if not reg_sent and now >= reg_at:
            if relay:
                try:
                    sock.sendto(register, relay)
                except OSError:
                    pass
            reg_sent = True
            print(f"[udp] sent 0x2004 registration ({register.hex()})")
        if relay_engaged and now - t0 >= relay_first:
            for t in targets:
                st = paths.setdefault(t, {"txid": None, "syn": None, "sent": 0.0,
                                          "tries": 0, "echo": False, "probed": 0.0,
                                          "ext": None, "ext_acked": False})
                if not st["echo"] and (st["syn"] is None
                                       or (st["tries"] < 4 and now - st["sent"] > 1.0)):
                    st["txid"] = random_peer_name()
                    st["syn"] = build_syn(st["txid"], secrets.token_bytes(20))
                    st["sent"] = now
                    st["tries"] += 1
                    try:
                        sock.sendto(st["syn"], t)
                    except OSError:
                        pass
                    print(f"[udp] simple SYN -> {t[0]}:{t[1]} txid={st['txid']} "
                          f"(try {st['tries']})")
                if not st["ext_acked"] and (st["echo"]
                                            or (st["probed"] and now - st["probed"] > 0.5)):
                    es = st["ext"]
                    if es is None:
                        es = {"txid": random_peer_name()[:12], "sent": 0.0, "tries": 0}
                        es["syn"] = build_extended_syn(es["txid"], secrets.token_bytes(20))
                        st["ext"] = es
                        print(f"[udp] extended SYN -> {t[0]}:{t[1]} txid={es['txid']}")
                    if es["tries"] == 0 or (es["tries"] < 4 and now - es["sent"] > 0.2):
                        es["sent"] = now
                        es["tries"] += 1
                        try:
                            sock.sendto(es["syn"], t)
                        except OSError:
                            pass
        try:
            data, addr = await asyncio.wait_for(loop.sock_recvfrom(sock, 4096), timeout=0.25)
        except asyncio.TimeoutError:
            continue
        if relay and (addr[0], addr[1]) == relay and not relay_engaged:
            relay_engaged = True
            print(f"[udp] relay engaged: first packet from {relay[0]}:{relay[1]} "
                  "- direct leg released")
        if len(data) >= 8 and data[4:8] == STUN_COOKIE:
            if data[0:2] == b"\x00\x01" and len(data) == 44:
                # The peer's own probe; answer 0101/0018 echoing its txid, as the app does.
                ack = build_peer_ack(data[8:20], secrets.token_bytes(20))
                try:
                    sock.sendto(ack, addr)
                except OSError:
                    pass
                st = paths.get((addr[0], addr[1]))
                if st is not None:
                    st["probed"] = loop.time()
                if ("probe", addr) not in printed:
                    printed.add(("probe", addr))
                    print(f"[udp] probe from {addr[0]}:{addr[1]} txid={data[8:20]!r} "
                          f"-> 0101 ack")
                continue
            if data[0:2] == b"\x01\x01" and len(data) == 52:
                for st in paths.values():
                    es = st["ext"]
                    if es and data[8:20] == es["txid"][:12].encode():
                        print(f"[udp] extended SYN acked by {addr[0]}:{addr[1]} "
                              f"txid={data[8:20]!r} - session up, media expected in ~40ms")
                        return {"addr": addr, "txid": data[8:20], "raw": data.hex()}
                continue
            if data[0:2] == b"\x01\x01" and len(data) == 44:
                if relay and (addr[0], addr[1]) == relay:
                    relay_echo = True
                ours = False
                for st in paths.values():
                    if st["txid"] and data[8:20] == st["txid"][:12].encode():
                        st["echo"] = True
                        ours = True
                if ("echo", addr) not in printed or ours:
                    printed.add(("echo", addr))
                    print(f"[udp] SYN echo from {addr[0]}:{addr[1]} txid={data[8:20]!r}"
                          + (" (ours)" if ours else ""))
                continue
            if addr not in printed:
                printed.add((addr, len(data)))
                print(f"[udp] {len(data)}B from {addr[0]}:{addr[1]}: {data[:32].hex()}...")
            continue
        if len(data) >= 12 and data[0] >> 6 == 2:
            # RTP already flowing (ack lost or camera armed early) - hand over to media.
            print(f"[udp] RTP from {addr[0]}:{addr[1]} before a clean ack - proceeding")
            return {"addr": addr, "txid": b"", "raw": data.hex()}
    if not relay_engaged:
        print(f"[udp] relay never engaged in {timeout}s - the direct leg was held for the "
              "whole window, as the app holds it (FINDINGS.md, 20:52 capture)")
    else:
        print(f"[udp] no session ack from {targets} in {timeout}s "
              "(camera not armed, or every candidate unreachable - see FINDINGS.md)")
    return None


# --------------------------------------------------------------------------------------
# RTP + AES + H.264 (the media leg)
# --------------------------------------------------------------------------------------


def decode_key16(s: str) -> bytes:
    """Session aesKey/aesIV: captured as 32 uppercase hex chars; also accept base64."""
    try:
        raw = bytes.fromhex(s)
        if len(raw) == 16:
            return raw
    except ValueError:
        pass
    try:
        raw = base64.b64decode(s)
        if len(raw) == 16:
            return raw
    except Exception:
        pass
    return s.encode()[:16].ljust(16, b"\0")


def rtp_header_len(pkt: bytes) -> int:
    """RTP header length incl. CSRC list and (if present) the extension header."""
    if len(pkt) < 12:
        return 0
    cc = pkt[0] & 0x0F
    off = 12 + 4 * cc
    if pkt[0] & 0x10 and len(pkt) >= off + 4:  # extension bit
        ext_words = int.from_bytes(pkt[off + 2:off + 4], "big")
        off += 4 + 4 * ext_words
    return off


class H264Assembler:
    """RFC 6184 depacketizer: single NAL (1-23), STAP-A (24), FU-A (28)."""

    def __init__(self) -> None:
        self.buf = bytearray()
        self.nal_count = 0
        self.fu_state: bytes | None = None
        self.saw_sps = self.saw_pps = self.saw_idr = False

    def feed(self, payload: bytes) -> list[bytes]:
        """Returns NAL units that completed with this payload."""
        out: list[bytes] = []
        if not payload:
            return out
        nal_type = payload[0] & 0x1F
        if 1 <= nal_type <= 23:
            out.append(payload)
        elif nal_type == 24:  # STAP-A
            i = 1
            while i + 2 <= len(payload):
                size = int.from_bytes(payload[i:i + 2], "big")
                nal = payload[i + 2:i + 2 + size]
                if nal:
                    out.append(nal)
                i += 2 + size
        elif nal_type == 28 and len(payload) >= 3:  # FU-A
            start, end = payload[1] & 0x80, payload[1] & 0x40
            if start:
                self.fu_state = bytes([(payload[0] & 0xE0) | (payload[1] & 0x1F)])
                self.buf = bytearray(self.fu_state)
            if self.fu_state is not None:
                self.buf += payload[2:]
                if end:
                    out.append(bytes(self.buf))
                    self.fu_state = None
                    self.buf = bytearray()
        for nal in out:
            self.nal_count += 1
            t = nal[0] & 0x1F
            self.saw_sps |= t == 7
            self.saw_pps |= t == 8
            self.saw_idr |= t == 5
        return out


async def receive_media(sock: socket.socket, aes_key: str, aes_iv: str, seconds: float,
                        aes_mode: str, try_decode: bool, out_dir: str = "/tmp") -> None:
    """Receive RTP, decrypt, depacketize H.264, decode the first frame when PyAV is present.

    [UNCERTAIN aes] Appendix B: payloads are AES-encrypted with the session key/iv; the mode
    is not in the capture. Default ctr-cont: one AES-CTR keystream continued across packets
    (the natural reading of a single key+iv per session). --aes-mode switches to ctr-reset
    (keystream restarts per packet), cfb/ofb, or none. First bytes are logged so a wrong
    mode is visible at a glance (H.264 starts with 00 00 00 01 / 0x67 / 0x68).
    """
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

    key = decode_key16(aes_key)
    iv = decode_key16(aes_iv)
    mode_obj = {"ctr-cont": modes.CTR(iv), "ctr-reset": modes.CTR(iv),
                "cfb": modes.CFB(iv), "ofb": modes.OFB(iv)}.get(aes_mode, modes.CTR(iv))
    dec = None if aes_mode == "none" else Cipher(algorithms.AES(key), mode_obj).decryptor()
    assembler = H264Assembler()
    loop = asyncio.get_running_loop()
    deadline = loop.time() + seconds
    n = nal_total = 0
    first_frame_done = False
    stream_path = os.path.join(out_dir, "qubo_stream.h264")
    with open(stream_path, "wb") as es:
        while loop.time() < deadline:
            try:
                data, addr = await asyncio.wait_for(loop.sock_recvfrom(sock, 4096), timeout=0.5)
            except asyncio.TimeoutError:
                continue
            # Control packets (44/52/60/70B, cookie-tagged) share this socket. Surface
            # them instead of dropping silently - after the peer-ack the camera keeps a
            # control exchange going before (and alongside) the RTP.
            if len(data) < 100 and data[4:8] == STUN_COOKIE:
                if len(data) == 44 and data[0:2] == b"\x00\x01":
                    # A late probe from the camera mid-media: keep answering so it stays armed.
                    try:
                        sock.sendto(build_peer_ack(data[8:20], secrets.token_bytes(20)), addr)
                    except OSError:
                        pass
                print(f"[udp-ctl] {len(data)}B from {addr[0]}:{addr[1]}: {data.hex()}")
                continue
            n += 1
            hlen = rtp_header_len(data)
            if hlen == 0 or hlen >= len(data):
                continue
            payload = data[hlen:]
            if dec is not None:
                if aes_mode == "ctr-reset":
                    dec = Cipher(algorithms.AES(key), modes.CTR(iv)).decryptor()
                payload = dec.update(payload)
            if n <= 3:
                print(f"[rtp] #{n} from {addr[0]}:{addr[1]} hdr={data[:12].hex()} "
                      f"pt={payload[:16].hex()}")
            for nal in assembler.feed(payload):
                es.write(b"\x00\x00\x00\x01" + nal)
                nal_total += 1
            es.flush()
            if try_decode and not first_frame_done and assembler.saw_sps and assembler.saw_idr:
                first_frame_done = decode_first_frame(stream_path, out_dir)
        print(f"[rtp] received {n} datagrams, {nal_total} NALs "
              f"(SPS={assembler.saw_sps} PPS={assembler.saw_pps} IDR={assembler.saw_idr}); "
              f"elementary stream at {stream_path}")
    if not nal_total:
        print("[rtp] no parseable H.264 - wrong AES mode or not H.264? try --aes-mode ctr-reset")


def decode_first_frame(es_path: str, out_dir: str) -> bool:
    """Decode the assembled stream with PyAV (ships with aiortc) and save the first frame.

    Returns True once a frame was written; the elementary stream stays on disk for ffmpeg
    either way.
    """
    try:
        import av
    except ImportError:
        print("[decode] PyAV not installed - elementary stream kept for ffmpeg")
        return True
    try:
        with av.open(es_path) as container:
            for frame in container.decode(video=0):
                png = os.path.join(out_dir, "qubo_first_frame.png")
                frame.to_image().save(png)
                fmt = frame.format.name if frame.format else "?"
                print(f"[decode] first frame: {frame.width}x{frame.height} format={fmt} "
                      f"-> {png}")
                return True
    except Exception as ex:
        print(f"[decode] PyAV could not decode yet: {type(ex).__name__} {ex}")
    return False


# --------------------------------------------------------------------------------------
# MQTT
# --------------------------------------------------------------------------------------


def mqtt_connect(client_id: str, username: str, password: str, unit: str):
    """Paho MQTT over TLS. Broker creds = userUUID + accessToken (capture-confirmed).

    Subscribes the app's own set (mqtt_probe.log): /monitor/<unit>/#, /control/<unit>/# and
    /config/<unit>, all QoS 2.
    """
    import paho.mqtt.client as mqtt

    messages: list[tuple[str, str]] = []

    def on_connect(client, userdata, flags, reason_code, properties=None):
        print(f"[mqtt] connected rc={reason_code}")
        for topic in (f"/monitor/{unit}/#", f"/control/{unit}/#", f"/config/{unit}"):
            client.subscribe(topic, qos=2)

    def on_message(client, userdata, msg):
        body = msg.payload.decode(errors="replace")
        messages.append((msg.topic, body))
        print(f"[mqtt] {msg.topic}: {body[:300]}")

    c = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id=client_id,
                    protocol=mqtt.MQTTv311)
    c.username_pw_set(username, password)
    c.tls_set()
    c.on_connect = on_connect
    c.on_message = on_message
    host, port = MQTT_BROKER.replace("ssl://", "").rsplit(":", 1)
    c.connect(host, int(port), keepalive=60)
    c.loop_start()
    return c, messages


# --------------------------------------------------------------------------------------
# selftest - everything that can be validated without touching the camera
# --------------------------------------------------------------------------------------


def cmd_selftest() -> None:
    failures = 0

    # 1. SYN bytes against Appendix B's literal layout.
    syn = build_syn("zTkp28PX1001p", bytes(range(20)))
    ok = (len(syn) == 52
          and syn[:8] == bytes.fromhex("00010020244a2112")
          and syn[8:21] == b"zTkp28PX1001p"
          and syn[21:32] == bytes.fromhex("0200040000000000080014")
          and syn[32:] == bytes(range(20)))
    print(f"[selftest] SYN layout (52B): {'OK' if ok else 'FAIL'}")
    failures += 0 if ok else 1

    # 1b. Extended SYN vs the capture byte-for-byte (09:27 frame 6301, camera-direct flow).
    ext = build_extended_syn("wsK7COXU1056",
                             bytes.fromhex("8e608ba3a9a278f0a87b8cb1a62d11ef948af8de"))
    ext_ref = ("00010028244a211277734b37434f5855313035360025000400000000"
               "7002000400000000000800148e608ba3a9a278f0a87b8cb1a62d11ef948af8de")
    ok = len(ext) == 60 and ext.hex() == ext_ref
    print(f"[selftest] extended SYN (60B, capture f6301): {'OK' if ok else 'FAIL'}")
    failures += 0 if ok else 1

    ref = parse_capture_start_call(CAPTURE_PATH) if os.path.exists(CAPTURE_PATH) else None
    if ref:
        # 2. Payload builder vs the live capture (key sets on both levels).
        mine = build_start_call_payload(
            camera=CAPTURE_CAMERA, self_device=CAPTURE_SELF_DEVICE,
            src_device_id=CAPTURE_SRC_DEVICE_ID, session_id="x",
            offer_data=build_candidate_list("10.0.0.5", 56902),
            aes_key="00" * 16, aes_iv="11" * 16)
        ok = set(mine) == set(ref)
        print(f"[selftest] envelope tops: {'OK' if ok else 'FAIL'} "
              f"mine-extra={sorted(set(mine) - set(ref))} ref-extra={sorted(set(ref) - set(mine))}")
        failures += 0 if ok else 1
        mp = mine["command"]["devices"]["services"]["p2pStreaming"]["commands"]["startP2pCall"]
        rp = ref["command"]["devices"]["services"]["p2pStreaming"]["commands"]["startP2pCall"]
        okp = set(mp["parameters"]) == set(rp["parameters"]) and set(mp) == set(rp)
        if not okp:
            print(f"[selftest]   param mine-extra="
                  f"{sorted(set(mp['parameters']) - set(rp['parameters']))} ref-extra="
                  f"{sorted(set(rp['parameters']) - set(mp['parameters']))}")
        print(f"[selftest] startP2pCall shape: {'OK' if okp else 'FAIL'}")
        failures += 0 if okp else 1

        # 3. Event parser against the captured camera answer.
        answer_line = None
        with open(CAPTURE_PATH) as f:
            for line in f:
                if "/monitor/" in line and "/p2pStreaming QoS" in line:
                    answer_line = line.split("| ", 1)[1].strip()
                    break
        if answer_line:
            parsed = parse_p2p_callback(
                f"/monitor/{CAPTURE_UNIT}/{CAPTURE_CAMERA}/p2pStreaming", answer_line,
                "d0000000-0000-4000-8000-000000000004")
            ok = bool(parsed) and parsed["event"] == "p2pCallStarted" and bool(parsed["candidates"])
            print(f"[selftest] event parser: {'OK' if ok else 'FAIL'} "
                  f"event={(parsed or {}).get('event')} "
                  f"endpoints={candidate_endpoints(parsed['candidates']) if parsed else []}")
            failures += 0 if ok else 1
    else:
        print(f"[selftest] capture not found at {CAPTURE_PATH}; capture comparisons skipped")

    # 4. FU-A reassembly. In FU-A the fragments carry the NAL payload WITHOUT its header
    # byte; the header byte is reconstructed from the FU indicator + FU header type field.
    asm = H264Assembler()
    nal = bytes.fromhex("65010203")
    got = (asm.feed(bytes([0x7C, 0x85]) + nal[1:3])
           + asm.feed(bytes([0x7C, 0x45]) + nal[3:]))
    ok = got == [nal] and asm.saw_idr
    print(f"[selftest] FU-A reassembly: {'OK' if ok else 'FAIL'}")
    failures += 0 if ok else 1

    print(f"[selftest] {'ALL OK' if failures == 0 else f'{failures} FAILURES'}")
    sys.exit(1 if failures else 0)


# --------------------------------------------------------------------------------------
# flow
# --------------------------------------------------------------------------------------


def get_iface_ip(iface: str) -> str | None:
    """IPv4 address of a named interface (macOS ipconfig)."""
    import subprocess
    try:
        out = subprocess.run(["ipconfig", "getifaddr", iface], capture_output=True,
                             text=True, timeout=5).stdout.strip()
        return out or None
    except (OSError, subprocess.SubprocessError):
        return None


def local_ip() -> str:
    """The default-route interface's address - NOT the first non-loopback.

    A VPN holds 0/1 + 128/1 routes here, so the UDP-connect trick (and any first-non-loopback
    pick) returns the utun address 198.51.100.15 - the mis-advertisement that pointed the camera
    and relay at a path that was never ours (FINDINGS.md, VPN finding).
    """
    import subprocess
    try:
        out = subprocess.run(["route", "-n", "get", "default"], capture_output=True,
                             text=True, timeout=5).stdout
        for line in out.splitlines():
            line = line.strip()
            if line.startswith("interface:"):
                ip = get_iface_ip(line.split(":", 1)[1].strip())
                if ip:
                    return ip
    except (OSError, subprocess.SubprocessError):
        pass
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("8.8.8.8", 80))  # fallback: source the OS would pick towards the internet
        return s.getsockname()[0]
    except OSError:
        return "127.0.0.1"
    finally:
        s.close()


async def run_multi_session(args, http, headers, mqtt, messages, unit, camera, self_device,
                            src_device_id, bind_ip, topic) -> None:
    """N parallel sessions - the app's shape (it runs four): one allocation, socket and start
    call each, all against the same camera. Tests whether the relay/camera needs the
    multi-flow structure (FINDINGS.md, "next tests")."""
    legs = []
    for _ in range(args.sessions):
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind((bind_ip or "0.0.0.0", args.port or 0))
        sock.setblocking(False)
        port = sock.getsockname()[1]
        alloc = (await allocate_p2p_v3(http, headers, args.sp, unit, camera, self_device, "")
                 or await allocate_credentials_v2(http, headers, args.sp, unit, self_device,
                                                  camera)) or {}
        sid = alloc.get("appSessionId") or str(uuid_mod.uuid4())
        key = alloc.get("aesKey") or secrets.token_hex(16).upper()
        iv = alloc.get("iv") or secrets.token_hex(16).upper()
        relay = None
        if alloc.get("serverIP") and alloc.get("serverPort"):
            relay = (alloc["serverIP"], int(alloc["serverPort"]))
        srflx = []
        if not args.no_stun:
            mapped = await discover_srflx(sock, args.stun_server)
            if mapped:
                srflx.append(mapped)
        if relay and not args.no_relay_candidate:
            srflx.append(f"{relay[0]}:{relay[1]}")
        offer = build_candidate_list(args.host_ip or bind_ip or local_ip(), port, srflx)
        payload = build_start_call_payload(
            camera=camera, self_device=self_device, src_device_id=src_device_id,
            session_id=sid, offer_data=offer, aes_key=key, aes_iv=iv,
            protocols=args.protocols, quality=args.quality, media=args.media)
        print(f"[multi] session {sid[:8]} socket :{port} relay={relay}")
        mqtt.publish(topic, json.dumps(payload), qos=2)
        legs.append({"sock": sock, "sid": sid, "key": key, "iv": iv, "relay": relay})
        await asyncio.sleep(0.6)

    answered: dict[str, Any] = {}
    deadline = time.time() + args.answer_timeout
    while time.time() < deadline and len(answered) < len(legs):
        for t, body in list(messages):
            for leg in legs:
                if leg["sid"] in answered:
                    continue
                ans = parse_p2p_callback(t, body, leg["sid"])
                if ans:
                    answered[leg["sid"]] = ans
                    print(f"[multi] {leg['sid'][:8]} answered: {ans['candidates']}")
        if len(answered) < len(legs):
            await asyncio.sleep(0.3)

    async def one_leg(leg):
        targets = (candidate_endpoints(answered[leg["sid"]]["candidates"])
                   if leg["sid"] in answered else [])
        if args.syn_target:
            ip, port = args.syn_target.rsplit(":", 1)
            targets.append((ip, int(port)))
        if not (targets or leg["relay"]):
            return None
        return await udp_exchange(leg["sock"], targets, args.peer_name or random_peer_name(),
                                  timeout=args.udp_timeout, relay=leg["relay"],
                                  relay_first=args.relay_first,
                                  require_relay_engagement=not args.no_relay_gate)

    results = await asyncio.gather(*(one_leg(leg) for leg in legs))
    for leg, res in zip(legs, results):
        if res:
            print(f"[multi] {leg['sid'][:8]} up via {res['addr'][0]}:{res['addr'][1]} "
                  f"- receiving media")
            await receive_media(leg["sock"], leg["key"], leg["iv"], args.seconds,
                                args.aes_mode, try_decode=not args.no_decode)
            break
    for leg in legs:
        print(f"[multi] endP2pCall {leg['sid'][:8]}")
        mqtt.publish(topic, json.dumps(build_end_call_payload(
            camera=camera, src_device_id=src_device_id, session_id=leg["sid"])), qos=2)
        await asyncio.sleep(0.4)
        leg["sock"].close()


async def run(args: argparse.Namespace) -> None:
    if args.command == "selftest":
        cmd_selftest()

    with open(SESSION_PATH) as f:
        s = json.load(f)
    auth = QuboAuth()
    auth.set_tokens(access_token=s["accessToken"], refresh_token=s.get("refreshToken"),
                    user_uuid=s.get("uuid", ""))
    client = QuboClient(auth)
    try:
        unit, camera = args.unit, args.camera
        self_device, src_device_id = args.self_device, args.src_device_id
        print(f"[session] user={auth.user_uuid} unit={unit} camera={camera} "
              f"self={self_device}")

        if args.command == "watch":
            mqtt, _ = mqtt_connect(
                build_client_id(args.device_name, unit), auth.user_uuid, s["accessToken"],
                unit)
            await asyncio.sleep(args.seconds)
            mqtt.loop_stop()
            mqtt.disconnect()
            return

        bind_ip = args.bind or (get_iface_ip(args.iface) if args.iface else None)

        if args.command == "natcheck":
            # One socket, several STUN destinations. If the mapped port differs per
            # destination the NAT is endpoint-dependent and the srflx we advertise is wrong
            # for the camera - the anti-spoofing hypothesis (FINDINGS.md, "What would
            # settle it"). With a VPN up, --bind <lan-ip> is what puts these probes on the
            # path the camera actually sees.
            print(f"[nat] default-route address: {local_ip()}  bind: {bind_ip or '0.0.0.0'}")
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.bind((bind_ip or "0.0.0.0", args.port or 0))
            sock.setblocking(False)
            local_port = sock.getsockname()[1]
            for server in ("stun.l.google.com:19302", "3.6.42.244:5349",
                           "stun.cloudflare.com:3478"):
                while True:  # drain a previous destination's late reply
                    try:
                        sock.recvfrom(2048)
                    except BlockingIOError:
                        break
                    except OSError:
                        break
                mapped = await discover_srflx(sock, server)
                print(f"[nat] {server:28} -> {mapped or 'no reply'} (local port {local_port})")
            sock.close()
            return

        if args.command == "end":
            mqtt, _ = mqtt_connect(
                build_client_id(args.device_name, unit), auth.user_uuid, s["accessToken"],
                unit)
            await asyncio.sleep(1.0)
            for sid in args.session_id:
                payload = build_end_call_payload(camera=camera, src_device_id=src_device_id,
                                                 session_id=sid)
                print(f"[end] endP2pCall {sid}")
                mqtt.publish(f"/control/{unit}/{camera}/p2pStreaming",
                             json.dumps(payload), qos=2)
                await asyncio.sleep(0.4)
            await asyncio.sleep(2.0)
            mqtt.loop_stop()
            mqtt.disconnect()
            return

        if args.command == "reboot":
            # The app's own reboot button (deviceReboot/reboot): forces the camera's WiFi
            # reassociation - the handshake generator for an air-side capture window.
            mqtt, _ = mqtt_connect(
                build_client_id(args.device_name, unit), auth.user_uuid, s["accessToken"],
                unit)
            await asyncio.sleep(1.0)
            topic = f"/control/{unit}/{camera}/deviceReboot"
            print(f"[reboot] publishing deviceReboot/reboot on {topic}")
            mqtt.publish(topic, json.dumps(build_reboot_payload(
                camera=camera, src_device_id=src_device_id)), qos=2)
            await asyncio.sleep(2.0)
            mqtt.loop_stop()
            mqtt.disconnect()
            return

        headers = await auth.get_headers()
        # StreamRestClient::createConnection sends Login-Device-Name besides User-UUID /
        # Subscriber-Key / Source-Device-Id / Token-Type (libcore-rest decompile), and sets
        # the libcurl agent as User-Agent (exact bytes: "libcurl-agent") - mirror both.
        headers = {**headers, "Login-Device-Name": args.login_device_name,
                   "User-Agent": "libcurl-agent"}
        async with aiohttp.ClientSession() as http:
            if args.command == "token":
                tok = await fetch_device_access_token(
                    http, headers, args.sp, unit, self_device, args.dest or camera)
                if tok:
                    print(f"[token] {tok}")
                return

            # --- flow ---------------------------------------------------------------
            media_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            media_sock.bind((bind_ip or "0.0.0.0", args.port or 0))
            media_sock.setblocking(False)
            my_port = media_sock.getsockname()[1]
            lan_ip = args.host_ip or bind_ip or local_ip()

            app_session_id = str(uuid_mod.uuid4())
            aes_key = args.aes_key or secrets.token_hex(16).upper()
            aes_iv = args.aes_iv or secrets.token_hex(16).upper()
            server_ip, server_port = None, None
            if args.allocate:
                if args.alloc == "v2":
                    alloc = await allocate_credentials_v2(
                        http, headers, args.sp, unit, self_device, camera)
                elif args.alloc == "v3":
                    alloc = await allocate_p2p_v3(http, headers, args.sp, unit, camera,
                                                   self_device, "")
                else:
                    # native order: the streaming path allocates via v3 /p2p; v2 is the
                    # legacy/local fast path. Keep v3 first, fall back to v2.
                    alloc = (await allocate_p2p_v3(http, headers, args.sp, unit, camera,
                                                   self_device, "")
                             or await allocate_credentials_v2(
                                 http, headers, args.sp, unit, self_device, camera))
                if alloc:
                    app_session_id = alloc.get("appSessionId") or app_session_id
                    aes_key = alloc.get("aesKey") or aes_key
                    aes_iv = alloc.get("iv") or alloc.get("aesIV") or aes_iv
                    server_ip = alloc.get("serverIP") or alloc.get("serverIp")
                    server_port = alloc.get("serverPort")
                    print(f"[flow] allocation: session={app_session_id} "
                          f"relay={server_ip}:{server_port}")
                else:
                    print("[flow] allocation failed - continuing with locally generated keys")
            else:
                print("[flow] --allocate not set: locally generated sessionId/keys "
                      "(dry validation only; a live run needs the allocation)")
            # The app fetches a device-access-token for [self -> camera] right before every
            # session (broker log: the two action:71 pushes at 16:05:59 precede the 16:06:02
            # start; the 16:47:58 burst precedes the 16:48:01 start) - GatewayManager gates it
            # with isDeviceTokenRequired. The relay/camera may authorize the session on it.
            if not args.no_device_token:
                token_dest = args.dest or args.camera_controller
                tok = await fetch_device_access_token(http, headers, args.sp, unit,
                                                      self_device, token_dest)
                print(f"[flow] device-access-token for dest={token_dest}: "
                      f"{'acquired' if tok else 'FAILED (continuing)'}")

            srflx = list(args.srflx or [])
            if not args.no_stun:
                mapped = await discover_srflx(media_sock, args.stun_server)
                if mapped:
                    print(f"[flow] STUN: our public address is {mapped}")
                    srflx.insert(0, mapped)
                else:
                    print("[flow] STUN lookup failed (continuing without srflx)")
            # The RELAY is advertised as a candidate too - the app's offerData carried
            # "<relayIP>:<relayPort>" in an srflx_N slot (16:05 capture: srflx_1 =
            # 13.204.183.153:<port>), which is how the camera learns where to register.
            # SDK source: setP2pTurnDetails(ip, port) fed from the pp2p credentials.
            if server_ip and server_port and not args.no_relay_candidate:
                srflx.append(f"{server_ip}:{server_port}")
            offer = build_candidate_list(lan_ip, my_port, srflx)
            print(f"[flow] our candidate(s): {offer}")

            payload = build_start_call_payload(
                camera=camera, self_device=self_device, src_device_id=src_device_id,
                session_id=app_session_id, offer_data=offer, aes_key=aes_key,
                aes_iv=aes_iv, protocols=args.protocols, quality=args.quality,
                media=args.media)
            topic = f"/control/{unit}/{camera}/p2pStreaming"

            if args.dry_run:
                print(f"[flow] DRY RUN - would publish on {topic}:")
                print(json.dumps(payload, indent=1))
                ref = parse_capture_start_call(CAPTURE_PATH)
                if ref:
                    diff = set(payload) ^ set(ref)
                    print(f"[flow] envelope-top diff vs capture: {sorted(diff) or 'none'}")
                print("[flow] stopping before publish (readiness check)")
                media_sock.close()
                return

            mqtt, messages = mqtt_connect(
                build_client_id(args.device_name, unit), auth.user_uuid, s["accessToken"],
                unit)
            await asyncio.sleep(1.5)
            if args.heartbeat:
                # The app keeps a retained presence marker on its own heartbeat topic; after
                # it disconnects its will flips it to offline (captured: operationState
                # "offline"). Claiming the phone's identity in a start call while that marker
                # says offline is incoherent - restore it first, like the app does on login.
                hb = {"devices": {
                    "action": 140,
                    "deviceUUID": self_device,
                    "entityUUID": args.self_entity,
                    "msgSequenceId": int(time.time() * 1000),
                    "operationState": "online",
                    "serviceProviderId": args.sp,
                    "srcDeviceId": src_device_id,
                    "timestamp": int(time.time() * 1000),
                    "unitUUID": unit,
                    "userUUID": auth.user_uuid,
                }}
                hb_topic = f"/monitor/{unit}/{self_device}/heartbeat"
                print(f"[flow] publishing online heartbeat (retained) on {hb_topic}")
                mqtt.publish(hb_topic, json.dumps(hb), qos=2, retain=True)
                await asyncio.sleep(0.5)
            if args.sessions > 1:
                await run_multi_session(args, http, headers, mqtt, messages, unit, camera,
                                        self_device, src_device_id, bind_ip, topic)
                media_sock.close()
                mqtt.loop_stop()
                mqtt.disconnect()
                return
            print(f"[flow] publishing startP2pCall on {topic}")
            mqtt.publish(topic, json.dumps(payload), qos=2)

            answer = None
            deadline = time.time() + args.answer_timeout
            while time.time() < deadline and answer is None:
                for t, body in list(messages):
                    if t == f"/monitor/{unit}/{camera}/p2pStreaming":
                        answer = parse_p2p_callback(t, body, app_session_id)
                        if answer:
                            break
                if answer is None:
                    await asyncio.sleep(0.3)
            if answer:
                print(f"[flow] camera answered: event={answer['event']} "
                      f"handle={answer['handle_name']}")
                print(f"[flow] camera candidates: {answer['candidates']}")
            else:
                print(f"[flow] no p2pCallStarted for session {app_session_id} within "
                      f"{args.answer_timeout}s - camera not armed or answer missed")

            targets: list[tuple[str, int]] = []
            if answer:
                targets += candidate_endpoints(answer["candidates"])
            if args.syn_target:
                ip, port = args.syn_target.rsplit(":", 1)
                targets.append((ip, int(port)))
            relay = (server_ip, int(server_port)) if server_ip and server_port else None

            if targets or relay:
                result = await udp_exchange(media_sock, targets,
                                            args.peer_name or random_peer_name(),
                                            timeout=args.udp_timeout, relay=relay,
                                            relay_first=args.relay_first,
                                            require_relay_engagement=not args.no_relay_gate)
                if result:
                    # No post-ack keepalive: the capture shows the app sending nothing to the
                    # camera after the handshake and the camera streaming anyway (f6317+).
                    await receive_media(media_sock, aes_key, aes_iv, args.seconds,
                                        args.aes_mode, try_decode=not args.no_decode)
            else:
                print("[flow] no targets to handshake with")
            # Always tear the session down, like the app does - abandoning one leaves it
            # live on the camera and it starts refusing new calls.
            print(f"[flow] sending endP2pCall for {app_session_id}")
            mqtt.publish(topic, json.dumps(build_end_call_payload(
                camera=camera, src_device_id=src_device_id, session_id=app_session_id)),
                qos=2)
            await asyncio.sleep(0.5)
            media_sock.close()
            mqtt.loop_stop()
            mqtt.disconnect()
    finally:
        await client.close()
        try:
            await auth.close()
        except Exception:
            pass


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("command",
                   choices=["selftest", "watch", "token", "flow", "end", "natcheck",
                            "reboot"])
    p.add_argument("--session-id", action="append", default=[],
                   help="end: sessionId(s) to tear down with endP2pCall")
    p.add_argument("--sp", default=DEFAULT_SP)
    p.add_argument("--unit", default=CAPTURE_UNIT)
    p.add_argument("--camera", default=CAPTURE_CAMERA)
    p.add_argument("--camera-controller", default=CAPTURE_CAMERA_CONTROLLER,
                   help="the camera's controllerUUID (device-access-token dest)")
    p.add_argument("--login-device-name", default="MobileApp_<mobile-id>",
                   help="Login-Device-Name header the SDK's stream client sends (the phone's "
                        "unit device name)")
    p.add_argument("--no-device-token", action="store_true",
                   help="skip the device-access-token fetch before startP2pCall")
    p.add_argument("--self-device", default=CAPTURE_SELF_DEVICE,
                   help="the app-side node uuid (reuses the phone's unit 'mobile' device)")
    p.add_argument("--src-device-id", default=CAPTURE_SRC_DEVICE_ID,
                   help="the Android-ID-style id used as srcDeviceId")
    p.add_argument("--device-name", default="qubopy",
                   help="stands in for Build.MANUFACTURER_MODEL in the clientId")
    p.add_argument("--dest", default=None,
                   help="token command: the target's controllerUUID (default: camera uuid)")
    p.add_argument("--allocate", action="store_true",
                   help="call the pp2p credentials allocation (needed for a live run)")
    p.add_argument("--alloc", default="native", choices=["native", "v2", "v3"],
                   help="which allocation route: native = v3 /p2p first (the streaming "
                        "path), v2 = the legacy pp2p/credentials route")
    p.add_argument("--dry-run", action="store_true",
                   help="build everything, print the payload, stop before publishing")
    p.add_argument("--port", type=int, default=0, help="local UDP port to offer")
    p.add_argument("--host-ip", default=None, help="override the offered LAN ip")
    p.add_argument("--bind", default=None,
                   help="bind the UDP sockets to this local ip (use the camera's LAN ip "
                        "when a VPN would otherwise carry egress; also becomes the host "
                        "candidate)")
    p.add_argument("--iface", default=None,
                   help="bind to this interface's address instead of --bind (macOS, e.g. en1)")
    p.add_argument("--srflx", action="append",
                   help="extra srflx_N candidate 'ip:port' (repeatable)")
    p.add_argument("--no-stun", action="store_true",
                   help="skip the one-shot STUN public-address lookup")
    # The app STUNs Qubo's own coturn from each media socket right before the SYN trains
    # (pcaps: 3.6.42.244:5349, one binding per socket) - the relay may pair peers using
    # its own STUN's view of a peer's mapping, so a mapping learned from any other server
    # could leave the register unpaired (FINDINGS.md, STUN-operator thread).
    p.add_argument("--stun-server", default="3.6.42.244:5349")
    p.add_argument("--no-relay-candidate", action="store_true",
                   help="do not advertise the relay endpoint in offerData")
    p.add_argument("--heartbeat", action="store_true",
                   help="publish a retained online heartbeat for the claimed self device first")
    p.add_argument("--self-entity", default="d0000000-0000-4000-8000-000000000005",
                   help="the self device's entityUUID (capture: the phone's)")
    p.add_argument("--aes-key", default=None, help="override (32 hex chars / b64)")
    p.add_argument("--aes-iv", default=None)
    p.add_argument("--aes-mode", default="ctr-cont",
                   choices=["ctr-cont", "ctr-reset", "cfb", "ofb", "none"])
    p.add_argument("--no-decode", action="store_true",
                   help="skip PyAV decode; keep the elementary stream only")
    p.add_argument("--peer-name", default="",
                   help="13-char peer name: 8 alnum + 4 digits + 'p'; default = freshly "
                        "generated per run with a monotonic counter, like the SDK")
    p.add_argument("--syn-target", default=None,
                   help="extra handshake target 'ip:port' (e.g. the relay)")
    p.add_argument("--relay-first", type=float, default=0.0,
                   help="run the relay leg alone for N seconds before the direct (camera) "
                        "candidates join - the captures' order (0 = simultaneous)")
    p.add_argument("--sessions", type=int, default=1,
                   help="parallel sessions, the app's shape (it runs four); 1 = the normal "
                        "single-session flow")
    p.add_argument("--no-relay-gate", action="store_true",
                   help="fire the direct (camera) leg without waiting for the relay to "
                        "engage; the app never does (default: wait for the relay's first "
                        "packet)")
    p.add_argument("--protocols", default="udp", choices=["udp", "tcp", "both"])
    p.add_argument("--quality", default="high", choices=["low", "medium", "high", "auto"])
    p.add_argument("--media", default="both",
                   choices=["videoOnly", "audioOnly", "both", "none"])
    p.add_argument("--answer-timeout", type=float, default=15.0)
    p.add_argument("--udp-timeout", type=float, default=10.0)
    p.add_argument("--seconds", type=float, default=15.0)
    args = p.parse_args()
    try:
        asyncio.run(run(args))
    except KeyboardInterrupt:
        sys.exit(130)


if __name__ == "__main__":
    main()
