#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ptpip_firmware_update.sh --dat PATH [options]

Options:
  --dat PATH              Firmware DAT to send.
  --execute               Actually open PTP/IP and upload. Default is dry-run.
  --host IP               Camera AP endpoint. Default: 192.168.0.1.
  --port PORT             Camera PTP/IP port. Default: 55740.
  --friendly-name NAME    PTP/IP InitiatorFriendlyName. Default: project token.
  --wifi-iface IFACE      Wi-Fi interface expected to route to camera.
  --guid HEX              16-byte GUID for generated InitCommandRequest.
  --init-payload PATH     Exact InitCommandRequest to replay.
  --tail-profile NAME     Init tail profile. Default: liveview.
  --transfer-file-name N  Firmware object name sent over PTP. Default: FUP_FILE.DAT.
  --chunk-size BYTES      Chunk size. Default: 0x100000.
  --timeout SEC           Socket timeout. Default: 30.
  -h, --help              Show this help.

This is the PTP/IP upload half of firmware update. Run the BLE prepare half
first so the camera is in firmware receive mode and its AP is up. Dry-run mode
only builds the upload plan and artifacts; use --execute for the destructive
camera write.
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
transfer_file_name="${FUJI_FIRMWARE_TRANSFER_FILE_NAME:-FUP_FILE.DAT}"
chunk_size="${FUJI_FIRMWARE_CHUNK_SIZE:-0x100000}"
timeout="${FUJI_PTPIP_TIMEOUT:-30}"
dat_path=""
execute=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dat)
      dat_path="$2"
      shift 2
      ;;
    --execute)
      execute=1
      shift
      ;;
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
    --guid)
      ptpip_guid="$2"
      shift 2
      ;;
    --init-payload)
      init_payload="$2"
      shift 2
      ;;
    --tail-profile)
      tail_profile="$2"
      shift 2
      ;;
    --transfer-file-name)
      transfer_file_name="$2"
      shift 2
      ;;
    --chunk-size)
      chunk_size="$2"
      shift 2
      ;;
    --timeout)
      timeout="$2"
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

if [[ -z "$dat_path" ]]; then
  echo "missing required --dat PATH" >&2
  usage >&2
  exit 2
fi

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test]'" >&2
  exit 1
fi

if [[ ! -f "$dat_path" ]]; then
  echo "DAT file not found: $dat_path" >&2
  exit 1
fi

if [[ -z "$friendly_name" ]]; then
  friendly_name="$("$python_bin" -c 'from rce.tools.fuji_ble_gps.device_identity import default_device_name; print(default_device_name())')"
fi

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

mkdir -p "$session_dir"
log_file="$session_dir/firmware_update.log"

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

camera_route_iface="$(route_interface "$host")"
internet_route_iface="$(route_interface 1.1.1.1)"

log "session=$session_dir"
log "target=$host:$port"
log "friendly_name=$friendly_name"
log "wifi_interface=$wifi_iface"
log "camera_route=$camera_route_iface"
log "internet_route=$internet_route_iface"

if [[ "$execute" == "1" ]]; then
  if [[ -z "$wifi_iface" || "$camera_route_iface" != "$wifi_iface" ]]; then
    log "error=camera endpoint route is not on Wi-Fi"
    exit 3
  fi
else
  log "dry_run=true"
fi

ptpip_args=(
  firmware-upload
  --session-dir "$session_dir"
  --dat "$dat_path"
  --host "$host"
  --port "$port"
  --friendly-name "$friendly_name"
  --timeout "$timeout"
  --tail-profile "$tail_profile"
  --transfer-file-name "$transfer_file_name"
  --chunk-size "$chunk_size"
)

if [[ "$execute" != "1" ]]; then
  ptpip_args+=(--dry-run)
fi
if [[ -n "$init_payload" ]]; then
  ptpip_args+=(--init-payload "$init_payload")
fi
if [[ -n "$ptpip_guid" ]]; then
  ptpip_args+=(--guid "$ptpip_guid")
fi

log "+ $python_bin -m rce.tools.fuji_ble_gps.ptpip ${ptpip_args[*]}"
set +e
"$python_bin" -m rce.tools.fuji_ble_gps.ptpip "${ptpip_args[@]}" 2>&1 | tee -a "$log_file"
rc=${PIPESTATUS[0]}
set -e

if [[ -f "$session_dir/summary.json" ]]; then
  "$python_bin" -c 'import json, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
for key in (
    "dry_run",
    "dat_size",
    "dat_sha256",
    "chunk_count",
    "last_chunk_length",
    "tcp_connect",
    "response_present",
    "open_session_response_present",
    "firmware_setup_completed",
    "firmware_object_info_sent",
    "firmware_chunks_sent",
    "firmware_bytes_sent",
    "firmware_upload_completed",
    "close_session_response_present",
    "error",
):
    if key in data:
        print(f"{key}={data[key]}")
' "$session_dir/summary.json" | tee -a "$log_file" >&2
fi

log "summary=$session_dir/summary.json"
exit "$rc"
