#!/usr/bin/env python3
"""Probe Fuji live-view ISO control properties over PTP/IP (GFX100 II).


  - Phase 1 (default, read-only): GetDevicePropDesc + GetDevicePropValue for
    0x500f (ExposureIndex), 0xd02a (PROPERTY_ISO), 0xd02b (MOVIE_ISO),
    0xd212 (live-view bundle), plus context 0x500e / 0xd246.
  - Phase 2 (--phase2): no-op writeback of the *current* value to a writable
    prop, then read back the full set.
  - Phase 3 (--phase3 --target N): exactly one safe ISO change. N is clamped
    to 80..320; values >=400 are refused (the known command-socket-killer).

Builds on fuji-remote's rce.tools.fuji_ble_gps.ptpip primitives and adds
GetDevicePropDesc (0x1014) + a PTP DevicePropDesc decoder, which the base
library does not implement.

All raw request/response packets, a JSON summary, and a markdown table are
written to --session-dir for replay/compare.
"""
from __future__ import annotations

import argparse
import json
import socket
import struct
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from rce.tools.fuji_ble_gps import ptpip  # noqa: E402

PTP_GET_DEVICE_PROP_DESC = 0x1014
PTP_RESPONSE_OK = 0x2001

# (name, byte_size, signed) keyed by PTP DataType code.
DATATYPE = {
    0x0001: ("INT8", 1, True),
    0x0002: ("UINT8", 1, False),
    0x0003: ("INT16", 2, True),
    0x0004: ("UINT16", 2, False),
    0x0005: ("INT32", 4, True),
    0x0006: ("UINT32", 4, False),
    0x0007: ("INT64", 8, True),
    0x0008: ("UINT64", 8, False),
}

# Fuji XSDK XSDK_SENSITIVITY_ISO* superset (SDK13410 ProgrammingReference §4.1.9):
# manual ISO is the literal value; AUTO is a negative sentinel. The camera only
# accepts a value currently in its Cap list (GetDevicePropDesc(0xD02A) enum).
SDK_ISO_MANUAL = [50, 60, 64, 80, 100, 125, 160, 200, 250, 320, 400, 500, 640, 800,
                  1000, 1250, 1600, 2000, 2500, 3200, 4000, 5000, 6400, 8000, 10000,
                  12800, 16000, 20000, 25600, 32000, 40000, 51200, 64000, 80000, 102400]
SDK_ISO_AUTO = {-1: "AUTO_1", -2: "AUTO_2", -3: "AUTO_3", -4: "AUTO_4", -10: "AUTO",
                -400: "AUTO≤400", -800: "AUTO≤800", -1600: "AUTO≤1600",
                -3200: "AUTO≤3200", -6400: "AUTO≤6400"}


def encode_iso(value: int) -> bytes:
    """ISO as signed long → uint32 LE (manual=literal, AUTO=negative sentinel)."""
    return (value & 0xFFFFFFFF).to_bytes(4, "little")


def _read_int(payload: bytes, off: int, size: int, signed: bool) -> tuple[int, int]:
    raw = payload[off : off + size]
    if len(raw) != size:
        raise ValueError(f"need {size} bytes at offset {off}, have {len(raw)}")
    return int.from_bytes(raw, "little", signed=signed), off + size


