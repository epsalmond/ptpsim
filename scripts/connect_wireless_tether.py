#!/usr/bin/env python3
"""Connect to a GFX100 II over WIRELESS INFRASTRUCTURE TETHER — no BLE, no AP launch.

Reproduces the desktop "FUJIFILM Tether App" wireless connection, reverse-engineered from


  1. KNOCK: send the Fuji PCSS/1.0 "DISCOVERY" datagram to UDP <camera>:51562 with the
     HOST header set to THIS host's IP. The camera arms its PTP-IP listener (and learns
     where to connect back). No UDP reply is sent.
  2. PTP-IP: TCP connect to <camera>:15740 (standard PTP-IP — NOT the 55740 family the
     reference app/AP path uses), send Init_Command_Request (GUID + our IP + friendly name + zeros
     tail), read Init_Command_Ack (camera GUID + model), then OpenSession + GetDeviceInfo.

IMPORTANT — WORKS ONCE PER BOOT: after one successful session the camera stops answering
the knock (returns ICMP port-unreachable on 51562) until it is power-cycled. Power-cycle
the camera before each run.

Usage:
  PYTHONPATH=. python scripts/connect_wireless_tether.py <camera_ip> [--my-ip A.B.C.D]
      [--guid HEX32] [--name NAME] [--knock-only] [--no-knock] [--keep-open]

Exit 0 = OpenSession succeeded (camera under wireless tether control).
"""
from __future__ import annotations

import argparse
import socket
import struct
import sys
import time

from rce.tools.fuji_ble_gps import ptpip

PCSS_PORT = 51562
PTP_PORT = 15740
DEFAULT_GUID = "f2e4538fada5485d87b27f0bd3d5ded0"  # matches the captured desktop session


def my_ip_for(dst: str) -> str:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect((dst, 9))
        return s.getsockname()[0]
    finally:
        s.close()


def build_knock(my_ip: str) -> bytes:
    # exact on-wire form observed: HOST = the PC's own IP, trailing NUL after blank line
    return (
        "DISCOVERY * HTTP/1.1\r\n"
        f"HOST: {my_ip}\r\n"
        "MX: 5\r\n"
        "SERVICE: PCSS/1.0\r\n\x00"
    ).encode()


def send_knock(cam_ip: str, my_ip: str) -> None:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(2)
    pkt = build_knock(my_ip)
    print(f"[knock] UDP {cam_ip}:{PCSS_PORT}  HOST={my_ip}  ({len(pkt)}B)")
    sock.sendto(pkt, (cam_ip, PCSS_PORT))
    # camera arms silently; an ICMP port-unreachable shows up as OSError on a later recv
    try:
        data, src = sock.recvfrom(2048)
        print(f"[knock] unexpected UDP reply from {src}: {data!r}")
    except socket.timeout:
        pass
    except OSError as exc:
        print(f"[knock] ICMP/refused ({exc}) — camera may be spent (reboot) or asleep")
    finally:
        sock.close()


def build_desktop_init(name: str, my_ip: str, guid_hex: str) -> bytes:
    """Desktop-tether Init_Command_Request: like the 'zeros' profile but with the 4 bytes
    after the GUID replaced by our IP in little-endian order (camera connect-back hint)."""
    init = ptpip.build_init_command_request(name, "zeros", guid=ptpip.parse_guid_hex(guid_hex))
    ip_field = socket.inet_aton(my_ip)[::-1]  # 192.168.7.49 -> bytes 31 07 a8 c0
    return init[:24] + ip_field + init[28:]


def parse_init_ack(data: bytes) -> dict:
    out: dict = {"len": len(data), "raw_type": None, "conn_no": None, "guid": None, "name": None}
    if len(data) < 28:
        return out
    _length, ptype = struct.unpack_from("<II", data, 0)
    out["raw_type"] = ptype
    out["conn_no"] = struct.unpack_from("<I", data, 8)[0]
    out["guid"] = data[12:28].hex()
    name_field = data[28:]
    out["name"] = name_field.decode("utf-16le", errors="replace").split("\x00", 1)[0]
    return out


def connect_ptpip(cam_ip: str, my_ip: str, guid_hex: str, name: str, timeout: float) -> int:
    print(f"[ptpip] TCP connect {cam_ip}:{PTP_PORT}")
    try:
        sock = socket.create_connection((cam_ip, PTP_PORT), timeout)
    except OSError as exc:
        print(f"[ptpip] connect FAILED ({exc}) — knock not accepted; power-cycle the camera and retry")
        return 2
    sock.settimeout(timeout)

    init = build_desktop_init(name, my_ip, guid_hex)
    sock.sendall(init)
    ack = ptpip.recv_packet(sock)
    info = parse_init_ack(ack)
    if info["raw_type"] != 2:
        print(f"[ptpip] no Init_Command_Ack (got type {info['raw_type']}, {info['len']}B) — abort")
        sock.close()
        return 3
    print(f"[ptpip] Init_Command_Ack: camera='{info['name']}' guid={info['guid']} conn#={info['conn_no']}")

    sock.sendall(ptpip.build_open_session())
    resp = ptpip.recv_packet(sock)
    hdr = ptpip.ptp_container_header(resp)
    ok = hdr.get("code") in (ptpip.PTP_RESPONSE_OK, ptpip.PTP_RESPONSE_SESSION_ALREADY_OPEN)
    print(f"[ptpip] OpenSession -> 0x{hdr.get('code', 0):04x} {'OK' if ok else 'FAILED'}")
    if not ok:
        sock.close()
        return 4

    # prove control: GetDeviceInfo (0x1001)
    sock.sendall(ptpip.build_ptp_command(0x1001, transaction_id=2))
    di = ptpip.recv_packet(sock)
    print(f"[ptpip] GetDeviceInfo -> {len(di)}B data phase (control confirmed)")
    return sock


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="connect-wireless-tether")
    p.add_argument("camera_ip")
    p.add_argument("--my-ip", default=None, help="this host's IP (default: auto from route to camera)")
    p.add_argument("--guid", default=DEFAULT_GUID)
    p.add_argument("--name", default="mbp")
    p.add_argument("--timeout", type=float, default=6.0)
    p.add_argument("--knock-only", action="store_true", help="send the PCSS knock and exit")
    p.add_argument("--no-knock", action="store_true", help="skip the knock (camera already armed)")
    p.add_argument("--keep-open", action="store_true", help="leave the session open (default: CloseSession)")
    args = p.parse_args(argv)

    my_ip = args.my_ip or my_ip_for(args.camera_ip)

    if not args.no_knock:
        send_knock(args.camera_ip, my_ip)
        time.sleep(0.4)  # let the camera arm its listener
    if args.knock_only:
        return 0

    result = connect_ptpip(args.camera_ip, my_ip, args.guid, args.name, args.timeout)
    if isinstance(result, int):
        return result

    sock = result
    print("[ok] wireless tether session established — no BLE, no AP")
    if not args.keep_open:
        try:
            sock.sendall(ptpip.build_close_session(transaction_id=3))
            ptpip.recv_packet(sock)
        except OSError:
            pass
        sock.close()
        print("[ok] CloseSession sent (camera now spent until power-cycle)")
    else:
        print("[ok] session left OPEN (socket not retained by this CLI; use as a library for control)")
        sock.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
