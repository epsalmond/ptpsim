#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

python3 - "$ROOT" <<'PY'
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(sys.argv[1])
TRUE_SIZE = 0x00000001230CA400
CHUNK_SIZE = 0x00BFFFE0
FINAL_LOW = 0x22FFCF80
FINAL_LEN = 0x000CD480
FINAL_HIGH = 0x1


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def ptp_string(value):
    if not value:
        return b"\x00"
    encoded = value.encode("utf-16le")
    units = len(encoded) // 2 + 1
    return bytes([units]) + encoded + b"\x00\x00"


def std_init_request():
    body = bytes([0x42]) * 16 + ptp_string("ptpsim-acceptance") + struct.pack("<I", 0x00010000)
    return struct.pack("<II", len(body) + 8, 1) + body


def op_frame(code, tid, params):
    body = struct.pack("<HHI", 1, code, tid)
    body += b"".join(struct.pack("<I", p) for p in params)
    return struct.pack("<I", len(body) + 4) + body


def read_exact(sock, n):
    chunks = []
    remaining = n
    while remaining:
        chunk = sock.recv(min(1024 * 1024, remaining))
        if not chunk:
            raise EOFError(f"socket closed with {remaining} bytes remaining")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_std_frame(sock):
    prefix = read_exact(sock, 4)
    length = struct.unpack("<I", prefix)[0]
    return prefix + read_exact(sock, length - 4)


def read_comp_frame(sock):
    prefix = read_exact(sock, 4)
    length = struct.unpack("<I", prefix)[0]
    body = read_exact(sock, length - 4)
    typ, code, tid = struct.unpack("<HHI", body[:8])
    rest = body[8:]
    if typ == 3:
        params = list(struct.unpack("<" + "I" * (len(rest) // 4), rest)) if rest else []
        return typ, code, tid, params
    return typ, code, tid, rest


def send_op(sock, code, tid, params):
    sock.sendall(op_frame(code, tid, params))


def read_ok(sock, expected_tid):
    typ, resp, resp_tid, params = read_comp_frame(sock)
    assert typ == 3, f"expected response, got type {typ}"
    assert resp == 0x2001, f"expected OK, got {resp:#x}"
    assert resp_tid == expected_tid, f"response tid {resp_tid} != {expected_tid}"
    assert params == [], params


def read_data_reply(sock, expected_op, expected_tid, keep_head=False):
    header = read_exact(sock, 12)
    length, typ, code, tid = struct.unpack("<IHHI", header)
    assert typ == 2, f"expected data frame, got type {typ}"
    assert code == expected_op, f"data code {code:#x} != {expected_op:#x}"
    assert tid == expected_tid, f"data tid {tid} != {expected_tid}"
    payload_len = length - 12
    remaining = payload_len
    head = bytearray()
    while remaining:
        chunk = sock.recv(min(1024 * 1024, remaining))
        if not chunk:
            raise EOFError(f"socket closed inside data frame with {remaining} bytes remaining")
        if keep_head and len(head) < 65536:
            take = min(65536 - len(head), len(chunk))
            head.extend(chunk[:take])
        remaining -= len(chunk)

    typ, resp, resp_tid, params = read_comp_frame(sock)
    assert typ == 3, f"expected response, got type {typ}"
    assert resp == 0x2001, f"expected OK, got {resp:#x}"
    assert resp_tid == expected_tid, f"response tid {resp_tid} != {expected_tid}"
    return payload_len, params, bytes(head)


def send_data_op(sock, code, tid, params, payload):
    send_op(sock, code, tid, params)
    sock.sendall(struct.pack("<IHHI", len(payload) + 12, 2, code, tid) + payload)
    read_ok(sock, tid)


def get_prop(sock, tid, prop):
    send_op(sock, 0x1015, tid, [prop])
    _, _, payload = read_data_reply(sock, 0x1015, tid, keep_head=True)
    return payload


def set_prop(sock, tid, prop, payload):
    send_data_op(sock, 0x1016, tid, [prop], payload)


def parse_u32_array(payload):
    count = struct.unpack_from("<I", payload, 0)[0]
    return list(struct.unpack_from("<" + "I" * count, payload, 4))


def make_sparse_mov(path):
    base = path.parents[3] / "base.MOV"
    subprocess.run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=1",
            "-t",
            "1",
            "-c:v",
            "mjpeg",
            "-q:v",
            "5",
            "-movflags",
            "+faststart",
            str(base),
        ],
        check=True,
    )
    data = base.read_bytes()
    base.unlink()
    path.write_bytes(data)
    remaining = TRUE_SIZE - len(data)
    if remaining < 16:
        raise RuntimeError("base MOV unexpectedly larger than target")
    with path.open("ab") as f:
        f.write(struct.pack(">I4sQ", 1, b"free", remaining))
        f.truncate(TRUE_SIZE)
    subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "default=nw=1:nk=1",
            str(path),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
    )


