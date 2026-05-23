#!/usr/bin/env python3
"""Probe GFX100 II movie/video mode over the wired (or Wi-Fi) PTP-IP tether.

Connects via the PCSS knock + PTP-IP path (connect_wireless_tether), then dumps DevicePropDesc +
current value for PASM and the movie/live-view properties — to see what the desktop/15740 path
exposes that the reference app/55740 path didn't, and whether Movie mode (0x500e=0x8003) is settable. Can
also --set a property and --get/--desc arbitrary codes.

Usage:
  PYTHONPATH=. python scripts/movie_probe.py <camera_ip>
      [--set 0xPROP=0xVAL[/N]]   # write a prop (N = value byte width, default 4)
      [--desc 0xPROP] [--get 0xPROP]   # add ad-hoc codes to the dump
Once-per-boot: power-cycle the camera if the knock gets no callback.
"""
import argparse
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import connect_wireless_tether as cwt  # noqa: E402
import probe_iso_liveview as piv        # noqa: E402  (DevicePropDesc / value decoders)
from rce.tools.fuji_ble_gps import ptpip  # noqa: E402

# PASM + movie/live-view props to characterize (incl. ones inert on the reference app path)
PROBE = [
    (0x500e, "ExposureProgram/PASM (Movie=0x8003?)"),
    (0x5013, "still/movie or capture mode?"),
    (0xd174, "LiveView ImageSize"),
    (0xd173, "LiveView ImageQuality"),
    (0xd1bc, "LiveView Mode"),
    (0xd23c, "LiveView aspect ratio"),
    (0xd247, "movie record rate?"),
    (0xd24c, "movie record rate?"),
    (0xd253, "movie record rate?"),
    (0xd16f, "(desktop-set)"),
    (0xd170, "(desktop-polled)"),
    (0xd171, "(desktop-set)"),
    (0xd1b8, "(desktop-desc)"),
]


def supported_ops(sock, tid):
    """Fetch DeviceInfo and return the SupportedOperations list (vendor 0x9xxx ops included)."""
    data, _ = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1001, tid))
    try:
        off = 2 + 4 + 2  # StandardVersion, VendorExtensionID, VendorExtensionVersion
        slen = data[off]
        off += 1 + slen * 2  # VendorExtensionDesc (UTF-16 string)
        off += 2             # FunctionalMode
        nops = struct.unpack_from("<I", data, off)[0]
        off += 4
        ops = [struct.unpack_from("<H", data, off + 2 * i)[0] for i in range(nops)]
        return ops, tid + 1
    except (struct.error, IndexError):
        return [], tid + 1


def get_desc(sock, prop, tid):
    data, code = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1014, tid, prop))
    return data, code, tid + 1


def get_val(sock, prop, tid):
    data, code = cwt.ptp_op(sock, ptpip.build_get_device_prop_value(prop, tid))
    return data, code, tid + 1


def set_val(sock, prop, value, nbytes, tid):
    sock.sendall(ptpip.build_set_device_prop_value(prop, tid))
    sock.sendall(ptpip.build_ptp_data_container(ptpip.PTP_SET_DEVICE_PROP_VALUE, tid,
                                                value.to_bytes(nbytes, "little")))
    _, code = cwt.ptp_op(sock, b"")
    return code, tid + 1


