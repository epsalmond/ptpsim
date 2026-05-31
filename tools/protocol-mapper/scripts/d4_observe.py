#!/usr/bin/env python3
"""D4 consult (2026-05-29T0746Z): minimal paced PTP-IP session vs isolated test camera
10.42.0.169:15740, so D4's 1 Hz ps loop + script-trace + sniff can correlate what fires
on Linux when a tether host connects. READ-ONLY (no state changes, no firmware ops).

Sequence (UTC-timestamped, ~7 s between commands):
  PCSS knock -> camera callback -> NOTIFY -> PTP-IP Init -> OpenSession (tid=1) + GetDeviceInfo (tid=2)
  T+7  GetDeviceInfo            tid=3   (re-issue, so D4 can see a 2nd op)
  T+14 GetDevicePropDesc 0x5001 tid=4   (BatteryLevel, benign)
  T+21 GetDevicePropValue 0x5001 tid=5
  T+28 CloseSession             tid=6
"""
import datetime
import os
import socket as _socket
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))  # protocol-mapper root, so `rce` resolves

import connect_wireless_tether as cwt  # noqa: E402  (sys.path set above so `rce` resolves)
from rce.tools.fuji_ble_gps import ptpip  # noqa: E402

CAM = "10.42.0.169"
PROP = 0x5001                                # BatteryLevel (standard PTP), benign read
PACE = 7.0


def ts():
    return datetime.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def log(msg):
    print(f"[{ts()}] {msg}", flush=True)


def main():
    my_ip = cwt.my_ip_for(CAM)
    log(f"BEGIN d4_observe vs {CAM} (my_ip={my_ip})")
    log("knock + listen for camera callback ...")
    srv = cwt.open_callback_listener(cwt.CALLBACK_PORT)
    cb = cwt.wait_for_callback(srv, CAM, my_ip, 12, 10.0, False)
    srv.close()
    if cb is None:
        log("FATAL: camera never called back -- abort")
        return 2
    notify = cwt.handle_notify(cb)
    cb.close()
    log(f"NOTIFY parsed: dscport={notify.get('dscport')} camera={notify.get('camera_name')!r}")
    sock = cwt.connect_ptpip(CAM, my_ip, cwt.DEFAULT_GUID, "d3-wire-d4", 6.0, notify["dscport"])
    if isinstance(sock, int):
        log(f"FATAL: connect_ptpip failed code={sock}")
        return 3
    log("PTP-IP ESTABLISHED (OpenSession tid=1 + GetDeviceInfo tid=2 inside cwt.connect_ptpip)")

    log(f"sleep {PACE}s")
    time.sleep(PACE)
    log("OP GetDeviceInfo tid=3 (re-issue)")
    d, rc = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1001, 3))
    log(f"  -> resp=0x{(rc or 0):04x} data={len(d)}B")

    log(f"sleep {PACE}s")
    time.sleep(PACE)
    log(f"OP GetDevicePropDesc 0x{PROP:04X} tid=4")
    d, rc = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1014, 4, PROP))
    log(f"  -> resp=0x{(rc or 0):04x} data={len(d)}B hex[:32]={d[:32].hex()}")

    log(f"sleep {PACE}s")
    time.sleep(PACE)
    log(f"OP GetDevicePropValue 0x{PROP:04X} tid=5")
    d, rc = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1015, 5, PROP))
    log(f"  -> resp=0x{(rc or 0):04x} data={len(d)}B hex={d.hex()}")

    log(f"sleep {PACE}s")
    time.sleep(PACE)
    log("OP CloseSession tid=6")
    try:
        sock.sendall(ptpip.build_close_session(transaction_id=6))
        pkt = ptpip.recv_packet(sock)
        rc = ptpip.ptp_container_header(pkt).get("code")
        log(f"  -> resp=0x{(rc or 0):04x}")
    except _socket.error as e:
        log(f"  -> CloseSession failed (peer may have closed): {e}")
    sock.close()
    log("END d4_observe.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
