#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/camera_ap_ptpip_probe_flow.sh [options]

Options:
  --device-name NAME      Laptop/app name for BLE registration. Also used as
                          PTP/IP friendly name unless --ptpip-friendly-name is set.
  --address ADDRESS       Explicit CoreBluetooth UUID/address for BLE AP prep.
  --timeout SEC           BLE scan/connect timeout. Default: 45.
  --ap-state-timeout SEC  Seconds to wait for AP launched state. Default: 15.
  --wifi-timeout SEC      Seconds to wait for Wi-Fi IP. Default: 20.
  --ptpip-timeout SEC     PTP/IP socket timeout. Default: 5.
  --ptpip-friendly-name N PTP/IP InitiatorFriendlyName. Default: --device-name.
  --ptpip-tail-profile N  PTP/IP generated init tail profile: liveview, get,
                          or zeros. Default: liveview.
  --ptpip-guid HEX        16-byte GUID for generated InitCommandRequest
                          packets. Ignored when --ptpip-init-payload is used.
  --ptpip-init-payload P  Send an exact captured InitCommandRequest packet
                          instead of generating one.
  --ptpip-open-session    After PTP/IP init ack, send raw PTP OpenSession.
  --ptpip-get-prop HEX    After OpenSession, send PTP GetDevicePropValue.
  --ptpip-get-object-info H
                          After OpenSession, send PTP GetObjectInfo for object
                          handle H.
  --ptpip-get-thumb H     After OpenSession, send PTP GetThumb for object
                          handle H.
  --ptpip-app-sequence N After OpenSession, run a named observed reference app PTP
                          sequence. Current: sdcard-browse-bootstrap,
                          sdcard-current-object-info,
                          sdcard-current-object-thumbnail,
                          sdcard-folder-and-dates,
                          sdcard-object-handles.
  --temporary-wifi-internet
                          Allow Wi-Fi to temporarily leave the internet network
                          for the camera AP, then restore the previous Wi-Fi
                          SSID before this script returns.
  --restore-wifi-ssid S   SSID to restore after --temporary-wifi-internet.
                          Default: detected current Wi-Fi SSID.
  --hold-ble SEC          Diagnostic only: keep the BLE AP-launch connection
                          open after AP launch. Default: 0.
  --no-screen-read        Do not run camera LCD classification at flow
                          transition points. Default: screen reads enabled.
  --screen-device NAME    Camera capture device. Default: iPhone.
  --screen-warmup SEC     Camera capture warmup. Default: 2.
  --screen-zoom VALUE     Camera capture center-crop zoom. Default: 2.
  -h, --help              Show this help.

Runs the AP handoff critical path in sequence:
  1. BLE read credentials and launch camera AP with function value take/0400.
  2. Connect macOS Wi-Fi to the camera AP while preserving Ethernet internet,
     or temporarily taking over Wi-Fi when --temporary-wifi-internet is set.
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
ptpip_friendly_name="${FUJI_PTPIP_FRIENDLY_NAME:-}"
ptpip_tail_profile="${FUJI_PTPIP_TAIL_PROFILE:-liveview}"
ptpip_guid="${FUJI_PTPIP_GUID:-}"
ptpip_init_payload="${FUJI_PTPIP_INIT_PAYLOAD:-}"
ptpip_open_session="${FUJI_PTPIP_OPEN_SESSION:-0}"
ptpip_get_prop="${FUJI_PTPIP_GET_PROP:-}"
ptpip_get_object_info="${FUJI_PTPIP_GET_OBJECT_INFO:-}"
ptpip_get_thumb="${FUJI_PTPIP_GET_THUMB:-}"
ptpip_app_sequence="${FUJI_PTPIP_APP_SEQUENCE:-}"
hold_ble="${FUJI_CAMERA_AP_HOLD_AFTER_LAUNCH:-0}"
temporary_wifi_internet="${FUJI_TEMPORARY_WIFI_INTERNET:-0}"
restore_wifi_ssid="${FUJI_RESTORE_WIFI_SSID:-}"
restore_wifi_timeout="${FUJI_RESTORE_WIFI_TIMEOUT:-30}"
screen_read_enabled="${FUJI_SCREEN_READ:-1}"
screen_device="${FUJI_SCREEN_DEVICE_NAME:-iPhone}"
screen_warmup="${FUJI_SCREEN_WARMUP:-2}"
screen_zoom="${FUJI_SCREEN_ZOOM:-2}"

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
    --ptpip-friendly-name)
      ptpip_friendly_name="$2"
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
    --ptpip-get-object-info)
      ptpip_get_object_info="$2"
      ptpip_open_session=1
      shift 2
      ;;
    --ptpip-get-thumb)
      ptpip_get_thumb="$2"
      ptpip_open_session=1
      shift 2
      ;;
    --ptpip-app-sequence)
      ptpip_app_sequence="$2"
      ptpip_open_session=1
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
    --hold-ble)
      hold_ble="$2"
      shift 2
      ;;
    --no-screen-read)
      screen_read_enabled=0
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

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

