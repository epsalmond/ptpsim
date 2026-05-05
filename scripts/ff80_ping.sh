#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ff80_ping.sh [options]

Options:
  --session-dir DIR     Output directory. Default:
                        rce/sessions/ff80_ping_<utc timestamp>.
  --vendor-id HEX       USB vendor id. Default: 0x04cb.
  --product-id HEX      USB product id. Default: 0xff80.
  --recipient NAME      USB request recipient passed to ff80.py.
                        One of: device, endpoint, interface, other.
                        Default: other.
  --poll-timeout SEC    Passive USB poll timeout before active ping. Default: 3.
  --skip-poll           Skip passive USB enumeration and run active ping only.
  --no-trace            Do not pass --trace to ff80.py. Default: trace on.
  -h, --help            Show this help.

Runs one active FF80 ping and saves the raw output plus a one-line summary.
USB enumeration is advisory evidence only; the active FF80 ping is the state
proof. Timeout/stall/traceback text is treated as failure even if ff80.py exits
successfully.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-$repo_root/.venv/bin/python}"

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

vendor_id="0x04cb"
product_id="0xff80"
recipient="${FUJI_FF80_RECIPIENT:-other}"
poll_timeout=3
skip_poll=0
trace=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --session-dir)
      session_dir="$2"
      shift 2
      ;;
    --vendor-id)
      vendor_id="$2"
      shift 2
      ;;
    --product-id)
      product_id="$2"
      shift 2
      ;;
    --recipient)
      recipient="$2"
      shift 2
      ;;
    --poll-timeout)
      poll_timeout="$2"
      shift 2
      ;;
    --skip-poll)
      skip_poll=1
      shift
      ;;
    --no-trace)
      trace=0
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

case "$recipient" in
  device|endpoint|interface|other) ;;
  *)
    echo "invalid --recipient: $recipient" >&2
    exit 2
    ;;
esac

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  exit 1
fi

if [[ ! -f "$ff80_dir/ff80.py" ]]; then
  echo "missing FF80 tool: $ff80_dir/ff80.py" >&2
  exit 1
fi

mkdir -p "$session_dir/logs"
manifest="$session_dir/manifest.txt"
summary="$session_dir/summary.txt"

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

ff80_args=(--vendor-id "$vendor_id" --product-id "$product_id" --recipient "$recipient")
if [[ "$trace" -eq 1 ]]; then
  ff80_args+=(--trace)
fi
ff80_args+=(ping)

log "session_dir=$session_dir"
if [[ "$skip_poll" -eq 0 ]]; then
  run_optional_logged usb_poll scripts/poll_fuji_usb_devices.sh \
    --vendor-id "$vendor_id" \
    --product-id "$product_id" \
    --timeout "$poll_timeout" \
    --exit-on-match \
    --summary-every 1
fi

if run_logged ff80_ping "$python_bin" "$ff80_dir/ff80.py" "${ff80_args[@]}"; then
  {
    echo "ff80_ping=present"
    echo "session_dir=$session_dir"
    echo "log=$session_dir/logs/ff80_ping.log"
  } | tee "$summary"
else
  {
    echo "ff80_ping=failed"
    echo "session_dir=$session_dir"
    echo "log=$session_dir/logs/ff80_ping.log"
  } | tee "$summary"
  exit 1
fi
