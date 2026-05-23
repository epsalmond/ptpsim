#!/usr/bin/env python3
"""tether_view.py — hold a wireless-tether PTP-IP session open and serve live-view over HTTP.

Establishes the BLE/AP-free wireless tether session (see connect_wireless_tether.py), then runs ONE
session-driver thread that owns the PTP socket. It starts live view (InitiateOpenCapture 0x101C) and
continuously pulls JPEG frames — GetObjectInfo / GetObject / DeleteObject on handle 0x80000001, the
desktop tether's live-view loop — into a frame hub, servicing queued property commands between
frames. An HTTP server exposes:

  /          viewer page (<img src=/stream>)
  /stream    multipart/x-mixed-replace MJPEG — point a browser or phone here (the "remote view")
  /snapshot  one current JPEG frame
  /prop?code=0xDDDD[&set=0xVALUE]   read (or write) a device property, returns JSON

Once-per-boot: power-cycle the camera before launching. Live view takes over the camera body LCD
(remote-operation mode), as expected.

Usage: PYTHONPATH=. python scripts/tether_view.py <camera_ip> [--http-port 8080] [--my-ip A.B.C.D]
"""
import argparse
import json
import os
import queue
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import connect_wireless_tether as cwt  # noqa: E402  (sibling script: connect building blocks)
from rce.tools.fuji_ble_gps import ptpip  # noqa: E402

LV_HANDLE = 0x80000001
OP_INITIATE_OPEN_CAPTURE = 0x101C
OP_DELETE_OBJECT = 0x100B


class FrameHub:
    def __init__(self) -> None:
        self.cond = threading.Condition()
        self.jpeg: bytes | None = None
        self.seq = 0
        self.fps = 0.0

    def publish(self, jpeg: bytes, fps: float) -> None:
        with self.cond:
            self.jpeg = jpeg
            self.fps = fps
            self.seq += 1
            self.cond.notify_all()

    def wait_after(self, last_seq: int, timeout: float = 5.0):
        with self.cond:
            if self.seq == last_seq:
                self.cond.wait(timeout)
            return self.seq, self.jpeg


class Command:
    """A property op the HTTP thread hands to the driver thread (which owns the socket)."""
    def __init__(self, fn) -> None:
        self.fn = fn
        self.done = threading.Event()
        self.result = None

    def run(self, sock, tid: int) -> int:
        try:
            self.result, tid = self.fn(sock, tid)
        except Exception as exc:  # surface to the HTTP caller rather than killing the driver
            self.result = {"error": repr(exc)}
        self.done.set()
        return tid


HUB = FrameHub()
CMD_Q: "queue.Queue[Command]" = queue.Queue()
STOP = threading.Event()


def _start_live_view(sock, tid: int) -> int:
    _, code = cwt.ptp_op(sock, ptpip.build_ptp_command(OP_INITIATE_OPEN_CAPTURE, tid, 0, 0))
    print(f"[lv] InitiateOpenCapture(0x101C) -> 0x{(code or 0):04x}")
    return tid + 1


def driver(sock, tid: int) -> None:
    """Owns the PTP socket: start live view, then loop frames + interleave queued commands.
    Publishes only complete JPEGs (FFD8..FFD9) and re-arms live view when frames stall (the camera
    drops the live-view object during AF/capture)."""
    tid = _start_live_view(sock, tid)
    frames = 0
    bad = 0
    t0 = time.time()
    while not STOP.is_set():
        try:
            cmd = CMD_Q.get_nowait()
        except queue.Empty:
            cmd = None
        if cmd is not None:
            tid = cmd.run(sock, tid)
            continue
        try:
            cwt.ptp_op(sock, ptpip.build_get_object_info(LV_HANDLE, tid))
            tid += 1
            data, code = cwt.ptp_op(sock, ptpip.build_get_object(LV_HANDLE, tid))
            tid += 1
            cwt.ptp_op(sock, ptpip.build_ptp_command(OP_DELETE_OBJECT, tid, LV_HANDLE))
            tid += 1
        except (OSError, RuntimeError) as exc:
            print(f"[lv] frame op error ({exc}) — re-arming live view")
            time.sleep(0.3)
            try:
                tid = _start_live_view(sock, tid)
            except (OSError, RuntimeError):
                break
            continue
        if data[:2] == b"\xff\xd8" and data[-2:] == b"\xff\xd9":
            frames += 1
            bad = 0
            dt = time.time() - t0
            fps = frames / dt if dt > 0 else 0.0
            HUB.publish(data, fps)
            if frames % 30 == 0:
                print(f"[lv] {frames} frames, {fps:.1f} fps, last={len(data)}B")
        else:
            bad += 1
            if bad % 8 == 0:  # ~0.5s of stalled/torn frames -> AF or capture interrupted live view
                print(f"[lv] {bad} stalled frames (AF/capture?) — re-arming live view")
                try:
                    tid = _start_live_view(sock, tid)
                except (OSError, RuntimeError):
                    break
    STOP.set()
    print("[lv] driver stopped")


