#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: scripts/evidence/_run.sh <evidence-command> [args...]" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test]'" >&2
  exit 1
fi

exec "$python_bin" -m rce.tools.fuji_ble_gps.evidence "$@"
