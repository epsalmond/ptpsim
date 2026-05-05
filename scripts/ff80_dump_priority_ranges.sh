#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ff80_dump_priority_ranges.sh [options]

Options:
  --session-dir DIR     Output directory. Default:
                        rce/sessions/ff80_priority_dumps_<utc timestamp>.
  --include-risky-low   Include low ThreadX runtime ranges below 0x00100000.
                        These are useful, but an earlier 0x00000000 read
                        wedged FF80 until reboot. Default: skip them.
  --only-risky-low      Dump only the low ThreadX runtime ranges below
                        0x00100000.
  --next-targets        Dump the follow-up RAM targets in ascending address
                        order: backlog ADRP/code/data ranges plus the earlier
                        task slot and boot-populated dispatch-table ranges.
  --gap-targets         Dump the next RAM gaps around populated code/runtime
                        windows, widened globals, and the next message-pool
                        continuation.
  --low-watermark       Probe downward from the lowest known-good RAM address
                        with 16-byte reads and ping verification only.
  --ram-size-probes     Probe sparse high RAM-map addresses with 16-byte reads
                        to test the 512 MiB DDR-window hypothesis.
  --safe-fill-gaps      Fill known uncovered low-map gaps while deliberately
                        excluding the hazardous 0x00002000..0x00040000 range.
  --stop-on-fail        Stop on the first failed probe/dump. Default.
  --continue-on-fail    Continue after a failed range if FF80 ping still works.
  -h, --help            Show this help.

This is a read-only FF80 range collector. For each range it:

1. Confirms the device enumerates as 04cb:ff80.
2. Runs FF80 ping.
3. Reads 16 bytes from the range start.
4. Runs FF80 ping again.
5. Dumps the bounded range.
6. Runs FF80 ping again.
7. Records SHA256, requested size, and actual byte count.

If an FF80 command reports USB timeout, USB pipe stall, device-not-found, jig
error, or traceback, the command is treated as failed even if the upstream
script exits 0.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-$repo_root/.venv/bin/python}"

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

include_risky_low=0
only_risky_low=0
next_targets=0
gap_targets=0
low_watermark=0
ram_size_probes=0
safe_fill_gaps=0
stop_on_fail=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --session-dir)
      session_dir="$2"
      shift 2
      ;;
    --include-risky-low)
      include_risky_low=1
      shift
      ;;
    --only-risky-low)
      only_risky_low=1
      include_risky_low=1
      shift
      ;;
    --next-targets)
      next_targets=1
      shift
      ;;
    --gap-targets)
      gap_targets=1
      shift
      ;;
    --low-watermark)
      low_watermark=1
      shift
      ;;
    --ram-size-probes)
      ram_size_probes=1
      shift
      ;;
    --safe-fill-gaps)
      safe_fill_gaps=1
      shift
      ;;
    --stop-on-fail)
      stop_on_fail=1
      shift
      ;;
    --continue-on-fail)
      stop_on_fail=0
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

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  exit 1
fi

if [[ ! -f "$ff80_dir/ff80.py" ]]; then
  echo "missing FF80 tool: $ff80_dir/ff80.py" >&2
  exit 1
fi

mkdir -p "$session_dir/probes" "$session_dir/dumps" "$session_dir/logs"
summary="$session_dir/summary.tsv"
manifest="$session_dir/manifest.txt"

printf 'label\taddress\trequested_size\tactual_bytes\tstatus\tpath\tsha256\tnote\n' >"$summary"

log() {
  printf '%s\n' "$*" | tee -a "$manifest" >&2
}

addr_token() {
  local value="$1"
  printf '%08x' "$(( value ))"
}

range_end_token() {
  local addr="$1"
  local size="$2"
  printf '%08x' "$(( addr + size ))"
}

file_size_bytes() {
  local path="$1"
  if stat -f '%z' "$path" >/dev/null 2>&1; then
    stat -f '%z' "$path"
  else
    stat -c '%s' "$path"
  fi
}