def decode_device_prop_desc(payload: bytes) -> dict:
    """Decode a standard PTP DevicePropDesc dataset payload."""
    out: dict = {"raw_hex": payload.hex(), "payload_bytes": len(payload)}
    if len(payload) < 5:
        out["decode_error"] = "payload too short for DevicePropDesc"
        return out
    prop, dtype = struct.unpack_from("<HH", payload, 0)
    get_set = payload[4]
    out["prop_code"] = f"0x{prop:04x}"
    out["data_type"] = f"0x{dtype:04x}"
    info = DATATYPE.get(dtype)
    out["data_type_name"] = info[0] if info else "UNKNOWN"
    out["get_set"] = get_set
    out["writable"] = get_set == 1
    if info is None:
        out["decode_error"] = "unknown datatype; value/form left raw"
        out["tail_hex"] = payload[5:].hex()
        return out
    _, size, signed = info
    off = 5
    try:
        out["factory_default"], off = _read_int(payload, off, size, signed)
        out["current_value"], off = _read_int(payload, off, size, signed)
        form_flag = payload[off]
        off += 1
        out["form_flag"] = form_flag
        if form_flag == 0x01:
            minimum, off = _read_int(payload, off, size, signed)
            maximum, off = _read_int(payload, off, size, signed)
            step, off = _read_int(payload, off, size, signed)
            out["form"] = "range"
            out["range_min"] = minimum
            out["range_max"] = maximum
            out["range_step"] = step
        elif form_flag == 0x02:
            count = struct.unpack_from("<H", payload, off)[0]
            off += 2
            values = []
            for _ in range(count):
                value, off = _read_int(payload, off, size, signed)
                values.append(value)
            out["form"] = "enum"
            out["enum_count"] = count
            out["enum_values"] = values
            out["enum_values_hex"] = [f"0x{v:08x}" for v in values]
        else:
            out["form"] = "none"
    except (ValueError, struct.error) as exc:
        out["decode_error"] = str(exc)
    out["trailing_hex"] = payload[off:].hex() if off <= len(payload) else ""
    return out


def decode_prop_value(payload: bytes) -> dict:
    """Decode a raw GetDevicePropValue payload (no datatype prefix)."""
    out: dict = {"raw_hex": payload.hex(), "payload_bytes": len(payload)}
    if len(payload) in (1, 2, 4, 8):
        unsigned = int.from_bytes(payload, "little", signed=False)
        out["uint_le"] = unsigned
        if len(payload) == 4:
            out["uint32_masked"] = unsigned & 0x7FFFFFFF
            out["flag_bit_0x80000000"] = bool(unsigned & 0x80000000)
    return out


