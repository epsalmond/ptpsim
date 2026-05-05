#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ff80_drht_entry_sweep.sh [options]

Options:
  --session-dir DIR     Output directory. Default:
                        rce/sessions/ff80_drht_entry_sweep_<utc timestamp>.
  --lock-path PATH      Camera USB lock path. Default:
                        $HOME/fuji/locks/camera-usb.lock.
  --vendor-id HEX       USB vendor id. Default: 0x04cb.
  --product-id HEX      USB product id. Default: 0xff80.
  --recipient NAME      USB request recipient: device, endpoint, interface,
                        or other. Default: other.
  -h, --help            Show this help.

Read-only FF80 DRHT task entry sweep:
  1. Reads +0x158 name, +0x178 entry_fn, and +0x198 entry_arg for 178 records.
  2. Writes entry_fn_map.tsv.
  3. Dumps 64 KiB at page-aligned updatedat and Linux_loa entry functions.

No RAM writes, cfgdata writes, hack load/exec, key injection, or firmware
operations are used. The script pings after each read and stops if ping fails
twice.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-$repo_root/.venv/bin/python}"

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

lock_path="${CAMERA_USB_LOCK_PATH:-$HOME/fuji/locks/camera-usb.lock}"
vendor_id="0x04cb"
product_id="0xff80"
recipient="${FUJI_FF80_RECIPIENT:-other}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --session-dir)
      session_dir="$2"
      shift 2
      ;;
    --lock-path)
      lock_path="$2"
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

if [[ ! -d "$ff80_dir" ]]; then
  echo "missing FF80 reference directory: $ff80_dir" >&2
  exit 1
fi

mkdir -p "$session_dir" "$(dirname "$lock_path")"

if ! mkdir "$lock_path" 2>/dev/null; then
  echo "camera USB lock is held or unavailable: $lock_path" >&2
  exit 1
fi
trap 'rmdir "$lock_path" 2>/dev/null || true' EXIT

export FUJI_FF80_SESSION_DIR="$session_dir"
export FUJI_FF80_DIR="$ff80_dir"
export FUJI_FF80_VENDOR_ID="$vendor_id"
export FUJI_FF80_PRODUCT_ID="$product_id"
export FUJI_FF80_RECIPIENT="$recipient"

"$python_bin" - <<'PY'
from __future__ import annotations

import csv
import json
import os
import struct
import sys
from dataclasses import dataclass
from pathlib import Path

session_dir = Path(os.environ["FUJI_FF80_SESSION_DIR"])
ff80_dir = Path(os.environ["FUJI_FF80_DIR"])
vendor_id = int(os.environ["FUJI_FF80_VENDOR_ID"], 0)
product_id = int(os.environ["FUJI_FF80_PRODUCT_ID"], 0)
recipient_name = os.environ["FUJI_FF80_RECIPIENT"]

sys.path.insert(0, str(ff80_dir))

import usb1  # type: ignore
import fftlib  # type: ignore
import ffjlib  # type: ignore

RECIPIENTS = {
    "device": usb1.RECIPIENT_DEVICE,
    "interface": usb1.RECIPIENT_INTERFACE,
    "endpoint": usb1.RECIPIENT_ENDPOINT,
    "other": usb1.RECIPIENT_OTHER,
}

