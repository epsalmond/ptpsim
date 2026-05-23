#!/usr/bin/env python3
"""Connect to a GFX100 II over WIRELESS INFRASTRUCTURE TETHER — no BLE, no AP launch.

Reproduces the desktop "FUJIFILM Tether App" wireless connection, reverse-engineered from


  0. LISTEN on TCP 51560 (a callback port hardcoded in all the Fuji transport binaries).
  1. KNOCK every 10 s: UDP datagram to camera:51562 carrying the Fuji PCSS/1.0 "DISCOVERY"
     request with HOST = THIS host's IP (no UDP reply is sent).
  2. WAIT: when the camera is ready it ARPs for our IP and **connects back to our :51560**
     (the camera is the client). That inbound connection is the "ready" signal. If the
     camera is not ready it ICMP-rejects the knock; we just re-knock at the next 10 s tick.
  3. PTP-IP: TCP connect out to camera:15740 (standard PTP-IP — NOT the 55740 family the
     reference app/AP path uses), send Init_Command_Request (GUID + our IP + name + zeros tail),
     read Init_Command_Ack, then OpenSession + GetDeviceInfo.

IMPORTANT — WORKS ONCE PER BOOT: after one session the camera stops accepting the knock
(ICMP port-unreachable on 51562, never ARPs/calls back) until power-cycled.

Usage:
  PYTHONPATH=. python scripts/connect_wireless_tether.py <camera_ip> [--my-ip A.B.C.D]
      [--guid HEX32] [--name NAME] [--retries N] [--interval 10] [--keep-open]

Exit 0 = OpenSession succeeded (camera under wireless tether control).
"""
from __future__ import annotations

import argparse
import socket
import struct
import sys
import time

from rce.tools.fuji_ble_gps import ptpip

PCSS_PORT = 51562       # UDP knock destination (camera listens here when ready)
CALLBACK_PORT = 51560   # TCP port the PC listens on; camera dials back here (hardcoded in binaries)
PTP_PORT = 15740        # standard PTP-IP control port the desktop tether uses
DEFAULT_GUID = "f2e4538fada5485d87b27f0bd3d5ded0"  # matches the captured desktop session


def my_ip_for(dst: str) -> str:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect((dst, 9))
        return s.getsockname()[0]
    finally:
        s.close()


def build_knock(my_ip: str) -> bytes:
    # exact on-wire form observed: HOST = the PC's own IP, single CRLF + trailing NUL
    return (
        "DISCOVERY * HTTP/1.1\r\n"
        f"HOST: {my_ip}\r\n"
        "MX: 5\r\n"
        "SERVICE: PCSS/1.0\r\n\x00"
    ).encode()


def send_knock(cam_ip: str, my_ip: str) -> None:
    # fresh ephemeral source port each knock, like the app
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        pkt = build_knock(my_ip)
        sock.sendto(pkt, (cam_ip, PCSS_PORT))
        print(f"[knock] UDP {cam_ip}:{PCSS_PORT}  HOST={my_ip}  ({len(pkt)}B)")
    finally:
        sock.close()


def open_callback_listener(port: int) -> socket.socket:
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(1)
    return srv


def wait_for_callback(srv: socket.socket, cam_ip: str, my_ip: str, retries: int,
                      interval: float, no_knock: bool):
    """Knock every <interval>s; return the camera's inbound callback socket when it dials :51560."""
    attempts = max(1, retries + 1)
    for i in range(attempts):
        if not no_knock:
            send_knock(cam_ip, my_ip)
        srv.settimeout(interval)
        try:
            conn, addr = srv.accept()
        except socket.timeout:
            print(f"[poll] no callback yet (attempt {i + 1}/{attempts}); re-knock")
            continue
        if addr[0] != cam_ip:
            print(f"[poll] ignoring callback from {addr[0]} (not the camera)")
            conn.close()
            continue
        print(f"[ready] camera dialed back from {addr[0]}:{addr[1]} -> our :{CALLBACK_PORT}")
        return conn
    return None


