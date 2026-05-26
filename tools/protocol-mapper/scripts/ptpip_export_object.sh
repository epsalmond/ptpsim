#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ptpip_export_object.sh --session-dir PATH --output-dir PATH [options]

Options:
  --session-dir PATH    Existing ptpip_probe session containing get_object_payload.jpg.
  --output-dir PATH     Destination directory for the exported JPEG and manifest.
  --filename NAME       Override filename. Default: ObjectInfo filename or handle fallback.
  --force               Overwrite an existing exported file and manifest.
  -h, --help            Show this help.

Validates that get_object_payload.jpg starts with JPEG SOI and ends with JPEG
EOI before writing the exported file. A JSON manifest is written next to the
exported JPEG.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
session_dir=""
output_dir=""
filename=""
force=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --session-dir)
      session_dir="$2"
      shift 2
      ;;
    --output-dir)
      output_dir="$2"
      shift 2
      ;;
    --filename)
      filename="$2"
      shift 2
      ;;
    --force)
      force=1
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

if [[ -z "$session_dir" ]]; then
  echo "missing --session-dir" >&2
  usage >&2
  exit 2
fi

if [[ -z "$output_dir" ]]; then
  echo "missing --output-dir" >&2
  usage >&2
  exit 2
fi

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test]'" >&2
  exit 1
fi

args=(export-object --session-dir "$session_dir" --output-dir "$output_dir")
if [[ -n "$filename" ]]; then
  args+=(--filename "$filename")
fi
if [[ "$force" == "1" ]]; then
  args+=(--force)
fi

"$python_bin" -m rce.tools.fuji_ble_gps.ptpip "${args[@]}"
