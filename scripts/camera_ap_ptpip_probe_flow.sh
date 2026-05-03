#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/camera_ap_ptpip_probe_flow.sh [options]

Options:
  --device-name NAME      Laptop/app name for BLE registration and PTP/IP.
  --address ADDRESS       Explicit CoreBluetooth UUID/address for BLE AP prep.
  --timeout SEC           BLE scan/connect timeout. Default: 45.
  --ap-state-timeout SEC  Seconds to wait for AP launched state. Default: 15.
  --wifi-timeout SEC      Seconds to wait for Wi-Fi IP. Default: 20.
  --ptpip-timeout SEC     PTP/IP socket timeout. Default: 5.
  --ptpip-tail-profile N  PTP/IP generated init tail profile: liveview, get,
                          or zeros. Default: liveview.
  --ptpip-guid HEX        16-byte GUID for generated InitCommandRequest
                          packets. Ignored when --ptpip-init-payload is used.
  --ptpip-init-payload P  Send an exact captured InitCommandRequest packet
                          instead of generating one.
  --ptpip-open-session    After PTP/IP init ack, send raw PTP OpenSession.
  --ptpip-get-prop HEX    After OpenSession, send PTP GetDevicePropValue.
  --hold-ble SEC          Diagnostic only: keep the BLE AP-launch connection
                          open after AP launch. Default: 0.
  -h, --help              Show this help.

Runs the AP handoff critical path in sequence:
  1. BLE read credentials and launch camera AP with function value take/0400.
  2. Connect macOS Wi-Fi to the camera AP while preserving Ethernet internet.
  3. Probe 192.168.0.1:55740 with PTP/IP Init_Command_Request.

Use this when the camera is about to enter, or has just entered, its app search
window. The passphrase remains in the 0600 credentials file and is not printed.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
device_name="${FUJI_DEVICE_NAME:-}"
camera_address="${FUJI_CAMERA_ADDRESS:-}"
ble_timeout="${FUJI_BLE_TIMEOUT:-45}"
ap_state_timeout="${FUJI_CAMERA_AP_STATE_TIMEOUT:-15}"
wifi_timeout="${FUJI_WIFI_TIMEOUT:-20}"
ptpip_timeout="${FUJI_PTPIP_TIMEOUT:-5}"
ptpip_tail_profile="${FUJI_PTPIP_TAIL_PROFILE:-liveview}"
ptpip_guid="${FUJI_PTPIP_GUID:-}"
ptpip_init_payload="${FUJI_PTPIP_INIT_PAYLOAD:-}"
ptpip_open_session="${FUJI_PTPIP_OPEN_SESSION:-0}"
ptpip_get_prop="${FUJI_PTPIP_GET_PROP:-}"
hold_ble="${FUJI_CAMERA_AP_HOLD_AFTER_LAUNCH:-0}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --device-name)
      device_name="$2"
      shift 2
      ;;
    --address)
      camera_address="$2"
      shift 2
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
    --ptpip-tail-profile)
      ptpip_tail_profile="$2"
      shift 2
      ;;
    --ptpip-guid)
      ptpip_guid="$2"
      shift 2
      ;;
    --ptpip-init-payload)
      ptpip_init_payload="$2"
      shift 2
      ;;
    --ptpip-open-session)
      ptpip_open_session=1
      shift
      ;;
    --ptpip-get-prop)
      ptpip_get_prop="$2"
      ptpip_open_session=1
      shift 2
      ;;
    --hold-ble)
      hold_ble="$2"
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

if [[ -z "$device_name" ]]; then
  device_name="$("$python_bin" -c 'from rce.tools.fuji_ble_gps.device_identity import default_device_name; print(default_device_name())')"
fi

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

mkdir -p "$flow_dir"
summary="$flow_dir/summary.txt"

log() {
  printf '%s\n' "$*" | tee -a "$summary" >&2
}

log "flow=$flow_dir"
log "device_name=$device_name"