def parse_init_ack(data: bytes) -> dict:
    out: dict = {"len": len(data), "raw_type": None, "conn_no": None, "guid": None,
                 "name": None, "fail_reason": None}
    if len(data) >= 8:
        _length, out["raw_type"] = struct.unpack_from("<II", data, 0)
    if out["raw_type"] == 5 and len(data) >= 12:  # Init_Fail: reason is a PTP response code
        out["fail_reason"] = struct.unpack_from("<I", data, 8)[0]
    if out["raw_type"] == 2 and len(data) >= 28:  # Init_Command_Ack
        out["conn_no"] = struct.unpack_from("<I", data, 8)[0]
        out["guid"] = data[12:28].hex()
        out["name"] = data[28:].decode("utf-16le", errors="replace").split("\x00", 1)[0]
    return out


def handle_notify(conn: socket.socket) -> dict:
    """On the :51560 callback the camera sends a PCSS NOTIFY announcing CAMERANAME and the
    DSCPORT to use for PTP-IP. The PC MUST reply 'HTTP/1.1 200 OK' (else the camera aborts;
    a 403 would reject). Returns parsed headers incl. 'dscport'."""
    conn.settimeout(3)
    try:
        data = conn.recv(2048)
    except OSError:
        data = b""
    text = data.decode("latin1")
    hdrs = {}
    for line in text.split("\r\n"):
        if ":" in line:
            k, _, v = line.partition(":")
            hdrs[k.strip().upper()] = v.strip()
    out = {
        "camera": hdrs.get("CAMERANAME"),
        "dsc": hdrs.get("DSC"),
        "dscport": int(hdrs["DSCPORT"]) if hdrs.get("DSCPORT", "").isdigit() else PTP_PORT,
        "raw": text.split("\r\n", 1)[0],
    }
    print(f"[notify] {out['raw']}  CAMERANAME={out['camera']} DSC={out['dsc']} DSCPORT={out['dscport']}")
    try:
        conn.sendall(b"HTTP/1.1 200 OK\r\n\x00")  # accept; 403 would reject
        print("[notify] sent HTTP/1.1 200 OK (accept)")
    except OSError as exc:
        print(f"[notify] failed to send 200 OK ({exc})")
    return out


def connect_ptpip(cam_ip: str, my_ip: str, guid_hex: str, name: str, timeout: float, port: int):
    """Open the PTP-IP command channel + OpenSession. The camera signals 'busy' two ways while it
    finishes coming up: an Init_Fail(0x2019) packet (resend on the same socket) OR a TCP RST
    (reconnect). Retry through both."""
    init = build_desktop_init(name, my_ip, guid_hex)
    info: dict = {}
    sock = None
    for attempt in range(12):
        if sock is None:
            try:
                sock = socket.create_connection((cam_ip, port), timeout)
                sock.settimeout(timeout)
                print(f"[ptpip] TCP connect {cam_ip}:{port}")
            except OSError as exc:
                print(f"[ptpip] connect failed ({exc}) — retry {attempt + 1}/12")
                time.sleep(0.3)
                continue
        try:
            sock.sendall(init)
            info = parse_init_ack(ptpip.recv_packet(sock))
        except OSError as exc:  # camera RST the socket = busy; reconnect and retry
            print(f"[ptpip] init reset ({exc}) — reconnect {attempt + 1}/12")
            try:
                sock.close()
            except OSError:
                pass
            sock = None
            time.sleep(0.3)
            continue
        if info["raw_type"] == 2:
            break
        if info["raw_type"] == 5:
            print(f"[ptpip] Init_Fail 0x{info['fail_reason']:04x}"
                  f"{' (Device_Busy)' if info['fail_reason'] == 0x2019 else ''} — retry {attempt + 1}/12")
            time.sleep(0.2)
            continue  # resend on the same socket (matches the capture)
        print(f"[ptpip] unexpected init response (type {info['raw_type']}, {info['len']}B) — reconnect")
        sock.close()
        sock = None
        time.sleep(0.3)
    if info.get("raw_type") != 2:
        if sock is not None:
            sock.close()
        print("[ptpip] camera never acked Init_Command_Request after retries — power-cycle it")
        return 3
    print(f"[ptpip] Init_Command_Ack: camera='{info['name']}' guid={info['guid']} conn#={info['conn_no']}")

    sock.sendall(ptpip.build_open_session())
    hdr = ptpip.ptp_container_header(ptpip.recv_packet(sock))
    ok = hdr.get("code") in (ptpip.PTP_RESPONSE_OK, ptpip.PTP_RESPONSE_SESSION_ALREADY_OPEN)
    print(f"[ptpip] OpenSession -> 0x{hdr.get('code', 0):04x} {'OK' if ok else 'FAILED'}")
    if not ok:
        sock.close()
        return 4

    sock.sendall(ptpip.build_ptp_command(0x1001, transaction_id=2))  # GetDeviceInfo
    di = ptpip.recv_packet(sock)
    print(f"[ptpip] GetDeviceInfo -> {len(di)}B data phase (control confirmed)")
    return sock


