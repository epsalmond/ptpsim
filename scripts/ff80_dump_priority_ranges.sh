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
  --ram-16gb-probes     Probe sparse 32-bit aperture boundaries with 16-byte
                        reads to test the 16 GB hardware hypothesis limits.
  --bootrom-recon-probes
                        Probe likely bootrom/MMIO high-zone addresses with
                        16-byte reads; dump 64 KiB only for non-zero/non-FF
                        probe bytes.
  --drht-code-pages     Probe DRHT-derived code pages with 16-byte reads; dump
                        64 KiB only for non-zero/non-FF probe bytes.
  --updatedat-followup  Dump updatedat follow-up code/data pages needed to
                        chase verifier callees and update-task globals.
  --updatedat-constants
                        Dump updatedat constant/table pages referenced by the
                        dispatcher and verifier-adjacent routines.
  --updatedat-subdispatcher
                        Dump the bounded 0x032b72cc + 0x4000 updatedat
                        subdispatcher window.
  --verifier-bypass-followup
                        Dump mandatory F-0011 verifier-bypass follow-up
                        getter, 0x53d1 gate-table, and IPC/helper ranges.
  --include-verifier-bypass-optional
                        Include optional F-0011 follow-up ranges at
                        0x02d40000 and 0x023a0000. Implies
                        --verifier-bypass-followup.
  --f0011-upstream-followup
                        Dump mandatory F-0011 upstream-caller follow-up state:
                        0x02608000 + 0x1000 for Getter B bitmask context.
  --include-f0011-upstream-contingent
                        Include all Tier 2 F-0011 upstream code extensions.
                        Implies --f0011-upstream-followup.
  --include-f0011-upstream-03200000
  --include-f0011-upstream-03240000
  --include-f0011-upstream-03260000
  --include-f0011-upstream-031c0000
                        Include one Tier 2 F-0011 upstream code extension.
                        Each implies --f0011-upstream-followup.
  --known-syslogs       Dump the known syslog buffer headers as bounded RAM
                        reads. Includes the five canonical headers plus the
                        later 0x00507000 candidate from safe-fill analysis.
  --linux-kernel-hunt   Dump the first 6 MiB of the documented Linux RAM
                        window, 0x08000000..0x08600000, in 64 KiB chunks.
  --include-wedging-fffff000
                        Include 0xfffff000 in --ram-16gb-probes. This boundary
                        timed out live and wedged FF80 ping until cold boot.
                        Also allows --bootrom-recon-probes to dump through the
                        final 0xfffff000 page.
  --include-wedging-fffc0000
                        Include 0xfffc0000 in --bootrom-recon-probes. This
                        address timed out live on a 16-byte read and wedged
                        FF80 ping until cold boot, so it is skipped by default.
  --include-wedging-fff00000
                        Include 0xfff00000 in --bootrom-recon-probes. This
                        address timed out live on a 16-byte read and wedged
                        FF80 ping until cold boot, so it is skipped by default.
  --include-wedging-ffe00000
                        Include 0xffe00000 in --bootrom-recon-probes. This
                        address timed out live on a 16-byte read and wedged
                        FF80 ping until cold boot, so it is skipped by default.
  --skip-address HEX    Skip any queued probe/range with this start address.
                        May be repeated. Useful for batching crash findings
                        without editing this script after every wedge.
  --skip-address-file PATH
                        Read additional skip addresses from a text file. Blank
                        lines and # comments are ignored.
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
ram_16gb_probes=0
bootrom_recon_probes=0
drht_code_pages=0
updatedat_followup=0
updatedat_constants=0
updatedat_subdispatcher=0
verifier_bypass_followup=0
include_verifier_bypass_optional=0
f0011_upstream_followup=0
include_f0011_upstream_03200000=0
include_f0011_upstream_03240000=0
include_f0011_upstream_03260000=0
include_f0011_upstream_031c0000=0
known_syslogs=0
linux_kernel_hunt=0
include_wedging_fffff000=0
include_wedging_fffc0000=0
include_wedging_fff00000=0
include_wedging_ffe00000=0
safe_fill_gaps=0
stop_on_fail=1
skip_addresses=()
skip_address_files=()

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
    --ram-16gb-probes)
      ram_16gb_probes=1
      shift
      ;;
    --bootrom-recon-probes)
      bootrom_recon_probes=1
      shift
      ;;
    --drht-code-pages)
      drht_code_pages=1
      shift
      ;;
    --updatedat-followup)
      updatedat_followup=1
      shift
      ;;
    --updatedat-constants)
      updatedat_constants=1
      shift
      ;;
    --updatedat-subdispatcher)
      updatedat_subdispatcher=1
      shift
      ;;
    --verifier-bypass-followup)
      verifier_bypass_followup=1
      shift
      ;;
    --include-verifier-bypass-optional)
      verifier_bypass_followup=1
      include_verifier_bypass_optional=1
      shift
      ;;
    --f0011-upstream-followup)
      f0011_upstream_followup=1
      shift
      ;;
    --include-f0011-upstream-contingent)
      f0011_upstream_followup=1
      include_f0011_upstream_03200000=1
      include_f0011_upstream_03240000=1
      include_f0011_upstream_03260000=1
      include_f0011_upstream_031c0000=1
      shift
      ;;
    --include-f0011-upstream-03200000)
      f0011_upstream_followup=1
      include_f0011_upstream_03200000=1
      shift
      ;;
    --include-f0011-upstream-03240000)
      f0011_upstream_followup=1
      include_f0011_upstream_03240000=1
      shift
      ;;
    --include-f0011-upstream-03260000)
      f0011_upstream_followup=1
      include_f0011_upstream_03260000=1
      shift
      ;;
    --include-f0011-upstream-031c0000)
      f0011_upstream_followup=1
      include_f0011_upstream_031c0000=1
      shift
      ;;
    --known-syslogs)
      known_syslogs=1
      shift
      ;;
    --linux-kernel-hunt)
      linux_kernel_hunt=1
      shift
      ;;
    --include-wedging-fffff000)
      include_wedging_fffff000=1
      shift
      ;;
    --include-wedging-fffc0000)
      include_wedging_fffc0000=1
      shift
      ;;
    --include-wedging-fff00000)
      include_wedging_fff00000=1
      shift
      ;;
    --include-wedging-ffe00000)
      include_wedging_ffe00000=1
      shift
      ;;
    --skip-address)
      skip_addresses+=("$2")
      shift 2
      ;;
    --skip-address-file)
      skip_address_files+=("$2")
      shift 2
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