class Session:
    """One PTP/IP control session with raw-packet capture and liveness tracking."""

    def __init__(self, sock, session_dir: Path, summary: dict) -> None:
        self.sock = sock
        self.dir = session_dir
        self.summary = summary
        self.tid = 2  # OpenSession used tid 1
        self.alive = True
        self.steps: list[dict] = []

    def _write(self, name: str, data: bytes) -> None:
        (self.dir / name).write_bytes(data)

    def _recv(self) -> tuple[bytes, str]:
        try:
            pkt = ptpip.recv_packet(self.sock)
        except socket.timeout:
            self.alive = False
            return b"", "timeout"
        except (OSError, RuntimeError) as exc:
            self.alive = False
            return b"", repr(exc)
        if not pkt:
            self.alive = False
            return b"", "socket_closed"
        return pkt, ""

    def _txn_get(self, code: int, params: tuple[int, ...], prefix: str) -> dict:
        tid = self.tid
        self.tid += 1
        command = ptpip.build_ptp_command(code, tid, *params)
        self._write(f"{prefix}_request.bin", command)
        result: dict = {
            "prefix": prefix,
            "code": f"0x{code:04x}",
            "params": [f"0x{p:08x}" for p in params],
            "transaction_id": tid,
            "data_present": False,
            "response_present": False,
        }
        try:
            self.sock.sendall(command)
        except OSError as exc:
            self.alive = False
            result["send_error"] = repr(exc)
            self.steps.append(result)
            return result
        first, err = self._recv()
        if err:
            result["recv_error"] = err
            self.steps.append(result)
            return result
        header = ptpip.ptp_container_header(first)
        data = b""
        response = b""
        if header.get("container_type") == ptpip.PTP_CONTAINER_DATA:
            data = first
            response, err2 = self._recv()
            if err2:
                result["recv_error"] = err2
        else:
            response = first
        if data:
            self._write(f"{prefix}_data.bin", data)
            result["data_present"] = True
            result["data_bytes"] = len(data)
            payload = ptpip.ptp_data_payload(data)
            result["payload_hex"] = payload.hex()
            result["payload_bytes"] = len(payload)
        if response:
            self._write(f"{prefix}_response.bin", response)
            rhdr = ptpip.ptp_container_header(response)
            result["response_present"] = True
            result["response_code"] = f"0x{rhdr.get('code'):04x}"
            result["response_ok"] = rhdr.get("code") == PTP_RESPONSE_OK
        self.steps.append(result)
        return result

    def get_prop_desc(self, prop: int) -> dict:
        result = self._txn_get(PTP_GET_DEVICE_PROP_DESC, (prop,), f"desc_{prop:04x}")
        if result.get("data_present"):
            payload = bytes.fromhex(result["payload_hex"])
            result["descriptor"] = decode_device_prop_desc(payload)
        return result

    def get_prop_value(self, prop: int) -> dict:
        result = self._txn_get(ptpip.PTP_GET_DEVICE_PROP_VALUE, (prop,), f"get_{prop:04x}")
        if result.get("data_present"):
            payload = bytes.fromhex(result["payload_hex"])
            result["value"] = decode_prop_value(payload)
        return result

    def set_prop_value(self, prop: int, value: bytes, tag: str) -> dict:
        tid = self.tid
        self.tid += 1
        prefix = f"set_{prop:04x}_{tag}"
        command = ptpip.build_set_device_prop_value(prop, tid)
        data = ptpip.build_ptp_data_container(ptpip.PTP_SET_DEVICE_PROP_VALUE, tid, value)
        self._write(f"{prefix}_request.bin", command)
        self._write(f"{prefix}_data.bin", data)
        result: dict = {
            "prefix": prefix,
            "action": "set",
            "prop": f"0x{prop:04x}",
            "value_hex": value.hex(),
            "transaction_id": tid,
            "response_present": False,
        }
        try:
            self.sock.sendall(command)
            self.sock.sendall(data)
        except OSError as exc:
            self.alive = False
            result["send_error"] = repr(exc)
            self.steps.append(result)
            return result
        response, err = self._recv()
        if err:
            result["recv_error"] = err
        elif response:
            self._write(f"{prefix}_response.bin", response)
            rhdr = ptpip.ptp_container_header(response)
            result["response_present"] = True
            result["response_bytes"] = len(response)
            result["response_code"] = f"0x{rhdr.get('code'):04x}"
            result["response_ok"] = rhdr.get("code") == PTP_RESPONSE_OK
        self.steps.append(result)
        return result


# Big 3 + context. Standard PTP: 0x500D ExposureTime (shutter), 0x5007 FNumber
# (aperture), 0x500F ExposureIndex (ISO). Fuji vendor: 0xD02A still ISO, 0xD02B
# movie ISO, 0xD212 live-view bundle. Context: 0x500E exposure program, 0xD246
# still/movie flag. Step ops 0x902C/0x902D are operations, documented separately.
READ_DESC_PROPS = [0x500D, 0x5007, 0x500F, 0xD02A, 0xD02B, 0x500E, 0xD246]
READ_VALUE_PROPS = [0x500D, 0x5007, 0x500F, 0xD02A, 0xD02B, 0xD212, 0x500E, 0xD246]


def live_view_handshake(session: Session, lv_size: int = 0, lv_quality: int = 0) -> dict:
    """Enter RemoteShooting/live-view mode (PTP_PROPERTIES_REFERENCE §4.1) so the
    camera answers property queries: DF00=6, DF01=22, negotiate DF2A version.
    If lv_size/lv_quality given, set 0xD174/0xD173 BEFORE InitiateOpenCapture (pre-start)
    to test whether they configure the through-picture stream."""
    out: dict = {}
    out["df00_set6"] = session.set_prop_value(0xDF00, struct.pack("<H", 6), "lv")
    out["df01_set22"] = session.set_prop_value(0xDF01, struct.pack("<H", 22), "lv")
    getver = session.get_prop_value(0xDF2A)
    out["df2a_get"] = getver
    raw = bytes.fromhex(getver.get("value", {}).get("raw_hex", "")) if getver.get("value") else b""
    if raw:
        target = min(int.from_bytes(raw, "little"), 4)
        out["df2a_set"] = session.set_prop_value(0xDF2A, target.to_bytes(len(raw), "little"), "lv")
    if lv_size:
        out["pre_set_size"] = session.set_prop_value(0xD174, struct.pack("<H", lv_size), "lvsize")
    if lv_quality:
        out["pre_set_quality"] = session.set_prop_value(0xD173, struct.pack("<H", lv_quality), "lvqual")
    # InitiateOpenCapture(0,0) — start live view. The reference app does this BEFORE exposure
    # writes; without a running live-view capture the camera ACKs prop writes but
    # ignores them. This is the control-grant the earlier probes were missing.
    out["initiate_open_capture"] = session._txn_get(0x101C, (0, 0), "initiateopencapture")
    return out


