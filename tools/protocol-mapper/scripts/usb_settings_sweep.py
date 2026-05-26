#!/usr/bin/env python3
"""Toggle-and-diff the GFX100 II camera-settings .dat over **USB tether** (PTP-over-USB).

The Wi-Fi/PCSS equivalent is `backup_sweep.py`; this is the same idea on the USB transport,
for when the body is in USB-tether mode: set a stills image-quality property to an alternate
legal value, pull a fresh .dat, diff vs the before-snapshot → the residual run is that
property's byte offset in the backup blob.

USB transport gotchas this script handles (Linux host):
  * gvfs gphoto2/mtp volume-monitors race us for the interface. We kill them by exact comm
    (pkill -x, never -f — `-f gvfsd-gphoto2` self-matches this script's own argv) and, if the
    claim is busy, issue a USBDEVFS_RESET and claim IMMEDIATELY in the same process. The claim
    is then HELD for the whole session so nothing else can re-grab it.
  * PTP-USB container: <u32 len><u16 type><u16 code><u32 txid>[payload]. type 1=cmd,2=data,3=resp.
  * The body occasionally re-enumerates mid-sweep (ENODEV); we reconnect + retry the prop once.

Backup pull (same as pull_backup.py): GetObjectInfo(0x1008,h=0) then GetObject(0x1009,h=0).

Usage:
  python3 scripts/usb_settings_sweep.py [--props 0xd001,0xd007,...] [--session DIR] [--no-restore]
Default props = the stills IQ cluster (film sim / DR / WB-temp / WB / DOF).
"""
import argparse
import datetime as dt
import fcntl
import json
import os
import struct
import subprocess
import sys
import time
from pathlib import Path

import usb.core
import usb.util

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe_iso_liveview as piv  # noqa: E402  (decode_device_prop_desc, DATATYPE)
import settings_map as sm         # noqa: E402  (diff + KNOWN_NOISE filter)

FUJI_VID = 0x04CB
USBDEVFS_RESET = 0x5514
MAGIC = b"FUJIFILMX-BACKUP"
CMD, DATA, RESP = 1, 2, 3
RESP_OK = 0x2001
BACKUP_HANDLE = 0
DENY_PROPS = {0xD17F}  # 0xd17f=5 is Shooting-Menu Reset — wipes settings. NEVER set.

# Stills image-quality cluster + a known-legal alternate value per prop (catalog enums,
# PROPERTY_CATALOG.md §"Image / Film"). We supply targets because only ~6 props expose a
# rich GetDevicePropDesc over the wire, so enum-discovery is unreliable for the rest.
#   prop : (name, nbytes, [candidate legal values to pick an alternate from])
IQ_TARGETS = {
    0xD001: ("Film simulation", 2, [0x01, 0x02, 0x03, 0x04]),       # Provia/Velvia/Astia/ClassicChrome
    0xD007: ("Dynamic range", 2, [0xFFFF, 100, 200, 400]),          # AUTO/100/200/400 %
    0xD017: ("WB color temp (K)", 2, [5000, 5500, 6500]),
    0x5005: ("White balance (preset)", 2, [0x02, 0x04, 0x06]),      # auto/daylight/...
    0xD028: ("DOF scale", 2, [1, 2]),
}