def dump(sock, prop, label, tid):
    data, dcode, tid = get_desc(sock, prop, tid)
    desc = piv.decode_device_prop_desc(data) if data else {"decode_error": f"no desc (0x{(dcode or 0):04x})"}
    val_data, vcode, tid = get_val(sock, prop, tid)
    val = piv.decode_prop_value(val_data) if val_data else {}
    w = desc.get("writable")
    form = desc.get("form")
    cur = desc.get("current_value")
    rng = ""
    if form == "range":
        rng = f"range[{desc.get('range_min')}..{desc.get('range_max')} step {desc.get('range_step')}]"
    elif form == "enum":
        rng = f"enum{desc.get('enum_values_hex')}"
    print(f"  0x{prop:04x} {label}")
    print(f"        type={desc.get('data_type_name')} writable={w} current={cur} "
          f"value={val.get('uint_le', val.get('raw_hex', ''))}")
    if rng:
        print(f"        {rng}")
    if desc.get("decode_error"):
        print(f"        descERR={desc['decode_error']} raw={desc.get('raw_hex','')[:64]}")
    return tid


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="movie-probe")
    p.add_argument("camera_ip")
    p.add_argument("--my-ip", default=None)
    p.add_argument("--guid", default=cwt.DEFAULT_GUID)
    p.add_argument("--name", default="mbp")
    p.add_argument("--set", default=None, help="0xPROP=0xVAL[/N]  write before dumping")
    p.add_argument("--desc", action="append", default=[], help="extra prop code to dump")
    p.add_argument("--retries", type=int, default=12)
    p.add_argument("--record", type=float, default=None,
                   help="movie record test: InitiateMovieCapture(0x9020), hold N seconds, stop")
    args = p.parse_args(argv)

    my_ip = args.my_ip or cwt.my_ip_for(args.camera_ip)
    srv = cwt.open_callback_listener(cwt.CALLBACK_PORT)
    print(f"[listen] :{cwt.CALLBACK_PORT}")
    cb = cwt.wait_for_callback(srv, args.camera_ip, my_ip, args.retries, 10.0, False)
    srv.close()
    if cb is None:
        print("[fail] no callback — power-cycle the camera (once per boot)")
        return 2
    notify = cwt.handle_notify(cb)
    cb.close()
    sock = cwt.connect_ptpip(args.camera_ip, my_ip, args.guid, args.name, 6.0, notify["dscport"])
    if isinstance(sock, int):
        return sock
    tid = 3

    if args.set:
        code_s, val_s = args.set.split("=", 1)
        nbytes = 4
        if "/" in val_s:
            val_s, nb = val_s.split("/", 1)
            nbytes = int(nb)
        prop = int(code_s, 0)
        value = int(val_s, 0)
        rc, tid = set_val(sock, prop, value, nbytes, tid)
        print(f"[set] 0x{prop:04x} = 0x{value:x} ({nbytes}B) -> resp 0x{(rc or 0):04x}")

    ops, tid = supported_ops(sock, tid)
    vendor = [f"0x{o:04x}" for o in ops if o >= 0x9000]
    print(f"[info] {len(ops)} ops supported; vendor 0x9xxx: {vendor}")
    print(f"[info] 0x9020 movie-capture supported: {0x9020 in ops}")

    if args.record is not None:
        import time
        _, lvc = cwt.ptp_op(sock, ptpip.build_ptp_command(0x101C, tid, 0, 0))
        tid += 1
        print(f"[rec] InitiateOpenCapture(0x101C) -> 0x{(lvc or 0):04x}")
        # try 0x9020 a few ways: bare, with (0,0), with (0)
        for label, params in [("(0,0)", (0, 0)), ("(0)", (0,)), ("bare", ())]:
            _, rc = cwt.ptp_op(sock, ptpip.build_ptp_command(0x9020, tid, *params))
            tid += 1
            print(f"[rec] InitiateMovieCapture(0x9020) {label} -> 0x{(rc or 0):04x}")
            if rc == ptpip.PTP_RESPONSE_OK:
                print(f"[rec] RECORDING ({label}) — holding {args.record}s")
                time.sleep(args.record)
                _, rc2 = cwt.ptp_op(sock, ptpip.build_ptp_command(0x9020, tid, *params))
                tid += 1
                print(f"[rec] 0x9020 {label} again (stop) -> 0x{(rc2 or 0):04x}")
                break

    print("[dump] PASM + movie/live-view property landscape:")
    probe = PROBE + [(int(c, 0), "(adhoc)") for c in args.desc]
    for prop, label in probe:
        try:
            tid = dump(sock, prop, label, tid)
        except (OSError, RuntimeError) as exc:
            print(f"  0x{prop:04x} {label}: error {exc}")
            break

    try:
        sock.sendall(ptpip.build_close_session(transaction_id=900))
        sock.close()
    except OSError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
