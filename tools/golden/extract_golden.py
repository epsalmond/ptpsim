#!/usr/bin/env python3
"""extract_golden.py — the canonical "golden packet" cherry-picker.

A golden packet is the bytes of ONE protocol frame, redacted and **labeled**, so
it doubles as documentation: instead of keeping a whole pcap to reference during
analysis, the public repo carries small self-describing fixtures under
`packages/camera-config-data/golden/`.

This tool only ever emits those minimal, redacted, labeled artifacts — never the
source capture. The source is read in place (it lives outside the repo) and only
its *name* (not its path or bytes) is recorded in the golden's provenance.

Source kinds:
  frida   A frida payload blob (or a dir of them). Frida already cherry-picks
          payloads at runtime, so each blob is one frame — pick by decoded op/type.
  pcap    A pcap/pcapng. REQUIRES --host: only the conversation with that single
          address is touched (nothing else from the file is read into a golden).
          Uses tshark.
  raw     A single .bin already containing one frame.

Use `scan` first to list the decodable frames in a blob dir and choose one.

Examples:

  extract_golden.py frida --blobs <dir> --select op:0x1002 \
      --label open-session-request --description "OpenSession on reference app cmd channel" \
      --transport app --firmware 02.30
  extract_golden.py pcap  --file capture.pcapng --host 192.168.0.1 --port 55740 \
      --select op:0x1001 --label get-device-info-request --transport pcss
"""
from __future__ import annotations

import argparse
import datetime as _dt
import glob
import os
import struct
import subprocess
import sys

# --- redaction --------------------------------------------------------------
# Known device-identifying byte sequences, replaced with an equal-length marker
# so frame offsets are preserved. Extend via --redact <hex>.
KNOWN_REDACTIONS = {
    "0870b0610a8b4593b2e79357dd36e050": "device-guid",  # GFX100 II BLE/PTP GUID
}


def redact(buf: bytes, extra_hex: list[str]) -> tuple[bytes, list[str]]:
    applied: list[str] = []
    out = buf
    patterns = dict(KNOWN_REDACTIONS)
    for h in extra_hex:
        patterns[h.lower()] = f"custom:{h.lower()}"
    for hexpat, name in patterns.items():
        pat = bytes.fromhex(hexpat)
        if pat and pat in out:
            out = out.replace(pat, b"\x00" * len(pat))
            applied.append(name)
    return out, applied


# --- minimal framing decode (mirrors ptp-core / protocol-primitives) --------
STD_TYPES = {1: "InitCommandRequest", 2: "InitCommandAck", 6: "OperationRequest",
             7: "OperationResponse", 8: "Event", 9: "StartData", 10: "Data", 12: "EndData"}
FUJI_TYPES = {1: "OperationRequest", 2: "OperationResponse", 9: "StartData",
              10: "Data", 12: "EndData"}


def decode_header(buf: bytes) -> dict | None:
    """Identify a frame's framing/type/op without a full payload parse."""
    if len(buf) < 8:
        return None
    length = struct.unpack_from("<I", buf, 0)[0]
    if length != len(buf):
        return None
    # Standard PTP/IP: 4-byte type. InitCommandRequest is the 82-byte hello.
    t32 = struct.unpack_from("<I", buf, 4)[0]
    if t32 in STD_TYPES:
        info = {"framing": "ptpip-standard", "type": STD_TYPES[t32]}
        if t32 in (6, 7) and len(buf) >= 14:  # op/resp: dataphase(4) code(2) tid(4)
            info["code"] = struct.unpack_from("<H", buf, 12)[0]
            info["tid"] = struct.unpack_from("<I", buf, 14)[0] if len(buf) >= 18 else None
        return info
    # Fuji compressed: 2-byte type @4, code @6, tid @8.
    t16 = struct.unpack_from("<H", buf, 4)[0]
    if t16 in FUJI_TYPES:
        info = {"framing": "fuji-compressed", "type": FUJI_TYPES[t16]}
        if len(buf) >= 12:
            info["code"] = struct.unpack_from("<H", buf, 6)[0]
            info["tid"] = struct.unpack_from("<I", buf, 8)[0]
        return info
    return None


