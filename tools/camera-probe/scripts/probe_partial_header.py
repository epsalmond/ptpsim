#!/usr/bin/env python3
"""LINCHPIN VALIDATION: can we GetPartialObject a stored image's HEADER ONLY (cheap metadata
read) over the PCSS tether, without pulling the whole file? If yes, the image-database /
star-filter / dedup / >4GB-handling vision is unblocked: enumerate handles, read each header
range for rating/subject/EXIF, transfer only what the user wants.

Path: PCSS connect -> Session -> image_receive_handshake(20) [coexist, NOT a takeover] ->
0xD620 count -> 0xD621 handles -> GetObjectInfo(0x1008) -> GetPartialObject(0x101b, h, 0, N).
Read-only (safe tier) except the non-takeover DF01=20 mode set; CloseSession at the end.

Usage: PYTHONPATH=. python3 scripts/probe_partial_header.py <camera_ip> [--bytes 4096]
"""
import argparse
import os
import struct
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import connect_wireless_tether as cwt          # noqa: E402
import pull_backup as pb                        # noqa: E402
import probe_iso_liveview as piv                # noqa: E402

OBJFMT = {0x3801: "JPEG/EXIF", 0x3000: "Undefined", 0xb103: "RAF(raw)",
          0x300d: "MOV", 0xb982: "MOV", 0x3808: "TIFF"}


def parse_handles(payload: bytes):
    if len(payload) < 4:
        return []
    n = struct.unpack_from("<I", payload, 0)[0]
    return [struct.unpack_from("<I", payload, 4 + 4 * i)[0] for i in range(min(n, (len(payload) - 4) // 4))]


def objinfo_fields(payload: bytes):
    # PTP ObjectInfo: StorageID u32 @0, ObjectFormat u16 @4, ProtStatus u16 @6,
    # ObjectCompressedSize u32 @8
    if len(payload) < 12:
        return None, None
    fmt = struct.unpack_from("<H", payload, 4)[0]
    size = struct.unpack_from("<I", payload, 8)[0]
    return fmt, size


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="probe-partial-header")
    ap.add_argument("camera_ip")
    ap.add_argument("--bytes", type=int, default=4096, help="header byte count to GetPartialObject")
    ap.add_argument("--session-dir", default="/tmp/partial_header_probe")
    args = ap.parse_args(argv)

    sdir = Path(args.session_dir)
    sdir.mkdir(parents=True, exist_ok=True)
    sock = pb.connect(args.camera_ip, None, cwt.DEFAULT_GUID, "mbp", 13)
    if sock is None:
        print("[fail] no PCSS connect")
        return 2
    sess = piv.Session(sock, sdir, {})
    sess.tid = 3  # connect did Init + OpenSession(1) + GetDeviceInfo(2)
    try:
        hs = piv.image_receive_handshake(sess, mode=20)
        print(f"[mode] image-receive(20) df01={hs.get('df01_set',{}).get('response_code')} "
              f"ver={hs.get('ver_set',{}).get('response_code')}")

        cnt = piv._u32(sess.get_prop_value(0xD620))
        print(f"[count] 0xD620 object count = {cnt}")

        # try vendor handle list 0xD621 first, fall back to standard GetObjectHandles
        h = sess._txn_get(0xD621, (), "handles_d621")
        handles = parse_handles(bytes.fromhex(h.get("payload_hex", ""))) if h.get("data_present") else []
        if not handles:
            h2 = sess._txn_get(0x1007, (0xFFFFFFFF, 0, 0), "handles_1007")
            handles = parse_handles(bytes.fromhex(h2.get("payload_hex", ""))) if h2.get("data_present") else []
        print(f"[handles] {len(handles)} found; first few: {[hex(x) for x in handles[:5]]}")
        if not handles:
            print("[stop] no stored-image handles to test (is there media on the card?)")
            return 1

        for handle in handles[:3]:
            info = sess._txn_get(0x1008, (handle,), f"objinfo_{handle:08x}")
            fmt, size = objinfo_fields(bytes.fromhex(info.get("payload_hex", ""))) if info.get("data_present") else (None, None)
            fmtname = OBJFMT.get(fmt, f"0x{fmt:04x}" if fmt is not None else "?")
            oversize = size == 0xFFFFFFFF
            print(f"\n[obj 0x{handle:08x}] format={fmtname} size={size} "
                  f"({'>4GB ceiling (0xFFFFFFFF)' if oversize else f'{size} B' if size else '?'})")

            part = sess._txn_get(0x101b, (handle, 0, args.bytes), f"partial_{handle:08x}")
            if not part.get("data_present"):
                print(f"  [partial] NO DATA — resp={part.get('response_code')} "
                      f"(GetPartialObject may be unsupported / wrong mode)")
                continue
            payload = bytes.fromhex(part["payload_hex"])
            print(f"  [partial] GetPartialObject(h,0,{args.bytes}) -> {len(payload)} B, resp={part.get('response_code')}")
            print(f"  [header]  {payload[:16].hex(' ')}")
            sig = ("JPEG/JFIF" if payload[:2] == b"\xff\xd8" else
                   "TIFF/RAF-LE" if payload[:2] == b"II" else
                   "RAF" if payload[:4] == b"FUJI" else "?")
            print(f"  [sig]     {sig}")
    finally:
        try:
            sess.close_session()
        except OSError:
            pass
        sock.close()
    print("\n[done] partial-header probe complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
