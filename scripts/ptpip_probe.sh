#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ptpip_probe.sh [options]

Options:
  --host IP             Camera AP endpoint. Default: 192.168.0.1.
  --port PORT           Camera PTP/IP port. Default: 55740.
  --friendly-name NAME  PTP/IP InitiatorFriendlyName. Default: project
                        reference app-shaped host token.
  --wifi-iface IFACE    Wi-Fi interface expected to route to the camera.
                        Default: detected from networksetup.
  --tail-profile NAME   Init tail profile: liveview, get, or zeros.
                        Default: liveview.
  --init-payload PATH   Send an exact captured InitCommandRequest packet
                        instead of generating one.
  --open-session        After InitCommandAck, send raw PTP OpenSession
                        transaction 1 with session id 1.
  --get-prop HEX        After OpenSession, send PTP GetDevicePropValue for
                        the given property, for example 0xd212.
  --timeout SEC         Socket timeout. Default: 5.
  --connect-only        Only test TCP connect; do not send Init_Command_Request.
  -h, --help            Show this help.

This probes the camera-side AP socket. It records route evidence, opens TCP to
the camera endpoint, and by default sends an reference app-shaped 82-byte PTP/IP
InitCommandRequest with a fixed UTF-16LE InitiatorFriendlyName field and the
observed reference app live-view tail. No Wi-Fi passphrase is read or logged.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
host="${FUJI_CAMERA_AP_TARGET_IP:-192.168.0.1}"
port="${FUJI_CAMERA_PTPIP_PORT:-55740}"
friendly_name="${FUJI_DEVICE_NAME:-}"
wifi_iface="${FUJI_WIFI_INTERFACE:-}"
tail_profile="${FUJI_PTPIP_TAIL_PROFILE:-liveview}"
init_payload="${FUJI_PTPIP_INIT_PAYLOAD:-}"
timeout="${FUJI_PTPIP_TIMEOUT:-5}"
connect_only=0
open_session=0
get_prop=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host)
      host="$2"
      shift 2
      ;;
    --port)
      port="$2"
      shift 2
      ;;
    --friendly-name)
      friendly_name="$2"
      shift 2
      ;;
    --wifi-iface)
      wifi_iface="$2"
      shift 2
      ;;
    --tail-profile)
      tail_profile="$2"
      shift 2
      ;;
    --init-payload)
      init_payload="$2"
      shift 2
      ;;
    --timeout)
      timeout="$2"
      shift 2
      ;;
    --connect-only)
      connect_only=1
      shift
      ;;
    --open-session)
      open_session=1
      shift
      ;;
    --get-prop)
      get_prop="$2"
      open_session=1
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test]'" >&2
  exit 1
fi

if [[ -z "$friendly_name" ]]; then
  friendly_name="$("$python_bin" -c 'from rce.tools.fuji_ble_gps.device_identity import default_device_name; print(default_device_name())')"
fi

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

mkdir -p "$session_dir"
log_file="$session_dir/probe.log"

log() {
  printf '%s\n' "$*" | tee -a "$log_file" >&2
}

capture() {
  local label="$1"
  shift
  "$@" >"$session_dir/$label.txt" 2>&1 || true
}

route_interface() {
  /sbin/route -n get "$1" 2>/dev/null | awk '/interface:/{print $2; exit}'
}

if [[ -z "$wifi_iface" ]]; then
  capture networksetup_hardware_ports networksetup -listallhardwareports
  wifi_iface="$(
    networksetup -listallhardwareports |
      awk '
        /^Hardware Port: (Wi-Fi|AirPort)$/ {want=1; next}
        want && /^Device: / {print $2; exit}
      '
  )"
fi

capture route_default /sbin/route -n get default
capture route_internet /sbin/route -n get 1.1.1.1
capture route_camera /sbin/route -n get "$host"
capture ifconfig_en0 ifconfig en0

camera_route_iface="$(route_interface "$host")"
internet_route_iface="$(route_interface 1.1.1.1)"

log "session=$session_dir"
log "target=$host:$port"
log "friendly_name=$friendly_name"
log "wifi_interface=$wifi_iface"
log "camera_route=$camera_route_iface"
log "internet_route=$internet_route_iface"

if [[ -z "$wifi_iface" || "$camera_route_iface" != "$wifi_iface" ]]; then
  "$python_bin" -c 'import json, sys
