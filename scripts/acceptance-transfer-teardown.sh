#!/usr/bin/env bash
# Exercise completed image-transfer teardown against the real simulator service.
#
# The fixture is intentionally bounded. It checks protocol ordering and the
# simulator's generic transport cleanup; it does not claim that a physical
# camera has completed its own graceful shutdown.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
for command in cargo python3; do
    command -v "$command" >/dev/null || {
        echo "error: $command is required" >&2
        exit 127
    }
done

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ptpsim-transfer-teardown.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT INT TERM

(cd "$ROOT" && cargo build --locked -q -p camera-sim-service)
PTPSIM_BIN="${PTPSIM_BIN:-$ROOT/target/debug/camera-sim-service}"
[[ -x "$PTPSIM_BIN" ]] || {
    echo "error: camera-sim-service binary not found at $PTPSIM_BIN" >&2
    exit 1
}

python3 - "$ROOT" "$PTPSIM_BIN" "$TMP_ROOT" <<'PY'
import json
import os
import socket
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(sys.argv[1])
PTPSIM_BIN = sys.argv[2]
TMP_ROOT = Path(sys.argv[3])
MANIFEST = ROOT / "packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml"
ARTIFACT_ROOT = os.environ.get("PTPSIM_TRANSFER_ARTIFACT_ROOT")
if ARTIFACT_ROOT:
    ARTIFACT_ROOT = Path(ARTIFACT_ROOT)
    ARTIFACT_ROOT.mkdir(parents=True, exist_ok=True)

TAIL_MARKER = b"PTPSIM-TEARDOWN-TAIL"
MOV_PREFIX = b"ftypqt  mov"
MIN_TRANSFER_SIZE = len(MOV_PREFIX) + len(TAIL_MARKER)
sizes_text = os.environ.get("PTPSIM_TRANSFER_SIZES")
if sizes_text:
    TRANSFER_SIZES = tuple(int(value) for value in sizes_text.split(","))
else:
    TRANSFER_SIZES = (int(os.environ.get("PTPSIM_TRANSFER_SIZE", 40 * 1024 * 1024 + 17)),)
if not TRANSFER_SIZES or not all(
    MIN_TRANSFER_SIZE <= size <= 64 * 1024 * 1024 * 1024 for size in TRANSFER_SIZES
):
    raise SystemExit("each transfer size must leave room for the MOV prefix and tail marker")
TRANSFER_TOTAL = sum(TRANSFER_SIZES)
if TRANSFER_TOTAL > 64 * 1024 * 1024 * 1024:
    raise SystemExit("the combined transfer size must not exceed 64 GiB")
CASE_NAMES = tuple(
    name for name in os.environ.get(
        "PTPSIM_TRANSFER_CASES", "orderly,rejection,timeout,abrupt,abort"
    ).split(",") if name
)
if not CASE_NAMES or any(
    name not in {"orderly", "rejection", "timeout", "abrupt", "abort"}
    for name in CASE_NAMES
):
    raise SystemExit("PTPSIM_TRANSFER_CASES contains an unknown case")
TRANSFER_OP = 0x101B
CLOSE_SESSION = 0x1003
OK = 0x2001
DEVICE_BUSY = 0x2019


def free_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def app_init_request():
    name = "ptpsim-teardown"
    name_field = name.encode("utf-16le") + b"\x00\x00"
    name_field += b"\x00" * (54 - len(name_field))
    body = bytes([0x42]) * 16 + bytes(4) + name_field
    assert len(body) == 74
    return struct.pack("<II", 82, 1) + body


def operation_frame(code, tid, params):
    body = struct.pack("<HHI", 1, code, tid)
    body += b"".join(struct.pack("<I", value) for value in params)
    return struct.pack("<I", len(body) + 4) + body