DRHT_ADDRS = [
    0x76320, 0x7afb0, 0x7b1e0, 0x7b410, 0x7b640, 0x7b870, 0x7baa0, 0x7bcd0, 0x7bf00,
    0x7c130, 0x7c360, 0x7c590, 0x7c7c0, 0x7c9f0, 0x7cc20, 0x7ce50, 0x7d080, 0x7d2b0,
    0x7d4e0, 0x7d710, 0x7d940, 0x7db70, 0x7dda0, 0x7dfd0, 0x7e200, 0x7e430, 0x7e660,
    0x7e890, 0x7eac0, 0x7ecf0, 0x7ef20, 0x7f150, 0x7f380, 0x7f5b0, 0x7f7e0, 0x7fa10,
    0x7fc40, 0x7fe70, 0x800a0, 0x802d0, 0x80500, 0x80730, 0x80960, 0x80b90, 0x80dc0,
    0x80ff0, 0x81220, 0x81450, 0x81680, 0x818b0, 0x81ae0, 0x81f40, 0x82170, 0x823a0,
    0x825d0, 0x82800, 0x82a30, 0x82c60, 0x82e90, 0x830c0, 0x832f0, 0x83520, 0x83750,
    0x83980, 0x83de0, 0x84010, 0x84240, 0x84470, 0x846a0, 0x848d0, 0x84b00, 0x84d30,
    0x84f60, 0x85190, 0x853c0, 0x855f0, 0x85820, 0x85a50, 0x85c80, 0x869a0, 0x86bd0,
    0x86e00, 0x87260, 0x878f0, 0x87b20, 0x87d50, 0x883e0, 0x88610, 0x88840, 0x88a70,
    0x88ca0, 0x88ed0, 0x89100, 0x89330, 0x89560, 0x89790, 0x899c0, 0x89bf0, 0x89e20,
    0x8a050, 0x8a280, 0x8a4b0, 0x8a6e0, 0x8a910, 0x8ab40, 0x8ad70, 0x8afa0, 0x8b630,
    0x8bcc0, 0x8bef0, 0x8c120, 0x8c350, 0x8c580, 0x8c7b0, 0x8c9e0, 0x8cc10, 0x8ce40,
    0x8d4d0, 0x8d700, 0x8d930, 0x8db60, 0x8dd90, 0x8dfc0, 0x8e420, 0x8e880, 0x8eab0,
    0x8ece0, 0x8ef10, 0x8f140, 0x8f370, 0x8f5a0, 0x8fa00, 0x8fc30, 0x8fe60, 0x90090,
    0x902c0, 0x904f0, 0x90720, 0x90950, 0x90db0, 0x90fe0, 0x91210, 0x91440, 0x91670,
    0x918a0, 0x91ad0, 0x91d00, 0x91f30, 0x92160, 0x92390, 0x925c0, 0x927f0, 0x92a20,
    0x92c50, 0x92e80, 0x930b0, 0x932e0, 0x93510, 0x93740, 0x93970, 0x93ba0, 0x93dd0,
    0x94000, 0x94230, 0x94460, 0x94690, 0x948c0, 0x94d20, 0x94f50, 0x95180, 0x953b0,
    0x955e0, 0x95810, 0x97f70, 0x981a0, 0x983d0, 0x98600, 0x98830,
]

EXPECTED_RECORD_COUNT = 178
if len(DRHT_ADDRS) != EXPECTED_RECORD_COUNT:
    raise SystemExit(f"internal address-list error: expected {EXPECTED_RECORD_COUNT}, got {len(DRHT_ADDRS)}")

manifest = session_dir / "manifest.txt"
summary_json = session_dir / "summary.json"
report_txt = session_dir / "report.txt"
entry_tsv = session_dir / "entry_fn_map.tsv"
entry_dir = session_dir / "entry_fns"
arg_dir = session_dir / "entry_args"
name_dir = session_dir / "names"
dump_dir = session_dir / "dumps"
probe_dir = session_dir / "probes"
for directory in (entry_dir, arg_dir, name_dir, dump_dir, probe_dir):
    directory.mkdir(parents=True, exist_ok=True)


def log(message: str) -> None:
    print(message)
    with manifest.open("a", encoding="utf-8") as out:
        out.write(message + "\n")


def is_forbidden_read(addr: int, size: int) -> str | None:
    end = addr + size
    if addr == 0:
        return "toxic low address 0x00000000"
    if addr in {0xFFFC0000, 0xFFF00000, 0xFFE00000}:
        return "known-wedging high MMIO/ROM address"
    if 0xF0000000 <= addr <= 0xFFFFFFFF:
        return "this run skips all 0xf0000000..0xffffffff addresses"
    if addr < 0xFB000000 and end > 0xFA000000:
        return "overlaps active 0xfa000000..0xfaffffff MMIO range"
    if addr < 0xFFA00000 and end > 0xFF900000:
        return "overlaps active 0xff900000..0xff9fffff MMIO range"
    return None


def clean_name(raw: bytes) -> str:
    text = raw.split(b"\x00", 1)[0].decode("ascii", errors="replace")
    return text.strip(" \x00")


def hex64(value: int) -> str:
    return f"0x{value:016x}"


@dataclass
class ReadAttempt:
    label: str
    addr: int
    size: int


class StopSweep(Exception):
    pass


