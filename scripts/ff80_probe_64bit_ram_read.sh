#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ff80_probe_64bit_ram_read.sh [options]

Options:
  --session-dir DIR     Output directory. Default:
                        rce/sessions/ff80_64bit_ram_read_<utc timestamp>.
  --lock-path PATH      Camera USB lock path. Default:
                        $HOME/fuji/locks/camera-usb.lock.
  --vendor-id HEX       USB vendor id. Default: 0x04cb.
  --product-id HEX      USB product id. Default: 0xff80.
  --recipient NAME      USB request recipient: device, endpoint, interface,
                        or other. Default: other.
  -h, --help            Show this help.

Runs the five 16-byte probes from rce/notes/ff80_64bit_ram_read_probe.md.
The wrapper holds the camera USB lock, pings before starting, pings after each
probe, and stops if ping fails twice in a row.
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

mkdir -p "$session_dir/probes" "$session_dir/logs" "$(dirname "$lock_path")"

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

import json
import os
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

fftlib.trace = True
ffjlib.trace = True

manifest = session_dir / "manifest.txt"
summary_json = session_dir / "summary.json"
summary_txt = session_dir / "summary.txt"
probes_dir = session_dir / "probes"


def log(message: str) -> None:
    print(message)
    with manifest.open("a", encoding="utf-8") as out:
        out.write(message + "\n")


@dataclass(frozen=True)
class Probe:
    key: str
    label: str
    high32: int
    low32: int


PROBES = [
    Probe("probe1", "baseline 0x40000000", 0, 0x40000000),
    Probe("probe2", "baseline 0x29B00000", 0, 0x29B00000),
    Probe("probe3", "high=1 + 0x40000000", 1, 0x40000000),
    Probe("probe4", "high=1 + 0x29B00000", 1, 0x29B00000),
    Probe("probe5", "high=2 + 0x00000000", 2, 0x00000000),
]


def with_jig(fn):
    with usb1.USBContext() as context:
        usb_h = context.openByVendorIDAndProductID(vendor_id, product_id, skip_on_error=True)
        if usb_h is None:
            raise RuntimeError(f"device {vendor_id:04x}:{product_id:04x} not found")
        log(f"target {vendor_id:04x}:{product_id:04x} recipient={recipient_name}")
        fft = fftlib.ftl(usb_h, recipient=RECIPIENTS[recipient_name])
        jig = ffjlib.jig(fft)
        with usb_h.claimInterface(0):
            opened = False
            try:
                fft.open_session()
                opened = True
                return fn(jig)
            finally:
                if opened:
                    try:
                        fft.close_session()
                    except Exception as exc:
                        log(f"close_session_failed: {type(exc).__name__}: {exc}")


def ping_once(tag: str) -> tuple[bool, str | None]:
    log(f"+ ping {tag}")

    def _ping(jig):
        ping = jig.ftl.ping().hex()
        nop = jig.ftl.nop().hex()
        return f"ping={ping} nop={nop}"

    try:
        detail = with_jig(_ping)
        log(f"ping_ok {tag} {detail}")
        return True, None
    except Exception as exc:
        detail = f"{type(exc).__name__}: {exc}"
        log(f"ping_failed {tag} {detail}")
        return False, detail


def read_ram64(probe: Probe) -> tuple[bytes, bytes]:
    if probe.high32 == 0 and probe.low32 == 0:
        raise RuntimeError("refusing toxic low RAM read high32=0 low32=0")
    if 0xFA000000 <= probe.low32 <= 0xFAFFFFFF:
        raise RuntimeError(f"refusing MMIO low32=0x{probe.low32:08x}")
    if 0xFF900000 <= probe.low32 <= 0xFF9FFFFF:
        raise RuntimeError(f"refusing MMIO low32=0x{probe.low32:08x}")

    def _read(jig):
        old_debug = jig.get_config_usb_debug()
        if old_debug == 0:
            jig.set_config_usb_debug(1)
        params = bytearray(16)
        params[4:8] = probe.high32.to_bytes(4, "little")
        params[8:12] = probe.low32.to_bytes(4, "little")
        params[12:16] = (16).to_bytes(4, "little")
        try:
            result = jig._mem_op(0x200001, 2, params, 16)
            response_params = bytes(result[0:16])
            if response_params != params:
                log(
                    f"response_params_mismatch {probe.key}: "
                    f"sent={params.hex()} got={response_params.hex()}"
                )
            return bytes(result[16:]), response_params
        finally:
            if old_debug == 0:
                try:
                    jig.set_config_usb_debug(old_debug)
                except Exception as exc:
                    log(f"debug_restore_failed {probe.key}: {type(exc).__name__}: {exc}")

    return with_jig(_read)


