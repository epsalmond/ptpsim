#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/diagnose_macos_bluetooth_state.sh [options]

Options:
  --name NAME       Device name to look for. Default: GFX100 II
  --scan           Also run a BLE advertisement scan with the project CLI.
  --timeout SEC    BLE scan timeout when --scan is used. Default: 12.
  -h, --help       Show this help.

This script records deterministic macOS Bluetooth state sources and prints
explicit findings. It does not infer pairing state from camera UI text alone.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

device_name="${FUJI_CAMERA_NAME:-GFX100 II}"
scan=0
timeout="${FUJI_BLE_TIMEOUT:-12}"
python_bin="${PYTHON_BIN:-.venv/bin/python}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name)
      device_name="$2"
      shift 2
      ;;
    --scan)
      scan=1
      shift
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

ts="$(date -u +%Y%m%dT%H%M%SZ)"

mkdir -p "$out_dir"

run_capture() {
  local label="$1"
  shift
  echo "+ $*" >&2
  {
    "$@"
    echo "exit_code=0"
  } >"$out_dir/$label.txt" 2>&1 || {
    local rc=$?
    echo "exit_code=$rc" >>"$out_dir/$label.txt"
  }
}

run_capture computer_name scutil --get ComputerName
run_capture local_hostname hostname -s
run_capture system_profiler_json system_profiler SPBluetoothDataType -json
run_capture system_profiler_text system_profiler SPBluetoothDataType
run_capture bluetooth_plist plutil -p /Library/Preferences/com.apple.bluetooth.plist
run_capture bluetooth_legacy_plist plutil -p /Library/Preferences/com.apple.Bluetooth.plist
run_capture bluetooth_ioreg ioreg -r -c IOBluetoothDevice -l

if command -v blueutil >/dev/null 2>&1; then
  run_capture blueutil_paired blueutil --paired
  run_capture blueutil_connected blueutil --connected
else
  printf 'blueutil not installed\n' >"$out_dir/blueutil_paired.txt"
  printf 'blueutil not installed\n' >"$out_dir/blueutil_connected.txt"
fi

if [[ "$scan" == "1" ]]; then
  if [[ ! -x "$python_bin" ]]; then
    echo "missing python executable for BLE scan: $python_bin" >"$out_dir/ble_scan.txt"
  else
    run_capture ble_scan "$python_bin" -m rce.tools.fuji_ble_gps.cli scan --timeout "$timeout"
  fi
fi

contains_name() {
  local path="$1"
  grep -F -i -q "$device_name" "$path"
}

system_profiler_device_state() {
  awk -v name="$device_name:" '
    BEGIN { section = "present_unknown"; target = tolower(name) }
    /^[[:space:]]*Connected:/ { section = "connected" }
    /^[[:space:]]*Not Connected:/ { section = "not_connected" }
    index(tolower($0), target) { print section; found = 1; exit }
    END { if (!found) exit 1 }
  ' "$out_dir/system_profiler_text.txt"
}

print_state() {
  local key="$1"
  local value="$2"
  printf '%-34s %s\n' "$key:" "$value"
}

echo "state_dir=$out_dir"
print_state "target_name" "$device_name"

if contains_name "$out_dir/bluetooth_plist.txt" || contains_name "$out_dir/bluetooth_legacy_plist.txt"; then
  print_state "macos_known_device_plist" "present"
else
  print_state "macos_known_device_plist" "absent"
fi

if contains_name "$out_dir/bluetooth_ioreg.txt"; then
  print_state "macos_ioreg_device" "present"
else
  print_state "macos_ioreg_device" "absent"
fi

system_profiler_state="$(system_profiler_device_state || true)"
if [[ -n "$system_profiler_state" ]]; then
  print_state "system_profiler_device" "$system_profiler_state"
elif contains_name "$out_dir/system_profiler_json.txt"; then
  print_state "system_profiler_device" "present_unknown"
else
  print_state "system_profiler_device" "absent"
fi

if [[ "$scan" == "1" ]]; then
  if contains_name "$out_dir/ble_scan.txt"; then
    print_state "ble_advertisement_scan" "present"
  else
    print_state "ble_advertisement_scan" "absent"
  fi
else
  print_state "ble_advertisement_scan" "not_run"
fi