def matches(info: dict, select: str | None) -> bool:
    if not select:
        return True
    kind, _, val = select.partition(":")
    if kind == "op":
        return info.get("code") == int(val, 16)
    if kind == "type":
        return info.get("type", "").lower() == val.lower()
    return False


# --- sources ----------------------------------------------------------------
def blobs_in(path: str) -> list[str]:
    if os.path.isdir(path):
        return sorted(glob.glob(os.path.join(path, "*.bin")))
    return [path]


def pcap_payload(file: str, host: str, port: int | None) -> bytes:
    """Reassemble the TCP payload of the single conversation with `host`."""
    flt = f"ip.addr=={host} or ipv6.addr=={host}"
    if port:
        flt = f"({flt}) and tcp.port=={port}"
    out = subprocess.run(
        ["tshark", "-r", file, "-Y", flt, "-T", "fields", "-e", "tcp.payload"],
        capture_output=True, text=True, check=True,
    ).stdout
    data = bytearray()
    for line in out.splitlines():
        line = line.strip().replace(":", "")
        if line:
            data += bytes.fromhex(line)
    return bytes(data)


def first_frame(stream: bytes) -> bytes | None:
    """Take the first length-prefixed frame off a reassembled byte stream."""
    if len(stream) < 4:
        return None
    n = struct.unpack_from("<I", stream, 0)[0]
    if 8 <= n <= len(stream):
        return stream[:n]
    return None


# USB-PTP container types (PIMA 15740 USB transport).
USB_PTP_TYPES = {1: "OperationRequest", 2: "Data", 3: "OperationResponse", 4: "Event"}


def usb_control_containers(file: str):
    """Yield (bytes, info) for each self-contained USB-PTP *control* container
    (op/response/event) in a usbmon capture. Bulk data containers (type 2) span
    many URBs and are skipped — they are the file transfer, never a golden."""
    out = subprocess.run(
        ["tshark", "-r", file, "-Y", "usb.capdata", "-T", "fields", "-e", "usb.capdata"],
        capture_output=True, text=True, check=True,
    ).stdout
    for line in out.splitlines():
        h = line.strip().replace(":", "")
        if len(h) < 24:  # need at least the 12-byte header
            continue
        try:
            b = bytes.fromhex(h)
        except ValueError:
            continue
        length, ctype, code, tid = struct.unpack_from("<IHHI", b, 0)
        # Whole control container present in this one capdata frame.
        if ctype in (1, 3, 4) and length == len(b) and length >= 12:
            yield b, {"framing": "usb-ptp", "type": USB_PTP_TYPES[ctype], "code": code, "tid": tid}


# --- output -----------------------------------------------------------------
def emit(args, source_name, kind, address, raw: bytes, info=None):
    """Write a labeled golden. `info` (framing/type/code/tid) may be supplied by
    the caller (e.g. usbmon, whose framing can't be auto-detected from bytes);
    otherwise it is auto-decoded."""
    redacted, applied = redact(raw, args.redact or [])
    if info is None:
        info = decode_header(redacted) or {}
    label = args.label
    out_dir = args.out
    os.makedirs(out_dir, exist_ok=True)
    path = os.path.join(out_dir, f"{label}.yaml")
    code = info.get("code")
    doc = {
        "type": info.get("type", "unknown"),
        "op": f"0x{code:04x}" if code is not None else None,
        "tid": info.get("tid"),
    }
    lines = [
        "# Golden packet — labeled fixture doubling as documentation.",
        "# Minimal, redacted, single frame. Source bytes are NOT the capture.",
        f"label: {label}",
        "description: >-",
        f"  {args.description}",
        "source:",
        f"  capture: {source_name}        # name only — not the path, not the file",
        f"  kind: {kind}",
        f"  address: {address if address else 'null'}",
        f"  selector: {args.select or 'null'}",
        f"  extracted: \"{_dt.date.today().isoformat()}\"",
        f"transport: {args.transport}",
        f"firmware: \"{args.firmware}\"" if args.firmware else "firmware: null",
        f"framing: {info.get('framing', 'unknown')}",
        "decoded:",
        f"  type: \"{doc['type']}\"",
        f"  op: \"{doc['op']}\"" if doc["op"] else "  op: null",
        f"  tid: {doc['tid'] if doc['tid'] is not None else 'null'}",
        f"redactions: [{', '.join(applied)}]",
        f"bytes_hex: \"{redacted.hex()}\"",
    ]
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"[golden] {label}: {doc['type']} {doc['op']} ({len(redacted)}B, "
          f"redacted={applied or 'none'}) -> {path}")