class USBPTP:
    def __init__(self):
        self.dev = self.intf = self.ino = None
        self.tid = 0

    def _find(self):
        dev = usb.core.find(idVendor=FUJI_VID)
        if not dev:
            return None, None, None
        cfg = dev.get_active_configuration()
        intf = next((i for i in cfg if i.bInterfaceClass == 6), None)
        return dev, intf, (intf.bInterfaceNumber if intf else None)

    def _kill_monitors(self):
        for c in ("gvfs-gphoto2-vo", "gvfs-mtp-volume"):
            subprocess.run(["pkill", "-x", c], capture_output=True)

    def _try_claim_open(self):
        """One claim attempt; returns True only if a test OpenSession actually round-trips."""
        self._kill_monitors()
        dev, intf, ino = self._find()
        if not dev:
            return False
        try:
            if dev.is_kernel_driver_active(ino):
                dev.detach_kernel_driver(ino)
        except Exception:
            pass
        try:
            usb.util.claim_interface(dev, ino)
        except usb.core.USBError:
            return False
        self.dev, self.intf, self.ino = dev, intf, ino
        self._bind_eps()
        try:
            self.txn(0x1002, (1,))  # verify the link is live (OpenSession; AlreadyOpen is fine)
            return True
        except usb.core.USBError:
            try:
                usb.util.release_interface(dev, ino)
            except Exception:
                pass
            return False

    def _reset(self):
        dev = usb.core.find(idVendor=FUJI_VID)
        if not dev:
            return
        try:
            fd = os.open("/dev/bus/usb/%03d/%03d" % (dev.bus, dev.address), os.O_WRONLY)
            fcntl.ioctl(fd, USBDEVFS_RESET, 0)
            os.close(fd)
        except Exception:
            pass
        # NO settle sleep: post-reset the leaked claim is cleared but gvfs/leak re-grabs within
        # a second — must claim INSTANTLY in the tight loop below before that window closes.

    def claim(self):
        """Get a VERIFIED USB-PTP session. The leaked-claim/gvfs race means the ONLY reliable
        recipe is: clear the claim with USBDEVFS_RESET, then claim instantly (tight loop, no
        sleep, killing gvfs monitors each spin) before anything re-grabs the freed interface."""
        if self._try_claim_open():
            print(f"[usb] claimed iface {self.ino} (clean) + OpenSession verified")
            return
        for round_ in range(4):
            self._reset()
            for _ in range(60):  # instant tight loop right after reset
                if self._try_claim_open():
                    print(f"[usb] claimed iface {self.ino} post-reset (round {round_}) + verified")
                    return
        sys.exit("could not establish a working USB-PTP session (busy after resets)")

    def _bind_eps(self):
        self.ep_out = usb.util.find_descriptor(
            self.intf, custom_match=lambda e: usb.util.endpoint_direction(e.bEndpointAddress) == 0
            and usb.util.endpoint_type(e.bmAttributes) == 2)
        self.ep_in = usb.util.find_descriptor(
            self.intf, custom_match=lambda e: usb.util.endpoint_direction(e.bEndpointAddress) == 0x80
            and usb.util.endpoint_type(e.bmAttributes) == 2)

    def txn(self, code, params=(), data_out=None):
        self.tid += 1
        t = self.tid
        self.ep_out.write(struct.pack("<IHHI", 12 + 4 * len(params), CMD, code, t)
                          + b"".join(struct.pack("<I", p) for p in params), timeout=8000)
        if data_out is not None:
            self.ep_out.write(struct.pack("<IHHI", 12 + len(data_out), DATA, code, t) + data_out,
                              timeout=15000)
        data = b""
        for _ in range(4096):
            try:
                pkt = bytes(self.ep_in.read(0x4000, timeout=20000))
            except usb.core.USBError as e:
                if e.errno in (32, 5):  # stall: clear + GetDeviceStatus
                    try:
                        self.dev.clear_halt(self.ep_in.bEndpointAddress)
                    except Exception:
                        pass
                    st = bytes(self.dev.ctrl_transfer(0xA1, 0x67, 0, 0, 0x40))
                    return data, (struct.unpack_from("<H", st, 2)[0] if len(st) >= 4 else None)
                raise
            if len(pkt) < 12:
                break
            ln, typ, c, _ = struct.unpack_from("<IHHI", pkt[:12])
            if typ == DATA:
                payload = pkt[12:]
                while len(payload) + 12 < ln:
                    payload += bytes(self.ep_in.read(0x4000, timeout=20000))
                data = payload[: ln - 12]
            elif typ == RESP:
                return data, c
        return data, None

    def release(self):
        try:
            self.txn(0x1003)  # CloseSession
        except Exception:
            pass
        try:
            usb.util.release_interface(self.dev, self.ino)
        except Exception:
            pass


def pull_backup(p):
    _, ic = p.txn(0x1008, (BACKUP_HANDLE,))
    data, dc = p.txn(0x1009, (BACKUP_HANDLE,))
    return data, ic, dc


def get_desc(p, prop):
    data, code = p.txn(0x1014, (prop,))
    return (piv.decode_device_prop_desc(data) if data and code == RESP_OK else None), code


