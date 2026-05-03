#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ptpip_inventory_init.sh [--json] [PATH...]

Scans captured .bin payloads and decoded .jsonl traces for Fuji-shaped 82-byte
PTP/IP Init_Command_Request packets and prints source, GUID, friendly name,
tail profile, and packet length. Default paths are rce/reference/ptp_decoded
and rce/sessions.
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

for arg in "$@"; do
  case "$arg" in
    -h|--help)
      usage
      exit 0
      ;;
  esac
done

exec "$python_bin" -m rce.tools.fuji_ble_gps.ptpip inventory-init "$@"
