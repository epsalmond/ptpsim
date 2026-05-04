#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
python_bin="${PYTHON_BIN:-$repo_root/.venv/bin/python}"

cd "$repo_root"
exec "$python_bin" -m rce.tools.fuji_ble_gps.ff80_analysis "$@"
