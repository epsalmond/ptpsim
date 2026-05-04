#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/poll_fuji_usb_devices.sh [options]

Options:
  --vendor-id HEX       USB vendor id to watch. Default: 0x04cb.
  --product-id HEX      Optional product id filter, for example 0xff80.
  --interval SEC        Sleep between polls. Default: 0.02. Use 0 for busy poll.
  --summary-every SEC   Print absent heartbeat this often. Default: 2.
  --all                 Print every poll, not just state changes and heartbeats.
  --once                Poll once and exit.
  --exit-on-match       Exit 0 as soon as a matching device is present.
  -h, --help            Show this help.

Examples:
  scripts/poll_fuji_usb_devices.sh
  scripts/poll_fuji_usb_devices.sh --product-id 0xff80 --exit-on-match
  scripts/poll_fuji_usb_devices.sh --product-id 0xff80 --interval 0

This uses libusb enumeration directly, which is much faster than
system_profiler or gphoto2 auto-detect. It does not claim the device or send any
USB commands.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ ! -x "$python_bin" ]]; then
  echo "missing python executable: $python_bin" >&2
  echo "run: python3 -m venv .venv && .venv/bin/python -m pip install -e '.[test,vision,usb]'" >&2
  exit 1
fi

exec "$python_bin" - "$@" <<'PY'
from __future__ import annotations

import argparse
import datetime as dt
import sys
import time


def int_auto(value: str) -> int:
    try:
        return int(value, 0)
    except ValueError:
        return int(value, 16)


def utc_now() -> str:
    return dt.datetime.now(dt.UTC).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def load_usb1():
    try:
        import usb1  # type: ignore
    except ImportError:
        print(
            "missing Python dependency: libusb1\n"
            "run: .venv/bin/python -m pip install -e '.[usb]'",
            file=sys.stderr,
        )
        raise SystemExit(1)
    return usb1


def safe(call, default=None):
    try:
        return call()
    except Exception:
        return default


def descriptor_signature(dev) -> tuple:
    return (
        safe(dev.getBusNumber, -1),
        safe(dev.getDeviceAddress, -1),
        safe(dev.getVendorID, -1),
        safe(dev.getProductID, -1),
        safe(dev.getDeviceClass, -1),
        safe(dev.getDeviceSubClass, -1),
        safe(dev.getDeviceProtocol, -1),
        tuple(safe(dev.getPortNumberList, ()) or ()),
    )


def format_device(dev) -> str:
    vid = safe(dev.getVendorID, 0)
    pid = safe(dev.getProductID, 0)
    bus = safe(dev.getBusNumber, -1)
    address = safe(dev.getDeviceAddress, -1)
    device_class = safe(dev.getDeviceClass, -1)
    subclass = safe(dev.getDeviceSubClass, -1)
    protocol = safe(dev.getDeviceProtocol, -1)
    ports = tuple(safe(dev.getPortNumberList, ()) or ())
    port_text = ".".join(str(part) for part in ports) if ports else "-"
    return (
        f"{vid:04x}:{pid:04x}"
        f" bus={bus}"
        f" address={address}"
        f" class={device_class:02x}/{subclass:02x}/{protocol:02x}"
        f" ports={port_text}"
    )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Poll libusb for Fujifilm USB devices without claiming them."
    )
    parser.add_argument("--vendor-id", type=int_auto, default=0x04CB)
    parser.add_argument("--product-id", type=int_auto)
    parser.add_argument("--interval", type=float, default=0.02)
    parser.add_argument("--summary-every", type=float, default=2.0)
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--once", action="store_true")
    parser.add_argument("--exit-on-match", action="store_true")
    args = parser.parse_args(argv)

    if args.interval < 0:
        parser.error("--interval must be >= 0")
    if args.summary_every < 0:
        parser.error("--summary-every must be >= 0")
    return args


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    usb1 = load_usb1()

    target = f"{args.vendor_id:04x}"
    if args.product_id is not None:
        target += f":{args.product_id:04x}"
    else:
        target += ":*"

    print(
        f"{utc_now()} polling vendor={target} interval={args.interval:g}s "
        f"summary_every={args.summary_every:g}s",
        flush=True,
    )

    last_signature = None
    last_summary = 0.0
    polls = 0

    with usb1.USBContext() as context:
        while True:
            polls += 1
            now = time.monotonic()
            devices = []
            for dev in context.getDeviceList(skip_on_error=True):
                if safe(dev.getVendorID) != args.vendor_id:
                    continue
                if args.product_id is not None and safe(dev.getProductID) != args.product_id:
                    continue
                devices.append(dev)

            devices.sort(key=descriptor_signature)
            signature = tuple(descriptor_signature(dev) for dev in devices)
            changed = signature != last_signature
            heartbeat = (
                args.summary_every > 0
                and not devices
                and now - last_summary >= args.summary_every
            )

            if args.all or changed or heartbeat:
                status = "present" if devices else "absent"
                detail = "; ".join(format_device(dev) for dev in devices)
                if not detail:
                    detail = "-"
                print(
                    f"{utc_now()} {status} count={len(devices)} polls={polls} {detail}",
                    flush=True,
                )
                if not devices:
                    last_summary = now

            last_signature = signature

            if devices and args.exit_on_match:
                return 0
            if args.once:
                return 0 if devices else 1
            if args.interval:
                time.sleep(args.interval)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
PY
