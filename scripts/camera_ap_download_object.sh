#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/camera_ap_download_object.sh --handle HANDLE [options]

Options:
  --handle H            PTP object handle to download, for example 0x0000000c.
  --device-name NAME    Laptop/app name for BLE registration. Default: project host token.
  --address ADDRESS     Explicit CoreBluetooth UUID/address for BLE AP prep.
  --output-dir PATH     Destination directory. Default: rce/downloads/camera_ap_download_<timestamp>.
  --filename NAME       Override exported JPEG filename.
  --force               Overwrite an existing exported file and manifest.
  --timeout SEC         BLE scan/connect timeout. Default: 45.
  --ap-state-timeout S  Seconds to wait for AP launched state. Default: 15.
  --wifi-timeout SEC    Seconds to wait for Wi-Fi IP. Default: 20.
  --ptpip-timeout SEC   PTP/IP socket timeout. Default: 5.
  --ptpip-guid HEX      16-byte GUID for generated InitCommandRequest packets.
                         Default: accepted reference app GUID currently used for live tests.
  --ptpip-friendly-name N
                         PTP/IP InitiatorFriendlyName. Default: --device-name.
  --temporary-wifi-internet
                         Allow Wi-Fi to join the camera AP, then restore internet Wi-Fi.
  --restore-wifi-ssid S SSID to restore after --temporary-wifi-internet.
  --no-screen-read      Do not run camera LCD classification at flow transition points.
  --screen-device NAME  Camera capture device. Default: iPhone.
  --screen-warmup SEC   Camera capture warmup. Default: 2.
  --screen-zoom VALUE   Camera capture center-crop zoom. Default: 2.
  -h, --help            Show this help.

Runs the deterministic AP/PTP flow, requests the object handle with standard
GetObjectInfo and GetObject, then validates and exports the complete JPEG
payload. If the flow fails, export is not attempted; use the preserved session
artifacts to inspect evidence first.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
handle=""
device_name="${FUJI_DEVICE_NAME:-}"
camera_address="${FUJI_CAMERA_ADDRESS:-}"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

filename=""
force=0
ble_timeout="${FUJI_BLE_TIMEOUT:-45}"
ap_state_timeout="${FUJI_CAMERA_AP_STATE_TIMEOUT:-15}"
wifi_timeout="${FUJI_WIFI_TIMEOUT:-20}"
ptpip_timeout="${FUJI_PTPIP_TIMEOUT:-5}"
ptpip_guid="${FUJI_PTPIP_GUID:-f2e4538fada5485d87b27f0bd3d5ded0}"
ptpip_friendly_name="${FUJI_PTPIP_FRIENDLY_NAME:-}"
temporary_wifi_internet="${FUJI_TEMPORARY_WIFI_INTERNET:-0}"
restore_wifi_ssid="${FUJI_RESTORE_WIFI_SSID:-}"
screen_read=1
screen_device="${FUJI_SCREEN_DEVICE_NAME:-iPhone}"
screen_warmup="${FUJI_SCREEN_WARMUP:-2}"
screen_zoom="${FUJI_SCREEN_ZOOM:-2}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --handle)
      handle="$2"
      shift 2
      ;;
    --device-name)
      device_name="$2"
      shift 2
      ;;
    --address)
      camera_address="$2"
      shift 2
      ;;
    --output-dir)
      output_dir="$2"
      shift 2
      ;;
    --filename)
      filename="$2"
      shift 2
      ;;
    --force)
      force=1
      shift
      ;;
    --timeout)
      ble_timeout="$2"
      shift 2
      ;;
    --ap-state-timeout)
      ap_state_timeout="$2"
      shift 2
      ;;
    --wifi-timeout)
      wifi_timeout="$2"
      shift 2
      ;;
    --ptpip-timeout)
      ptpip_timeout="$2"
      shift 2
      ;;
    --ptpip-guid)
      ptpip_guid="$2"
      shift 2
      ;;
    --ptpip-friendly-name)
      ptpip_friendly_name="$2"
      shift 2
      ;;
    --temporary-wifi-internet)
      temporary_wifi_internet=1
      shift
      ;;
    --restore-wifi-ssid)
      restore_wifi_ssid="$2"
      shift 2
      ;;
    --no-screen-read)
      screen_read=0
      shift
      ;;
    --screen-device)
      screen_device="$2"
      shift 2
      ;;
    --screen-warmup)
      screen_warmup="$2"
      shift 2
      ;;
    --screen-zoom)
      screen_zoom="$2"
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

