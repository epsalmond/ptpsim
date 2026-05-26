#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/firmware_update_prepare.sh --dat PATH --claim-version VERSION [options]

Options:
  --dat PATH             Firmware DAT used for file-size evidence.
  --claim-version VER    Version string to claim in BLE request, e.g. 2.41.
  --name NAME            Camera BLE name substring. Default: GFX100 II.
  --address ADDRESS      Explicit CoreBluetooth UUID/address. Skips scanning.
  --device-name NAME     Laptop name written to camera. Default: project token.
  --timeout SEC          BLE scan/connect timeout. Default: 45.
  --product-name NAME    BLE request product name. Default: GFX100 II.
  --request-file-name N  BLE request filename. Default: GXUP0006.DAT.
  --ap-state-timeout SEC Seconds to wait for AP launched state. Default: 15.
  --notify-timeout SEC   Seconds to wait for FW state notify. Default: 10.
  --skip-register        Do not write Fuji registration first.
  --skip-registration-ack Do not write reference app-style registration ack.
  --pair-trigger-first   Read protected pairing characteristic before registration.
  --no-read-passphrase   Do not read/store wifi_credentials.json.
  -h, --help             Show this help.

This performs the BLE half of firmware update: read Wi-Fi credentials, write the
92-byte FirmwareUpdateRequestInfo, write FUNCTION_LAUNCH=0x0500, and wait for
the camera AP launched state. It does not upload firmware bytes.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
camera_name="${FUJI_CAMERA_NAME:-GFX100 II}"
camera_address="${FUJI_CAMERA_ADDRESS:-}"
device_name="${FUJI_DEVICE_NAME:-}"
timeout="${FUJI_BLE_TIMEOUT:-45}"
product_name="${FUJI_FIRMWARE_PRODUCT_NAME:-GFX100 II}"
request_file_name="${FUJI_FIRMWARE_REQUEST_FILE_NAME:-GXUP0006.DAT}"
ap_state_timeout="${FUJI_CAMERA_AP_STATE_TIMEOUT:-15}"
notify_timeout="${FUJI_FIRMWARE_NOTIFY_TIMEOUT:-10}"
dat_path=""
claim_version=""
skip_register=0
write_registration_ack=1
pair_trigger_first=0
read_passphrase=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dat)
      dat_path="$2"
      shift 2
      ;;
    --claim-version)
      claim_version="$2"
      shift 2
      ;;
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
    --product-name)
      product_name="$2"
      shift 2
      ;;
    --request-file-name)
      request_file_name="$2"
      shift 2
      ;;
    --ap-state-timeout)
      ap_state_timeout="$2"
      shift 2
      ;;
    --notify-timeout)
      notify_timeout="$2"
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

if [[ -z "$dat_path" || -z "$claim_version" ]]; then
  echo "missing required --dat PATH or --claim-version VERSION" >&2
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

if [[ -z "$device_name" ]]; then
  device_name="$("$python_bin" -c 'from rce.tools.fuji_ble_gps.device_identity import default_device_name; print(default_device_name())')"
fi

args=(
  firmware-prepare
  --name "$camera_name"
  --device-name "$device_name"
  --timeout "$timeout"
  --dat "$dat_path"
  --claim-version "$claim_version"
  --product-name "$product_name"
  --request-file-name "$request_file_name"
  --ap-state-timeout "$ap_state_timeout"
  --notify-timeout "$notify_timeout"
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

if [[ "$read_passphrase" != "1" ]]; then
  args+=(--no-read-passphrase)
fi

echo "+ $python_bin -m rce.tools.fuji_ble_gps.cli ${args[*]}" >&2
"$python_bin" -m rce.tools.fuji_ble_gps.cli "${args[@]}"
