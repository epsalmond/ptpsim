#!/usr/bin/env python3
"""Fuji PCSS/1.0 (PC Shoot Service) SSDP-style camera discovery probe.
Sends 'DISCOVERY * HTTP/1.1 ... SERVICE: PCSS/1.0' over UDP:1900 (unicast+broadcast),
listens for the camera's reply, then re-checks whether TCP 55740 opens."""
import socket, sys, time

CAM = sys.argv[1] if len(sys.argv)>1 else "192.168.5.192"
PORT = 1900
def req(host_hdr):
    return ("DISCOVERY * HTTP/1.1\r\n"
            f"HOST: {host_hdr}:{PORT}\r\n"
            "MX: 5\r\n"
            "SERVICE: PCSS/1.0\r\n\r\n").encode()

s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
s.bind(("0.0.0.0", PORT))   # listen on 1900 for replies/NOTIFY
s.settimeout(6)

targets = [(CAM, "unicast→cam"),
           ("192.168.5.255","subnet-bcast"),
           ("255.255.255.255","global-bcast")]
for addr,label in targets:
    pkt = req("255.255.255.255")
    try:
        s.sendto(pkt, (addr, PORT)); print(f"sent DISCOVERY → {addr}:{PORT} ({label})")
    except OSError as e:
        print(f"send {addr} failed: {e}")

print("--- listening 6s for PCSS replies ---")
t0=time.time()
got=False
while time.time()-t0 < 6:
    try:
        data,src = s.recvfrom(4096)
        got=True
        print(f"<<< {src[0]}:{src[1]}  {len(data)}B")
        print(data.decode('latin1'))
        print("-"*40)
    except socket.timeout:
        break
if not got: print("(no UDP reply)")
s.close()

# re-check TCP 55740 right after
for p in (55740,55741,55742):
    c=socket.socket(); c.settimeout(2)
    r=c.connect_ex((CAM,p)); c.close()
    print(f"TCP {p}: {'OPEN' if r==0 else 'closed (%d)'%r}")