def main():
    with tempfile.TemporaryDirectory(prefix="ptpsim-large-mov-") as tmp:
        tmp = Path(tmp)
        card = tmp / "card" / "DCIM" / "100_FUJI"
        card.mkdir(parents=True)
        mov = card / "DSCF8476.MOV"
        make_sparse_mov(mov)
        assert mov.stat().st_size == TRUE_SIZE

        command, event, live, control = [free_port() for _ in range(4)]
        proc = subprocess.Popen(
            [
                "cargo",
                "run",
                "-q",
                "-p",
                "camera-sim-service",
                "--",
                "--manifest",
                str(ROOT / "packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml"),
                "--media-root",
                str(tmp / "card"),
                "--connection",
                "app",
                "--command-bind",
                f"127.0.0.1:{command}",
                "--event-bind",
                f"127.0.0.1:{event}",
                "--liveview-bind",
                f"127.0.0.1:{live}",
                "--control-bind",
                f"127.0.0.1:{control}",
            ],
            cwd=ROOT,
        )
        try:
            deadline = time.time() + 45
            while True:
                try:
                    sock = socket.create_connection(("127.0.0.1", command), timeout=1)
                    break
                except OSError:
                    if proc.poll() is not None:
                        raise RuntimeError(f"camera-sim-service exited with {proc.returncode}")
                    if time.time() > deadline:
                        raise TimeoutError("service did not start")
                    time.sleep(0.25)

            with sock:
                sock.settimeout(30)
                sock.sendall(std_init_request())
                ack = read_std_frame(sock)
                assert struct.unpack_from("<I", ack, 4)[0] == 2, "InitCommandAck expected"

                tid = 1
                send_op(sock, 0x1002, tid, [1])
                read_ok(sock, tid)

                # App image-import entry. The real GFX100 II app path does not
                # use 0x1007; it runs the vendor/bootstrap block, then reads
                # D620/D621 for count + handles before chunking with 0x101B.
                tid += 1
                get_prop(sock, tid, 0xD212)
                tid += 1
                set_prop(sock, tid, 0xDF01, struct.pack("<H", 0x14))
                tid += 1
                get_prop(sock, tid, 0xDF28)
                tid += 1
                set_prop(sock, tid, 0xDF28, struct.pack("<I", 3))
                tid += 1
                set_prop(sock, tid, 0xD226, struct.pack("<H", 0))
                tid += 1
                set_prop(sock, tid, 0xD227, struct.pack("<H", 0))
                tid += 1
                get_prop(sock, tid, 0xD244)
                tid += 1
                send_op(sock, 0x9054, tid, [0x10000001])
                read_ok(sock, tid)
                tid += 1
                send_op(sock, 0x9055, tid, [0x10000001])
                read_ok(sock, tid)
                tid += 1
                send_op(sock, 0x9050, tid, [])
                read_ok(sock, tid)
                tid += 1
                get_prop(sock, tid, 0xD212)
                tid += 1
                get_prop(sock, tid, 0xD22B)
                tid += 1
                send_op(sock, 0x9053, tid, [0, 0x7530])
                read_ok(sock, tid)
                tid += 1
                get_prop(sock, tid, 0xD212)

                tid += 1
                count_payload = get_prop(sock, tid, 0xD620)
                count = struct.unpack("<I", count_payload[:4])[0]
                assert count == 1, count
                tid += 1
                handles_payload = get_prop(sock, tid, 0xD621)
                handles = parse_u32_array(handles_payload)
                assert len(handles) == 1, handles
                handle = handles[0]

                tid += 1
                send_op(sock, 0x1008, tid, [handle])
                _, _, info = read_data_reply(sock, 0x1008, tid, keep_head=True)
                fmt = struct.unpack_from("<H", info, 4)[0]
                reported_size = struct.unpack_from("<I", info, 8)[0]
                assert fmt == 0x300D, hex(fmt)
                assert reported_size == 0xFFFFFFFF, hex(reported_size)

                tid += 1
                send_op(sock, 0x9803, tid, [handle, 0xDC04])
                _, _, true_size_payload = read_data_reply(sock, 0x9803, tid, keep_head=True)
                true_size = struct.unpack("<Q", true_size_payload[:8])[0]
                assert true_size == TRUE_SIZE, hex(true_size)

                tid += 1
                send_op(sock, 0x1015, tid, [0xD235])
                _, _, chunk_payload = read_data_reply(sock, 0x1015, tid, keep_head=True)
                chunk_size = struct.unpack("<I", chunk_payload[:4])[0]
                assert chunk_size == CHUNK_SIZE, hex(chunk_size)

                total = 0
                requests = 0
                saw_high = False
                first_head = b""
                while total < true_size:
                    want = min(chunk_size, true_size - total)
                    low = total & 0xFFFFFFFF
                    high = total >> 32
                    saw_high = saw_high or high == 1
                    requests += 1
                    tid += 1
                    send_op(sock, 0x101B, tid, [handle, low, want, high])
                    got, params, head = read_data_reply(sock, 0x101B, tid, keep_head=(total == 0))
                    assert got == want, (got, want, total)
                    assert params == [want], params
                    if total == 0:
                        first_head = head
                        assert b"ftyp" in first_head[:64], "first chunk lacks ftyp"
                        assert b"moov" in first_head, "first chunk lacks faststart moov"
                    total += got
                    if requests % 50 == 0:
                        print(f"downloaded {total}/{true_size} bytes in {requests} chunks", flush=True)

                assert requests == 389, requests
                assert saw_high, "never crossed high offset word"
                assert low == FINAL_LOW and high == FINAL_HIGH and want == FINAL_LEN, (
                    hex(low),
                    hex(high),
                    hex(want),
                )
                assert total == true_size
                print(f"large MOV acceptance OK: {total} bytes, {requests} chunks")
        finally:
            try:
                req = urllib.request.Request(f"http://127.0.0.1:{control}/shutdown", method="POST")
                urllib.request.urlopen(req, timeout=2).read()
            except Exception:
                pass
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.terminate()
                proc.wait(timeout=5)


if __name__ == "__main__":
    main()
PY
