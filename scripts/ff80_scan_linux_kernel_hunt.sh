#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ff80_scan_linux_kernel_hunt.sh --session-dir DIR

Scan FF80 linux_kernel_hunt dumps for ARM64 Linux Image headers and Linux
runtime strings. The scanner expects a priority-dump session summary.tsv with
linux_kernel_hunt_* rows, then writes linux_kernel_hunt_scan.txt and
linux_kernel_hunt_scan.json in that session directory.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
python_bin="${PYTHON_BIN:-$repo_root/.venv/bin/python}"
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
  echo "missing required --session-dir" >&2
  usage >&2
  exit 2
fi

cd "$repo_root"

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  exit 1
fi

"$python_bin" - "$session_dir" <<'PY'
from __future__ import annotations

import csv
import hashlib
import json
import re
import struct
import sys
from pathlib import Path


BASE_RE = re.compile(r"_([0-9a-fA-F]{8})_([0-9a-fA-F]{8})\.bin$")


def parse_dump_path(path: Path) -> tuple[int, int]:
    match = BASE_RE.search(path.name)
    if not match:
        raise ValueError(f"cannot parse address range from {path.name}")
    return int(match.group(1), 16), int(match.group(2), 16)


def printable_context(blob: bytes, offset: int, length: int) -> str:
    start = max(0, offset - 32)
    end = min(len(blob), offset + length + 96)
    return "".join(chr(value) if 32 <= value < 127 else "." for value in blob[start:end])


def main(argv: list[str]) -> int:
    session = Path(argv[1])
    summary = session / "summary.tsv"
    if not summary.exists():
        print(f"missing summary: {summary}", file=sys.stderr)
        return 1

    chunks: list[tuple[int, int, Path, bytes]] = []
    with summary.open(newline="") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            if row.get("status") != "ok":
                continue
            if not row.get("label", "").startswith("linux_kernel_hunt_"):
                continue
            if not row.get("path"):
                continue
            path = Path(row["path"])
            start, end = parse_dump_path(path)
            chunks.append((start, end, path, path.read_bytes()))

    if not chunks:
        print(f"no ok linux_kernel_hunt_* rows found in {summary}", file=sys.stderr)
        return 1

    chunks.sort(key=lambda item: item[0])
    expected = chunks[0][0]
    contiguous = True
    for start, end, path, data in chunks:
        if start != expected:
            contiguous = False
        if len(data) != end - start:
            print(
                f"warning: {path} size {len(data)} does not match range 0x{start:08x}..0x{end:08x}",
                file=sys.stderr,
            )
        expected = end

    blob = b"".join(data for _, _, _, data in chunks)
    range_start = chunks[0][0]
    range_end = chunks[-1][1]
    needles = {
        "arm64_image_magic": b"ARM\x64",
        "linux_version": b"Linux version",
        "version_4_9_92": b"4.9.92",
        "swapper_0": b"swapper/0",
        "init_task": b"init_task",
        "dev_isgc": b"/dev/isgc",
        "marble_rpmsg": b"marble_rpmsg",
        "isgc_rpmsg": b"isgc_rpmsg",
        "isgc": b"isgc",
        "rpmsg": b"rpmsg",
        "initramfs": b"initramfs",
        "rootfs": b"rootfs",
    }

    hits: list[dict[str, str]] = []
    for label, needle in needles.items():
        offset = 0
        while True:
            found = blob.find(needle, offset)
            if found < 0:
                break
            hits.append(
                {
                    "label": label,
                    "address": f"0x{range_start + found:08x}",
                    "offset": f"0x{found:x}",
                    "needle_hex": needle.hex(),
                    "context_ascii": printable_context(blob, found, len(needle)),
                }
            )
            offset = found + 1

    headers: list[dict[str, object]] = []
    for hit in [item for item in hits if item["label"] == "arm64_image_magic"]:
        magic_addr = int(hit["address"], 16)
        header_addr = magic_addr - 0x38
        header_offset = header_addr - range_start
        if not (0 <= header_offset and header_offset + 0x40 <= len(blob)):
            continue
        header = blob[header_offset : header_offset + 0x40]
        code0, code1 = struct.unpack_from("<II", header, 0)
        text_offset, image_size, flags, _res2, _res3, _res4 = struct.unpack_from("<QQQQQQ", header, 8)
        magic, res5 = struct.unpack_from("<II", header, 0x38)
        plausible = magic == 0x644D5241 and 0 < image_size < 0x40000000 and text_offset < 0x40000000
        image_end = header_addr + image_size if plausible else None
        headers.append(
            {
                "header_address": f"0x{header_addr:08x}",
                "magic_address": f"0x{magic_addr:08x}",
                "code0": f"0x{code0:08x}",
                "code1": f"0x{code1:08x}",
                "text_offset": f"0x{text_offset:x}",
                "image_size": f"0x{image_size:x}",
                "image_end": f"0x{image_end:08x}" if image_end is not None else None,
                "flags": f"0x{flags:x}",
                "magic": f"0x{magic:08x}",
                "res5": f"0x{res5:08x}",
                "plausible": plausible,
                "header_hex": header.hex(),
            }
        )

    result = {
        "session": str(session),
        "dump_count": len(chunks),
        "contiguous": contiguous,
        "range_start": f"0x{range_start:08x}",
        "range_end": f"0x{range_end:08x}",
        "total_bytes": len(blob),
        "sha256_concat": hashlib.sha256(blob).hexdigest(),
        "hits": hits,
        "arm64_headers": headers,
    }

    (session / "linux_kernel_hunt_scan.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    lines = [
        f"session={session}",
        f"dump_count={len(chunks)}",
        f"contiguous={contiguous}",
        f"range=0x{range_start:08x}..0x{range_end:08x}",
        f"total_bytes={len(blob)}",
        f"sha256_concat={result['sha256_concat']}",
        f"hit_count={len(hits)}",
        f"arm64_header_count={len(headers)}",
    ]
    for hit in hits[:200]:
        lines.append(f"hit {hit['label']} {hit['address']} offset={hit['offset']} context={hit['context_ascii']}")
    if len(hits) > 200:
        lines.append(f"hits_truncated={len(hits) - 200}")
    for header in headers:
        lines.append("arm64_header " + json.dumps(header, sort_keys=True))

    text = "\n".join(lines) + "\n"
    (session / "linux_kernel_hunt_scan.txt").write_text(text)
    print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
PY
