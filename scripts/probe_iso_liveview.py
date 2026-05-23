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

# ISO label -> raw uint32, from observed safe iOS telemetry. Capped at 320.
SAFE_ISO = {
    80: 0x00000050,
    100: 0x00000064,
    125: 0x0000007D,
    160: 0x000000A0,
    200: 0x000000C8,
    250: 0x000000FA,
    320: 0x00000140,
}
ISO_HARD_CAP = 320


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


def live_view_handshake(session: Session) -> dict:
    """Enter RemoteShooting/live-view mode (PTP_PROPERTIES_REFERENCE §4.1) so the
    camera answers property queries: DF00=6, DF01=22, negotiate DF2A version."""
    out: dict = {}
    out["df00_set6"] = session.set_prop_value(0xDF00, struct.pack("<H", 6), "lv")
    out["df01_set22"] = session.set_prop_value(0xDF01, struct.pack("<H", 22), "lv")
    getver = session.get_prop_value(0xDF2A)
    out["df2a_get"] = getver
    raw = bytes.fromhex(getver.get("value", {}).get("raw_hex", "")) if getver.get("value") else b""
    if raw:
        target = min(int.from_bytes(raw, "little"), 4)
        out["df2a_set"] = session.set_prop_value(0xDF2A, target.to_bytes(len(raw), "little"), "lv")
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


def phase3(session: Session, write_prop: int, target: int, phase1_result: dict) -> dict:
    if target >= 400:
        return {"refused": f"ISO {target} >= 400 is the known command-socket killer; not attempted"}
    if target > ISO_HARD_CAP:
        return {"refused": f"ISO {target} exceeds hard cap {ISO_HARD_CAP}"}
    if target not in SAFE_ISO:
        return {"refused": f"ISO {target} not in proven-safe set {sorted(SAFE_ISO)}"}
    desc = phase1_result["descriptors"].get(f"0x{write_prop:04x}", {}).get("descriptor", {})
    size = DATATYPE.get(int(desc.get("data_type", "0x0"), 16), ("", 4, False))[1] if desc.get("data_type") else 4
    value = SAFE_ISO[target].to_bytes(size, "little")
    write = session.set_prop_value(write_prop, value, f"iso{target}")
    readback = {p: session.get_prop_value(p) for p in (0x500F, 0xD02A, 0xD02B, 0xD212) if session.alive}
    return {
        "write_prop": f"0x{write_prop:04x}",
        "target_iso": target,
        "value_hex": value.hex(),
        "write": write,
        "readback": {f"0x{k:04x}": v for k, v in readback.items()},
    }


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
    parser.add_argument("--write-prop", default="0x500f", help="prop to write in phase 3")
    parser.add_argument("--target", type=int, default=0, help="phase-3 target ISO (80..320)")
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
            out["live_view_handshake"] = live_view_handshake(session)
            out["phase1"] = phase1(session)
            out["phase1_table"] = render_table(out["phase1"])
            out["session_alive_after_phase1"] = session.alive
            if args.phase2 and session.alive:
                out["phase2"] = phase2(session, out["phase1"])
                out["session_alive_after_phase2"] = session.alive
            if args.phase3 and session.alive:
                out["phase3"] = phase3(session, int(args.write_prop, 16), args.target, out["phase1"])
                out["session_alive_after_phase3"] = session.alive
            out["steps"] = session.steps
    (session_dir / "summary.json").write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    (session_dir / "phase1_table.md").write_text(out.get("phase1_table", "") + "\n")
    print(out.get("phase1_table", "(no phase1 table)"))
    print(f"\nsummary: {session_dir / 'summary.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