def phase1(session: Session) -> dict:
    # Values first (GetDevicePropValue 0x1015 is reference app-proven), incl. the 0xD212
    # bundle, so a later descriptor timeout cannot lose the value evidence.
    values: dict = {}
    for prop in READ_VALUE_PROPS:
        if not session.alive:
            break
        values[f"0x{prop:04x}"] = session.get_prop_value(prop)
    descriptors: dict = {}
    for prop in READ_DESC_PROPS:
        if not session.alive:
            break
        descriptors[f"0x{prop:04x}"] = session.get_prop_desc(prop)
    return {"descriptors": descriptors, "values": values}


def phase2(session: Session, phase1_result: dict) -> dict:
    out: dict = {}
    for prop in (0x500F, 0xD02A):
        if not session.alive:
            break
        desc = phase1_result["descriptors"].get(f"0x{prop:04x}", {}).get("descriptor", {})
        value = phase1_result["values"].get(f"0x{prop:04x}", {}).get("value", {})
        if not desc.get("writable"):
            out[f"0x{prop:04x}"] = {"skipped": "not writable per descriptor"}
            continue
        raw = bytes.fromhex(value.get("raw_hex", ""))
        if not raw:
            out[f"0x{prop:04x}"] = {"skipped": "no current value to write back"}
            continue
        write = session.set_prop_value(prop, raw, "noop")
        readback = {p: session.get_prop_value(p) for p in (0x500F, 0xD02A, 0xD02B, 0xD212) if session.alive}
        out[f"0x{prop:04x}"] = {"writeback": write, "readback": {f"0x{k:04x}": v for k, v in readback.items()}}
    return out


def phase3(session: Session, write_prop: int, target: int, phase1_result: dict,
           force: bool = False, pc_priority: bool = False) -> dict:
    """Set ISO via SetDevicePropValue(0xD02A, signed-long-as-uint32). Only writes a
    value currently in the camera's Cap list (GetDevicePropDesc enum) unless --force,
    because writing an out-of-list value is what dropped the socket previously."""
    desc = phase1_result["descriptors"].get(f"0x{write_prop:04x}", {}).get("descriptor", {})
    valid = desc.get("enum_values", []) if desc.get("form") == "enum" else []
    out: dict = {
        "write_prop": f"0x{write_prop:04x}",
        "target_iso": target,
        "label": SDK_ISO_AUTO.get(target, "manual" if target in SDK_ISO_MANUAL else "unknown"),
        "current_cap_list": valid,
    }
    if pc_priority:
        # XSDK_SetPriorityMode(PC) → SetDevicePropValue(0xD207, 2) grants the PC
        # control of exposure (default is Camera Priority, which ignores remote sets).
        out["pc_priority_write"] = session.set_prop_value(0xD207, struct.pack("<H", 2), "pcpri")
    if not force:
        if not valid:
            out["refused"] = ("camera Cap list is empty (ISO in AUTO/locked state) — not "
                              "accepting manual ISO over the wire now; switch ISO mode/dial or --force")
            return out
        if target not in valid:
            out["refused"] = f"ISO {target} not in current Cap list {valid} (use a listed value or --force)"
            return out
    # Encode at the prop's datatype width (0x5007 aperture=UINT16 → 2 bytes;
    # 0xD02A ISO=UINT32 → 4 bytes). signed two's-complement covers AUTO sentinels.
    dt = int(desc.get("data_type", "0x0006"), 16) if desc.get("data_type") else 0x0006
    width = DATATYPE.get(dt, ("", 4, False))[1]
    value = (target & ((1 << (width * 8)) - 1)).to_bytes(width, "little")
    out["value_hex"] = value.hex()
    out["write"] = session.set_prop_value(write_prop, value, f"set{target}")
    out["readback"] = {f"0x{p:04x}": session.get_prop_value(p)
                       for p in (write_prop, 0xD02A, 0xD212) if session.alive}
    return out


