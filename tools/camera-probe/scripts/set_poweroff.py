#!/usr/bin/env python3
"""Set the GFX100 II Auto Power Off over the tether. DPC 0xD364 (CustomAutoPowerOff), UINT8.
Value 0 = OFF/never (confirmed 2026-04-30). Set this before long sweeps so the camera can't
auto-power-off mid-run and drop the session.

Usage: PYTHONPATH=. python scripts/set_poweroff.py <camera_ip> [--value 0]
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import connect_wireless_tether as cwt  # noqa: E402
import probe_iso_liveview as piv        # noqa: E402
import pull_backup as pb                # noqa: E402
from rce.tools.fuji_ble_gps import ptpip  # noqa: E402

POWEROFF_PROP = 0xD364


def set_autopoweroff(sock, value, tid):
    """Read 0xD364 desc, set it (UINT8), return (before, after_resp, next_tid)."""
    data, code = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1014, tid, POWEROFF_PROP))
    tid += 1
    desc = piv.decode_device_prop_desc(data) if data and code == ptpip.PTP_RESPONSE_OK else {}
    before = desc.get("current_value")
    sock.sendall(ptpip.build_set_device_prop_value(POWEROFF_PROP, tid))
    sock.sendall(ptpip.build_ptp_data_container(
        ptpip.PTP_SET_DEVICE_PROP_VALUE, tid, value.to_bytes(1, "little")))
    _, rc = cwt.ptp_op(sock, b"")
    return before, rc, tid + 1


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="set-poweroff")
    p.add_argument("camera_ip")
    p.add_argument("--my-ip", default=None)
    p.add_argument("--guid", default=cwt.DEFAULT_GUID)
    p.add_argument("--name", default="mbp")
    p.add_argument("--retries", type=int, default=12)
    p.add_argument("--value", type=int, default=0, help="0=OFF/never (default); higher=timeout step")
    args = p.parse_args(argv)

    sock = pb.connect(args.camera_ip, args.my_ip, args.guid, args.name, args.retries)
    if sock is None:
        return 2
    try:
        before, rc, _ = set_autopoweroff(sock, args.value, 3)
        ok = rc == ptpip.PTP_RESPONSE_OK
        print(f"[poweroff] 0x{POWEROFF_PROP:04x}: {before} -> {args.value}  resp=0x{(rc or 0):04x} "
              f"({'OK — auto power off ' + ('disabled' if args.value == 0 else 'set') if ok else 'FAILED'})")
    finally:
        try:
            sock.sendall(ptpip.build_close_session(transaction_id=900))
            sock.close()
        except OSError:
            pass
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