data = {
  "host": sys.argv[2],
  "port": int(sys.argv[3]),
  "friendly_name": sys.argv[4],
  "tcp_connect": "not_attempted",
  "init_sent": False,
  "response_present": False,
  "open_session_sent": False,
  "open_session_response_present": False,
  "get_prop_sent": False,
  "get_prop_response_present": False,
  "route_check": "failed",
  "wifi_interface": sys.argv[5],
  "camera_route": sys.argv[6],
  "internet_route": sys.argv[7],
  "error": "camera endpoint route is not on Wi-Fi",
}
open(sys.argv[1], "w", encoding="utf-8").write(json.dumps(data, indent=2, sort_keys=True) + "\n")
' "$session_dir/summary.json" "$host" "$port" "$friendly_name" "$wifi_iface" "$camera_route_iface" "$internet_route_iface"
  log "error=camera endpoint route is not on Wi-Fi"
  log "summary=$session_dir/summary.json"
  exit 3
fi

set +e
"$python_bin" - "$session_dir" "$host" "$port" "$friendly_name" "$timeout" "$connect_only" "$tail_profile" "$init_payload" "$open_session" "$get_prop" <<'PY'
from __future__ import annotations

from pathlib import Path
import json
import socket
import struct
import sys
import time
import uuid


TAIL_PROFILES = {
    "liveview": bytes.fromhex("8d002c0000000000000000000000fa0005003d000000000000000000"),
    "get": bytes.fromhex("92004700000000000000000000002f00870059000000000000000000"),
    "zeros": b"\x00" * 28,
}


def build_init_command_request(friendly_name: str, tail_profile: str) -> bytes:
    if tail_profile not in TAIL_PROFILES:
        raise ValueError(f"unknown tail profile {tail_profile!r}")
    tail = TAIL_PROFILES[tail_profile]
    if len(tail) != 28:
        raise ValueError(f"tail profile {tail_profile!r} has length {len(tail)}, expected 28")
    guid = uuid.uuid4().bytes
    name_utf16 = friendly_name.encode("utf-16le")
    name_field = (name_utf16 + b"\x00\x00")[:26].ljust(26, b"\x00")
    payload = guid + b"\x00\x00\x00\x00" + name_field + tail
    return struct.pack("<II", len(payload) + 8, 1) + payload


def packet_header(data: bytes) -> dict[str, int | str]:
    if len(data) < 8:
        return {"length": len(data), "packet_type": "short"}
    length, packet_type = struct.unpack("<II", data[:8])
    return {"length": length, "packet_type": packet_type}


def ptp_header(data: bytes) -> dict[str, int | str]:
    if len(data) < 12:
        return {"length": len(data), "container_type": "short"}
    length, container_type, code, transaction_id = struct.unpack("<IHHI", data[:12])
    return {
        "length": length,
        "container_type": container_type,
        "code": code,
        "transaction_id": transaction_id,
    }


