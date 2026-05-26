#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/camera_ap_prepare.sh [options]

Options:
  --name NAME             Camera BLE name substring. Default: GFX100 II
  --address ADDRESS       Explicit CoreBluetooth UUID/address. Skips scanning.
  --device-name NAME      Laptop name written to camera. Default: project
                          reference app-shaped host token.
  --timeout SEC           BLE scan/connect timeout. Default: 45.
  --launch-ap MODE        AP launch mode: get, take, or none. Default: take.
  --ap-state-timeout SEC  Seconds to wait for AP launched state. Default: 15.
  --hold-after-launch SEC Keep BLE connected this many seconds after AP launch.
                          Default: 0.
  --skip-register         Do not write Fuji registration before reading AP info.
  --skip-registration-ack Do not write reference app-style registration ack.
  --pair-trigger-first    Read a protected pairing characteristic before
                          registration in the same BLE connection.
  --no-read-passphrase    Do not read/store wifi_credentials.json.
  -h, --help              Show this help.

This script performs the BLE half of camera AP handoff. It reads SSID/BSSID/AP
state, optionally reads the sensitive AP passphrase into a 0600 credentials
file, and optionally launches the camera AP. The passphrase is not printed.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
camera_name="${FUJI_CAMERA_NAME:-GFX100 II}"
camera_address="${FUJI_CAMERA_ADDRESS:-}"
device_name="${FUJI_DEVICE_NAME:-}"
timeout="${FUJI_BLE_TIMEOUT:-45}"
launch_ap="${FUJI_CAMERA_AP_LAUNCH:-take}"
ap_state_timeout="${FUJI_CAMERA_AP_STATE_TIMEOUT:-15}"
hold_after_launch="${FUJI_CAMERA_AP_HOLD_AFTER_LAUNCH:-0}"
skip_register=0
write_registration_ack=1
pair_trigger_first=0
read_passphrase=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name)
      camera_name="$2"
      shift 2
      ;;
    --address)
      camera_address="$2"
      shift 2
      ;;
    --device-name)
      device_name="$2"
      shift 2
      ;;
    --timeout)
      timeout="$2"
      shift 2
      ;;
    --launch-ap)
      launch_ap="$2"
      shift 2
      ;;
    --ap-state-timeout)
      ap_state_timeout="$2"
      shift 2
      ;;
    --hold-after-launch)
      hold_after_launch="$2"
      shift 2
      ;;
    --skip-register)
      skip_register=1
      shift
      ;;
    --skip-registration-ack)
      write_registration_ack=0
      shift
      ;;
    --pair-trigger-first)
      pair_trigger_first=1
      shift
      ;;
    --no-read-passphrase)
      read_passphrase=0
      shift
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

args=(
  wifi-info
  --name "$camera_name"
  --device-name "$device_name"
  --timeout "$timeout"
  --launch-ap "$launch_ap"
  --ap-state-timeout "$ap_state_timeout"
  --hold-after-launch "$hold_after_launch"
)

if [[ -n "$camera_address" ]]; then
  args+=(--address "$camera_address")
fi

if [[ "$skip_register" == "1" ]]; then
  args+=(--skip-register)
elif [[ "$write_registration_ack" == "1" ]]; then
  args+=(--write-registration-ack)
fi

if [[ "$pair_trigger_first" == "1" ]]; then
  args+=(--pair-trigger-first)
fi

if [[ "$read_passphrase" == "1" ]]; then
  args+=(--read-passphrase)
fi

echo "+ $python_bin -m rce.tools.fuji_ble_gps.cli ${args[*]}" >&2
"$python_bin" -m rce.tools.fuji_ble_gps.cli "${args[@]}"
