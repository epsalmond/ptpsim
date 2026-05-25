#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test]'" >&2
  exit 1
fi

cat >&2 <<'MSG'
This step intentionally performs a short CoreBluetooth scan to trigger the
macOS Bluetooth permission prompt for the current terminal app.

Approve Bluetooth access in System Settings when prompted, then rerun the live
camera test.
MSG

"$python_bin" -m rce.tools.fuji_ble_gps.cli scan --timeout 3