def vendor_step(session: Session, opcode: int, direction: int) -> dict:
    """Camera-managed relative step (shutter 0x902C / aperture 0x902D); param = direction
    (1=up/0=down per reference app). Read result from the 0xD212 bundle."""
    result = session._txn_get(opcode, (direction & 0xFFFFFFFF,), f"step_{opcode:04x}_{direction}")
    result["readback_d212"] = session.get_prop_value(0xD212) if session.alive else None
    return result


def stream_liveview(host: str, tp_port: int, session_dir: Path, max_frames: int,
                    max_secs: float, timeout: float, save_frames: int = 3) -> dict:
    """After InitiateOpenCapture(0x101C) on the command channel, the camera PUSHES the
    through-picture JPEG stream on a separate channel (55742). Frame protocol:
    <u32 LE total-length incl. the 4-byte prefix><14-byte header (seq# at +4)><JPEG…>.
    A frame's body can span several TCP reads, so read exactly `length` bytes per frame.
    Pure read path — does not take control from the photographer (works in Camera Priority)."""
    out: dict = {"tp_port": tp_port, "frames": 0, "jpegs_saved": 0, "sizes": [],
                 "size_min": None, "size_max": None, "errors": []}
    start = time.monotonic()
    saved = 0
    last_save = 0.0
    save_interval = (max_secs / save_frames) if save_frames else 1e9  # spread saves across the run
    try:
        tp = socket.create_connection((host, tp_port), timeout)
    except OSError as exc:
        out["errors"].append(f"connect {tp_port}: {exc!r}")
        return out
    tp.settimeout(timeout)
    with tp:
        while out["frames"] < max_frames and (time.monotonic() - start) < max_secs:
            hdr = ptpip.read_exact(tp, 4)
            if len(hdr) != 4:
                out["errors"].append("tp closed / short length")
                break
            total = int.from_bytes(hdr, "little")
            if total < 4 or total > 64 * 1024 * 1024:
                out["errors"].append(f"bad frame length {total}")
                break
            body = ptpip.read_exact(tp, total - 4)
            if len(body) != total - 4:
                out["errors"].append("short frame body")
                break
            soi = body.find(b"\xff\xd8")
            if soi < 0:
                continue  # non-JPEG through-picture payload (telemetry)
            eoi = body.find(b"\xff\xd9", soi)
            jpeg = body[soi:eoi + 2] if eoi > 0 else body[soi:]
            out["frames"] += 1
            sz = len(jpeg)
            if len(out["sizes"]) < 120:
                out["sizes"].append(sz)
            out["size_min"] = sz if out["size_min"] is None else min(out["size_min"], sz)
            out["size_max"] = sz if out["size_max"] is None else max(out["size_max"], sz)
            now = time.monotonic()
            if eoi > 0 and saved < save_frames and (saved == 0 or now - last_save >= save_interval):
                secs = int(now - start)
                (session_dir / f"liveview_frame_{saved:03d}_{secs:02d}s.jpg").write_bytes(jpeg)
                saved += 1
                last_save = now
    out["jpegs_saved"] = saved
    elapsed = time.monotonic() - start
    out["elapsed_s"] = round(elapsed, 2)
    out["fps"] = round(out["frames"] / elapsed, 2) if elapsed > 0 else 0
    return out


