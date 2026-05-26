#!/usr/bin/env python3
"""Pull the GFX100 II camera-settings backup (.dat, ~69500 bytes) over the wired/Wi-Fi
PTP-IP tether — no SD card, no USB cable.


  GetObjectInfo (0x1008) handle=0  -> ~0x434-byte custom Info buffer (data phase)
  GetObject     (0x1009) handle=0  -> the backup payload (data phase)
Connection uses the validated PCSS knock + PTP-IP path (connect_wireless_tether).

Usage:
  PYTHONPATH=. python scripts/pull_backup.py <camera_ip> [--out PATH] [--tag NAME]
Once-per-boot: power-cycle the camera if the knock gets no callback.
"""
import argparse
import datetime as dt
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import connect_wireless_tether as cwt  # noqa: E402
from rce.tools.fuji_ble_gps import ptpip  # noqa: E402

BACKUP_HANDLE = 0
MAGIC = b"FUJIFILMX-BACKUP"
BAND_OFFSET = 0x052D  # 0x01 = 5GHz, 0x00 = 2.4GHz (wire-confirmed 2026-05-13 + -23)


def band_of(dat: bytes):
    """Return ('5GHz'|'2.4GHz'|'unknown', recommend_change: bool) for a backup blob."""
    b = dat[BAND_OFFSET]
    return ({1: "5GHz", 0: "2.4GHz"}.get(b, f"unknown(0x{b:02x})"), b == 0)


def pull_backup(sock, tid):
    """Return (payload_bytes, info_bytes, next_tid). payload is the .dat blob."""
    info, icode = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1008, tid, BACKUP_HANDLE))
    tid += 1
    if icode != ptpip.PTP_RESPONSE_OK:
        raise RuntimeError(f"GetObjectInfo(handle=0) -> 0x{(icode or 0):04x}")
    data, dcode = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1009, tid, BACKUP_HANDLE))
    tid += 1
    if dcode != ptpip.PTP_RESPONSE_OK:
        raise RuntimeError(f"GetObject(handle=0) -> 0x{(dcode or 0):04x}")
    return data, info, tid


def connect(camera_ip, my_ip, guid, name, retries):
    my_ip = my_ip or cwt.my_ip_for(camera_ip)
    srv = cwt.open_callback_listener(cwt.CALLBACK_PORT)
    print(f"[listen] :{cwt.CALLBACK_PORT}")
    cb = cwt.wait_for_callback(srv, camera_ip, my_ip, retries, 10.0, False)
    srv.close()
    if cb is None:
        print("[fail] no callback — power-cycle the camera (once per boot)")
        return None
    notify = cwt.handle_notify(cb)
    cb.close()
    sock = cwt.connect_ptpip(camera_ip, my_ip, guid, name, 6.0, notify["dscport"])
    if isinstance(sock, int):
        return None
    return sock


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="pull-backup")
    p.add_argument("camera_ip")
    p.add_argument("--my-ip", default=None)
    p.add_argument("--guid", default=cwt.DEFAULT_GUID)
    p.add_argument("--name", default="mbp")
    p.add_argument("--retries", type=int, default=12)
    p.add_argument("--out", default=None, help="output .dat path (default: timestamped)")
    p.add_argument("--tag", default="", help="label inserted into the default filename")
    p.add_argument("--check-band", action="store_true",
                   help="report Wi-Fi band + whether to recommend switching to 5GHz")
    args = p.parse_args(argv)

    sock = connect(args.camera_ip, args.my_ip, args.guid, args.name, args.retries)
    if sock is None:
        return 2
    try:
        data, info, _ = pull_backup(sock, 3)
    finally:
        try:
            sock.sendall(ptpip.build_close_session(transaction_id=900))
            sock.close()
        except OSError:
            pass

    out = args.out
    if out is None:
        ts = dt.datetime.now().strftime("%Y%m%dT%H%M%S")
        tag = f"_{args.tag}" if args.tag else ""

    with open(out, "wb") as f:
        f.write(data)

    magic_ok = data[:16] == MAGIC
    print(f"[info] info-buffer {len(info)} bytes (expect ~0x434={0x434})")
    print(f"[ok] wrote {out} ({len(data)} bytes; expect 69500)  magic={'OK' if magic_ok else data[:16]!r}")
    if args.check_band and magic_ok:
        band, recommend = band_of(data)
        print(f"[band] Wi-Fi band = {band} (offset 0x{BAND_OFFSET:04X} = 0x{data[BAND_OFFSET]:02x})")
        print(f"[band] recommend switching to 5GHz: {'YES — currently 2.4GHz' if recommend else 'no'}")
    return 0 if magic_ok and len(data) == 69500 else 1


if __name__ == "__main__":
    sys.exit(main())
