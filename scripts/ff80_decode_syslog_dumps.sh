#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/ff80_decode_syslog_dumps.sh --session-dir DIR [--output-dir DIR]

Decode bounded FF80 syslog RAM-buffer dumps from a priority-dump session into
plain text. The decoder expects a session summary.tsv with syslog_* rows, and
writes one text file per dump plus all_syslogs.txt and index.tsv.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
python_bin="${PYTHON_BIN:-$repo_root/.venv/bin/python}"
session_dir=""
output_dir=""

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

"$python_bin" - "$session_dir" "$output_dir" <<'PY'
from __future__ import annotations

import csv
import re
import struct
import sys
from pathlib import Path


def parse_base(path: Path) -> int:
    match = re.search(r"_([0-9a-fA-F]{8})_([0-9a-fA-F]{8})\.bin$", path.name)
    if not match:
        raise ValueError(f"cannot parse base address from {path.name}")
    return int(match.group(1), 16)


def printable(data: bytes) -> str:
    return "".join(chr(value) if 32 <= value < 127 else "." for value in data)


def main(argv: list[str]) -> int:
    session = Path(argv[1])
    output = Path(argv[2]) if len(argv) > 2 and argv[2] else session / "syslog_text"
    summary_path = session / "summary.tsv"
    if not summary_path.exists():
        print(f"missing summary: {summary_path}", file=sys.stderr)
        return 1

    output.mkdir(parents=True, exist_ok=True)
    header_struct = struct.Struct("<20sH2B4HI")
    stride = 0x14
    record_start_delta = 0x48
    combined_lines: list[str] = []
    index_lines = ["label\taddress\ttext_path\trecord_count\tnonzero_record_count\tsha256"]

    with summary_path.open(newline="") as handle:
        rows = [
            row
            for row in csv.DictReader(handle, delimiter="\t")
            if row.get("status") == "ok" and row.get("path") and row.get("label", "").startswith("syslog_")
        ]

    if not rows:
        print(f"no ok syslog_* rows found in {summary_path}", file=sys.stderr)
        return 1

    for row in rows:
        dump_path = Path(row["path"])
        data = dump_path.read_bytes()
        base = parse_base(dump_path)
        header_at = data.find(b"syslog Ver 3.0")
        out_path = output / f"{row['label']}_{base:08x}.txt"

        lines = [
            f"label: {row['label']}",
            f"source: {dump_path}",
            f"base_address: 0x{base:08x}",
            f"requested_size: {row['requested_size']}",
            f"actual_bytes: {row['actual_bytes']}",
            f"sha256: {row['sha256']}",
        ]
        record_count = 0
        nonzero_count = 0

        if header_at < 0:
            lines.append("syslog_header: not found")
        else:
            header_addr = base + header_at
            lines.extend(
                [
                    f"syslog_header_offset: 0x{header_at:04x}",
                    f"syslog_header_address: 0x{header_addr:08x}",
                ]
            )
            header_slice = data[header_at : header_at + header_struct.size]
            if len(header_slice) == header_struct.size:
                magic, marker, byte0, byte1, h0, h1, h2, h3, word = header_struct.unpack(
                    header_slice
                )
                magic_text = magic.rstrip(b"\x00").decode("ascii", errors="replace")
                lines.append(f"magic: {magic_text}")
                lines.append(
                    "header_fields: "
                    f"marker=0x{marker:04x} byte0=0x{byte0:02x} byte1=0x{byte1:02x} "
                    f"h0=0x{h0:04x} h1=0x{h1:04x} h2=0x{h2:04x} h3=0x{h3:04x} "
                    f"word=0x{word:08x}"
                )
            else:
                lines.append("header_fields: truncated")
            lines.extend(
                [
                    f"record_stride: 0x{stride:x}",
                    f"record_start_offset: 0x{header_at + record_start_delta:04x}",
                    "",
                    "records:",
                ]
            )
            start = header_at + record_start_delta
            for offset in range(start, len(data) - stride + 1, stride):
                chunk = data[offset : offset + stride]
                record_count += 1
                if chunk == b"\x00" * stride or chunk == b"\xff" * stride:
                    continue
                nonzero_count += 1
                words = struct.unpack_from("<5I", chunk, 0)
                u16_02 = struct.unpack_from("<H", chunk, 2)[0]
                word_text = ",".join(f"0x{word:08x}" for word in words)
                lines.append(
                    f"  record={record_count - 1:03d} offset=0x{offset:04x} "
                    f"addr=0x{base + offset:08x} b0=0x{chunk[0]:02x} "
                    f"b1=0x{chunk[1]:02x} u16_02=0x{u16_02:04x} "
                    f"u32=[{word_text}] hex={chunk.hex()} ascii={printable(chunk)}"
                )

        lines.extend(["", f"record_count: {record_count}", f"nonzero_record_count: {nonzero_count}"])
        out_path.write_text("\n".join(lines) + "\n")
        combined_lines.extend(["=" * 80, *lines])
        index_lines.append(
            f"{row['label']}\t0x{base:08x}\t{out_path}\t{record_count}\t{nonzero_count}\t{row['sha256']}"
        )

    (output / "all_syslogs.txt").write_text("\n".join(combined_lines) + "\n")
    (output / "index.tsv").write_text("\n".join(index_lines) + "\n")
    print(output)
    print("\n".join(index_lines))
    return 0


raise SystemExit(main(sys.argv))
PY