def jpeg_dims(jpeg: bytes) -> tuple:
    """(width, height) from the first SOF marker (0xFFC0-0xFFCF except DHT/DAC/RST)."""
    i = 2
    while i + 9 < len(jpeg):
        if jpeg[i] != 0xFF:
            i += 1
            continue
        marker = jpeg[i + 1]
        if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
            h = int.from_bytes(jpeg[i + 5:i + 7], "big")
            w = int.from_bytes(jpeg[i + 7:i + 9], "big")
            return (w, h)
        seg = int.from_bytes(jpeg[i + 2:i + 4], "big")
        i += 2 + seg
    return (0, 0)


SIZE_NAMES = {1: "L", 2: "M", 3: "S"}
QUAL_NAMES = {1: "FINE", 2: "NORMAL", 3: "BASIC"}


def _read_frames(tp, secs: float) -> dict:
    """Read frames from an already-open through-picture socket for `secs`."""
    rec = {"frames": 0, "bytes": [], "dims": None}
    start = time.monotonic()
    while (time.monotonic() - start) < secs:
        hdr = ptpip.read_exact(tp, 4)
        if len(hdr) != 4:
            break
        total = int.from_bytes(hdr, "little")
        if total < 4 or total > 64 * 1024 * 1024:
            break
        body = ptpip.read_exact(tp, total - 4)
        soi = body.find(b"\xff\xd8")
        if soi < 0:
            continue
        eoi = body.find(b"\xff\xd9", soi)
        jpeg = body[soi:eoi + 2] if eoi > 0 else body[soi:]
        rec["frames"] += 1
        rec["bytes"].append(len(jpeg))
        if eoi > 0:
            rec["dims"] = jpeg_dims(jpeg)  # track latest (catches size change)
    el = time.monotonic() - start
    b = rec.pop("bytes")
    rec["fps"] = round(rec["frames"] / el, 1) if el > 0 else 0
    rec["bytes_avg"] = round(sum(b) / len(b)) if b else 0
    rec["bytes_min"], rec["bytes_max"] = (min(b), max(b)) if b else (0, 0)
    rec["kbps"] = round(sum(b) / 1024 / el, 1) if el > 0 and b else 0
    return rec


def map_liveview(session: Session, host: str, tp_port: int, secs: float, timeout: float) -> dict:
    """Sweep live-view size (0xD174) × quality (0xD173) on ONE held-open through-picture
    socket (the camera refuses a 2nd TP connect per 0x101C). Set props on the command
    channel, drain the transition, then measure dims/fps/bytes/bandwidth per config."""
    out: dict = {"configs": []}
    try:
        tp = socket.create_connection((host, tp_port), timeout)
    except OSError as exc:
        out["error"] = repr(exc)
        return out
    tp.settimeout(timeout)
    with tp:
        for size in (1, 2, 3):
            for qual in (1, 2, 3):
                if not session.alive:
                    break
                sw = session.set_prop_value(0xD174, struct.pack("<H", size), f"lvsize{size}")
                qw = session.set_prop_value(0xD173, struct.pack("<H", qual), f"lvqual{qual}")
                try:
                    _read_frames(tp, 0.8)          # drain the transition
                    m = _read_frames(tp, secs)     # measure
                except OSError as exc:
                    out["configs"].append({"size": SIZE_NAMES[size], "quality": QUAL_NAMES[qual],
                                           "error": repr(exc)})
                    break
                out["configs"].append({
                    "size": f"{size}({SIZE_NAMES[size]})", "quality": f"{qual}({QUAL_NAMES[qual]})",
                    "set_size_ok": sw.get("response_ok"), "set_qual_ok": qw.get("response_ok"),
                    "dims": m.get("dims"), "fps": m.get("fps"), "bytes_avg": m.get("bytes_avg"),
                    "bytes_min": m.get("bytes_min"), "bytes_max": m.get("bytes_max"),
                    "kbps": m.get("kbps"), "frames": m.get("frames"),
                })
    return out