def read_exact(sock, size):
    chunks = []
    remaining = size
    while remaining:
        chunk = sock.recv(min(1024 * 1024, remaining))
        if not chunk:
            raise EOFError(f"socket closed with {remaining} bytes remaining")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_frame(sock):
    prefix = read_exact(sock, 4)
    length = struct.unpack("<I", prefix)[0]
    return prefix + read_exact(sock, length - 4)


def read_compressed(sock):
    frame = read_frame(sock)
    length, packet_type, code, tid = struct.unpack("<IHHI", frame[:12])
    assert length == len(frame), (length, len(frame))
    payload = frame[12:]
    if packet_type == 3:
        params = list(struct.unpack("<" + "I" * (len(payload) // 4), payload)) if payload else []
        return packet_type, code, tid, params
    return packet_type, code, tid, payload


def send_operation(sock, code, tid, params=()):
    sock.sendall(operation_frame(code, tid, params))


def read_response(sock, expected_tid):
    packet_type, code, tid, params = read_compressed(sock)
    assert packet_type == 3, (packet_type, code, tid)
    assert tid == expected_tid, (tid, expected_tid)
    return code, params


def read_data_reply(sock, expected_op, expected_tid, prefix_limit=0, suffix_limit=0):
    header = read_exact(sock, 12)
    length, packet_type, code, tid = struct.unpack("<IHHI", header)
    assert packet_type == 2, (packet_type, code, tid)
    assert code == expected_op, (code, expected_op)
    assert tid == expected_tid, (tid, expected_tid)
    remaining = length - 12
    prefix = bytearray()
    suffix = bytearray()
    while remaining:
        chunk = sock.recv(min(1024 * 1024, remaining))
        if not chunk:
            raise EOFError(f"socket closed inside data frame with {remaining} bytes remaining")
        if len(prefix) < prefix_limit:
            prefix.extend(chunk[: prefix_limit - len(prefix)])
        if suffix_limit:
            suffix.extend(chunk)
            del suffix[:-suffix_limit]
        remaining -= len(chunk)
    response, params = read_response(sock, expected_tid)
    assert response == OK, hex(response)
    return length - 12, params, bytes(prefix), bytes(suffix)


def get_property(sock, tid, code):
    send_operation(sock, 0x1015, tid, [code])
    _, _, payload, _ = read_data_reply(sock, 0x1015, tid, prefix_limit=4096)
    return payload


def set_property(sock, tid, code, value, width):
    send_operation(sock, 0x1016, tid, [code])
    payload = value.to_bytes(width, "little")
    sock.sendall(struct.pack("<IHHI", len(payload) + 12, 2, 0x1016, tid) + payload)
    response, params = read_response(sock, tid)
    assert response == OK, hex(response)
    assert params == [], params


def decode_u32_array(payload):
    count = struct.unpack_from("<I", payload, 0)[0]
    assert len(payload) == 4 + count * 4, (len(payload), count)
    return list(struct.unpack_from("<" + "I" * count, payload, 4))


def prepare_media(card):
    for index, size in enumerate(TRANSFER_SIZES, start=1):
        path = card / "DCIM/100_FUJI" / f"DSCF847{index}.MOV"
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("wb") as stream:
            stream.write(MOV_PREFIX)
            stream.truncate(size)
            stream.seek(size - len(TAIL_MARKER))
            stream.write(TAIL_MARKER)
        assert path.stat().st_size == size


def http_json(control, method, path, body=None):
    if (
        method == "POST"
        and path == "/shutdown"
        and os.environ.get("PTPSIM_ACCEPTANCE_TEST_SHUTDOWN") == "malformed-json"
    ):
        raise json.JSONDecodeError("simulated malformed shutdown response", "", 0)
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(
        f"http://127.0.0.1:{control}{path}",
        method=method,
        data=data,
        headers={"Content-Type": "application/json"} if data is not None else {},
    )
    with urllib.request.urlopen(request, timeout=2) as response:
        return json.loads(response.read())


def wait_health(control, process, log):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"simulator exited with {process.returncode}; see {log}")
        try:
            return http_json(control, "GET", "/healthz")
        except (OSError, urllib.error.URLError):
            time.sleep(0.05)
    raise TimeoutError(f"simulator did not become ready; see {log}")


def wait_state(control, predicate, description):
    deadline = time.monotonic() + 5
    latest = None
    while time.monotonic() < deadline:
        latest = http_json(control, "GET", "/state")
        if predicate(latest):
            return latest
        time.sleep(0.05)
    raise AssertionError(f"state did not reach {description}: {latest}")


def wait_trace(control, predicate, description):
    deadline = time.monotonic() + 5
    latest = None
    while time.monotonic() < deadline:
        latest = http_json(control, "GET", "/trace?after=0")
        if predicate(latest):
            return latest
        time.sleep(0.05)
    raise AssertionError(f"trace did not reach {description}: {latest}")


def command_endpoint(health):
    return health["bind"].rsplit(":", 1)


def complete_transfer(sock):
    sock.settimeout(10)
    sock.sendall(app_init_request())
    init_ack = read_frame(sock)
    assert struct.unpack_from("<I", init_ack, 4)[0] == 2, init_ack[:8]

    tid = 1
    send_operation(sock, 0x1002, tid, [1])
    code, params = read_response(sock, tid)
    assert code == OK and params == [], (hex(code), params)

    # GFX100 II app image-import entry, matching the existing large MOV
    # acceptance. These are manifest-backed setup operations, not new camera
    # behavior introduced by this fixture.
    setup = [
        ("get", 0xD212),
        ("set", 0xDF01, 0x14, 2),
        ("get", 0xDF28),
        ("set", 0xDF28, 3, 4),
        ("set", 0xD226, 0, 2),
        ("set", 0xD227, 0, 2),
        ("get", 0xD244),
    ]
    for item in setup:
        tid += 1
        if item[0] == "get":
            get_property(sock, tid, item[1])
        else:
            set_property(sock, tid, item[1], item[2], item[3])
    for op_code in (0x9054, 0x9055, 0x9050):
        tid += 1
        send_operation(sock, op_code, tid, [0x10000001] if op_code != 0x9050 else [])
        code, _ = read_response(sock, tid)
        assert code == OK, (hex(op_code), hex(code))
    for prop in (0xD212, 0xD22B):
        tid += 1
        get_property(sock, tid, prop)
    tid += 1
    send_operation(sock, 0x9053, tid, [0, 0x7530])
    code, _ = read_response(sock, tid)
    assert code == OK, hex(code)
    tid += 1
    get_property(sock, tid, 0xD212)

    tid += 1
    count_payload = get_property(sock, tid, 0xD620)
    count = struct.unpack_from("<I", count_payload, 0)[0]
    assert count == len(TRANSFER_SIZES), (count, len(TRANSFER_SIZES))
    tid += 1
    handles = decode_u32_array(get_property(sock, tid, 0xD621))
    assert len(handles) == len(TRANSFER_SIZES), handles
    chunks = 0
    for handle, transfer_size in zip(handles, TRANSFER_SIZES):
        tid += 1
        send_operation(sock, 0x1008, tid, [handle])
        object_info_len, _, object_info, _ = read_data_reply(sock, 0x1008, tid, prefix_limit=64)
        assert object_info_len >= 12
        reported_size = struct.unpack_from("<I", object_info, 8)[0]
        assert reported_size == min(transfer_size, 0xFFFFFFFF), (reported_size, transfer_size)

        tid += 1
        chunk_payload = get_property(sock, tid, 0xD235)
        chunk_size = struct.unpack_from("<I", chunk_payload, 0)[0]
        assert chunk_size > 0

        total = 0
        first = b""
        last = b""
        while total < transfer_size:
            want = min(chunk_size, transfer_size - total)
            tid += 1
            send_operation(sock, TRANSFER_OP, tid, [handle, total & 0xFFFFFFFF, want, total >> 32])
            got, params, prefix, suffix = read_data_reply(
                sock,
                TRANSFER_OP,
                tid,
                prefix_limit=16 if total == 0 else 0,
                suffix_limit=len(TAIL_MARKER) if total + want == transfer_size else 0,
            )
            assert got == want and params == [want], (got, want, params)
            if total == 0:
                first = prefix
            if total + want == transfer_size:
                last = suffix
            total += got
            chunks += 1
        assert total == transfer_size
        assert first.startswith(b"ftyp"), first
        assert TAIL_MARKER in last, last
    return tid, chunks


def assert_trace(trace, case, close_tid=None):
    events = trace["events"]
    close_events = [event for event in events if event["kind"] == "ptpip.close_session"]
    end_events = [event for event in events if event["kind"] == "ptpip.command.closed"]
    fault_events = [event for event in events if event["kind"] == "ptpip.fault.applied"]
    assert end_events, (case, events)
    end = end_events[-1]
    if case == "orderly":
        assert close_events, events
        close = close_events[-1]
        assert close["operation"] == "0x1003"
        assert close["transaction_id"] == close_tid
        assert close["response_code"] == "0x2001"
        assert close["outcome"] == "ok"
        assert close["sequence"] < end["sequence"]
        assert end["outcome"] == "peerClosedAfterCloseSession"
        assert not fault_events, fault_events
    elif case == "rejection":
        assert len(close_events) == 1, events
        close = close_events[0]
        assert close["response_code"] == "0x2019"
        assert close["outcome"] == "nonOk"
        assert end["outcome"] == "transportLost"
        assert len(fault_events) == 1, fault_events
        assert fault_events[0]["operation"] == "0x1003"
        assert fault_events[0]["fault_kind"] == "failResponse"
        assert fault_events[0]["response_code"] == "0x2019"
    elif case == "timeout":
        assert len(close_events) == 1, events
        close = close_events[0]
        assert close.get("response_code") is None
        assert close["outcome"] == "timeout"
        assert end["outcome"] == "transportLost"
        assert len(fault_events) == 1, fault_events
        assert fault_events[0]["operation"] == "0x1003"
        assert fault_events[0]["fault_kind"] == "suppress"
        assert fault_events[0].get("response_code") is None
    elif case == "abrupt":
        assert not close_events, events
        assert end["outcome"] == "transportLost"
        assert not fault_events, fault_events
    elif case == "abort":
        assert len(close_events) == 1, events
        close = close_events[0]
        assert close.get("response_code") is None
        assert close["outcome"] == "transportAbort"
        assert end["outcome"] == "serverAborted"
        assert len(fault_events) == 1, fault_events
        assert fault_events[0]["operation"] == "0x1003"
        assert fault_events[0]["fault_kind"] == "close"
        assert fault_events[0].get("response_code") is None
    else:
        raise AssertionError(case)


def run_case(case):
    case_root = (ARTIFACT_ROOT / case) if ARTIFACT_ROOT else (TMP_ROOT / case)
    if ARTIFACT_ROOT and case_root.exists():
        raise SystemExit(
            f"artifact case directory already exists: {case_root}; "
            "choose another PTPSIM_TRANSFER_ARTIFACT_ROOT"
        )
    card = case_root / "card"
    card.mkdir(parents=True)
    prepare_media(card)
    control = free_port()
    log = case_root / "camera-sim-service.log"
    log_stream = log.open("wb")
    process = subprocess.Popen(
        [
            PTPSIM_BIN,
            "--instance-id",
            f"transfer-teardown-{case}",
            "--manifest",
            str(MANIFEST),
            "--media-root",
            str(card),
            "--connection",
            "app",
            "--command-bind",
            "127.0.0.1:0",
            "--event-bind",
            "127.0.0.1:0",
            "--liveview-bind",
            "127.0.0.1:0",
            "--control-bind",
            f"127.0.0.1:{control}",
        ],
        cwd=ROOT,
        stdout=log_stream,
        stderr=subprocess.STDOUT,
    )
    try:
        health = wait_health(control, process, log)
        host, port = command_endpoint(health)
        sock = socket.create_connection((host, int(port)), timeout=5)
        try:
            transfer_tid, chunks = complete_transfer(sock)
            state = wait_state(
                control,
                lambda value: value["session_open"] is True
                and value["phase"] == "imageImport",
                "completed transfer session",
            )
            assert state["session_open"] is True, state
            assert state["phase"] == "imageImport", state

            expected_fault = {
                "rejection": {
                    "operation": "0x1003",
                    "mutation": {"type": "failResponse", "response": "0x2019"},
                },
                "timeout": {
                    "operation": "0x1003",
                    "mutation": {"type": "suppress", "stage": "response"},
                },
                "abort": {
                    "operation": "0x1003",
                    "mutation": {"type": "close", "stage": "command"},
                },
            }.get(case)
            if expected_fault:
                fault_result = http_json(control, "POST", "/faults", expected_fault)
                assert fault_result["ok"] is True, fault_result

            close_tid = transfer_tid + 1
            if case == "orderly":
                send_operation(sock, CLOSE_SESSION, close_tid)
                response, params = read_response(sock, close_tid)
                assert response == OK and params == [], (hex(response), params)
                sock.close()
            elif case == "rejection":
                send_operation(sock, CLOSE_SESSION, close_tid)
                response, params = read_response(sock, close_tid)
                assert response == DEVICE_BUSY and params == [], (hex(response), params)
                state = http_json(control, "GET", "/state")
                assert state["session_open"] is True, state
                assert state["phase"] == "imageImport", state
                sock.close()
            elif case == "timeout":
                sock.settimeout(0.5)
                send_operation(sock, CLOSE_SESSION, close_tid)
                try:
                    read_response(sock, close_tid)
                except socket.timeout:
                    pass
                else:
                    raise AssertionError("suppressed CloseSession unexpectedly returned")
                state = wait_state(control, lambda value: value["session_open"] is False, "camera-side close after suppressed response")
                assert state["phase"] == "closed", state
                sock.close()
            elif case == "abrupt":
                sock.close()
            elif case == "abort":
                send_operation(sock, CLOSE_SESSION, close_tid)
                try:
                    read_response(sock, close_tid)
                except EOFError:
                    pass
                else:
                    raise AssertionError("command close fault unexpectedly returned a response")
                sock.close()
            else:
                raise AssertionError(case)
            wait_state(control, lambda value: value["session_open"] is False, "transport cleanup")
        finally:
            try:
                sock.close()
            except OSError:
                pass

        trace = wait_trace(
            control,
            lambda value: any(event["kind"] == "ptpip.command.closed" for event in value["events"]),
            "command transport end",
        )
        assert_trace(trace, case, close_tid if case != "abrupt" else None)
        result = {"case": case, "bytes": TRANSFER_TOTAL, "chunks": chunks}
        RESULTS.append(result)
        if ARTIFACT_ROOT:
            (ARTIFACT_ROOT / "results.json").write_text(
                json.dumps({"status": "running", "cases": RESULTS}, indent=2) + "\n"
            )
        print(f"{case}: OK ({TRANSFER_TOTAL} bytes, {chunks} chunks)")
    finally:
        try:
            http_json(control, "POST", "/shutdown", {})
        except Exception:
            pass
        finally:
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            finally:
                log_stream.close()


RESULTS = []
for case_name in CASE_NAMES:
    run_case(case_name)
if ARTIFACT_ROOT:
    (ARTIFACT_ROOT / "results.json").write_text(
        json.dumps({"status": "passed", "cases": RESULTS}, indent=2) + "\n"
    )
print("transfer teardown acceptance passed")
PY