# --- commands ---------------------------------------------------------------
def cmd_scan(args):
    for b in blobs_in(args.blobs):
        raw = open(b, "rb").read()
        info = decode_header(raw)
        if info:
            code = info.get("code")
            print(f"{os.path.basename(b):24s} {info['framing']:16s} {info['type']:20s} "
                  f"{'0x%04x' % code if code is not None else '':8s} {len(raw)}B")


def cmd_frida(args):
    for b in blobs_in(args.blobs):
        raw = open(b, "rb").read()
        info = decode_header(raw)
        if info and matches(info, args.select):
            emit(args, os.path.basename(os.path.dirname(os.path.abspath(b))) or os.path.basename(b),
                 "frida", None, raw)
            return
    sys.exit(f"no frame matching {args.select!r} found in {args.blobs}")


def cmd_pcap(args):
    if not args.host:
        sys.exit("pcap mode requires --host: only that single address is touched")
    stream = pcap_payload(args.file, args.host, args.port)
    frame = first_frame(stream) if not args.select else None
    if frame is None:
        # scan frames in the stream for the selector
        off = 0
        while off + 4 <= len(stream):
            n = struct.unpack_from("<I", stream, off)[0]
            if not (8 <= n <= len(stream) - off):
                break
            f = stream[off:off + n]
            if matches(decode_header(f) or {}, args.select):
                frame = f
                break
            off += n
    if frame is None:
        sys.exit(f"no frame matching {args.select!r} from {args.host}")
    emit(args, os.path.basename(args.file), "pcap", args.host, frame)


def cmd_raw(args):
    raw = open(args.file, "rb").read()
    emit(args, os.path.basename(args.file), "raw", None, raw)


def cmd_usbscan(args):
    for _b, info in usb_control_containers(args.file):
        print(f"{info['type']:20s} 0x{info['code']:04x} tid={info['tid']}")


def cmd_usbmon(args):
    for b, info in usb_control_containers(args.file):
        if matches(info, args.select):
            emit(args, os.path.basename(args.file), "usbmon", None, b, info=info)
            return
    sys.exit(f"no USB-PTP control container matching {args.select!r} in {args.file}")


def main(argv=None):
    p = argparse.ArgumentParser(description="Cherry-pick redacted, labeled golden packets.")
    sub = p.add_subparsers(dest="cmd", required=True)

    def common(sp):
        sp.add_argument("--label", required=True)
        sp.add_argument("--description", default="")
        sp.add_argument("--transport", default="app")
        sp.add_argument("--firmware", default="")
        sp.add_argument("--select", default=None, help="op:0xXXXX | type:NAME")
        sp.add_argument("--redact", action="append", help="extra hex pattern to redact")
        sp.add_argument("--out", default="packages/camera-config-data/golden")

    sp = sub.add_parser("scan")
    sp.add_argument("--blobs", required=True)

    sp = sub.add_parser("frida")
    sp.add_argument("--blobs", required=True)
    common(sp)

    sp = sub.add_parser("pcap")
    sp.add_argument("--file", required=True)
    sp.add_argument("--host", required=True)
    sp.add_argument("--port", type=int)
    common(sp)

    sp = sub.add_parser("raw")
    sp.add_argument("--file", required=True)
    common(sp)

    sp = sub.add_parser("usbscan")
    sp.add_argument("--file", required=True)

    sp = sub.add_parser("usbmon")
    sp.add_argument("--file", required=True)
    common(sp)

    args = p.parse_args(argv)
    {
        "scan": cmd_scan, "frida": cmd_frida, "pcap": cmd_pcap, "raw": cmd_raw,
        "usbscan": cmd_usbscan, "usbmon": cmd_usbmon,
    }[args.cmd](args)


if __name__ == "__main__":
    main()
