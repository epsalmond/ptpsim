#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ptpip_compare_init.sh [options]

Options:
  --reference PATH      Captured InitCommandRequest packet. Default:
                        rce/reference/ptp_decoded/liveview_payload_00000061.bin.
  --candidate PATH      Candidate InitCommandRequest packet. If omitted, generate
                        one from --friendly-name, --tail-profile, and --guid.
  --friendly-name NAME  Friendly name for generated candidate. Default: project
                        device identity.
  --tail-profile NAME   Generated candidate tail profile. Default: liveview.
  --guid HEX            16-byte generated candidate GUID as hex.
  -h, --help            Show this help.

Compares Fuji-shaped 82-byte PTP/IP Init_Command_Request packets field by field.
This is an offline artifact-inspection command; it does not touch BLE, Wi-Fi, or
the camera.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test]'" >&2
  exit 1
fi

reference="rce/reference/ptp_decoded/liveview_payload_00000061.bin"
args=(compare-init)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --reference)
      reference="$2"
      shift 2
      ;;
    --candidate)
      args+=(--candidate "$2")
      shift 2
      ;;
    --friendly-name)
      args+=(--friendly-name "$2")
      shift 2
      ;;
    --tail-profile)
      args+=(--tail-profile "$2")
      shift 2
      ;;
    --guid)
      args+=(--guid "$2")
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

exec "$python_bin" -m rce.tools.fuji_ble_gps.ptpip "${args[@]}" --reference "$reference"