def sweep_props(session: Session, props: list[int]) -> dict:
    """Read GetDevicePropValue then GetDevicePropDesc for each prop (value first =
    more robust; a desc timeout that kills the session leaves the rest 'aborted')."""
    out: dict = {}
    for p in props:
        key = f"0x{p:04x}"
        if not session.alive:
            out[key] = {"aborted": "session dead"}
            continue
        v = session.get_prop_value(p)
        rec = {"val_resp": v.get("response_code"), "value": v.get("value", {})}
        if session.alive:
            d = session.get_prop_desc(p)
            de = d.get("descriptor", {})
            rec.update({"desc_resp": d.get("response_code"), "data_type": de.get("data_type_name"),
                        "writable": de.get("writable"), "form": de.get("form"),
                        "current": de.get("current_value"), "enum": de.get("enum_values")})
        out[key] = rec
    return out


def render_table(phase1_result: dict) -> str:
    lines = ["| prop | datatype | get/set | current | form | legal values |", "|---|---|---|---|---|---|"]
    for prop in READ_DESC_PROPS:
        key = f"0x{prop:04x}"
        d = phase1_result["descriptors"].get(key, {}).get("descriptor", {})
        if not d:
            r = phase1_result["descriptors"].get(key, {})
            lines.append(f"| {key} | (no descriptor) | | | | {r.get('response_code', r.get('recv_error', '?'))} |")
            continue
        form = d.get("form", "?")
        if form == "enum":
            legal = ", ".join(str(v) for v in d.get("enum_values", []))
        elif form == "range":
            legal = f"{d.get('range_min')}..{d.get('range_max')} step {d.get('range_step')}"
        else:
            legal = ""
        gs = "RW" if d.get("writable") else "RO"
        lines.append(
            f"| {key} | {d.get('data_type_name')} | {gs} | {d.get('current_value')} | {form} | {legal} |"
        )
    return "\n".join(lines)


