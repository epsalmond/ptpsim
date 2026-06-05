#!/usr/bin/env python3
"""Passive PCSS/1.0 discovery listener — binds UDP :51562 and :1900, prints any frames.

Use this when another Fuji device on the LAN is doing PCSS, when a co-resident
desktop tether app is exchanging knocks, or when the camera is sending NOTIFY to
a broadcast/multicast address we want to see. The active path is
`scripts/connect_wireless_tether.py`; this is the passive read-only counterpart.

Binds:
  - UDP 0.0.0.0:51562  — WIRE-CONFIRMED PCSS knock port (DISCOVERY destination).
  - UDP 0.0.0.0:1900   — the original SSDP port; firmware doesn't use it but
                         other Fuji devices / mDNS-style traffic might. Cheap insurance.
  - Optional IP_ADD_MEMBERSHIP for 239.255.255.250 on :1900 (the SSDP multicast group).

Usage:
  python3 scripts/pcss_listen.py [--bundle path.jsonl] [--mcast] [--port 51562]
                                 [--no-1900] [--duration SECS]

Bundle output: one JSONL fact per parsed frame, schema
  {kind: "pcss.frame", ts, src_ip, src_port, dst_port, verb, status_code,
   headers, trailing_nul, frame_bytes_hex}

Exit on Ctrl-C or after --duration seconds.
"""
from __future__ import annotations

import argparse
import json
import os
import select
import socket
import struct
import sys
import time

# Allow `python3 scripts/pcss_listen.py` from the repo root or anywhere else.
_REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if _REPO not in sys.path:
    sys.path.insert(0, _REPO)

from protocol_mapper.pcss_frame import parse_pcss_frame  # noqa: E402

PCSS_KNOCK_PORT = 51562
SSDP_PORT = 1900
SSDP_MCAST_GROUP = "239.255.255.250"


def _bind_udp(port: int) -> socket.socket:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    except OSError:
        pass
    s.bind(("0.0.0.0", port))
    return s


def _join_mcast(s: socket.socket, group: str) -> None:
    mreq = struct.pack("=4sl", socket.inet_aton(group), socket.INADDR_ANY)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, mreq)


def _open_bundle(path: str | None):
    """Return a callable `emit(rec)` that writes JSONL, or None."""
    if not path:
        return None
    fh = open(path, "a", buffering=1)

    def emit(rec: dict) -> None:
        fh.write(json.dumps(rec) + "\n")

    emit._fh = fh  # keep alive
    return emit


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="pcss-listen", description=__doc__)
    p.add_argument("--bundle", default=None, help="append JSONL frame facts here")
    p.add_argument("--mcast", action="store_true",
                   help="also IP_ADD_MEMBERSHIP to 239.255.255.250 on :1900 (insurance)")
    p.add_argument("--no-1900", action="store_true", help="skip binding UDP :1900")
    p.add_argument("--port", type=int, default=PCSS_KNOCK_PORT,
                   help=f"override the PCSS knock port (default {PCSS_KNOCK_PORT})")
    p.add_argument("--duration", type=float, default=0.0,
                   help="exit after N seconds (default: run until Ctrl-C)")
    args = p.parse_args(argv)

    sockets: list[socket.socket] = []
    try:
        s_pcss = _bind_udp(args.port)
        sockets.append(s_pcss)
        print(f"[listen] UDP 0.0.0.0:{args.port}  (PCSS knock)")
    except OSError as exc:
        print(f"[fail] cannot bind UDP :{args.port} ({exc})")
        return 5

    if not args.no_1900:
        try:
            s_ssdp = _bind_udp(SSDP_PORT)
            sockets.append(s_ssdp)
            print(f"[listen] UDP 0.0.0.0:{SSDP_PORT}  (legacy SSDP-style)")
            if args.mcast:
                try:
                    _join_mcast(s_ssdp, SSDP_MCAST_GROUP)
                    print(f"[listen] joined multicast {SSDP_MCAST_GROUP} on :{SSDP_PORT}")
                except OSError as exc:
                    print(f"[warn] IP_ADD_MEMBERSHIP failed ({exc}) — continuing without mcast")
        except OSError as exc:
            print(f"[warn] cannot bind UDP :{SSDP_PORT} ({exc}) — continuing without it")

    emit = _open_bundle(args.bundle)
    if emit:
        print(f"[bundle] appending JSONL frame facts -> {args.bundle}")

    deadline = (time.time() + args.duration) if args.duration > 0 else None
    print("[listen] waiting for PCSS frames (Ctrl-C to stop) ...")
    count = 0
    try:
        while True:
            if deadline is not None:
                remaining = deadline - time.time()
                if remaining <= 0:
                    break
                timeout = min(1.0, remaining)
            else:
                timeout = 1.0
            ready, _, _ = select.select(sockets, [], [], timeout)
            for sock in ready:
                try:
                    data, src = sock.recvfrom(4096)
                except OSError as exc:
                    print(f"[warn] recvfrom failed ({exc})")
                    continue
                dst_port = sock.getsockname()[1]
                frame = parse_pcss_frame(data)
                ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
                if frame is None:
                    print(f"[{ts}] {src[0]}:{src[1]} -> :{dst_port}  "
                          f"({len(data)}B, not a PCSS frame) {data[:60]!r}...")
                    continue
                count += 1
                print(f"[{ts}] {src[0]}:{src[1]} -> :{dst_port}  "
                      f"verb={frame.verb!r}  status={frame.status_code}  "
                      f"NUL={frame.trailing_nul}  headers={frame.headers}")
                if emit:
                    rec = {
                        "kind": "pcss.frame",
                        "ts": ts,
                        "src_ip": src[0],
                        "src_port": src[1],
                        "dst_port": dst_port,
                    }
                    rec.update(frame.to_dict())
                    emit(rec)
    except KeyboardInterrupt:
        print()
    finally:
        for s in sockets:
            try:
                s.close()
            except OSError:
                pass
        if emit and hasattr(emit, "_fh"):
            emit._fh.close()
    print(f"[done] parsed {count} PCSS frame(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