def read_exact(sock: socket.socket, size: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < size:
        chunk = sock.recv(size - len(chunks))
        if not chunk:
            break
        chunks.extend(chunk)
    return bytes(chunks)


def recv_packet(sock: socket.socket) -> bytes:
    header = read_exact(sock, 4)
    if not header:
        return b""
    if len(header) != 4:
        raise RuntimeError(f"short packet length header: {len(header)} bytes")
    (length,) = struct.unpack("<I", header)
    if length < 8 or length > 1024 * 1024:
        raise RuntimeError(f"invalid packet length: {length}")
    return header + read_exact(sock, length - 4)


def build_open_session() -> bytes:
    return struct.pack("<IHHII", 16, 1, 0x1002, 1, 1)


def parse_u16_or_hex(value: str) -> int:
    parsed = int(value, 16 if value.lower().startswith("0x") else 10)
    if not 0 <= parsed <= 0xFFFF:
        raise ValueError(f"property out of uint16 range: {value!r}")
    return parsed


def build_get_device_prop_value(prop: int) -> bytes:
    return struct.pack("<IHHII", 16, 1, 0x1015, 2, prop)


session_dir = Path(sys.argv[1])
host = sys.argv[2]
port = int(sys.argv[3])
friendly_name = sys.argv[4]
timeout = float(sys.argv[5])
connect_only = sys.argv[6] == "1"
tail_profile = sys.argv[7]
init_payload = sys.argv[8]
open_session = sys.argv[9] == "1"
get_prop = sys.argv[10]

summary: dict[str, object] = {
    "host": host,
    "port": port,
    "friendly_name": friendly_name,
    "tcp_connect": "absent",
    "init_sent": False,
    "response_present": False,
    "open_session_sent": False,
    "open_session_response_present": False,
    "get_prop": get_prop,
    "get_prop_sent": False,
    "get_prop_response_present": False,
    "route_check": "passed",
    "tail_profile": tail_profile,
    "init_payload": init_payload,
}

start = time.monotonic()
try:
    with socket.create_connection((host, port), timeout=timeout) as sock:
        sock.settimeout(timeout)
        summary["tcp_connect"] = "present"
        summary["connect_elapsed_ms"] = round((time.monotonic() - start) * 1000)
        if not connect_only:
            if init_payload:
                request = Path(init_payload).read_bytes()
            else:
                request = build_init_command_request(friendly_name, tail_profile)
            (session_dir / "init_command_request.bin").write_bytes(request)
            sock.sendall(request)
            summary["init_sent"] = True
            try:
                response = recv_packet(sock)
            except socket.timeout:
                response = b""
                summary["response_error"] = "timeout"
            if response:
                (session_dir / "init_command_response.bin").write_bytes(response)
                summary["response_present"] = True
                summary["response_bytes"] = len(response)
                summary["response_header"] = packet_header(response)
                if open_session:
                    open_request = build_open_session()
                    (session_dir / "open_session_request.bin").write_bytes(open_request)
                    sock.sendall(open_request)
                    summary["open_session_sent"] = True
                    try:
                        open_response = recv_packet(sock)
                    except socket.timeout:
                        open_response = b""
                        summary["open_session_response_error"] = "timeout"
                    if open_response:
                        (session_dir / "open_session_response.bin").write_bytes(open_response)
                        summary["open_session_response_present"] = True
                        summary["open_session_response_bytes"] = len(open_response)
                        summary["open_session_response_header"] = ptp_header(open_response)
                        if get_prop:
                            prop = parse_u16_or_hex(get_prop)
                            get_prop_request = build_get_device_prop_value(prop)
                            (session_dir / "get_prop_request.bin").write_bytes(get_prop_request)
                            sock.sendall(get_prop_request)
                            summary["get_prop_sent"] = True
                            try:
                                get_prop_data = recv_packet(sock)
                            except socket.timeout:
                                get_prop_data = b""
                                summary["get_prop_data_error"] = "timeout"
                            if get_prop_data:
                                (session_dir / "get_prop_data.bin").write_bytes(get_prop_data)
                                summary["get_prop_data_present"] = True
                                summary["get_prop_data_bytes"] = len(get_prop_data)
                                summary["get_prop_data_header"] = ptp_header(get_prop_data)
                            try:
                                get_prop_response = recv_packet(sock)
                            except socket.timeout:
                                get_prop_response = b""
                                summary["get_prop_response_error"] = "timeout"
                            if get_prop_response:
                                (session_dir / "get_prop_response.bin").write_bytes(get_prop_response)
                                summary["get_prop_response_present"] = True
                                summary["get_prop_response_bytes"] = len(get_prop_response)
                                summary["get_prop_response_header"] = ptp_header(get_prop_response)
except OSError as exc:
    summary["error"] = repr(exc)

(session_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
print(json.dumps(summary, sort_keys=True))
if summary["tcp_connect"] != "present":
    raise SystemExit(1)
if not connect_only and not summary["response_present"]:
    raise SystemExit(2)
if open_session and not summary["open_session_response_present"]:
    raise SystemExit(4)
if get_prop and not summary["get_prop_response_present"]:
    raise SystemExit(5)
PY
rc=$?
set -e

if [[ -f "$session_dir/summary.json" ]]; then
  "$python_bin" -c 'import json, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
print("tcp_connect=" + str(data.get("tcp_connect", "")))
print("init_sent=" + str(data.get("init_sent", "")))
print("response_present=" + str(data.get("response_present", "")))
if data.get("response_header"):
    print("response_header=" + json.dumps(data["response_header"], sort_keys=True))
print("open_session_sent=" + str(data.get("open_session_sent", "")))
print("open_session_response_present=" + str(data.get("open_session_response_present", "")))
if data.get("open_session_response_header"):
    print("open_session_response_header=" + json.dumps(data["open_session_response_header"], sort_keys=True))
print("get_prop_sent=" + str(data.get("get_prop_sent", "")))
print("get_prop_response_present=" + str(data.get("get_prop_response_present", "")))
if data.get("get_prop_data_header"):
    print("get_prop_data_header=" + json.dumps(data["get_prop_data_header"], sort_keys=True))
if data.get("get_prop_response_header"):
    print("get_prop_response_header=" + json.dumps(data["get_prop_response_header"], sort_keys=True))
if data.get("error"):
    print("error=" + str(data["error"]))
' "$session_dir/summary.json" | tee -a "$log_file" >&2
fi

log "summary=$session_dir/summary.json"
exit "$rc"
