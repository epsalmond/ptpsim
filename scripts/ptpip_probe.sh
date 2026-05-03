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
  --guid HEX            16-byte GUID for generated InitCommandRequest packets.
                        Ignored when --init-payload is used.
  --init-payload PATH   Send an exact captured InitCommandRequest packet
                        instead of generating one.
  --open-session        After InitCommandAck, send raw PTP OpenSession
                        transaction 1 with session id 1.
  --get-prop HEX        After OpenSession, send PTP GetDevicePropValue for
                        the given property, for example 0xd212.
  --app-sequence NAME  After OpenSession, run a named observed reference app PTP
                        sequence. Current: sdcard-browse-bootstrap,
                        sdcard-current-object-info.
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
ptpip_guid="${FUJI_PTPIP_GUID:-}"
init_payload="${FUJI_PTPIP_INIT_PAYLOAD:-}"
timeout="${FUJI_PTPIP_TIMEOUT:-5}"
connect_only=0
open_session=0
get_prop=""
app_sequence=""

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
    --guid)
      ptpip_guid="$2"
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
    --app-sequence)
      app_sequence="$2"
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

ptpip_args=(
  probe
  --session-dir "$session_dir"
  --host "$host"
  --port "$port"
  --friendly-name "$friendly_name"
  --timeout "$timeout"
  --tail-profile "$tail_profile"
)

if [[ "$connect_only" == "1" ]]; then
  ptpip_args+=(--connect-only)
fi
if [[ -n "$init_payload" ]]; then
  ptpip_args+=(--init-payload "$init_payload")
fi
if [[ -n "$ptpip_guid" ]]; then
  ptpip_args+=(--guid "$ptpip_guid")
fi
if [[ "$open_session" == "1" ]]; then
  ptpip_args+=(--open-session)
fi
if [[ -n "$get_prop" ]]; then
  ptpip_args+=(--get-prop "$get_prop")
fi
if [[ -n "$app_sequence" ]]; then
  ptpip_args+=(--app-sequence "$app_sequence")
fi

log "+ $python_bin -m rce.tools.fuji_ble_gps.ptpip ${ptpip_args[*]}"
set +e
"$python_bin" -m rce.tools.fuji_ble_gps.ptpip "${ptpip_args[@]}" 2>&1 | tee -a "$log_file"
rc=${PIPESTATUS[0]}
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
if data.get("app_sequence"):
    print("app_sequence=" + str(data.get("app_sequence", "")))
    print("app_sequence_completed=" + str(data.get("app_sequence_completed", "")))
    print("app_sequence_steps=" + str(len(data.get("app_sequence_steps", []))))
if data.get("error"):
    print("error=" + str(data["error"]))
' "$session_dir/summary.json" | tee -a "$log_file" >&2
fi

log "summary=$session_dir/summary.json"
exit "$rc"
