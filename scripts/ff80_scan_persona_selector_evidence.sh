#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ff80_scan_persona_selector_evidence.sh [options]

Options:
  --session-root DIR    Root containing FF80 session directories.
                        Default: rce/sessions.
  --session-dir DIR     Include one specific session directory. May repeat.
  --output-dir DIR      Output directory. Default:
                        rce/sessions/ff80_persona_selector_scan_<utc timestamp>.
  -h, --help            Show this help.

Scans existing FF80 RAM dump artifacts offline for:

- Direct BL calls to cfgdata getter 0x0158bfc8.
- Nearby known cfgdata tag loads, especially tag 0x0d8.
- Runtime USB/persona evidence such as USB strings, raw PID/VID words, and
  descriptor byte patterns.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-$repo_root/.venv/bin/python}"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"


session_dirs=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --session-root)
      session_roots=("$2")
      shift 2
      ;;
    --session-dir)
      session_dirs+=("$2")
      shift 2
      ;;
    --output-dir)
      output_dir="$2"
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

case "$output_dir" in
  /*) ;;
  *) output_dir="$repo_root/$output_dir" ;;
esac

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  exit 1
fi

mkdir -p "$output_dir"
json_path="$output_dir/persona_selector_evidence.json"
text_path="$output_dir/persona_selector_evidence.txt"

args=(--persona-evidence --output-json "$json_path")
if [[ "${#session_dirs[@]}" -gt 0 ]]; then
  for session_dir in "${session_dirs[@]}"; do
    args+=(--session-dir "$session_dir")
  done
else
  for session_root in "${session_roots[@]}"; do
    args+=(--session-root "$session_root")
  done
fi

echo "output_dir=$output_dir"
echo "+ $python_bin -m rce.tools.fuji_ble_gps.ff80_analysis ${args[*]}"
"$python_bin" -m rce.tools.fuji_ble_gps.ff80_analysis "${args[@]}" | tee "$text_path"
echo "json=$json_path"
echo "text=$text_path"
