#!/usr/bin/env python3
"""D4 consult follow-up (2026-05-29T0827Z asks): paced PTP-IP session vs 10.42.0.169:15740
with Fuji VENDOR codes — answers D4's routing question (does ThreadX dispatch vendor ops
via a different RPC path than standard ops?). READ-ONLY, paced ~7 s between ops.

Sequence (UTC-timestamped):
  PCSS knock -> Init -> OpenSession (tid=1) + GetDeviceInfo (tid=2)  [cwt-internal]
  T+7   GetDeviceInfo            tid=3   standard, baseline (same as round 2)
  T+14  GetDevicePropDesc 0xD001 tid=4   Fuji VENDOR prop (FilmSimulation, advertised on 287-prop personas)
  T+21  0x9018 (no params)       tid=5   Fuji VENDOR opcode (GetLiveViewData) — expect 0x2002, routing test
  T+28  CloseSession             tid=6
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
PROP = 0xD001          # Fuji vendor: FilmSimulation
VENDOR_OP = 0x9018     # Fuji vendor: GetLiveViewData (no params -> 0x2002 GeneralError, safe)
PACE = 7.0


def ts():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def log(msg):
    print(f"[{ts()}] {msg}", flush=True)


def main():
    my_ip = cwt.my_ip_for(CAM)
    log(f"BEGIN d4_observe2 vs {CAM} (my_ip={my_ip})")
    log("knock + listen for camera callback ...")
    srv = cwt.open_callback_listener(cwt.CALLBACK_PORT)
    cb = cwt.wait_for_callback(srv, CAM, my_ip, 12, 10.0, False)
    srv.close()
    if cb is None:
        log("FATAL: camera never called back -- abort")
        return 2
    notify = cwt.handle_notify(cb)
    cb.close()
    log(f"NOTIFY parsed: dscport={notify.get('dscport')}")
    sock = cwt.connect_ptpip(CAM, my_ip, cwt.DEFAULT_GUID, "d3-wire-d4", 6.0, notify["dscport"])
    if isinstance(sock, int):
        log(f"FATAL: connect_ptpip failed code={sock}")
        return 3
    log("PTP-IP ESTABLISHED (OpenSession tid=1 + GetDeviceInfo tid=2 inside cwt.connect_ptpip)")

    log(f"sleep {PACE}s")
    time.sleep(PACE)
    log("OP GetDeviceInfo tid=3 (standard, baseline)")
    d, rc = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1001, 3))
    log(f"  -> resp=0x{(rc or 0):04x} data={len(d)}B")

    log(f"sleep {PACE}s")
    time.sleep(PACE)
    log(f"OP GetDevicePropDesc 0x{PROP:04X} tid=4  (Fuji VENDOR prop)")
    d, rc = cwt.ptp_op(sock, ptpip.build_ptp_command(0x1014, 4, PROP))
    log(f"  -> resp=0x{(rc or 0):04x} data={len(d)}B hex[:48]={d[:48].hex()}")

    log(f"sleep {PACE}s")
    time.sleep(PACE)
    log(f"OP 0x{VENDOR_OP:04X} tid=5 no params  (Fuji VENDOR opcode — routing test)")
    d, rc = cwt.ptp_op(sock, ptpip.build_ptp_command(VENDOR_OP, 5))
    log(f"  -> resp=0x{(rc or 0):04x} data={len(d)}B")

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
    log("END d4_observe2.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
