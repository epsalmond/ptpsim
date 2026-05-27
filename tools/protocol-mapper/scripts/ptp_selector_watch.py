#!/usr/bin/env python3
"""Poll GetDeviceInfo at a fixed cadence to catch a transient OperationsSupported change — built for
D5's selector-force idx2 window. READ-ONLY. One persistent PTP session (opened before the force), then
re-issues GetDeviceInfo every --interval s for --duration s, logging op-count + whether 0x9008 / 0x901B
are advertised. Captures the idx0->idx2->idx0 transition without tight real-time sync, so D5 can arm the
60 s force anytime inside the window.

Body must be in a mode != 2 (stills/video, NOT cardreader/auto) so the handler consults table[selector];
do not change the body dial during the window (a mode-change command overwrites the selector).
"""
import argparse
import datetime
import time

from ptp_propdesc_enum import PTPUSB, build_ptpip, parse_deviceinfo


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--transport", choices=["usb", "ptpip"], default="usb")
    ap.add_argument("--camera-ip", default=None)
    ap.add_argument("--my-ip", default=None)
    ap.add_argument("--guid", default=None)
    ap.add_argument("--name", default="mbp")
    ap.add_argument("--interval", type=float, default=2.0)
    ap.add_argument("--duration", type=float, default=120.0)
    args = ap.parse_args()
    if args.transport == "ptpip" and not args.camera_ip:
        ap.error("--transport ptpip requires --camera-ip")

    p = build_ptpip(args.camera_ip, args.my_ip, args.guid, args.name) if args.transport == "ptpip" else PTPUSB()
    p.open()
    t0 = time.time()
    print("utc\telapsed\tresp\top_count\thas_9008\thas_901B\tselector_guess")
    while time.time() - t0 < args.duration:
        d, rc = p.device_info()
        ops = parse_deviceinfo(d)["ops"]
        n = len(ops)
        guess = {24: "idx0", 20: "idx1", 22: "idx2!", 12: "idx3!", 25: "mode2"}.get(n, "?")
        ts = datetime.datetime.utcnow().strftime("%H:%M:%S")
        print("%s\t%6.1f\t0x%04x\t%d\t%s\t%s\t%s"
              % (ts, time.time() - t0, (rc or 0), n, 0x9008 in ops, 0x901B in ops, guess), flush=True)
        time.sleep(args.interval)


if __name__ == "__main__":
    main()
