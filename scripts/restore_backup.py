#!/usr/bin/env python3
"""Restore (write back) a camera-settings .dat to the GFX100 II over the tether.
This is the SetBackupSettings primitive — decoded + confirmed in BACKUP_RESTORE_PRIMITIVE.md
(USB), here ported to the network PCSS/PTP-IP session.

Wire flow (unsigned blob, no CRC):
  prelude: SetDevicePropValue d21c=0, d207=1, d21c=0   (d207 may answer 0x201c on tether — ok)
  SendObjectInfo(0x100C, params 0,0) + 1076-byte ObjectInfo (StorageID=0, ObjectFormat=0x5000,
      ProtStatus=0, CompressedSize=N, rest zero)
  SendObject(0x100D) + N-byte payload
Both respond 0x2001. **The camera REBOOTS to apply.**

Usage: PYTHONPATH=. python scripts/restore_backup.py <camera_ip> <backup.dat>
DANGER: writes camera settings + reboots. A failed restore does NOT partially apply (per RE).
"""
import argparse
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import connect_wireless_tether as cwt  # noqa: E402
import pull_backup as pb                # noqa: E402
from rce.tools.fuji_ble_gps import ptpip  # noqa: E402


def set_u16(sock, prop, value, tid):
    sock.sendall(ptpip.build_set_device_prop_value(prop, tid))
    sock.sendall(ptpip.build_ptp_data_container(
        ptpip.PTP_SET_DEVICE_PROP_VALUE, tid, struct.pack("<H", value)))
    _, code = cwt.ptp_op(sock, b"")
    return code, tid + 1


def restore(sock, payload, tid):
    # prelude (session prologue + per-restore prelude collapsed; d207 0x201c tolerated)
    for prop, val in ((0xd21c, 0), (0xd207, 1), (0xd21c, 0)):
        code, tid = set_u16(sock, prop, val, tid)
        print(f"[prelude] {prop:#06x}={val} -> 0x{(code or 0):04x}")

    info = bytearray(1076)
    struct.pack_into("<I", info, 0, 0)             # StorageID
    struct.pack_into("<H", info, 4, 0x5000)        # ObjectFormat = Backup
    struct.pack_into("<H", info, 6, 0)             # ProtectionStatus
    struct.pack_into("<I", info, 8, len(payload))  # CompressedSize
    sock.sendall(ptpip.build_ptp_command(0x100C, tid, 0, 0))
    sock.sendall(ptpip.build_ptp_data_container(0x100C, tid, bytes(info)))
    _, code = cwt.ptp_op(sock, b"")
    tid += 1
    print(f"[SendObjectInfo] resp=0x{(code or 0):04x}")
    if code != ptpip.PTP_RESPONSE_OK:
        return False, tid

    sock.sendall(ptpip.build_ptp_command(0x100D, tid))
    sock.sendall(ptpip.build_ptp_data_container(0x100D, tid, payload))
    _, code = cwt.ptp_op(sock, b"")
    tid += 1
    print(f"[SendObject] {len(payload)}B resp=0x{(code or 0):04x}")
    return code == ptpip.PTP_RESPONSE_OK, tid


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="restore-backup")
    p.add_argument("camera_ip")
    p.add_argument("backup")
    p.add_argument("--my-ip", default=None)
    p.add_argument("--guid", default=cwt.DEFAULT_GUID)
    p.add_argument("--name", default="mbp")
    p.add_argument("--retries", type=int, default=12)
    args = p.parse_args(argv)

    payload = open(args.backup, "rb").read()
    if payload[:16] != pb.MAGIC or len(payload) != 69500:
        print(f"[refuse] not a valid backup: magic={payload[:16]!r} len={len(payload)}")
        return 2
    print(f"[load] {args.backup} {len(payload)}B magic OK  band@0x052d=0x{payload[0x052d]:02x}")

    sock = pb.connect(args.camera_ip, args.my_ip, args.guid, args.name, args.retries)
    if sock is None:
        return 2
    ok = False
    try:
        ok, _ = restore(sock, payload, 3)
    finally:
        try:
            sock.close()   # do NOT CloseSession cleanly here; camera reboots on apply anyway
        except OSError:
            pass
    print("[done] RESTORE OK — camera will reboot to apply" if ok else "[done] restore FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
