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
import struct
import sys

import usb.core
import usb.util

FUJI_VID = 0x04cb
CMD, DATA, RESP = 1, 2, 3
OPEN_SESSION, GET_DEVICE_INFO = 0x1002, 0x1001
GET_PROP_VAL, SET_PROP_VAL = 0x1015, 0x1016


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
        self.tid = 0
        print(f"[usb] {self.dev.idVendor:04x}:{self.dev.idProduct:04x} "
              f"out={self.ep_out.bEndpointAddress:#x} in={self.ep_in.bEndpointAddress:#x}")

    def _txn(self, code, params=(), data_out=None):
        self.tid += 1
        tid = self.tid
        cmd = struct.pack("<IHHI", 12 + 4 * len(params), CMD, code, tid)
        cmd += b"".join(struct.pack("<I", p) for p in params)
        self.ep_out.write(cmd, timeout=8000)
        if data_out is not None:
            self.ep_out.write(struct.pack("<IHHI", 12 + len(data_out), DATA, code, tid) + data_out, timeout=15000)
        data = b""
        for _ in range(64):
            pkt = bytes(self.ep_in.read(0x4000, timeout=15000))
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
