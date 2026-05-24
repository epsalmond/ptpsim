#!/usr/bin/env python3
"""Auto-label the GFX100 II camera-settings .dat by sweeping PTP properties.

Strategy: connect once over the tether; pull a baseline .dat; then for each writable
property, set it to an alternate legal value, pull a fresh .dat, and diff against the
previous snapshot. The residual differing run (after filtering the save-counter/checksum
noise) is the byte offset(s) where that setting lives. Emits a prop -> offset label map.

This DRIVES BOTH the change and the save over PTP-IP — no SD card, no manual menu work.

Usage:
  PYTHONPATH=. python scripts/backup_sweep.py <camera_ip>
      [--props 0xd001,0xd247,...]   # restrict to specific props (default: built-in set)
      [--session DIR]               # where to drop snap_*.dat + report (default: timestamped)
      [--no-restore]                # leave swept props changed (default restores each)
Each snap = baseline + one changed prop; diff vs baseline isolates that prop's byte(s).
Default restores every prop after diffing, so the camera ends in its original state.
Once-per-boot: power-cycle the camera if the knock gets no callback.
"""
import argparse
import datetime as dt
import json
import os
import re
import struct
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import connect_wireless_tether as cwt  # noqa: E402
import probe_iso_liveview as piv        # noqa: E402
import settings_map as sm               # noqa: E402
import pull_backup as pb                # noqa: E402
from label_backup import load_names     # noqa: E402
from rce.tools.fuji_ble_gps import ptpip  # noqa: E402


# Props that are NOT settings — writing them performs a destructive ACTION. NEVER set these.
# 0xd17f value 5 = settings reset ("Shooting Menu Reset") — wiped shoot settings live 2026-05-23.
DENY_PROPS = {0xd17f}
# Catalog names whose props trigger actions rather than store a value.
DANGER_NAME = re.compile(r"reset|format|initiali|erase|delete|factory|firmware|wipe|clear",
                         re.IGNORECASE)


def supported_props(sock, tid):
    """Parse DeviceInfo and return the camera's actual DevicePropertiesSupported list."""
    data, _ = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1001, tid))
    tid += 1
    try:
        off = 2 + 4 + 2                       # StdVersion, VendorExtID, VendorExtVersion
        off += 1 + data[off] * 2              # VendorExtensionDesc string
        off += 2                              # FunctionalMode
        for _ in range(2):                    # skip OperationsSupported, EventsSupported
            n = struct.unpack_from("<I", data, off)[0]
            off += 4 + 2 * n
        n = struct.unpack_from("<I", data, off)[0]
        off += 4
        return [struct.unpack_from("<H", data, off + 2 * i)[0] for i in range(n)], tid
    except (struct.error, IndexError):
        return [], tid


def get_desc(sock, prop, tid):
    data, code = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1014, tid, prop))
    desc = piv.decode_device_prop_desc(data) if data and code == ptpip.PTP_RESPONSE_OK else None
    return desc, code, tid + 1


def set_val(sock, prop, value, nbytes, signed, tid):
    sock.sendall(ptpip.build_set_device_prop_value(prop, tid))
    sock.sendall(ptpip.build_ptp_data_container(
        ptpip.PTP_SET_DEVICE_PROP_VALUE, tid, value.to_bytes(nbytes, "little", signed=signed)))
    _, code = cwt.ptp_op(sock, b"")
    return code, tid + 1


def safe_to_sweep(desc, target):
    """Gate out internal-state props (timestamps/counters/handles). Enum values are legal
    by construction (camera-declared); ranges only if a small signed/unsigned 16-bit shift."""
    if target is None:
        return False
    if desc.get("form") == "enum":
        return True  # value is one the camera itself advertised as legal
    if desc.get("form") == "range":
        if desc.get("data_type_name") not in ("INT16", "UINT16"):
            return False  # 32/64-bit ranges = internal state, not a user setting
        cur = desc.get("current_value") or 0
        return abs(target) <= 10000 and abs(cur) <= 10000
    return False


