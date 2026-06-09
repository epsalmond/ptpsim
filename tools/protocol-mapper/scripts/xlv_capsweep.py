"""Sweep all XLV D-code properties for their get/cap response shape.

Writes one JSONL record per responding code to /tmp/xlv_capsweep.jsonl.

Requires an XLV bearer token and the camera's HTTP base URL. Set via env:

    XLV_TOKEN=<jwt>           bearer token; see docs/xlv-auth.md for how to obtain
    XLV_BASE=http://<ip>      camera HTTP base (e.g. http://192.168.x.y)

XLV uses a per-camera HMAC-signed JWT. To obtain a token you either:
  (a) drive the standard reference app pairing flow against your camera, or
  (b) extract the HMAC signing key from your camera's firmware and mint
      tokens directly. The signing-key material is camera-firmware-derived
      and is not shipped with this repo.
"""
import base64
import json
import os
import sys
import time

import requests

requests.packages.urllib3.disable_warnings()

TOK = os.environ.get("XLV_TOKEN")
B = os.environ.get("XLV_BASE")
if not TOK or not B:
    sys.exit("set XLV_TOKEN and XLV_BASE env vars; see module docstring")
H = {"XLV_Auth": "Bearer " + TOK}
S = requests.Session()
S.verify = False


def b64j(t):
    try:
        return json.loads(base64.b64decode(t + "=="))
    except Exception:
        return None


out = open("/tmp/xlv_capsweep.jsonl", "w")
resp = []
t0 = time.time()
for code in range(0xD000, 0xE000):
    h = f"{code:04X}"
    try:
        r = S.get(f"{B}/camera/functions/{h}/get", headers=H, timeout=3)
    except Exception:
        continue
    if r.status_code != 200:
        continue
    g = b64j(r.text)
    pv = (g or {}).get("property_code_value_list", []) if g else []
    val = pv[0]["value"] if pv else None
    try:
        rc = S.get(f"{B}/camera/functions/{h}/cap", headers=H, timeout=3)
        cap = b64j(rc.text) if rc.status_code == 200 else {"status": rc.status_code}
    except Exception:
        cap = {"err": 1}
    rec = {"code": f"0x{h}", "value": val, "cap": cap}
    out.write(json.dumps(rec) + "\n")
    out.flush()
    resp.append(h)
out.close()
print(f"DONE {len(resp)} responders in {time.time()-t0:.0f}s")
print("responders:", " ".join(resp))