mkdir -p "$flow_dir"
summary="$flow_dir/summary.txt"

log() {
  printf '%s\n' "$*" | tee -a "$summary" >&2
}

capture_flow() {
  local label="$1"
  shift
  "$@" >"$flow_dir/$label.txt" 2>&1 || true
}

detect_wifi_iface() {
  networksetup -listallhardwareports |
    awk '
      /^Hardware Port: (Wi-Fi|AirPort)$/ {want=1; next}
      want && /^Device: / {print $2; exit}
    '
}

current_wifi_ssid() {
  local iface="$1"
  local networksetup_output
  networksetup_output="$(networksetup -getairportnetwork "$iface" 2>/dev/null || true)"
  case "$networksetup_output" in
    "Current Wi-Fi Network: "*)
      printf '%s\n' "${networksetup_output#Current Wi-Fi Network: }"
      return 0
      ;;
  esac

  local airport_tool="/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport"
  if [[ -x "$airport_tool" ]]; then
    "$airport_tool" -I 2>/dev/null |
      awk -F': ' '/^[[:space:]]*SSID: / && $2 != "" {print $2; exit}'
  fi
}

restore_wifi_if_needed() {
  local rc=$?
  if [[ "${temporary_wifi_internet:-0}" == "1" && "${restore_wifi_needed:-0}" == "1" ]]; then
    log "+ restoring Wi-Fi internet network on $restore_wifi_iface to SSID '$restore_wifi_ssid'"
    set +e
    scripts/restore_wifi_internet.sh \
      --iface "$restore_wifi_iface" \
      --ssid "$restore_wifi_ssid" \
      --timeout "$restore_wifi_timeout" \
      --session-dir "$flow_dir/04_restore_wifi" 2>&1 | tee "$flow_dir/04_restore_wifi.log"
    local restore_rc=${PIPESTATUS[0]}
    set -e
    if [[ "$restore_rc" == "0" ]]; then
      log "internet_after_restore=present"
    else
      log "internet_after_restore=absent"
      log "restore_wifi_log=$flow_dir/04_restore_wifi.log"
      rc=9
    fi
  fi
  exit "$rc"
}

read_screen_state() {
  local label="$1"
  if [[ "$screen_read_enabled" != "1" ]]; then
    return 0
  fi

  local screen_log="$flow_dir/${label}_screen_state.log"
  local screen_args=(--device-name "$screen_device" --warmup "$screen_warmup")
  local screen_log_args="--device-name $screen_device --warmup $screen_warmup"
  if [[ -n "$screen_zoom" ]]; then
    screen_args+=(--zoom "$screen_zoom")
    screen_log_args="$screen_log_args --zoom $screen_zoom"
  fi

  log "+ scripts/read_camera_screen_state.sh $screen_log_args"
  set +e
  scripts/read_camera_screen_state.sh "${screen_args[@]}" 2>&1 | tee "$screen_log"
  local screen_rc=${PIPESTATUS[0]}
  set -e
  if [[ "$screen_rc" != "0" ]]; then
    echo "screen read failed at $label; log=$screen_log" >&2
    exit "$screen_rc"
  fi

  local screen_state
  screen_state="$(awk -F= '/^camera_screen_state=/{value=$2} END{print value}' "$screen_log")"
  if [[ -z "$screen_state" || "$screen_state" == "unknown" ]]; then
    echo "screen read at $label returned camera_screen_state=${screen_state:-missing}; refusing to continue" >&2
    echo "repair with: scripts/identify_unknown_elements.sh --capture <capture.json from $screen_log>" >&2
    exit 1
  fi
  log "${label}_screen_state=$screen_state"
}

log "flow=$flow_dir"
log "device_name=$device_name"
log "ptpip_friendly_name=$ptpip_friendly_name"

restore_wifi_needed=0
restore_wifi_iface=""
if [[ "$temporary_wifi_internet" == "1" ]]; then
  restore_wifi_iface="$(detect_wifi_iface)"
  if [[ -z "$restore_wifi_iface" ]]; then
    echo "could not detect Wi-Fi interface for temporary Wi-Fi restore" >&2
    exit 1
  fi
  if [[ -z "$restore_wifi_ssid" ]]; then
    restore_wifi_ssid="$(current_wifi_ssid "$restore_wifi_iface")"
  fi
  if [[ -z "$restore_wifi_ssid" ]]; then
    echo "could not detect current Wi-Fi SSID; pass --restore-wifi-ssid with --temporary-wifi-internet" >&2
    exit 1
  fi
  log "temporary_wifi_internet=enabled"
  log "restore_wifi_iface=$restore_wifi_iface"
  log "restore_wifi_ssid=$restore_wifi_ssid"
  trap restore_wifi_if_needed EXIT