def pick_target(desc):
    """Choose an alternate legal value different from current; None if none available."""
    cur = desc.get("current_value")
    if desc.get("form") == "enum":
        for v in desc.get("enum_values", []):
            if v != cur:
                return v
    elif desc.get("form") == "range":
        for v in (desc.get("range_min"), desc.get("range_max")):
            if v is not None and v != cur:
                return v
    return None


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="backup-sweep")
    p.add_argument("camera_ip")
    p.add_argument("--my-ip", default=None)
    p.add_argument("--guid", default=cwt.DEFAULT_GUID)
    p.add_argument("--name", default="mbp")
    p.add_argument("--retries", type=int, default=12)
    p.add_argument("--props", default=None, help="comma list of 0xPROP to sweep")
    p.add_argument("--session", default=None)
    p.add_argument("--no-restore", action="store_true",
                   help="leave each swept prop changed (default: restore to original)")
    p.add_argument("--dry-run", action="store_true",
                   help="read-only: list writable props + would-set targets, no writes")
    args = p.parse_args(argv)

    explicit_props = [int(x, 0) for x in args.props.split(",")] if args.props else None

    sess = args.session or os.path.expanduser(

    os.makedirs(sess, exist_ok=True)
    print(f"[sess] {sess}")

    sock = pb.connect(args.camera_ip, args.my_ip, args.guid, args.name, args.retries)
    if sock is None:
        return 2
    tid = 3
    results = []
    try:
        data, _, tid = pb.pull_backup(sock, tid)
        base_path = os.path.join(sess, "snap_000_baseline.dat")
        with open(base_path, "wb") as f:
            f.write(data)
        ok = data[:16] == pb.MAGIC and len(data) == 69500
        print(f"[base] {base_path} {len(data)}B magic={'OK' if ok else data[:16]!r}")

        if explicit_props is not None:
            props = explicit_props
        else:
            props, tid = supported_props(sock, tid)
        # NEVER auto-write action props (reset/format/etc.) — they aren't settings and wipe state.
        names = load_names()
        denied = [p for p in props
                  if p in DENY_PROPS or DANGER_NAME.search(names.get(p, ""))]
        props = [p for p in props if p not in denied]
        print(f"[props] sweeping {len(props)} candidate props "
              f"({'explicit' if explicit_props else 'DeviceInfo-supported'}); "
              f"blocked {len(denied)} action/destructive props: "
              f"{', '.join(f'0x{p:04x}' for p in denied) or 'none'}")

        for i, prop in enumerate(props, 1):
            if prop in DENY_PROPS:        # belt-and-suspenders, also for explicit --props
                print(f"[{i:03d}] 0x{prop:04x} DENYLISTED (destructive action) — skipped")
                continue
            desc, dcode, tid = get_desc(sock, prop, tid)
            if not desc or not desc.get("writable"):
                continue
            target = pick_target(desc)
            if not safe_to_sweep(desc, target):
                continue
            dt_code = int(desc.get("data_type", "0x0006"), 16)
            _, nbytes, signed = piv.DATATYPE.get(dt_code, ("UINT32", 4, False))
            cur = desc.get("current_value")
            if args.dry_run:
                print(f"[dry] 0x{prop:04x} {desc.get('data_type_name'):>6} "
                      f"{desc.get('form')} cur={cur} would_set={target}")
                results.append({"prop": f"0x{prop:04x}", "type": desc.get("data_type_name"),
                                "form": desc.get("form"), "cur": cur, "would_set": target})
                continue
            try:
                # pull a FRESH before-snapshot right before the set, so the diff is immune
                # to any accumulated drift / imperfect restores from earlier props.
                before, _, tid = pb.pull_backup(sock, tid)
                scode, tid = set_val(sock, prop, target, nbytes, signed, tid)
                after, _, tid = pb.pull_backup(sock, tid)
            except (OSError, RuntimeError) as exc:
                # this prop desynced/dropped the session — reconnect and skip it
                print(f"[{i:03d}] 0x{prop:04x} DESYNC ({exc}); reconnecting, skipping prop")
                results.append({"prop": f"0x{prop:04x}", "type": desc.get("data_type_name"),
                                "error": str(exc), "diff_runs": []})
                try:
                    sock.close()
                except OSError:
                    pass
                sock = pb.connect(args.camera_ip, args.my_ip, args.guid, args.name, args.retries)
                if sock is None:
                    print("[abort] reconnect failed — stopping sweep")
                    break
                tid = 3
                continue
            before_path = os.path.join(sess, f"snap_{i:03d}_0x{prop:04x}_before.dat")
            cur_path = os.path.join(sess, f"snap_{i:03d}_0x{prop:04x}.dat")
            with open(before_path, "wb") as f:
                f.write(before)
            with open(cur_path, "wb") as f:
                f.write(after)
            # diff before->after: isolates exactly this prop's bytes regardless of drift.
            runs = sm.diff(Path(before_path), Path(cur_path), ignore_known=True)
            rec = {"prop": f"0x{prop:04x}", "type": desc.get("data_type_name"),
                   "old": cur, "new": target, "set_resp": f"0x{(scode or 0):04x}",
                   "diff_runs": [{"off": r["off_hex"], "len": r["len"],
                                  "old": r["old"], "new": r["new"]} for r in runs]}
            results.append(rec)
            offs = ", ".join(f"{r['off_hex']}(+{r['len']})" for r in runs) or "NO DELTA"
            print(f"[{i:03d}] 0x{prop:04x} {desc.get('data_type_name'):>6} "
                  f"{cur}->{target} resp={rec['set_resp']}  => {offs}")
            if not args.no_restore and cur is not None:
                _, tid = set_val(sock, prop, cur, nbytes, signed, tid)
    finally:
        try:
            sock.sendall(ptpip.build_close_session(transaction_id=900))
            sock.close()
        except OSError:
            pass

    report = os.path.join(sess, "sweep_report.json")
    with open(report, "w") as f:
        json.dump(results, f, indent=2)
    labeled = [r for r in results if r.get("diff_runs")]
    print(f"\n[done] {len(results)} props processed, {len(labeled)} produced a byte delta")
    print(f"[report] {report}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
