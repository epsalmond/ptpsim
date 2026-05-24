#!/usr/bin/env python3
"""Targeted before->after probe for props that DESYNC the PTP stream on set (the bulk
backup_sweep skips them). Method: pull BEFORE, read current value, set an alternate, then
CLOSE + RECONNECT (the desync forces this anyway — we make it deliberate), pull AFTER, diff,
and set the value back. Pins the .dat byte for props the normal sweep can't reach.

Usage: PYTHONPATH=. python3 scripts/probe_desync_props.py <camera_ip> 0xd23b,0xd2a1
"""
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import connect_wireless_tether as cwt   # noqa: E402
import pull_backup as pb                 # noqa: E402
from rce.tools.fuji_ble_gps import ptpip  # noqa: E402

GUID, NAME, RETRIES = cwt.DEFAULT_GUID, "mbp", 13


def get_u16(sock, prop, tid):
    data, code = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1015, tid, prop))
    if code != ptpip.PTP_RESPONSE_OK or not data:
        return None, tid + 1
    return struct.unpack_from("<H", data, 0)[0], tid + 1


def set_u16(sock, prop, value, tid):
    sock.sendall(ptpip.build_set_device_prop_value(prop, tid))
    sock.sendall(ptpip.build_ptp_data_container(
        ptpip.PTP_SET_DEVICE_PROP_VALUE, tid, struct.pack("<H", value)))
    _, code = cwt.ptp_op(sock, b"")
    return code, tid + 1


def diff(a, b):
    return [i for i in range(min(len(a), len(b))) if a[i] != b[i]]


def main():
    ip = sys.argv[1]
    props = [int(x, 16) for x in sys.argv[2].split(",")]
    for prop in props:
        print(f"\n===== probing 0x{prop:04x} =====")
        sock = pb.connect(ip, None, GUID, NAME, RETRIES)
        if sock is None:
            print("  [skip] no connect")
            continue
        tid = 3
        before, _, tid = pb.pull_backup(sock, tid)
        cur, tid = get_u16(sock, prop, tid)
        if cur is None:
            print("  [skip] could not read current value")
            sock.close()
            continue
        alt = cur + 1 if cur < 3 else cur - 1
        print(f"  current=0x{cur:04x} -> setting alt=0x{alt:04x}")
        code, tid = set_u16(sock, prop, alt, tid)
        print(f"  set resp=0x{(code or 0):04x}; closing + reconnecting to clear desync")
        try:
            sock.close()
        except OSError:
            pass

        sock2 = pb.connect(ip, None, GUID, NAME, RETRIES)
        if sock2 is None:
            print("  [warn] could not reconnect to pull AFTER — value left at alt!")
            continue
        tid = 3
        after, _, tid = pb.pull_backup(sock2, tid)
        d = diff(before, after)
        print(f"  DIFF offsets: {[hex(x) for x in d]}" if d else "  NO DELTA (not persisted)")
        # restore original value
        code, tid = set_u16(sock2, prop, cur, tid)
        print(f"  restored to 0x{cur:04x} resp=0x{(code or 0):04x}")
        try:
            sock2.close()
        except OSError:
            pass


if __name__ == "__main__":
    main()