if [[ -z "$handle" ]]; then
  echo "missing --handle" >&2
  usage >&2
  exit 2
fi

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test]'" >&2
  exit 1
fi

if [[ -z "$device_name" ]]; then
  device_name="$("$python_bin" -c 'from rce.tools.fuji_ble_gps.device_identity import default_device_name; print(default_device_name())')"
fi
if [[ -z "$ptpip_friendly_name" ]]; then
  ptpip_friendly_name="$device_name"
fi


mkdir -p "$run_dir"
run_log="$run_dir/download.log"

log() {
  printf '%s\n' "$*" | tee -a "$run_log" >&2
}

flow_args=(
  --device-name "$device_name"
  --timeout "$ble_timeout"
  --ap-state-timeout "$ap_state_timeout"
  --wifi-timeout "$wifi_timeout"
  --ptpip-timeout "$ptpip_timeout"
  --ptpip-friendly-name "$ptpip_friendly_name"
  --ptpip-guid "$ptpip_guid"
  --ptpip-app-sequence sdcard-object-handles
  --ptpip-get-object-info "$handle"
  --ptpip-get-object "$handle"
  --screen-device "$screen_device"
  --screen-warmup "$screen_warmup"
  --screen-zoom "$screen_zoom"
)
if [[ -n "$camera_address" ]]; then
  flow_args+=(--address "$camera_address")
fi
if [[ "$temporary_wifi_internet" == "1" ]]; then
  flow_args+=(--temporary-wifi-internet)
fi
if [[ -n "$restore_wifi_ssid" ]]; then
  flow_args+=(--restore-wifi-ssid "$restore_wifi_ssid")
fi
if [[ "$screen_read" != "1" ]]; then
  flow_args+=(--no-screen-read)
fi

log "session=$run_dir"
log "download_output_dir=$output_dir"
log "+ scripts/camera_ap_ptpip_probe_flow.sh ${flow_args[*]}"
set +e
scripts/camera_ap_ptpip_probe_flow.sh "${flow_args[@]}" 2>&1 | tee "$run_dir/flow.log"
flow_rc=${PIPESTATUS[0]}
set -e
cat "$run_dir/flow.log" >>"$run_log"

if [[ "$flow_rc" != "0" ]]; then
  log "flow_rc=$flow_rc"
  log "export=skipped_flow_failed"
  exit "$flow_rc"
fi

flow_dir="$(awk -F= '/^flow=/{value=$2} END{print value}' "$run_dir/flow.log")"
if [[ -z "$flow_dir" ]]; then
  echo "could not find flow directory in $run_dir/flow.log" >&2
  exit 1
fi

ptpip_session="$(awk -F= '/^session=/{value=$2} END{print value}' "$flow_dir/03_ptpip_probe.log" 2>/dev/null || true)"
if [[ -z "$ptpip_session" ]]; then
  echo "could not find PTP/IP session directory in $flow_dir/03_ptpip_probe.log" >&2
  exit 1
fi

export_args=(--session-dir "$ptpip_session" --output-dir "$output_dir")
if [[ -n "$filename" ]]; then
  export_args+=(--filename "$filename")
fi
if [[ "$force" == "1" ]]; then
  export_args+=(--force)
fi

log "+ scripts/ptpip_export_object.sh --session-dir $ptpip_session --output-dir $output_dir"
scripts/ptpip_export_object.sh "${export_args[@]}" | tee "$run_dir/export.json"
log "export_manifest=$run_dir/export.json"
