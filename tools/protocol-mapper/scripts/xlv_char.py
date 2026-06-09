"""Characterize XLV liveview frame rate / quality under varying D173/D174 settings.

Requires an XLV bearer token and the camera's HTTP base URL. Set via env:

    XLV_TOKEN=<jwt>           bearer token; see docs/xlv-auth.md for how to obtain
    XLV_BASE=http://<ip>      camera HTTP base (e.g. http://192.168.x.y)

XLV uses a per-camera HMAC-signed JWT. To obtain a token you either:
  (a) drive the standard reference app pairing flow against your camera, or
  (b) extract the HMAC signing key from your camera's firmware and mint
      tokens directly. The signing-key material is camera-firmware-derived
      and is not shipped with this repo.

Also requires the `xlv_socketio` helper module on PYTHONPATH (not shipped).
"""
import os
import struct
import sys
import time

import requests
from xlv_socketio import XLVSocket as SioSession

requests.packages.urllib3.disable_warnings()

TOK = os.environ.get("XLV_TOKEN")
BASE = os.environ.get("XLV_BASE")
if not TOK or not BASE:
    sys.exit("set XLV_TOKEN and XLV_BASE env vars; see module docstring")
H = {"XLV_Auth": "Bearer " + TOK}
S = requests.Session()
S.verify = False


def jdims(b):
    i = 2
    while i < len(b) - 9:
        if b[i] != 0xFF:
            i += 1
            continue
        if b[i + 1] in (0xC0, 0xC1, 0xC2, 0xC3):
            return (
                struct.unpack(">H", b[i + 7 : i + 9])[0],
                struct.unpack(">H", b[i + 5 : i + 7])[0],
            )
        i += 2 + struct.unpack(">H", b[i + 2 : i + 4])[0]
    return None


def setp(c, v):
    try:
        return S.post(
            f"{BASE}/camera/functions/{c}/set",
            headers=H,
            json={"value": v},
            timeout=8,
        ).status_code
    except Exception:
        return "err"


def measure(sec=3.0):
    try:
        r = S.get(
            f"{BASE}/camera/functions/liveview",
            params={"xlrat": TOK},
            stream=True,
            timeout=10,
        )
    except Exception:
        return "stream-err"
    buf = bytearray()
    fr = []
    t0 = time.time()
    for ch in r.iter_content(8192):
        buf += ch
        while True:
            s = buf.find(b"\xff\xd8")
            e = buf.find(b"\xff\xd9", s + 2) if s >= 0 else -1
            if s >= 0 and e >= 0:
                j = bytes(buf[s : e + 2])
                del buf[: e + 2]
                fr.append((len(j), jdims(j)))
            else:
                break
        if time.time() - t0 > sec:
            break
    r.close()
    dt = time.time() - t0
    n = len(fr)
    if not n:
        return "NO FRAMES"
    avg = sum(f[0] for f in fr) // n
    d = fr[0][1]
    return f"{d[0]}x{d[1]}  {n/dt:.1f}fps  {avg//1024}KB/fr  ~{avg*8*(n/dt)/1e6:.1f}Mbps"


for prio, label in ((1, "REALTIME"), (0, "QUALITY")):
    print(f"\n===== FpsValue={prio} ({label} priority) =====")
    try:
        with SioSession(BASE, TOK, init_fps_priority=prio) as s:
            time.sleep(1.0)
            print("  default          :", measure())
            for sz in (1, 2, 3):
                print(f"  size D174={sz} set={setp('D174',sz)} :", measure(2.5))
            setp("D174", 1)
            for q in (1, 2, 3):
                print(f"  qual D173={q} set={setp('D173',q)} :", measure(2.5))
            setp("D173", 3)
    except Exception as e:
        print("  session err:", repr(e)[:120])
