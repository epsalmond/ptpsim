#!/usr/bin/env python3

RAW-conversion mode work (read/set 0xD185, send RAF via 0x900c/0x900d).

PTP-USB container: <u32 len><u16 type><u16 code><u32 txid>[params/payload].
  type 1=Command(OUT), 2=Data(OUT or IN), 3=Response(IN), 4=Event.
Bulk OUT = command + optional data-out; Bulk IN = optional data-in + response.

Usage:
  sudo python3 ptp_usb.py info                  # DeviceInfo + open
  sudo python3 ptp_usb.py get 0xD185            # GetDevicePropValue -> hex
  sudo python3 ptp_usb.py set 0xD185 <binfile>  # SetDevicePropValue from a file
"""
import platform
import struct
import subprocess
import sys
import time

import usb.core
import usb.util

FUJI_VID = 0x04cb
CMD, DATA, RESP = 1, 2, 3
OPEN_SESSION, GET_DEVICE_INFO = 0x1002, 0x1001
GET_PROP_VAL, SET_PROP_VAL = 0x1015, 0x1016

# macOS auto-grabs PTP cameras with ptpcamerad (per-user gui launchd domain, runs as the
# invoking user → killable without sudo). Reap it right before claiming so libusb can seize
# the interface. Best-effort: missing/not-running is fine.
_MAC_GRABBERS = ("ptpcamerad", "PTPCamera", "mscamerad")


def _kill_macos_grabbers():
    if platform.system() != "Darwin":
        return
    for name in _MAC_GRABBERS:
        subprocess.run(["killall", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def find_cam():
    dev = usb.core.find(idVendor=FUJI_VID)
    if dev is not None:
        cfg = dev.get_active_configuration()
        intf = next((i for i in cfg if i.bInterfaceClass == 6), cfg[(0, 0)])
        return dev, intf
    for d in usb.core.find(find_all=True):
        for cfg in d:
            for intf in cfg:
                if intf.bInterfaceClass == 6:  # still image / PTP
                    return d, intf
    sys.exit("no Fuji / PTP USB device found (plugged in + in a PTP/RAW-conv USB mode?)")


class PTPUSB:
    def __init__(self):
        last = None
        for _ in range(6):
            _kill_macos_grabbers()  # no-op off Darwin; reaps ptpcamerad before each claim attempt
            try:
                self.dev, intf = find_cam()
                try:
                    if self.dev.is_kernel_driver_active(intf.bInterfaceNumber):
                        self.dev.detach_kernel_driver(intf.bInterfaceNumber)
                except Exception:
                    pass
                self.ep_out = usb.util.find_descriptor(
                    intf, custom_match=lambda e: usb.util.endpoint_direction(e.bEndpointAddress) == usb.util.ENDPOINT_OUT
                    and usb.util.endpoint_type(e.bmAttributes) == usb.util.ENDPOINT_TYPE_BULK)
                self.ep_in = usb.util.find_descriptor(
                    intf, custom_match=lambda e: usb.util.endpoint_direction(e.bEndpointAddress) == usb.util.ENDPOINT_IN
                    and usb.util.endpoint_type(e.bmAttributes) == usb.util.ENDPOINT_TYPE_BULK)
                usb.util.claim_interface(self.dev, intf)  # commit the claim now so a grab surfaces here, not mid-txn
                self.tid = 0
                print(f"[usb] {self.dev.idVendor:04x}:{self.dev.idProduct:04x} "
                      f"out={self.ep_out.bEndpointAddress:#x} in={self.ep_in.bEndpointAddress:#x}",
                      file=sys.stderr)  # diagnostic to stderr so stdout stays clean for JSONL consumers
                return
            except usb.core.USBError as e:
                last = e
                time.sleep(0.3)  # let the killed grabber release / re-enumerate settle, then retry
        sys.exit(f"could not claim Fuji PTP interface after retries (last: {last})")

    def _txn(self, code, params=(), data_out=None):
        self.tid += 1
        tid = self.tid
        cmd = struct.pack("<IHHI", 12 + 4 * len(params), CMD, code, tid)
        cmd += b"".join(struct.pack("<I", p) for p in params)
        self.ep_out.write(cmd, timeout=8000)
        if data_out is not None:
            self.ep_out.write(struct.pack("<IHHI", 12 + len(data_out), DATA, code, tid), timeout=15000)
            CH = 0x100000  # 1 MiB chunks (libusb can't do one giant bulk write)
            for i in range(0, len(data_out), CH):
                self.ep_out.write(data_out[i:i + CH], timeout=30000)
        data = b""
        for _ in range(64):
            try:
                pkt = bytes(self.ep_in.read(0x4000, timeout=15000))
            except usb.core.USBError as e:
                # PTP-USB: device STALLed bulk-in to signal status — clear halt + Get Device Status
                if e.errno in (32, 5):
                    try:
                        self.dev.clear_halt(self.ep_in.bEndpointAddress)
                    except Exception:
                        pass
                    st = bytes(self.dev.ctrl_transfer(0xA1, 0x67, 0, 0, 0x40))  # Get Device Status
                    code = struct.unpack_from("<H", st, 2)[0] if len(st) >= 4 else None
                    return data, code
                raise
            if len(pkt) < 12:
                break
            ln, typ, c, t = struct.unpack("<IHHI", pkt[:12])
            if typ == DATA:
                payload = pkt[12:]
                while len(payload) + 12 < ln:
                    payload += bytes(self.ep_in.read(0x4000, timeout=15000))
                data = payload[: ln - 12]
            elif typ == RESP:
                return data, c
        return data, None

    def open(self):
        return self._txn(OPEN_SESSION, (1,))

    def device_info(self):
        return self._txn(GET_DEVICE_INFO)

    def get_prop(self, prop):
        return self._txn(GET_PROP_VAL, (prop,))

    def set_prop(self, prop, value_bytes):
        return self._txn(SET_PROP_VAL, (prop,), value_bytes)


def main():
    p = PTPUSB()
    _, rc = p.open()
    print(f"[open] resp=0x{(rc or 0):04x}")
    cmd = sys.argv[1] if len(sys.argv) > 1 else "info"
    if cmd == "info":
        d, rc = p.device_info()
        print(f"[deviceinfo] {len(d)}B resp=0x{(rc or 0):04x}")
    elif cmd == "get":
        prop = int(sys.argv[2], 16)
        d, rc = p.get_prop(prop)
        print(f"[get 0x{prop:04x}] {len(d)}B resp=0x{(rc or 0):04x}\n{d.hex()}")
    elif cmd == "set":
        prop = int(sys.argv[2], 16)
        val = open(sys.argv[3], "rb").read()
        _, rc = p.set_prop(prop, val)
        print(f"[set 0x{prop:04x}] {len(val)}B resp=0x{(rc or 0):04x}")


if __name__ == "__main__":
    main()
