#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ptpip_decode_session_artifacts.sh --session-dir PATH

Decodes known PTP/IP data artifacts from an existing probe session. Currently
this writes ObjectInfo JSON and JPEG thumbnail payloads next to the source
container files and prints a JSON summary.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
session_dir=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --session-dir)
      session_dir="$2"
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

if [[ -z "$session_dir" ]]; then
  echo "missing --session-dir" >&2
  usage >&2
  exit 2
fi

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test]'" >&2
  exit 1
fi

"$python_bin" -m rce.tools.fuji_ble_gps.ptpip decode-session --session-dir "$session_dir"
