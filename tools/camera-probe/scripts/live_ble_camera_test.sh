#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/live_ble_camera_test.sh [options]

Options:
  --name NAME          Camera BLE name substring. Default: GFX100 II
  --address ADDRESS    Explicit CoreBluetooth UUID/address. Skips scanning.
  --device-name NAME   Laptop name written to camera. Default: reference app-shaped
                       host-#### token from macOS LocalHostName, then
                       camera-safe ComputerName, then hostname.
  --lat FLOAT          Latitude for GPS write. Can also use FUJI_GPS_LAT.
  --lon FLOAT          Longitude for GPS write. Can also use FUJI_GPS_LON.
  --alt FLOAT          Altitude meters. Default: 0 or FUJI_GPS_ALT.
  --speed FLOAT        Speed m/s. Default: 0 or FUJI_GPS_SPEED.
  --repeat N           Number of GPS writes. Default: 1.
  --interval SEC       Seconds between repeated GPS writes. Default: 10.
  --timeout SEC        BLE scan/connect timeout. Default: 12.
  --pair-only          Connect and trigger macOS pairing only; do not register.
  --pair-trigger-first Read a protected pairing characteristic, then register
                       without disconnecting.
  --skip-location      Stop after registration; do not write GPS.
  --write-registration-ack
                       Write reference app-style registration ack for camera-side
                       persistence. Requires camera registration mode.
  --skip-registration-ack
                       Backward-compatible no-op; skipping ack is now default.
  -h, --help           Show this help.

The script uses one BLE session for live advertisement detection -> connect ->
discover -> register, then set-location when lat/lon are provided and
--skip-location is not set. Live detection connects to the matching BLEDevice
as soon as CoreBluetooth reports it, avoiding stale CoreBluetooth identifiers
from long scan windows.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
camera_name="${FUJI_CAMERA_NAME:-GFX100 II}"
camera_address="${FUJI_CAMERA_ADDRESS:-}"
device_name="${FUJI_DEVICE_NAME:-}"
lat="${FUJI_GPS_LAT:-}"
lon="${FUJI_GPS_LON:-}"
alt="${FUJI_GPS_ALT:-0}"
speed="${FUJI_GPS_SPEED:-0}"
repeat="${FUJI_GPS_REPEAT:-1}"
interval="${FUJI_GPS_INTERVAL:-10}"
timeout="${FUJI_BLE_TIMEOUT:-12}"
skip_location=0
pair_only=0
pair_trigger_first=0
skip_registration_ack=0
write_registration_ack=0

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
    --lat)
      lat="$2"
      shift 2
      ;;
    --lon)
      lon="$2"
      shift 2
      ;;
    --alt)
      alt="$2"
      shift 2
      ;;
    --speed)
      speed="$2"
      shift 2
      ;;
    --repeat)
      repeat="$2"
      shift 2
      ;;
    --interval)
      interval="$2"
      shift 2
      ;;
    --timeout)
      timeout="$2"
      shift 2
      ;;
    --pair-only)
      pair_only=1
      shift
      ;;
    --pair-trigger-first)
      pair_trigger_first=1
      shift
      ;;
    --skip-location)
      skip_location=1
      shift
      ;;
    --skip-registration-ack)
      skip_registration_ack=1
      shift
      ;;
    --write-registration-ack)
      write_registration_ack=1
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

run_cli() {
  echo "+ $python_bin -m rce.tools.fuji_ble_gps.cli $*" >&2
  "$python_bin" -m rce.tools.fuji_ble_gps.cli "$@"
}

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test]'" >&2
  exit 1
fi

if [[ -z "$device_name" ]]; then
  device_name="$("$python_bin" -c 'from rce.tools.fuji_ble_gps.device_identity import default_device_name; print(default_device_name())')"
fi

if [[ "$pair_only" == "1" ]]; then
  pair_args=(pair --name "$camera_name" --timeout "$timeout")
  if [[ -n "$camera_address" ]]; then
    pair_args+=(--address "$camera_address")
  fi
  run_cli "${pair_args[@]}"
  exit 0
fi

live_args=(
  live-test
  --name "$camera_name"
  --device-name "$device_name"
  --timeout "$timeout"
)

if [[ -n "$camera_address" ]]; then
  live_args+=(--address "$camera_address")
fi

if [[ "$write_registration_ack" == "1" ]]; then
  live_args+=(--write-registration-ack)
fi

if [[ "$pair_trigger_first" == "1" ]]; then
  live_args+=(--pair-trigger-first)
fi

if [[ "$skip_location" == "1" ]]; then
  run_cli "${live_args[@]}"
  echo "skipped GPS write by request" >&2
  exit 0
fi

if [[ -z "$lat" || -z "$lon" ]]; then
  run_cli "${live_args[@]}"
  echo "lat/lon not provided; stopped after registration without GPS write" >&2
  echo "rerun with --lat <float> --lon <float> or FUJI_GPS_LAT/FUJI_GPS_LON" >&2
  exit 0
fi

run_cli "${live_args[@]}" \
  --lat "$lat" \
  --lon "$lon" \
  --alt "$alt" \
  --speed "$speed" \
  --repeat "$repeat" \
  --interval "$interval"