record() {
  local label="$1"
  local addr="$2"
  local size="$3"
  local actual_bytes="$4"
  local status="$5"
  local path="$6"
  local sha="$7"
  local note="$8"
  printf '%s\t0x%08x\t0x%x\t%s\t%s\t%s\t%s\t%s\n' \
    "$label" "$(( addr ))" "$(( size ))" "$actual_bytes" "$status" "$path" "$sha" "$note" >>"$summary"
}

output_failed() {
  local path="$1"
  grep -Eqi 'USB timeout|USB pipe stalled|device .*not found|jig error|Traceback|AssertionError' "$path"
}

run_ff80_logged() {
  local label="$1"
  shift
  local log_file="$session_dir/logs/$label.log"
  log "+ ff80 $*"
  set +e
  (
    cd "$ff80_dir"
    "$python_bin" ff80.py "$@"
  ) >"$log_file" 2>&1
  local rc=$?
  set -e
  sed -n '1,120p' "$log_file" | tee -a "$manifest" >&2
  if [[ "$rc" -ne 0 ]] || output_failed "$log_file"; then
    log "FAILED: ff80 $*"
    return 1
  fi
  return 0
}

confirm_ff80() {
  local label="$1"
  local log_file="$session_dir/logs/$label.poll.log"
  log "+ scripts/poll_fuji_usb_devices.sh --product-id 0xff80 --timeout 3 --exit-on-match --summary-every 1"
  set +e
  scripts/poll_fuji_usb_devices.sh --product-id 0xff80 --timeout 3 --exit-on-match --summary-every 1 >"$log_file" 2>&1
  local rc=$?
  set -e
  cat "$log_file" | tee -a "$manifest" >&2
  [[ "$rc" -eq 0 ]]
}

ping_ff80() {
  local label="$1"
  run_ff80_logged "$label" --trace ping
}

dump_range() {
  local label="$1"
  local addr="$2"
  local size="$3"
  local addr_name
  local end_name
  addr_name="$(addr_token "$addr")"
  end_name="$(range_end_token "$addr" "$size")"
  local probe_path="$session_dir/probes/${label}_${addr_name}_probe_10.bin"
  local dump_path="$session_dir/dumps/${label}_${addr_name}_${end_name}.bin"

  log "=== $label addr=0x$addr_name size=0x$(printf '%x' "$(( size ))") ==="

  if ! ping_ff80 "${label}_pre_ping"; then
    record "$label" "$addr" "$size" "" "failed" "" "" "pre_ping_failed"
    return 1
  fi

  if ! run_ff80_logged "${label}_probe" ram read "0x$addr_name" -s 0x10 -o "$probe_path"; then
    record "$label" "$addr" "$size" "" "failed" "$probe_path" "" "probe_failed"
    return 1
  fi

  if ! ping_ff80 "${label}_post_probe_ping"; then
    record "$label" "$addr" "$size" "" "failed" "$probe_path" "" "post_probe_ping_failed"
    return 1
  fi

  if ! run_ff80_logged "${label}_dump" ram dump "0x$addr_name" -s "0x$(printf '%x' "$(( size ))")" -o "$dump_path"; then
    record "$label" "$addr" "$size" "" "failed" "$dump_path" "" "dump_failed"
    return 1
  fi

  if ! ping_ff80 "${label}_post_dump_ping"; then
    record "$label" "$addr" "$size" "" "failed" "$dump_path" "" "post_dump_ping_failed"
    return 1
  fi

  local sha
  local actual_bytes
  local note
  sha="$(shasum -a 256 "$dump_path" | awk '{print $1}')"
  actual_bytes="$(file_size_bytes "$dump_path")"
  note="dumped"
  if [[ "$actual_bytes" -ne "$(( size ))" ]]; then
    note="dumped_actual_size_differs_from_requested"
  fi
  record "$label" "$addr" "$size" "$actual_bytes" "ok" "$dump_path" "$sha" "$note"
  ls -l "$probe_path" "$dump_path" | tee -a "$manifest" >&2
}

