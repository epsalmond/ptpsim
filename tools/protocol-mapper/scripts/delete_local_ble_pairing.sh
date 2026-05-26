#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/delete_local_ble_pairing.sh [options]

Options:
  --name NAME       Bluetooth device name to remove. Default: GFX100 II
  --address ADDR    Bluetooth address/identifier to unpair when using blueutil.
  --ui-automate     Use System Settings UI automation if blueutil cannot find it.
  --no-open         Do not open macOS Bluetooth settings if blueutil is absent.
  -h, --help        Show this help.

This removes the laptop/macOS-side Bluetooth pairing when possible.

macOS itself does not ship a stable command-line unpair tool. `blueutil` is a
project macOS dependency for scripted unpairing. Install it with:

  scripts/install_macos_dependencies.sh

If `blueutil` is absent, this script opens System Settings as a manual fallback:

  System Settings -> Bluetooth -> GFX100 II -> Forget This Device
USAGE
}

device_name="${FUJI_CAMERA_NAME:-GFX100 II}"
device_address=""
open_settings=1
ui_automate=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name)
      device_name="$2"
      shift 2
      ;;
    --address)
      device_address="$2"
      shift 2
      ;;
    --ui-automate)
      ui_automate=1
      shift
      ;;
    --no-open)
      open_settings=0
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

if command -v blueutil >/dev/null 2>&1; then
  if [[ -n "$device_address" ]]; then
    echo "+ blueutil --unpair $device_address" >&2
    blueutil --unpair "$device_address"
    exit 0
  fi

  paired="$(blueutil --paired 2>/dev/null || true)"
  match="$(printf '%s\n' "$paired" | awk -v name="$device_name" 'index($0, name) { print; exit }')"
  if [[ -z "$match" ]]; then
    if [[ "$ui_automate" == "1" ]]; then
      exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/forget_bluetooth_device_via_system_settings.sh" --name "$device_name"
    fi
    echo "blueutil did not list a paired device matching: $device_name" >&2
    echo "paired devices:" >&2
    printf '%s\n' "$paired" >&2
    exit 1
  fi

  # blueutil output formats vary; accept common address forms at the start of
  # a line or after "address:".
  parsed_address="$(printf '%s\n' "$match" | sed -nE 's/^([0-9A-Fa-f:-]{11,17}).*/\1/p; s/.*address: *([0-9A-Fa-f:-]{11,17}).*/\1/p' | head -n 1)"
  if [[ -z "$parsed_address" ]]; then
    echo "matched $device_name but could not parse an address from:" >&2
    echo "$match" >&2
    echo "rerun with --address <addr>" >&2
    exit 1
  fi

  echo "+ blueutil --unpair $parsed_address" >&2
  blueutil --unpair "$parsed_address"
  exit 0
fi

cat >&2 <<MSG
blueutil is not installed, and macOS has no built-in stable CLI for forgetting
one Bluetooth LE device. Install project macOS dependencies with:

  scripts/install_macos_dependencies.sh

Remove the laptop-side pairing manually:
  System Settings -> Bluetooth -> ${device_name} -> Forget This Device

MSG

if [[ "$open_settings" == "1" ]]; then
  echo "+ open x-apple.systempreferences:com.apple.BluetoothSettings" >&2
  open "x-apple.systempreferences:com.apple.BluetoothSettings" || true
fi

exit 3
