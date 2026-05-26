#!/usr/bin/env python3
"""Issue a GFX100 II settings reset over the tether — a DELIBERATE, destructive action.

`SetDevicePropValue(0xd17f, 5)` = Shooting-Menu Reset (wipes shoot settings; PRESERVES Wi-Fi
band/network — confirmed live 2026-05-23). Other enum values likely map to the camera's other
Reset menu items (Set-up / All); run with --show to read the live descriptor's enum before choosing.
Also the standard remediation after a firmware downgrade (clears post-downgrade gremlins).

Usage:
  PYTHONPATH=. python scripts/issue_reset.py <camera_ip> --show          # read 0xd17f enum, no write
  PYTHONPATH=. python scripts/issue_reset.py <camera_ip> --value 5 --confirm   # perform the reset
WARNING: this erases settings and the camera typically reboots/drops the network afterward.
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import connect_wireless_tether as cwt  # noqa: E402
import probe_iso_liveview as piv        # noqa: E402
import pull_backup as pb                # noqa: E402
from rce.tools.fuji_ble_gps import ptpip  # noqa: E402

RESET_PROP = 0xD17F


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="issue-reset")
    p.add_argument("camera_ip")
    p.add_argument("--my-ip", default=None)
    p.add_argument("--guid", default=cwt.DEFAULT_GUID)
    p.add_argument("--name", default="mbp")
    p.add_argument("--retries", type=int, default=12)
    p.add_argument("--value", type=int, default=5, help="0xd17f enum value (5 = shooting reset)")
    p.add_argument("--show", action="store_true", help="read the 0xd17f descriptor enum, no write")
    p.add_argument("--confirm", action="store_true", help="REQUIRED to actually perform the reset")
    args = p.parse_args(argv)

    if not args.show and not args.confirm:
        print("[refuse] destructive. Pass --show to inspect, or --confirm to perform the reset.")
        return 2

    sock = pb.connect(args.camera_ip, args.my_ip, args.guid, args.name, args.retries)
    if sock is None:
        return 2
    tid = 3
    try:
        data, code = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1014, tid, RESET_PROP))
        tid += 1
        desc = piv.decode_device_prop_desc(data) if data and code == ptpip.PTP_RESPONSE_OK else {}
        print(f"[desc] 0x{RESET_PROP:04x} type={desc.get('data_type_name')} "
              f"current={desc.get('current_value')} form={desc.get('form')} "
              f"enum={desc.get('enum_values')}")
        if args.show:
            return 0
        sock.sendall(ptpip.build_set_device_prop_value(RESET_PROP, tid))
        sock.sendall(ptpip.build_ptp_data_container(
            ptpip.PTP_SET_DEVICE_PROP_VALUE, tid, args.value.to_bytes(2, "little")))
        _, rc = cwt.ptp_op(sock, b"")
        print(f"[reset] SetDevicePropValue(0x{RESET_PROP:04x}={args.value}) -> 0x{(rc or 0):04x} "
              f"({'OK — camera will reset/reboot' if rc == ptpip.PTP_RESPONSE_OK else 'see code'})")
    finally:
        try:
            sock.close()
        except OSError:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