def open_control_session(host: str, port: int, guid: str, name: str, timeout: float, session_dir: Path) -> tuple:
    summary: dict = {
        "host": host,
        "port": port,
        "guid": guid,
        "friendly_name": name,
        "tcp_connect": "absent",
        "init_response_present": False,
        "open_session_ok": False,
    }
    sock = socket.create_connection((host, port), timeout)
    sock.settimeout(timeout)
    summary["tcp_connect"] = "present"
    init = ptpip.build_init_command_request(name, "liveview", guid=ptpip.parse_guid_hex(guid))
    (session_dir / "init_command_request.bin").write_bytes(init)
    sock.sendall(init)
    init_resp = ptpip.recv_packet(sock)
    (session_dir / "init_command_response.bin").write_bytes(init_resp)
    summary["init_response_present"] = bool(init_resp)
    open_req = ptpip.build_open_session()
    (session_dir / "open_session_request.bin").write_bytes(open_req)
    sock.sendall(open_req)
    open_resp = ptpip.recv_packet(sock)
    (session_dir / "open_session_response.bin").write_bytes(open_resp)
    ohdr = ptpip.ptp_container_header(open_resp)
    summary["open_session_ok"] = ohdr.get("code") in (PTP_RESPONSE_OK, ptpip.PTP_RESPONSE_SESSION_ALREADY_OPEN)
    return sock, summary


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="probe-iso-liveview")
    parser.add_argument("--session-dir", type=Path, required=True)
    parser.add_argument("--host", default="192.168.0.1")
    parser.add_argument("--port", type=int, default=55740)
    parser.add_argument("--guid", default="f2e4538fada5485d87b27f0bd3d5ded0")
    parser.add_argument("--friendly-name", default="mbp-7274")
    parser.add_argument("--timeout", type=float, default=6.0)
    parser.add_argument("--phase2", action="store_true", help="no-op writeback of current values (mutates)")
    parser.add_argument("--phase3", action="store_true", help="one safe ISO change (mutates)")
    parser.add_argument("--write-prop", default="0xd02a", help="ISO write prop (Android reference app writes 0xD02A; manual=literal, auto=0x80000000|ceiling)")
    parser.add_argument("--target", type=int, default=0, help="phase-3 ISO: manual literal (e.g. 400) or AUTO sentinel (-1)")
    parser.add_argument("--force", action="store_true", help="phase-3: write even if not in the current Cap list (risks socket drop)")
    parser.add_argument("--pc-priority", action="store_true", help="phase-3: set PC Priority (0xD207=2) before the ISO write")
    parser.add_argument("--stream-frames", type=int, default=0, help="capture N live-view JPEG frames after 0x101C")
    parser.add_argument("--tp-port", type=int, default=55742, help="through-picture (JPEG stream) TCP port")
    parser.add_argument("--save-frames", type=int, default=3, help="how many JPEG frames to save (spread across the run)")
    parser.add_argument("--map-liveview", action="store_true", help="sweep size(0xD174)×quality(0xD173), measure dims/fps/bytes/bandwidth")
    parser.add_argument("--map-secs", type=float, default=2.5, help="seconds to measure per size×quality config")
    parser.add_argument("--lv-size", type=int, default=0, help="set 0xD174 size (1=L/2=M/3=S) BEFORE live-view start")
    parser.add_argument("--lv-quality", type=int, default=0, help="set 0xD173 quality (1=FINE/2=NORMAL/3=BASIC) before start")
    parser.add_argument("--stream-secs", type=float, default=10.0, help="max seconds to stream")
    parser.add_argument("--camera-priority", action="store_true", help="set 0xD207=1 (Camera Priority) before streaming")
    parser.add_argument("--sweep-props", default="", help="comma list of DPCs to read desc+value (full property sweep)")
    parser.add_argument("--step-op", default="", help="vendor relative-step opcode, e.g. 0x902c (shutter) / 0x902d (aperture)")
    parser.add_argument("--step-dir", type=int, default=1, help="step direction (1=up/0=down)")
    args = parser.parse_args(argv)

    session_dir = args.session_dir
    session_dir.mkdir(parents=True, exist_ok=True)
    out: dict = {"started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    try:
        sock, summary = open_control_session(
            args.host, args.port, args.guid, args.friendly_name, args.timeout, session_dir
        )
    except OSError as exc:
        out["connect_error"] = repr(exc)
        (session_dir / "summary.json").write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
        print(json.dumps(out, indent=2, sort_keys=True))
        return 1
    out.update(summary)
    with sock:
        if not summary["open_session_ok"]:
            out["aborted"] = "open session not OK"
        else:
            session = Session(sock, session_dir, out)
            if args.camera_priority:
                out["set_camera_priority"] = session.set_prop_value(0xD207, struct.pack("<H", 1), "campri")
            out["live_view_handshake"] = live_view_handshake(session, args.lv_size, args.lv_quality)
            if args.map_liveview:
                out["liveview_map"] = map_liveview(session, args.host, args.tp_port, args.map_secs, args.timeout)
            if args.stream_frames:
                out["liveview_stream"] = stream_liveview(args.host, args.tp_port, session_dir,
                                                         args.stream_frames, args.stream_secs, args.timeout,
                                                         save_frames=args.save_frames)
            if args.sweep_props:
                props = [int(x, 16) for x in args.sweep_props.split(",") if x.strip()]
                out["sweep"] = sweep_props(session, props)
                out["sweep_count"] = len(props)
            out["phase1"] = phase1(session)
            out["phase1_table"] = render_table(out["phase1"])
            out["session_alive_after_phase1"] = session.alive
            if args.phase2 and session.alive:
                out["phase2"] = phase2(session, out["phase1"])
                out["session_alive_after_phase2"] = session.alive
            if args.phase3 and session.alive:
                out["phase3"] = phase3(session, int(args.write_prop, 16), args.target, out["phase1"],
                                       force=args.force, pc_priority=args.pc_priority)
                out["session_alive_after_phase3"] = session.alive
            if args.step_op and session.alive:
                out["vendor_step"] = vendor_step(session, int(args.step_op, 16), args.step_dir)
                out["session_alive_after_step"] = session.alive
            out["steps"] = session.steps
    (session_dir / "summary.json").write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    (session_dir / "phase1_table.md").write_text(out.get("phase1_table", "") + "\n")
    print(out.get("phase1_table", "(no phase1 table)"))
    print(f"\nsummary: {session_dir / 'summary.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