probe_address() {
  local label="$1"
  local addr="$2"
  local addr_name
  addr_name="$(addr_token "$addr")"
  local probe_path="$session_dir/probes/${label}_${addr_name}_probe_10.bin"

  log "=== $label addr=0x$addr_name probe_size=0x10 ==="

  if ! ping_ff80 "${label}_pre_ping"; then
    record "$label" "$addr" 0x10 "" "failed" "" "" "pre_ping_failed"
    return 1
  fi

  if ! run_ff80_logged "${label}_probe" ram read "0x$addr_name" -s 0x10 -o "$probe_path"; then
    record "$label" "$addr" 0x10 "" "failed" "$probe_path" "" "probe_failed"
    return 1
  fi

  if ! ping_ff80 "${label}_post_probe_ping"; then
    record "$label" "$addr" 0x10 "" "failed" "$probe_path" "" "post_probe_ping_failed"
    return 1
  fi

  local sha
  local actual_bytes
  sha="$(shasum -a 256 "$probe_path" | awk '{print $1}')"
  actual_bytes="$(file_size_bytes "$probe_path")"
  record "$label" "$addr" 0x10 "$actual_bytes" "ok" "$probe_path" "$sha" "probed"
  ls -l "$probe_path" | tee -a "$manifest" >&2
}

ranges=()
probes=()

add_chunked_range() {
  local prefix="$1"
  local start="$2"
  local end="$3"
  local chunk_size="$4"
  local current="$(( start ))"
  local index=0
  while [[ "$current" -lt "$(( end ))" ]]; do
    local size="$(( end - current ))"
    if [[ "$size" -gt "$(( chunk_size ))" ]]; then
      size="$(( chunk_size ))"
    fi
    ranges+=("$(printf '%s_%02d 0x%08x 0x%x' "$prefix" "$index" "$current" "$size")")
    current="$(( current + size ))"
    index="$(( index + 1 ))"
  done
}

if [[ "$next_targets" -eq 1 ]]; then
  ranges+=(
    "code_bl_targets_44000 0x00044000 0x14000"
    "dispatch_table_57000 0x00057000 0x2000"
    "dispatch_table_59000_ext 0x00059000 0x3000"
    "adjacent_code_data_5e000 0x0005e000 0x2000"
    "runtime_data_a9000 0x000a9000 0x4000"
    "task_slot_tables 0x000e1000 0x3000"
    "dispatch_table_ea000 0x000ea000 0x1000"
    "dispatch_table_ed000 0x000ed000 0x2000"
    "secondary_globals_4c7000 0x004c7000 0x1000"
    "adrp_globals_4e7000 0x004e7000 0x1000"
  )
elif [[ "$gap_targets" -eq 1 ]]; then
  ranges+=(
    "pre_code_context_40000 0x00040000 0x4000"
    "bridge_5c000_5e000 0x0005c000 0x2000"
    "post_threadx_strings_60000 0x00060000 0x4000"
    "scheduler_to_a9000_gap_a1000 0x000a1000 0x8000"
    "a9000_to_task_records_gap_ad000 0x000ad000 0xa000"
    "task_tail_to_slots_bridge_e0000 0x000e0000 0x1000"
    "post_task_slots_gap_e4000 0x000e4000 0xa000"
    "secondary_globals_wide_4c0000 0x004c0000 0x10000"
    "adrp_globals_wide_4e0000 0x004e0000 0x10000"
    "msg_pool_5c8000_continuation 0x005c8000 0x40000"
  )
elif [[ "$low_watermark" -eq 1 ]]; then
  probes+=(
    "low_probe_30000 0x00030000"
    "low_probe_20000 0x00020000"
    "low_probe_10000 0x00010000"
    "low_probe_08000 0x00008000"
    "low_probe_04000 0x00004000"
    "low_probe_02000 0x00002000"
    "low_probe_01000 0x00001000"
  )
elif [[ "$ram_size_probes" -eq 1 ]]; then
  probes+=(
    "known_amp_29b00000 0x29b00000"
    "known_rpmsg_39a00000 0x39a00000"
    "known_isgc_39b00000 0x39b00000"
    "candidate_ddr_high_3f000000 0x3f000000"
    "candidate_ddr_high_3ff00000 0x3ff00000"
    "candidate_ddr_last_page_3ffff000 0x3ffff000"
  )
