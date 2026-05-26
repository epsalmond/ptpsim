"""Observation bundle — the seam. A JSONL stream of distinct facts.

Each fact answers: probing interface **A** (transport) in state **B** (mode/state) for prop/op
**C** (subject) returned **D** (result). Emitted at the shared PTP/IP op layer so any plan that
goes through `connect_wireless_tether.ptp_op` or `probe_iso_liveview.Session` contributes facts
without bespoke logging.

Record shape (one JSON object per line):
  {ts, kind:"ptpip.fact", transport, mode, state,
   subject:{kind:"prop"|"op", code, op?}, params:[...],
   result:{response, value_hex?|data_hex?, data_len?}, evidence?}

Portability: optional. Scripts import this lazily; if absent or no bundle is open, nothing emits.
Hygiene: a redaction pass strips the device GUID and caps large payloads (the manifest does not
need a 69 KB settings blob inline — length is recorded, bytes are not).
"""
from __future__ import annotations

import json
import re
import time

# PTP ops whose first param identifies a device property (so the fact is "prop C returned D").
_PROP_OPS = {0x1014: "GetDevicePropDesc", 0x1015: "GetDevicePropValue", 0x1016: "SetDevicePropValue"}
_MAX_DATA_HEX = 256  # bytes of payload kept inline; longer is summarized by length only
# Ops whose data carries device identity (e.g. GetDeviceInfo's serial, a UTF-16 PTP string) —
# never inline their bytes; record length only. TODO: parse + redact the DeviceInfo serial field
# so the op's non-PII fields can be kept.
_NO_INLINE_DATA = {0x1002}  # GetDeviceInfo

# Redaction: device GUIDs / serials. Add patterns here as transports expose more identifiers.
_REDACT = [
    (re.compile(r"0870b0610a8b4593b2e79357dd36e050", re.I), "<device-guid>"),
]

_SINK: "BundleWriter | None" = None
_CTX = {"transport": None, "mode": None, "state": None, "evidence": None}


def set_context(**kw) -> None:
    """Set the current (transport, mode, state, evidence) stamped onto subsequent facts."""
    for k, v in kw.items():
        if k in _CTX and v is not None:
            _CTX[k] = v


def open_bundle(path: str) -> "BundleWriter":
    global _SINK
    _SINK = BundleWriter(path)
    return _SINK


def close_bundle() -> None:
    global _SINK
    if _SINK is not None:
        _SINK.close()
        _SINK = None


def active() -> bool:
    return _SINK is not None


def _redact(hexstr: str) -> str:
    for pat, repl in _REDACT:
        hexstr = pat.sub(repl, hexstr)
    return hexstr


def observe(op: int, params: list[int] | None, data: bytes, code: int | None) -> None:
    """Emit one fact for a completed PTP op. No-op unless a bundle is open."""
    if _SINK is not None:
        _SINK.fact(op, list(params or []), data or b"", code)


class BundleWriter:
    def __init__(self, path: str) -> None:
        self.path = path
        self.f = open(path, "w")
        self.count = 0

    def _data_field(self, op: int, data: bytes) -> dict:
        if not data:
            return {}
        if op in _NO_INLINE_DATA:
            return {"data_len": len(data), "redacted": "identity-bearing"}
        if len(data) > _MAX_DATA_HEX:
            return {"data_len": len(data), "data_hex_head": _redact(data[:_MAX_DATA_HEX].hex())}
        return {"value_hex": _redact(data.hex())}

    def fact(self, op: int, params: list[int], data: bytes, code: int | None) -> None:
        rec: dict = {
            "ts": time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()),
            "kind": "ptpip.fact",
            "transport": _CTX["transport"],
            "mode": _CTX["mode"],
            "state": _CTX["state"],
        }
        if op in _PROP_OPS and params:
            rec["subject"] = {"kind": "prop", "code": f"0x{params[0]:04x}", "op": _PROP_OPS[op]}
            rec["params"] = [f"0x{p:08x}" for p in params[1:]]
        else:
            rec["subject"] = {"kind": "op", "code": f"0x{op:04x}"}
            rec["params"] = [f"0x{p:08x}" for p in params]
        rec["result"] = {"response": f"0x{code:04x}" if code is not None else None}
        rec["result"].update(self._data_field(op, data))
        if _CTX["evidence"]:
            rec["evidence"] = _CTX["evidence"]
        self.f.write(json.dumps(rec) + "\n")
        self.f.flush()
        self.count += 1

    def close(self) -> None:
        try:
            self.f.close()
        except OSError:
            pass