class Sweep:
    def __init__(self) -> None:
        self.last_attempt: ReadAttempt | None = None
        self.ping_failures: list[dict] = []
        self.read_failures: list[dict] = []

    def ping_or_stop(self, jig, label: str) -> None:
        for attempt in (1, 2):
            try:
                ping = jig.ftl.ping().hex()
                log(f"ping_ok {label} attempt={attempt} ping={ping}")
                return
            except Exception as exc:
                detail = f"{type(exc).__name__}: {exc}"
                log(f"ping_failed {label} attempt={attempt} {detail}")
                self.ping_failures.append(
                    {
                        "label": label,
                        "attempt": attempt,
                        "error": detail,
                        "last_read": self.last_attempt.__dict__ if self.last_attempt else None,
                    }
                )
        raise StopSweep(f"ping failed twice after {label}; last_read={self.last_attempt}")

    def read_ram(self, jig, label: str, addr: int, size: int) -> bytes:
        forbidden = is_forbidden_read(addr, size)
        if forbidden:
            raise RuntimeError(f"refusing read {label} at 0x{addr:08x} size=0x{size:x}: {forbidden}")
        self.last_attempt = ReadAttempt(label, addr, size)
        log(f"+ read {label} addr=0x{addr:08x} size=0x{size:x}")
        try:
            data = jig.debug_read_ram(addr, size)
        except Exception as exc:
            detail = f"{type(exc).__name__}: {exc}"
            self.read_failures.append({"label": label, "addr": addr, "size": size, "error": detail})
            log(f"read_failed {label} addr=0x{addr:08x} size=0x{size:x} {detail}")
            raise
        if len(data) != size:
            raise RuntimeError(f"short read {label}: expected {size}, got {len(data)}")
        self.ping_or_stop(jig, f"after_{label}")
        return data


def dump_if_interesting(sweep: Sweep, jig, label: str, start: int) -> dict:
    probe = sweep.read_ram(jig, f"{label}_probe", start, 0x10)
    probe_path = probe_dir / f"{label}_{start:08x}_probe_10.bin"
    probe_path.write_bytes(probe)
    all_zero = probe == b"\x00" * len(probe)
    all_ff = probe == b"\xff" * len(probe)
    result = {
        "label": label,
        "start": start,
        "probe_hex": probe.hex(),
        "probe_path": str(probe_path),
        "dump_path": None,
        "dumped": False,
        "reason": None,
    }
    if all_zero or all_ff:
        result["reason"] = "all_zero" if all_zero else "all_ff"
        log(f"{label}_skip_dump reason={result['reason']} start=0x{start:08x}")
        return result
    data = sweep.read_ram(jig, f"{label}_dump", start, 0x10000)
    dump_path = dump_dir / f"{label}_{start:08x}.bin"
    dump_path.write_bytes(data)
    result["dump_path"] = str(dump_path)
    result["dumped"] = True
    return result


summary: dict = {
    "session_dir": str(session_dir),
    "record_count": len(DRHT_ADDRS),
    "status": "started",
    "entry_range_count": 0,
    "anomalies": [],
    "jobs": {},
    "ping_failures": [],
    "read_failures": [],
}

log(f"session_dir={session_dir}")
log(f"target {vendor_id:04x}:{product_id:04x} recipient={recipient_name}")