elif [[ "$safe_fill_gaps" -eq 1 ]]; then
  add_chunked_range "fill_64000_9e000" 0x00064000 0x0009e000 0x10000
  ranges+=("fill_b7000_b7400 0x000b7000 0x400")
  add_chunked_range "fill_ef000_4c0000" 0x000ef000 0x004c0000 0x10000
  add_chunked_range "fill_4d0000_4e0000" 0x004d0000 0x004e0000 0x10000
  add_chunked_range "fill_4f0000_508000" 0x004f0000 0x00508000 0x10000
elif [[ "$only_risky_low" -eq 0 ]]; then
  ranges+=(
    "known_80000000 0x80000000 0x10000"
    "rpmsg_shared_head 0x39a00000 0x100000"
    "amp_isgc_shared_head 0x39b00000 0x100000"
    "amp_shared_head 0x29b00000 0x100000"
    "msg_pool_508000 0x00508000 0x20000"
    "msg_pool_528000 0x00528000 0x20000"
    "msg_pool_548000 0x00548000 0x20000"
    "msg_pool_568000 0x00568000 0x20000"
    "msg_pool_588000 0x00588000 0x20000"
    "msg_pool_5a8000 0x005a8000 0x20000"
    "ff80_descriptor_static_probe 0x0150b000 0x1000"
    "eis_gyro_strings_static_probe 0x011b8000 0x1000"
  )
fi

if [[ "$include_risky_low" -eq 1 ]]; then
  ranges+=(
    "threadx_scheduler_globals 0x0009e000 0x3000"
    "threadx_task_records 0x000b7320 0x29040"
    "threadx_task_record_ptrs 0x000ee4e0 0x800"
  )
elif [[ "$next_targets" -eq 0 && "$gap_targets" -eq 0 && "$low_watermark" -eq 0 && "$ram_size_probes" -eq 0 && "$safe_fill_gaps" -eq 0 ]]; then
  log "Skipping low ThreadX runtime ranges below 0x00100000. Use --include-risky-low to include them."
fi

log "session_dir=$session_dir"
confirm_ff80 preflight
ping_ff80 preflight_ping

if [[ "$low_watermark" -eq 1 || "$ram_size_probes" -eq 1 ]]; then
  if [[ "$low_watermark" -eq 1 ]]; then
    log "Low-watermark mode uses 16-byte reads only and intentionally skips 0x00000000."
  fi
  if [[ "$ram_size_probes" -eq 1 ]]; then
    log "RAM-size probe mode uses 16-byte reads only and pings after each probe."
    log "This is sparse map evidence, not a full contiguous RAM dump."
  fi
  for spec in "${probes[@]}"; do
    read -r label addr <<<"$spec"
    if ! probe_address "$label" "$addr"; then
      if ! ping_ff80 "${label}_failure_recovery_ping"; then
        log "FF80 ping failed after failed probe; stopping."
        confirm_ff80 postflight || true
        log "Summary: $summary"
        cat "$summary" | tee -a "$manifest" >&2
        exit 1
      fi
      if [[ "$stop_on_fail" -eq 1 ]]; then
        log "Stopping after failed probe: $label"
        confirm_ff80 postflight || true
        log "Summary: $summary"
        cat "$summary" | tee -a "$manifest" >&2
        exit 1
      fi
    fi
  done
  confirm_ff80 postflight
  log "Summary: $summary"
  cat "$summary" | tee -a "$manifest" >&2
  exit 0
fi

if [[ "$safe_fill_gaps" -eq 1 ]]; then
  log "Safe-fill mode deliberately skips hazardous 0x00002000..0x00040000."
fi

for spec in "${ranges[@]}"; do
  read -r label addr size <<<"$spec"
  if ! dump_range "$label" "$addr" "$size"; then
    if [[ "$stop_on_fail" -eq 1 ]]; then
      log "Stopping after failed range: $label"
      exit 1
    fi
    if ! ping_ff80 "${label}_failure_recovery_ping"; then
      log "FF80 ping failed after failed range; stopping despite --continue-on-fail."
      exit 1
    fi
  fi
done

confirm_ff80 postflight
log "Summary: $summary"
cat "$summary" | tee -a "$manifest" >&2