def build_desktop_init(name: str, my_ip: str, guid_hex: str) -> bytes:
    """Desktop-tether Init_Command_Request: the 'zeros' profile with the 4 bytes after the
    GUID replaced by our IP in little-endian order (the camera connect-back hint)."""
    init = ptpip.build_init_command_request(name, "zeros", guid=ptpip.parse_guid_hex(guid_hex))
    ip_field = socket.inet_aton(my_ip)[::-1]  # 192.168.7.49 -> bytes 31 07 a8 c0
    return init[:24] + ip_field + init[28:]


def ptp_op(sock: socket.socket, request: bytes):
    """Send a PTP-IP operation; gather any DATA container(s) then the RESPONSE code."""
    sock.sendall(request)
    data = bytearray()
    code = None
    while True:
        pkt = ptpip.recv_packet(sock)
        if not pkt:
            break
        hdr = ptpip.ptp_container_header(pkt)
        ct = hdr.get("container_type")
        if ct == ptpip.PTP_CONTAINER_DATA:
            data += ptpip.ptp_data_payload(pkt)
        elif ct == ptpip.PTP_CONTAINER_RESPONSE:
            code = hdr.get("code")
            break
    return bytes(data), code


def pull_images(sock: socket.socket, out_dir: str, max_images: int, list_only: bool, tid: int) -> int:
    """Enumerate the card (StorageIDs -> ObjectHandles) and download the newest images."""
    import os

    data, code = ptp_op(sock, ptpip.build_ptp_command(0x1004, tid))  # GetStorageIDs
    tid += 1
    storages = ptpip.decode_u32_array_payload(data) if data else []
    print(f"[work] StorageIDs {[hex(s) for s in storages]} (resp 0x{(code or 0):04x})")

    handles: list[int] = []
    for st in (storages or [0xFFFFFFFF]):
        data, code = ptp_op(sock, ptpip.build_ptp_command(0x1007, tid, st, 0, 0))  # GetObjectHandles
        tid += 1
        found = ptpip.decode_u32_array_payload(data) if data else []
        print(f"[work] storage 0x{st:08x}: {len(found)} objects (resp 0x{(code or 0):04x})")
        handles += found
    if not handles:
        print("[work] no objects on card")
        return tid

    targets = handles[-max_images:] if max_images else handles
    print(f"[work] {len(handles)} objects total; {'listing' if list_only else 'downloading'} {len(targets)}")
    if not list_only:
        os.makedirs(out_dir, exist_ok=True)
    for h in targets:
        data, code = ptp_op(sock, ptpip.build_get_object_info(h, tid))
        tid += 1
        try:
            info = ptpip.decode_object_info_payload(data)
        except ValueError as exc:
            print(f"[work] 0x{h:08x}: ObjectInfo decode failed ({exc})")
            continue
        name = info["filename"]
        print(f"[work] 0x{h:08x}  {name}  {info['object_format_name']}  "
              f"{info['image_pix_width']}x{info['image_pix_height']}  {info['object_compressed_size']}B")
        if list_only:
            continue
        data, code = ptp_op(sock, ptpip.build_get_object(h, tid))
        tid += 1
        fn = ptpip.safe_download_filename(name, f"obj_{h:08x}.bin")
        with open(os.path.join(out_dir, fn), "wb") as fh:
            fh.write(data)
        extra = ""
        if data[:2] == b"\xff\xd8":
            ji = ptpip.jpeg_payload_info(data)
            extra = f"  jpeg soi={ji['starts_with_soi']} eoi={ji['ends_with_eoi']}"
        print(f"[work]   saved {fn} ({len(data)}B, resp 0x{(code or 0):04x}){extra}")
    print(f"[work] saved to {out_dir}")
    return tid


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="connect-wireless-tether")
    p.add_argument("camera_ip")
    p.add_argument("--my-ip", default=None, help="this host's IP (default: auto from route to camera)")
    p.add_argument("--guid", default=DEFAULT_GUID)
    p.add_argument("--name", default="mbp")
    p.add_argument("--timeout", type=float, default=6.0)
    p.add_argument("--retries", type=int, default=12, help="knock attempts before giving up (10s each)")
    p.add_argument("--interval", type=float, default=10.0, help="seconds between knocks (app uses 10)")
    p.add_argument("--callback-port", type=int, default=CALLBACK_PORT)
    p.add_argument("--knock-only", action="store_true", help="send one knock and exit")
    p.add_argument("--no-knock", action="store_true", help="don't knock, just wait for a callback")
    p.add_argument("--keep-open", action="store_true", help="leave the session open (default: CloseSession)")
    p.add_argument("--list", action="store_true", help="enumerate the card (no download)")
    p.add_argument("--pull-images", type=int, metavar="N",
                   help="download the N newest images (0 = all)")
    p.add_argument("--out", default=None, help="download dir (default: rce/sessions/tether_pull_<ts>)")
    args = p.parse_args(argv)

    my_ip = args.my_ip or my_ip_for(args.camera_ip)

    if args.knock_only:
        send_knock(args.camera_ip, my_ip)
        return 0

    try:
        srv = open_callback_listener(args.callback_port)
    except OSError as exc:
        print(f"[fail] cannot listen on :{args.callback_port} ({exc}) — is another tether app running?")
        return 5
    print(f"[listen] TCP :{args.callback_port} for the camera's callback")

    callback = wait_for_callback(srv, args.camera_ip, my_ip, args.retries, args.interval, args.no_knock)
    srv.close()
    if callback is None:
        print(f"[fail] camera never called back after {args.retries + 1} knocks — power-cycle it (once per boot)")
        return 2
    # the camera sends a PCSS NOTIFY here announcing CAMERANAME + DSCPORT; we must 200-OK it
    notify = handle_notify(callback)
    callback.close()  # camera closes this channel right after the ack

    result = connect_ptpip(args.camera_ip, my_ip, args.guid, args.name, args.timeout, notify["dscport"])
    if isinstance(result, int):
        return result

    sock = result
    print("[ok] wireless tether session established — no BLE, no AP")

    if args.list or args.pull_images is not None:
        out = args.out or f"rce/sessions/tether_pull_{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}"
        try:
            pull_images(sock, out, args.pull_images or 0, args.list, tid=3)
        except OSError as exc:
            print(f"[work] aborted ({exc})")

    if not args.keep_open:
        try:
            sock.sendall(ptpip.build_close_session(transaction_id=3))
            ptpip.recv_packet(sock)
        except OSError:
            pass
        sock.close()
        print("[ok] CloseSession sent (camera now spent until power-cycle)")
    else:
        print("[ok] session left open; sockets not retained by this CLI (use as a library to drive PTP ops)")
        sock.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
