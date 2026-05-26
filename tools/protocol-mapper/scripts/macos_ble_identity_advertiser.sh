#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/macos_ble_identity_advertiser.sh [options]

Options:
  --name NAME                    BLE Local Name to advertise.
                                 Default: FUJI_BLE_IDENTITY_NAME, then
                                 FUJI_DEVICE_NAME, then default device name.
  --duration SEC                 Seconds to advertise. Default: 120.
  --no-advertise-gap-service     Do not attempt reserved GAP service 0x1800.
  --compile-only                 Build helper and exit.
  -h, --help                     Show this help.

This is an experiment for the GFX100 II blank registered-device-name issue.
It starts a CoreBluetooth peripheral advertiser with Local Name. The helper
attempts GAP 0x1800 / Device Name 0x2A00 by default, but macOS public
CoreBluetooth rejects that reserved service and falls back to Local Name only.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
name="${FUJI_BLE_IDENTITY_NAME:-${FUJI_DEVICE_NAME:-}}"
duration="${FUJI_BLE_IDENTITY_DURATION:-120}"
compile_only=0
extra_args=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name)
      name="$2"
      shift 2
      ;;
    --duration)
      duration="$2"
      shift 2
      ;;
    --no-advertise-gap-service)
      extra_args+=(--no-advertise-gap-service)
      shift
      ;;
    --compile-only)
      compile_only=1
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

if [[ -z "$name" ]]; then
  if [[ -x "$python_bin" ]]; then
    name="$("$python_bin" -c 'from rce.tools.fuji_ble_gps.device_identity import default_device_name; print(default_device_name())')"
  else
    name="$(hostname -s | tr -cd '[:alnum:]-' | cut -c 1-20)"
  fi
fi

if [[ -z "$name" ]]; then
  name="Fuji-Laptop"
fi

binary="$(scripts/build/build-ble-identity-advertiser.sh)"

if [[ "$compile_only" == "1" ]]; then
  echo "$binary"
  exit 0
fi

echo "+ $binary --name $name --duration $duration ${extra_args[*]-}" >&2
if [[ ${#extra_args[@]} -gt 0 ]]; then
  exec "$binary" --name "$name" --duration "$duration" "${extra_args[@]}"
else
  exec "$binary" --name "$name" --duration "$duration"
fi