case "$session_dir" in
  /*) ;;
  *) session_dir="$repo_root/$session_dir" ;;
esac

for skip_address_file in "${skip_address_files[@]}"; do
  if [[ ! -f "$skip_address_file" ]]; then
    echo "missing skip address file: $skip_address_file" >&2
    exit 1
  fi
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%%#*}"
    line="${line//[[:space:]]/}"
    if [[ -n "$line" ]]; then
      skip_addresses+=("$line")
    fi
  done <"$skip_address_file"
done

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

should_skip_address() {
  local addr="$1"
  local addr_value="$(( addr ))"
  local skip
  for skip in "${skip_addresses[@]}"; do
    if [[ "$(( skip ))" -eq "$addr_value" ]]; then
      return 0
    fi
  done
  return 1
}

record_runtime_skip() {
  local label="$1"
  local addr="$2"
  local size="$3"
  local addr_name
  addr_name="$(addr_token "$addr")"
  log "Skipping $label addr=0x$addr_name due to runtime skip address."
  record "$label" "$addr" "$size" "" "skipped" "" "" "runtime_skip_address"
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

probe_then_dump_if_interesting() {
  local label="$1"
  local addr="$2"
  local dump_size="$3"
  local addr_name
  local end_name
  addr_name="$(addr_token "$addr")"
  end_name="$(range_end_token "$addr" "$dump_size")"
  local probe_path="$session_dir/probes/${label}_${addr_name}_probe_10.bin"
  local dump_path="$session_dir/dumps/${label}_${addr_name}_${end_name}.bin"

  log "=== $label addr=0x$addr_name probe_size=0x10 conditional_dump=0x$(printf '%x' "$(( dump_size ))") ==="

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

  local probe_hex
  local probe_sha
  local probe_bytes
  probe_hex="$(od -An -tx1 -v "$probe_path" | tr -d ' \n')"
  probe_sha="$(shasum -a 256 "$probe_path" | awk '{print $1}')"
  probe_bytes="$(file_size_bytes "$probe_path")"
  if [[ "$probe_hex" == "00000000000000000000000000000000" ]]; then
    record "$label" "$addr" 0x10 "$probe_bytes" "ok" "$probe_path" "$probe_sha" "probe_all_zero_skip_dump"
    ls -l "$probe_path" | tee -a "$manifest" >&2
    return 0
  fi
  if [[ "$probe_hex" == "ffffffffffffffffffffffffffffffff" ]]; then
    record "$label" "$addr" 0x10 "$probe_bytes" "ok" "$probe_path" "$probe_sha" "probe_all_ff_skip_dump"
    ls -l "$probe_path" | tee -a "$manifest" >&2
    return 0
  fi

  log "Probe was non-zero/non-FF; dumping 0x$(printf '%x' "$(( dump_size ))") bytes."
  if ! run_ff80_logged "${label}_dump" ram dump "0x$addr_name" -s "0x$(printf '%x' "$(( dump_size ))")" -o "$dump_path"; then
    record "$label" "$addr" "$dump_size" "" "failed" "$dump_path" "" "dump_failed"
    return 1
  fi

  if ! ping_ff80 "${label}_post_dump_ping"; then
    record "$label" "$addr" "$dump_size" "" "failed" "$dump_path" "" "post_dump_ping_failed"
    return 1
  fi

  local dump_sha
  local actual_bytes
  local note
  dump_sha="$(shasum -a 256 "$dump_path" | awk '{print $1}')"
  actual_bytes="$(file_size_bytes "$dump_path")"
  note="dumped_after_interesting_probe"
  if [[ "$actual_bytes" -ne "$(( dump_size ))" ]]; then
    note="dumped_after_interesting_probe_actual_size_differs_from_requested"
  fi
  record "$label" "$addr" "$dump_size" "$actual_bytes" "ok" "$dump_path" "$dump_sha" "$note"
  ls -l "$probe_path" "$dump_path" | tee -a "$manifest" >&2
}

ranges=()
probes=()
conditional_probes=()

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
elif [[ "$ram_16gb_probes" -eq 1 ]]; then
  probes+=(
    "known_512m_window_last_page_3ffff000 0x3ffff000"
    "known_boot_window_80000000 0x80000000"
    "candidate_after_512m_window_40000000 0x40000000"
    "candidate_2g_minus_page_7ffff000 0x7ffff000"
    "candidate_3g_minus_page_bffff000 0xbffff000"
  )
  if [[ "$include_wedging_fffff000" -eq 1 ]]; then
    probes+=("known_wedging_4g_minus_page_fffff000 0xfffff000")
  fi
elif [[ "$bootrom_recon_probes" -eq 1 ]]; then
  final_64k_dump_size=0xf000
  if [[ "$include_wedging_fffff000" -eq 1 ]]; then
    final_64k_dump_size=0x10000
  fi
  if [[ "$include_wedging_fffc0000" -eq 1 ]]; then
    conditional_probes+=("known_wedging_bootrom_top_256k_fffc0000 0xfffc0000 0x10000")
  fi
  if [[ "$include_wedging_fff00000" -eq 1 ]]; then
    conditional_probes+=("known_wedging_bootrom_top_1m_fff00000 0xfff00000 0x10000")
  fi
  if [[ "$include_wedging_ffe00000" -eq 1 ]]; then
    conditional_probes+=("known_wedging_bootrom_top_2m_ffe00000 0xffe00000 0x10000")
  fi
  conditional_probes+=(
    "bootrom_final_64k_ffff0000 0xffff0000 $final_64k_dump_size"
    "bootrom_high_zone_start_f8000000 0xf8000000 0x10000"
    "bootrom_high_zone_mid_fc000000 0xfc000000 0x10000"
    "bootrom_high_zone_mid_fd000000 0xfd000000 0x10000"
    "bootrom_high_zone_mid_fe000000 0xfe000000 0x10000"
    "bootrom_upper_kernel_c0000000 0xc0000000 0x10000"
    "bootrom_mid_dram_40000000 0x40000000 0x10000"
  )
elif [[ "$drht_code_pages" -eq 1 ]]; then
  conditional_probes+=(
    "drht_fanin_011e0000 0x011e0000 0x10000"
    "drht_fanin_01590000 0x01590000 0x10000"
    "drht_fanin_015d0000 0x015d0000 0x10000"
    "drht_fanin_031e0000 0x031e0000 0x10000"
    "drht_fanin_03210000 0x03210000 0x10000"
    "drht_fanin_03230000 0x03230000 0x10000"
    "drht_fanin_03250000 0x03250000 0x10000"
    "drht_fanin_034f0000 0x034f0000 0x10000"
    "drht_fanin_03520000 0x03520000 0x10000"
    "drht_outlier_05fb0000 0x05fb0000 0x10000"
    "drht_outlier_05fe0000 0x05fe0000 0x10000"
    "drht_outlier_068b0000 0x068b0000 0x10000"
    "drht_outlier_068d0000 0x068d0000 0x10000"
    "drht_outlier_06920000 0x06920000 0x10000"
    "drht_outlier_06930000 0x06930000 0x10000"
    "drht_outlier_06940000 0x06940000 0x10000"
    "drht_outlier_069a0000 0x069a0000 0x10000"
    "drht_outlier_069b0000 0x069b0000 0x10000"
  )
elif [[ "$updatedat_followup" -eq 1 ]]; then
  ranges+=(
    "updatedat_crypto_callees_02d20000 0x02d20000 0x10000"
    "updatedat_crypto_callees_02d50000 0x02d50000 0x10000"
    "updatedat_continuation_032c0000 0x032c0000 0x10000"
    "updatedat_status_globals_04538000 0x04538000 0x20000"
    "updatedat_callback_globals_04730000 0x04730000 0x20000"
  )
elif [[ "$updatedat_constants" -eq 1 ]]; then
  ranges+=(
    "updatedat_constants_0355f000 0x0355f000 0x10000"
    "updatedat_constants_03561000 0x03561000 0x10000"
    "updatedat_constants_03563000 0x03563000 0x10000"
    "updatedat_constants_037a8000 0x037a8000 0x10000"
    "updatedat_constants_0381a000 0x0381a000 0x10000"
  )
elif [[ "$updatedat_subdispatcher" -eq 1 ]]; then
  ranges+=(
    "updatedat_subdispatcher_032b72cc 0x032b72cc 0x4000"
  )
elif [[ "$verifier_bypass_followup" -eq 1 ]]; then
  ranges+=(
    "f0011_cfgdata_getter_01588000 0x01588000 0x8000"
    "f0011_second_getter_015dc000 0x015dc000 0x4000"
    "f0011_gate_53d1_table_047f8000 0x047f8000 0x1000"
    "f0011_threadx_ipc_primitives_015c0000 0x015c0000 0x10000"
  )
  if [[ "$include_verifier_bypass_optional" -eq 1 ]]; then
    ranges+=(
      "f0011_crypto_gap_02d40000 0x02d40000 0x10000"
      "f0011_firmware_source_candidate_023a0000 0x023a0000 0x10000"
    )
  fi
elif [[ "$f0011_upstream_followup" -eq 1 ]]; then
  ranges+=(
    "f0011_getter_b_bitmask_02608000 0x02608000 0x1000"
  )
  if [[ "$include_f0011_upstream_031c0000" -eq 1 ]]; then
    ranges+=("f0011_upstream_code_031c0000 0x031c0000 0x10000")
  fi
  if [[ "$include_f0011_upstream_03200000" -eq 1 ]]; then
    ranges+=("f0011_upstream_code_03200000 0x03200000 0x10000")
  fi
  if [[ "$include_f0011_upstream_03240000" -eq 1 ]]; then
    ranges+=("f0011_upstream_code_03240000 0x03240000 0x10000")
  fi
  if [[ "$include_f0011_upstream_03260000" -eq 1 ]]; then
    ranges+=("f0011_upstream_code_03260000 0x03260000 0x10000")
  fi
elif [[ "$known_syslogs" -eq 1 ]]; then
  ranges+=(
    "syslog_secondary_globals_4c7000 0x004c7000 0x1000"
    "syslog_adrp_globals_4e7000 0x004e7000 0x1000"
    "syslog_safe_fill_candidate_507000 0x00507000 0x1000"
    "syslog_msg_pool_527000 0x00527000 0x1000"
    "syslog_msg_pool_547000 0x00547000 0x1000"
    "syslog_msg_pool_567000 0x00567000 0x1000"
  )
elif [[ "$linux_kernel_hunt" -eq 1 ]]; then
  add_chunked_range "linux_kernel_hunt_08000000_08600000" 0x08000000 0x08600000 0x10000
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
elif [[ "$next_targets" -eq 0 && "$gap_targets" -eq 0 && "$low_watermark" -eq 0 && "$ram_size_probes" -eq 0 && "$ram_16gb_probes" -eq 0 && "$bootrom_recon_probes" -eq 0 && "$drht_code_pages" -eq 0 && "$updatedat_followup" -eq 0 && "$updatedat_constants" -eq 0 && "$updatedat_subdispatcher" -eq 0 && "$verifier_bypass_followup" -eq 0 && "$f0011_upstream_followup" -eq 0 && "$known_syslogs" -eq 0 && "$linux_kernel_hunt" -eq 0 && "$safe_fill_gaps" -eq 0 ]]; then
  log "Skipping low ThreadX runtime ranges below 0x00100000. Use --include-risky-low to include them."
fi

log "session_dir=$session_dir"
confirm_ff80 preflight
ping_ff80 preflight_ping

if [[ "$low_watermark" -eq 1 || "$ram_size_probes" -eq 1 || "$ram_16gb_probes" -eq 1 ]]; then
  if [[ "$low_watermark" -eq 1 ]]; then
    log "Low-watermark mode uses 16-byte reads only and intentionally skips 0x00000000."
  fi
  if [[ "$ram_size_probes" -eq 1 ]]; then
    log "RAM-size probe mode uses 16-byte reads only and pings after each probe."
    log "This is sparse map evidence, not a full contiguous RAM dump."
  fi
  if [[ "$ram_16gb_probes" -eq 1 ]]; then
    log "16 GB hypothesis mode uses 16-byte reads only and pings after each probe."
    log "The current FF80 RAM API encodes only a 32-bit address, so this tests"
    log "visible 32-bit aperture boundaries; it cannot directly prove RAM above 4 GB."
    if [[ "$include_wedging_fffff000" -eq 0 ]]; then
      log "Skipping known-wedging 0xfffff000. Use --include-wedging-fffff000 only intentionally."
    fi
  fi
  for spec in "${probes[@]}"; do
    read -r label addr <<<"$spec"
    if should_skip_address "$addr"; then
      record_runtime_skip "$label" "$addr" 0x10
      continue
    fi
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

if [[ "$bootrom_recon_probes" -eq 1 || "$drht_code_pages" -eq 1 ]]; then
  if [[ "$bootrom_recon_probes" -eq 1 ]]; then
    log "Bootrom recon mode probes likely high-zone bootrom addresses."
    log "It only dumps when the 16-byte probe is neither all zero nor all FF."
    if [[ "$include_wedging_fffc0000" -eq 0 ]]; then
      log "Skipping known-wedging 0xfffc0000. Use --include-wedging-fffc0000 only intentionally."
    fi
    if [[ "$include_wedging_fff00000" -eq 0 ]]; then
      log "Skipping known-wedging 0xfff00000. Use --include-wedging-fff00000 only intentionally."
    fi
    if [[ "$include_wedging_ffe00000" -eq 0 ]]; then
      log "Skipping known-wedging 0xffe00000. Use --include-wedging-ffe00000 only intentionally."
    fi
    if [[ "$include_wedging_fffff000" -eq 0 ]]; then
      log "The 0xffff0000 conditional dump is limited to 0xf000 bytes to avoid known-wedging 0xfffff000."
    fi
  else
    log "DRHT code-page mode probes DRHT-derived entry pages."
    log "It only dumps when the 16-byte probe is neither all zero nor all FF."
  fi
  for spec in "${conditional_probes[@]}"; do
    read -r label addr dump_size <<<"$spec"
    if should_skip_address "$addr"; then
      record_runtime_skip "$label" "$addr" 0x10
      continue
    fi
    if ! probe_then_dump_if_interesting "$label" "$addr" "$dump_size"; then
      if ! ping_ff80 "${label}_failure_recovery_ping"; then
        log "FF80 ping failed after failed conditional probe; stopping."
        confirm_ff80 postflight || true
        log "Summary: $summary"
        cat "$summary" | tee -a "$manifest" >&2
        exit 1
      fi
      if [[ "$stop_on_fail" -eq 1 ]]; then
        log "Stopping after failed conditional probe: $label"
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
  if should_skip_address "$addr"; then
    record_runtime_skip "$label" "$addr" "$size"
    continue
  fi
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