def sweep_one(p, i, prop, sess, args, results):
    desc, dcode = get_desc(p, prop)
    cur = desc.get("current_value") if desc else None
    writable = desc.get("writable") if desc else None
    name, nbytes, cands = IQ_TARGETS.get(prop, (f"0x{prop:04x}", 2, []))
    vraw, vc = p.txn(0x1015, (prop,))
    live = int.from_bytes(vraw[:nbytes], "little") if vc == RESP_OK and len(vraw) >= nbytes else None
    base_val = cur if cur is not None else live
    target = next((c for c in cands if c != base_val), None)
    print(f"[{i:03d}] 0x{prop:04x} {name}: desc_writable={writable} cur={cur} "
          f"live=0x{(live or 0):x} desc_resp=0x{(dcode or 0):04x}")
    if args.dry_run or target is None:
        results.append({"prop": f"0x{prop:04x}", "name": name, "cur": cur, "live": live,
                        "writable": writable, "target": target})
        return
    before, _, _ = pull_backup(p)
    _, scode = p.txn(0x1016, (prop,), target.to_bytes(nbytes, "little"))
    after, _, _ = pull_backup(p)
    bp = os.path.join(sess, f"snap_{i:03d}_0x{prop:04x}_before.dat")
    cp = os.path.join(sess, f"snap_{i:03d}_0x{prop:04x}_after.dat")
    Path(bp).write_bytes(before)
    Path(cp).write_bytes(after)
    runs = sm.diff(Path(bp), Path(cp), ignore_known=True)
    offs = ", ".join(f"{r['off_hex']}(+{r['len']})" for r in runs) or "NO DELTA"
    print(f"        set {base_val}->{target} resp=0x{(scode or 0):04x}  => {offs}")
    results.append({"prop": f"0x{prop:04x}", "name": name, "old": base_val, "new": target,
                    "set_resp": f"0x{(scode or 0):04x}",
                    "diff_runs": [{"off": r["off_hex"], "len": r["len"],
                                   "old": r["old"], "new": r["new"]} for r in runs]})
    if not args.no_restore and base_val is not None:
        p.txn(0x1016, (prop,), base_val.to_bytes(nbytes, "little"))


def main(argv=None):
    ap = argparse.ArgumentParser(prog="usb-settings-sweep")
    ap.add_argument("--props", default=None, help="comma list of 0xPROP (default: stills IQ cluster)")
    ap.add_argument("--session", default=None)
    ap.add_argument("--no-restore", action="store_true")
    ap.add_argument("--dry-run", action="store_true", help="read desc + current value only, no writes")
    args = ap.parse_args(argv)

    props = [int(x, 0) for x in args.props.split(",")] if args.props else list(IQ_TARGETS)
    sess = args.session or os.path.expanduser(

    os.makedirs(sess, exist_ok=True)
    print(f"[sess] {sess}")

    p = USBPTP()
    p.claim()
    results = []
    try:
        _, oc = p.txn(0x1002, (1,))
        print(f"[open] resp=0x{(oc or 0):04x}")
        base, ic, dc = pull_backup(p)
        ok = base[:16] == MAGIC and len(base) == 69500
        with open(os.path.join(sess, "snap_000_baseline.dat"), "wb") as f:
            f.write(base)
        print(f"[base] {len(base)}B magic={'OK' if ok else base[:16]!r} "
              f"info=0x{(ic or 0):04x} obj=0x{(dc or 0):04x}")
        if not ok:
            print("[abort] baseline backup not valid — not in a state that serves the .dat over USB")
            return 2

        for i, prop in enumerate(props, 1):
            if prop in DENY_PROPS:
                print(f"[{i:03d}] 0x{prop:04x} DENYLISTED — skipped")
                continue
            for attempt in range(2):
                try:
                    sweep_one(p, i, prop, sess, args, results)
                    break
                except usb.core.USBError as e:
                    if e.errno == 19 and attempt == 0:  # ENODEV: brief re-enum — reconnect + retry
                        print(f"        [reconnect] 0x{prop:04x} disconnected ({e}); re-claiming")
                        time.sleep(1.5)
                        p.claim()
                        p.txn(0x1002, (1,))
                        continue
                    print(f"        [skip] 0x{prop:04x} {e}")
                    results.append({"prop": f"0x{prop:04x}", "error": str(e)})
                    break
    finally:
        p.release()
        print("[usb] released interface")

    rep = os.path.join(sess, "usb_sweep_report.json")
    with open(rep, "w") as f:
        json.dump(results, f, indent=2)
    labeled = [r for r in results if r.get("diff_runs")]
    print(f"\n[done] {len(results)} props, {len(labeled)} produced a byte delta\n[report] {rep}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
