#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ff80_dump_cfgdata.sh [--session-dir DIR]

Read-only FF80 cfgdata collector. It verifies the active FF80 command path
with ping before and after the dump, saves cfgdata.bin, and writes an offline
analysis JSON/text summary. Passive USB polling is recorded as advisory
evidence because macOS/libusb enumeration can transiently miss a device that
the active FF80 opener can still use.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-$repo_root/.venv/bin/python}"

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"


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

mkdir -p "$session_dir/logs"
manifest="$session_dir/manifest.txt"
cfgdata="$session_dir/cfgdata.bin"
analysis_json="$session_dir/analysis.json"
analysis_txt="$session_dir/analysis.txt"

log() {
  printf '%s\n' "$*" | tee -a "$manifest" >&2
}

output_failed() {
  local path="$1"
  grep -Eqi 'USB timeout|USB pipe stalled|device .*not found|jig error|Traceback|AssertionError' "$path"
}

run_logged() {
  local label="$1"
  shift
  local log_file="$session_dir/logs/$label.log"
  log "+ $*"
  set +e
  "$@" >"$log_file" 2>&1
  local rc=$?
  set -e
  sed -n '1,160p' "$log_file" | tee -a "$manifest" >&2
  if [[ "$rc" -ne 0 ]] || output_failed "$log_file"; then
    log "FAILED: $*"
    return 1
  fi
}

run_optional_logged() {
  local label="$1"
  shift
  local log_file="$session_dir/logs/$label.log"
  log "+ $*"
  set +e
  "$@" >"$log_file" 2>&1
  local rc=$?
  set -e
  sed -n '1,160p' "$log_file" | tee -a "$manifest" >&2
  if [[ "$rc" -ne 0 ]] || output_failed "$log_file"; then
    log "ADVISORY_FAILED: $*"
    return 0
  fi
}

run_ff80() {
  local label="$1"
  shift
  run_logged "$label" "$python_bin" "$ff80_dir/ff80.py" "$@"
}

log "session_dir=$session_dir"
run_ff80 preflight_ping --trace ping
run_optional_logged poll_observation scripts/poll_fuji_usb_devices.sh --product-id 0xff80 --timeout 3 --exit-on-match --summary-every 1
run_ff80 cfgdata_dump cfgdata dump -o "$cfgdata"
run_ff80 post_dump_ping --trace ping
run_logged analyze_cfgdata scripts/ff80_analyze_dumps.sh "$cfgdata" --output-json "$analysis_json"
cp "$session_dir/logs/analyze_cfgdata.log" "$analysis_txt"

sha="$(shasum -a 256 "$cfgdata" | awk '{print $1}')"
size="$(stat -f '%z' "$cfgdata" 2>/dev/null || stat -c '%s' "$cfgdata")"
log "cfgdata=$cfgdata"
log "cfgdata_size=$size"
log "cfgdata_sha256=$sha"
log "analysis_json=$analysis_json"