def verdict(results: dict[str, dict]) -> str:
    p1 = results.get("probe1", {}).get("hex")
    p2 = results.get("probe2", {}).get("hex")
    p3 = results.get("probe3", {}).get("hex")
    p4 = results.get("probe4", {}).get("hex")
    if not all([p1, p2, p3, p4]):
        return "inconclusive"
    if p3 != p1 or p4 != p2:
        return "64-bit confirmed"
    if p3 == p1 and p4 == p2:
        return "32-bit hard"
    return "inconclusive"


log(f"session_dir={session_dir}")
ok, err = ping_once("preflight")
if not ok:
    summary = {"verdict": "inconclusive", "error": err, "results": {}}
    summary_json.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    raise SystemExit(1)

results: dict[str, dict] = {}
last_attempted = None

for probe in PROBES:
    current_verdict = verdict(results)
    if probe.key == "probe5" and current_verdict == "32-bit hard":
        results[probe.key] = {
            "label": probe.label,
            "high32": probe.high32,
            "low32": probe.low32,
            "hex": None,
            "path": None,
            "error": "skipped: probes 3 and 4 confirmed high32 is ignored; low32=0 would be toxic",
            "skipped": True,
        }
        log(f"probe5_skipped {probe.label}: high32 ignored, refusing toxic low32=0 read")
        break
    last_attempted = f"{probe.key} {probe.label}"
    log(
        f"+ {probe.key} high32=0x{probe.high32:08x} "
        f"low32=0x{probe.low32:08x} size=16"
    )
    try:
        data, response_params = read_ram64(probe)
        hex_data = data.hex()
        path = probes_dir / f"{probe.key}_high{probe.high32:08x}_low{probe.low32:08x}.bin"
        path.write_bytes(data)
        results[probe.key] = {
            "label": probe.label,
            "high32": probe.high32,
            "low32": probe.low32,
            "hex": hex_data,
            "path": str(path),
            "response_params": response_params.hex(),
            "error": None,
        }
        log(f"{probe.key} {probe.label}: {hex_data}")
    except Exception as exc:
        detail = f"{type(exc).__name__}: {exc}"
        results[probe.key] = {
            "label": probe.label,
            "high32": probe.high32,
            "low32": probe.low32,
            "hex": None,
            "path": None,
            "error": detail,
        }
        log(f"{probe.key}_failed {probe.label}: {detail}")

    ok, err = ping_once(f"after_{probe.key}_1")
    results[probe.key]["post_ping_1"] = ok
    results[probe.key]["post_ping_1_error"] = err
    if not ok:
        ok2, err2 = ping_once(f"after_{probe.key}_2")
        results[probe.key]["post_ping_2"] = ok2
        results[probe.key]["post_ping_2_error"] = err2
        if not ok2:
            log(f"stopping_after_repeated_ping_failure last_attempted={last_attempted}")
            final_verdict = verdict(results)
            summary = {
                "verdict": final_verdict,
                "last_attempted": last_attempted,
                "results": results,
            }
            summary_json.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
            break
    if results[probe.key]["error"] is not None:
        final_verdict = verdict(results)
        summary = {
            "verdict": final_verdict,
            "last_attempted": last_attempted,
            "results": results,
        }
        summary_json.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
        break
else:
    final_verdict = verdict(results)
    summary = {"verdict": final_verdict, "last_attempted": last_attempted, "results": results}
    summary_json.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

if not summary_json.exists():
    final_verdict = verdict(results)
    summary = {"verdict": final_verdict, "last_attempted": last_attempted, "results": results}
    summary_json.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

summary = json.loads(summary_json.read_text(encoding="utf-8"))
lines = [
    f"probe1 baseline 0x40000000:        {summary['results'].get('probe1', {}).get('hex') or '<error>'}",
    f"probe2 baseline 0x29B00000:        {summary['results'].get('probe2', {}).get('hex') or '<error>'}",
    f"probe3 high=1 + 0x40000000:        {summary['results'].get('probe3', {}).get('hex') or '<error>'}",
    f"probe4 high=1 + 0x29B00000:        {summary['results'].get('probe4', {}).get('hex') or '<error>'}",
    f"probe5 high=2 + 0x00000000:        {summary['results'].get('probe5', {}).get('hex') or ('<skipped>' if summary['results'].get('probe5', {}).get('skipped') else '<error>')}",
    f"verdict: {summary['verdict']}",
]
summary_txt.write_text("\n".join(lines) + "\n", encoding="utf-8")
for line in lines:
    log(line)

if any(item.get("error") and not item.get("skipped") for item in summary["results"].values()):
    raise SystemExit(1)
if any(
    item.get("post_ping_1") is False and item.get("post_ping_2") is False
    for item in summary["results"].values()
):
    raise SystemExit(1)
PY