prepare_log="$flow_dir/01_camera_ap_prepare.log"
prepare_args=(
  --device-name "$device_name"
  --timeout "$ble_timeout"
  --launch-ap take
  --ap-state-timeout "$ap_state_timeout"
  --hold-after-launch "$hold_ble"
)
prepare_log_args="--device-name $device_name --timeout $ble_timeout --launch-ap take --ap-state-timeout $ap_state_timeout --hold-after-launch $hold_ble"
if [[ -n "$camera_address" ]]; then
  prepare_args+=(--address "$camera_address")
  prepare_log_args="$prepare_log_args --address $camera_address"
fi
log "+ scripts/camera_ap_prepare.sh $prepare_log_args"
scripts/camera_ap_prepare.sh \
  "${prepare_args[@]}" 2>&1 | tee "$prepare_log" &
prepare_pid=$!

credentials=""
ready_deadline=$((ble_timeout + ap_state_timeout + 15))
for _ in $(seq 1 "$ready_deadline"); do
  credentials="$(
    awk '/wrote sensitive Wi-Fi credentials /{value=$NF} END{print value}' "$prepare_log" 2>/dev/null || true
  )"
  if [[ -n "$credentials" && -r "$credentials" ]] && grep -q "ap_state=0180 label=launched" "$prepare_log"; then
    break
  fi
  if ! kill -0 "$prepare_pid" 2>/dev/null; then
    wait "$prepare_pid"
    echo "BLE AP prepare exited before credentials and AP launch were ready" >&2
    exit 1
  fi
  sleep 1
done

if [[ -z "$credentials" || ! -r "$credentials" ]]; then
  echo "could not find readable credentials path in $prepare_log" >&2
  wait "$prepare_pid" || true
  exit 1
fi

log "credentials=$credentials"

wifi_log="$flow_dir/02_connect_camera_ap_wifi.log"
log "+ scripts/connect_camera_ap_wifi.sh --credentials <redacted path> --timeout $wifi_timeout"
set +e
scripts/connect_camera_ap_wifi.sh --credentials "$credentials" --timeout "$wifi_timeout" 2>&1 | tee "$wifi_log"
wifi_rc=${PIPESTATUS[0]}
set -e
if [[ "$wifi_rc" != "0" ]]; then
  wait "$prepare_pid" || true
  exit "$wifi_rc"
fi

ptpip_log="$flow_dir/03_ptpip_probe.log"
ptpip_args=(--friendly-name "$device_name" --tail-profile "$ptpip_tail_profile" --timeout "$ptpip_timeout")
ptpip_log_args="--friendly-name $device_name --tail-profile $ptpip_tail_profile --timeout $ptpip_timeout"
if [[ -n "$ptpip_guid" ]]; then
  ptpip_args+=(--guid "$ptpip_guid")
  ptpip_log_args="$ptpip_log_args --guid $ptpip_guid"
fi
if [[ -n "$ptpip_init_payload" ]]; then
  ptpip_args+=(--init-payload "$ptpip_init_payload")
  ptpip_log_args="$ptpip_log_args --init-payload $ptpip_init_payload"
fi
if [[ "$ptpip_open_session" == "1" ]]; then
  ptpip_args+=(--open-session)
  ptpip_log_args="$ptpip_log_args --open-session"
fi
if [[ -n "$ptpip_get_prop" ]]; then
  ptpip_args+=(--get-prop "$ptpip_get_prop")
  ptpip_log_args="$ptpip_log_args --get-prop $ptpip_get_prop"
fi
log "+ scripts/ptpip_probe.sh $ptpip_log_args"
set +e
scripts/ptpip_probe.sh "${ptpip_args[@]}" 2>&1 | tee "$ptpip_log"
ptpip_rc=${PIPESTATUS[0]}
set -e

set +e
wait "$prepare_pid"
prepare_rc=$?
set -e

log "prepare_log=$prepare_log"
log "wifi_log=$wifi_log"
log "ptpip_log=$ptpip_log"

if [[ "$ptpip_rc" != "0" ]]; then
  exit "$ptpip_rc"
fi
exit "$prepare_rc"