with usb1.USBContext() as context:
    usb_h = context.openByVendorIDAndProductID(vendor_id, product_id, skip_on_error=True)
    if usb_h is None:
        raise SystemExit(f"device {vendor_id:04x}:{product_id:04x} not found")

    fft = fftlib.ftl(usb_h, recipient=RECIPIENTS[recipient_name])
    jig = ffjlib.jig(fft)
    sweep = Sweep()

    try:
        with usb_h.claimInterface(0):
            fft.open_session()
            try:
                sweep.ping_or_stop(jig, "preflight")
                usb_debug = jig.get_config_usb_debug()
                log(f"usb_debug_cfg_f7=0x{usb_debug:02x} read_only_check")
                summary["usb_debug_cfg_f7"] = usb_debug
                if usb_debug == 0:
                    raise RuntimeError(
                        "USB debug cfg byte 0xf7 is 0; strict read-only sweep cannot enable it. "
                        "Stock ff80.py ram read would cfgdata-write this byte before reading."
                    )

                rows: list[dict] = []
                for index, drht_addr in enumerate(DRHT_ADDRS, start=1):
                    token = f"{drht_addr:08x}"
                    name = sweep.read_ram(jig, f"record_{index:03d}_name_{token}", drht_addr + 0x158, 16)
                    entry = sweep.read_ram(jig, f"record_{index:03d}_entry_{token}", drht_addr + 0x178, 8)
                    arg = sweep.read_ram(jig, f"record_{index:03d}_arg_{token}", drht_addr + 0x198, 8)
                    (entry_fn,) = struct.unpack("<Q", entry)
                    (entry_arg,) = struct.unpack("<Q", arg)
                    name_text = clean_name(name)
                    name_dir.joinpath(f"{token}.bin").write_bytes(name)
                    entry_dir.joinpath(f"{token}.bin").write_bytes(entry)
                    arg_dir.joinpath(f"{token}.bin").write_bytes(arg)
                    row = {
                        "drht_addr": f"0x{drht_addr:08x}",
                        "name": name_text,
                        "entry_fn": hex64(entry_fn),
                        "entry_arg": hex64(entry_arg),
                    }
                    rows.append(row)
                    if entry_fn == 0 or entry_fn == 0xFFFFFFFFFFFFFFFF:
                        summary["anomalies"].append({**row, "reason": "null_or_all_ff_entry_fn"})
                    elif not (0x01000000 <= entry_fn <= 0x03FFFFFF):
                        summary["anomalies"].append({**row, "reason": "entry_fn_out_of_expected_range"})
                    if index % 10 == 0:
                        sweep.ping_or_stop(jig, f"job1_record_{index:03d}_pace")

                with entry_tsv.open("w", encoding="utf-8", newline="") as out:
                    writer = csv.DictWriter(out, fieldnames=["drht_addr", "name", "entry_fn", "entry_arg"], delimiter="\t")
                    writer.writeheader()
                    writer.writerows(rows)

                summary["entry_range_count"] = sum(
                    1 for row in rows if 0x01000000 <= int(row["entry_fn"], 16) <= 0x03FFFFFF
                )
                summary["jobs"]["entry_fn_map"] = str(entry_tsv)

                row_by_addr = {int(row["drht_addr"], 16): row for row in rows}
                for label, drht_addr in (("updatedat_entry", 0x95810), ("linux_loa_entry", 0x92E80)):
                    entry = int(row_by_addr[drht_addr]["entry_fn"], 16)
                    if not (0x01000000 <= entry <= 0x03FFFFFF):
                        raise RuntimeError(
                            f"{label} entry pointer out of expected range: drht=0x{drht_addr:08x} entry=0x{entry:x}"
                        )
                    start = entry & ~0xFFF
                    result = dump_if_interesting(sweep, jig, label, start)
                    result["entry"] = entry
                    result["drht_addr"] = drht_addr
                    summary["jobs"][label] = result

                summary["status"] = "ok"
            finally:
                try:
                    fft.close_session()
                except Exception as exc:
                    log(f"close_session_failed {type(exc).__name__}: {exc}")
    except StopSweep as exc:
        summary["status"] = "stopped"
        summary["stop_reason"] = str(exc)
    except Exception as exc:
        summary["status"] = "failed"
        summary["error"] = f"{type(exc).__name__}: {exc}"
        raise
    finally:
        summary["ping_failures"] = sweep.ping_failures
        summary["read_failures"] = sweep.read_failures
        summary_json = session_dir / "summary.json"
        summary_json.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

report_lines = [
    f"status={summary['status']}",
    f"entry_fn_map={entry_tsv}",
    f"entry_range_count={summary['entry_range_count']}",
    f"anomalies={len(summary['anomalies'])}",
]
for label in ("updatedat_entry", "linux_loa_entry"):
    job = summary["jobs"].get(label)
    if not isinstance(job, dict):
        continue
    first32 = ""
    if job.get("dump_path"):
        first32 = Path(job["dump_path"]).read_bytes()[:32].hex()
    report_lines.append(
        f"{label}=entry:{hex64(job.get('entry', 0))} start:0x{job.get('start', 0):08x} "
        f"dump:{job.get('dump_path')} first32:{first32 or job.get('probe_hex')}"
    )
if summary["ping_failures"]:
    report_lines.append(f"ping_failures={summary['ping_failures']}")
if summary["read_failures"]:
    report_lines.append(f"read_failures={summary['read_failures']}")
report_txt.write_text("\n".join(report_lines) + "\n", encoding="utf-8")
for line in report_lines:
    log(line)

if summary["status"] != "ok":
    raise SystemExit(1)
PY
