#!/usr/bin/env python3
"""Fuji PCSS/1.0 (PC Shoot Service) camera "knock" — WIRE-CONFIRMED 2026-05-23.

The desktop tether arms the camera's PTP-IP listener by sending an SSDP-style
'DISCOVERY * HTTP/1.1 ... SERVICE: PCSS/1.0' over UDP to the camera on port 51562,
with the HOST header set to the PC's OWN IP (tells the camera who/where to connect).
The camera sends NO UDP reply; if ready (fresh boot) it silently arms + the PTP-IP
TCP session comes up on camera:15740 (the camera also SYNs back to the PC). If the
camera is "spent" (already connected once since boot), the knock gets ICMP
port-unreachable on 51562 — so this only works ONCE PER BOOT.

Usage: pcss_discover.py <camera_ip> [my_ip]
  my_ip defaults to the source address the kernel picks for routing to the camera.
"""
import socket
import sys
import time

CAM = sys.argv[1] if len(sys.argv) > 1 else "192.168.5.192"
PCSS_PORT = 51562        # WIRE-CONFIRMED knock port (NOT 1900)
PTP_PORT = 15740         # standard PTP-IP control port the desktop tether uses


def my_ip_for(dst):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect((dst, 9))
        return s.getsockname()[0]
    finally:
        s.close()


MY_IP = sys.argv[2] if len(sys.argv) > 2 else my_ip_for(CAM)


def knock(host_ip):
    # exact on-wire form: trailing NUL after the blank line (observed)
    return ("DISCOVERY * HTTP/1.1\r\n"
            f"HOST: {host_ip}\r\n"
            "MX: 5\r\n"
            "SERVICE: PCSS/1.0\r\n\r\n\x00").encode()


s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.settimeout(3)
pkt = knock(MY_IP)
print(f"knock → {CAM}:{PCSS_PORT}  HOST={MY_IP}  ({len(pkt)}B)")
s.sendto(pkt, (CAM, PCSS_PORT))

# camera sends no PCSS reply; an ICMP port-unreachable surfaces here as OSError
try:
    data, src = s.recvfrom(4096)
    print(f"<<< unexpected UDP reply {src}: {data!r}")
except socket.timeout:
    print("(no UDP reply — expected; camera arms silently)")
except OSError as e:
    print(f"(no UDP reply / {e}) — refused knock = ICMP port-unreachable (camera spent/asleep)")
s.close()

# the real success signal: did camera:15740 (PTP-IP) come up?
time.sleep(0.5)
c = socket.socket()
c.settimeout(2)
r = c.connect_ex((CAM, PTP_PORT))
c.close()
if r == 0:
    print(f"TCP {PTP_PORT} (PTP-IP): OPEN — camera armed, ready for InitCommandRequest")
else:
    print(f"TCP {PTP_PORT} (PTP-IP): closed ({r}) — knock not accepted (reboot camera?)")