fi

read_screen_state "00_initial"

prepare_log="$flow_dir/01_camera_ap_prepare.log"
ap_ble_evidence_log="$flow_dir/01_camera_ap_ble_evidence.log"
record_ap_ble_evidence() {
  local session_dir=""
  session_dir="$(awk -F= '/^session=/{print $2; exit}' "$prepare_log" 2>/dev/null || true)"
  if [[ -z "$session_dir" ]]; then
    log "camera_ap_ble_evidence=skipped_no_session"
    return
  fi
  log "+ scripts/evidence/camera_ap_ble_session.sh --session-dir $session_dir"
  set +e
  scripts/evidence/camera_ap_ble_session.sh --session-dir "$session_dir" 2>&1 | tee "$ap_ble_evidence_log"
  local evidence_rc=${PIPESTATUS[0]}
  set -e
  if [[ "$evidence_rc" != "0" ]]; then
    log "camera_ap_ble_evidence_rc=$evidence_rc"
  fi
}
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
    set +e
    wait "$prepare_pid"
    prepare_rc=$?
    set -e
    read_screen_state "01_after_ap_prepare_exit"
    record_ap_ble_evidence
    echo "BLE AP prepare exited before credentials and AP launch were ready" >&2
    if [[ "$prepare_rc" == "0" ]]; then
      exit 1
    fi
    exit "$prepare_rc"
  fi
  sleep 1
done

if [[ -z "$credentials" || ! -r "$credentials" ]]; then
  echo "could not find readable credentials path in $prepare_log" >&2
  read_screen_state "01_after_ap_prepare_timeout"
  wait "$prepare_pid" || true
  record_ap_ble_evidence
  exit 1
fi

log "credentials=$credentials"
read_screen_state "01_after_ap_launch"

wifi_log="$flow_dir/02_connect_camera_ap_wifi.log"
wifi_args=(--credentials "$credentials" --timeout "$wifi_timeout")
wifi_log_args="--credentials <redacted path> --timeout $wifi_timeout"
if [[ "$temporary_wifi_internet" == "1" ]]; then
  wifi_args+=(--allow-wifi-internet-loss)
  wifi_log_args="$wifi_log_args --allow-wifi-internet-loss"
  restore_wifi_needed=1
fi
log "+ scripts/connect_camera_ap_wifi.sh $wifi_log_args"
set +e
scripts/connect_camera_ap_wifi.sh "${wifi_args[@]}" 2>&1 | tee "$wifi_log"
wifi_rc=${PIPESTATUS[0]}
set -e
if [[ "$wifi_rc" != "0" ]]; then
  wait "$prepare_pid" || true
  exit "$wifi_rc"
fi
read_screen_state "02_after_wifi_association"

ptpip_log="$flow_dir/03_ptpip_probe.log"
ptpip_args=(--friendly-name "$ptpip_friendly_name" --tail-profile "$ptpip_tail_profile" --timeout "$ptpip_timeout")
ptpip_log_args="--friendly-name $ptpip_friendly_name --tail-profile $ptpip_tail_profile --timeout $ptpip_timeout"
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
if [[ -n "$ptpip_get_object_info" ]]; then
  ptpip_args+=(--get-object-info "$ptpip_get_object_info")
  ptpip_log_args="$ptpip_log_args --get-object-info $ptpip_get_object_info"
fi
if [[ -n "$ptpip_get_thumb" ]]; then
  ptpip_args+=(--get-thumb "$ptpip_get_thumb")
  ptpip_log_args="$ptpip_log_args --get-thumb $ptpip_get_thumb"
fi
if [[ -n "$ptpip_app_sequence" ]]; then
  ptpip_args+=(--app-sequence "$ptpip_app_sequence")
  ptpip_log_args="$ptpip_log_args --app-sequence $ptpip_app_sequence"
fi
log "+ scripts/ptpip_probe.sh $ptpip_log_args"
set +e
scripts/ptpip_probe.sh "${ptpip_args[@]}" 2>&1 | tee "$ptpip_log"
ptpip_rc=${PIPESTATUS[0]}
set -e
read_screen_state "03_after_ptpip_probe"

set +e
wait "$prepare_pid"
prepare_rc=$?
set -e
record_ap_ble_evidence

log "prepare_log=$prepare_log"
log "wifi_log=$wifi_log"
log "ptpip_log=$ptpip_log"

if [[ "$ptpip_rc" != "0" ]]; then
  exit "$ptpip_rc"
fi
exit "$prepare_rc"