def submit(fn, timeout: float = 6.0):
    cmd = Command(fn)
    CMD_Q.put(cmd)
    cmd.done.wait(timeout)
    return cmd.result if cmd.done.is_set() else {"error": "timeout"}


def get_prop(code: int):
    def fn(sock, tid):
        data, rc = cwt.ptp_op(sock, ptpip.build_get_device_prop_value(code, tid))
        return {"prop": f"0x{code:04x}", "resp": f"0x{(rc or 0):04x}", "value_hex": data.hex()}, tid + 1
    return fn


def set_prop(code: int, value: int, nbytes: int):
    def fn(sock, tid):
        payload = value.to_bytes(nbytes, "little")
        req = ptpip.build_ptp_data_container(ptpip.PTP_SET_DEVICE_PROP_VALUE, tid, payload)
        sock.sendall(ptpip.build_set_device_prop_value(code, tid))  # command phase
        sock.sendall(req)                                           # data phase
        _, rc = cwt.ptp_op(sock, b"")  # read response only (request already sent)
        return {"set": f"0x{code:04x}", "value": f"0x{value:x}", "resp": f"0x{(rc or 0):04x}"}, tid + 1
    return fn


PAGE = b"""<!doctype html><html><head><title>GFX100 II live</title>
<style>body{margin:0;background:#111}img{width:100vw;height:100vh;object-fit:contain}</style>
</head><body><img src="/stream"></body></html>"""


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # quiet
        pass

    def do_GET(self):
        u = urlparse(self.path)
        if u.path == "/":
            self._bytes(PAGE, "text/html")
        elif u.path == "/snapshot":
            _, jpeg = HUB.wait_after(0, 2.0)
            if jpeg:
                self._bytes(jpeg, "image/jpeg")
            else:
                self.send_error(503, "no frame yet")
        elif u.path == "/stream":
            self._stream()
        elif u.path == "/prop":
            q = parse_qs(u.query)
            code = int(q["code"][0], 0)
            if "set" in q:
                val = int(q["set"][0], 0)
                nbytes = int(q.get("bytes", ["4"])[0])
                res = submit(set_prop(code, val, nbytes))
            else:
                res = submit(get_prop(code))
            self._bytes(json.dumps(res).encode(), "application/json")
        else:
            self.send_error(404)

    def _bytes(self, body: bytes, ctype: str):
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "multipart/x-mixed-replace; boundary=frame")
        self.end_headers()
        last = 0
        try:
            while not STOP.is_set():
                last, jpeg = HUB.wait_after(last, 5.0)
                if not jpeg:
                    continue
                self.wfile.write(b"--frame\r\nContent-Type: image/jpeg\r\n")
                self.wfile.write(b"Content-Length: %d\r\n\r\n" % len(jpeg))
                self.wfile.write(jpeg)
                self.wfile.write(b"\r\n")
        except (BrokenPipeError, ConnectionResetError):
            pass


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="tether-view")
    p.add_argument("camera_ip")
    p.add_argument("--my-ip", default=None)
    p.add_argument("--guid", default=cwt.DEFAULT_GUID)
    p.add_argument("--name", default="mbp")
    p.add_argument("--http-port", type=int, default=8080)
    p.add_argument("--retries", type=int, default=12)
    p.add_argument("--interval", type=float, default=10.0)
    args = p.parse_args(argv)

    my_ip = args.my_ip or cwt.my_ip_for(args.camera_ip)
    srv = cwt.open_callback_listener(cwt.CALLBACK_PORT)
    print(f"[listen] TCP :{cwt.CALLBACK_PORT}")
    callback = cwt.wait_for_callback(srv, args.camera_ip, my_ip, args.retries, args.interval, False)
    srv.close()
    if callback is None:
        print("[fail] no callback — power-cycle the camera (once per boot)")
        return 2
    notify = cwt.handle_notify(callback)
    callback.close()
    sock = cwt.connect_ptpip(args.camera_ip, my_ip, args.guid, args.name, 6.0, notify["dscport"])
    if isinstance(sock, int):
        return sock
    print("[ok] session up — starting live view + HTTP")

    drv = threading.Thread(target=driver, args=(sock, 3), daemon=True)
    drv.start()

    httpd = ThreadingHTTPServer(("0.0.0.0", args.http_port), Handler)
    print(f"[http] open http://{my_ip}:{args.http_port}/  (live view; /snapshot, /prop?code=0xd02a)")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\n[stop] shutting down")
    finally:
        STOP.set()
        time.sleep(0.3)
        try:
            sock.sendall(ptpip.build_close_session(transaction_id=999))
        except OSError:
            pass
        sock.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
