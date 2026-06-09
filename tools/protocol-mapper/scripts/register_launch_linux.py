#!/usr/bin/env python3
"""Linux: register this host with the camera (GATT-level, no SMP bond) and launch
the live-view-control Wi-Fi AP, driving fuji-remote's tested camera.py flow.

Fuji "pairing" is GATT registration (PAIRING_KEY -> CONNECTED_DEVICE_NAME -> ack),
not an SMP bond. camera.wifi_info() does: live-advertisement match (yields the
PAIRING_KEY from Fujifilm manufacturer data) -> bare connect -> register ->
FUNCTION_LAUNCH=take (0400) -> poll AP_STATE until launched.

Set the BlueZ adapter alias to the device name first (bluetoothctl system-alias
testhost) so the camera persists/binds that GAP name.

Writes a small status JSON to --status-file; exit 0 iff AP launched.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from rce.tools.fuji_ble_gps import uuids  # noqa: E402
from rce.tools.fuji_ble_gps.ble_backend import BleakBackend  # noqa: E402
from rce.tools.fuji_ble_gps.camera import FujiCamera  # noqa: E402
from rce.tools.fuji_ble_gps.session import Session  # noqa: E402


async def run(args: argparse.Namespace) -> dict:
    # This firmware exposes connected-device service 91f1de68 (not the mbp's RED
    # 123d8f06) and lacks f557d96b; the registration-ID-equivalent read/write
    # char here is 7ede1988. Point the registration ack at it.
    uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER = args.id_char.lower()
    session = Session(label="iso_register_launch")
    backend = BleakBackend(session)
    camera = FujiCamera(backend, session)
    info = await camera.wifi_info(
        name=args.name,
        device_name=args.device_name,
        timeout=args.scan_timeout,
        do_register=not args.no_register,
        ack_registration=True,
        address=args.address or None,
        launch_ap=args.function,
        ap_state_timeout=args.ap_timeout,
        read_passphrase=False,
    )
    info["session_dir"] = str(session.path)
    return info


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="register-launch-linux")
    parser.add_argument("--name", default="GFX100 II", help="advertised name fragment to match")
    parser.add_argument("--address", default="", help="explicit BD_ADDR (skips advertisement match + PAIRING_KEY)")
    parser.add_argument("--device-name", default="testhost", help="name registered to the camera")
    parser.add_argument("--function", default="take", choices=["take", "get", "fw_transfer"])
    parser.add_argument("--scan-timeout", type=float, default=40.0)
    parser.add_argument("--ap-timeout", type=float, default=25.0)
    parser.add_argument("--id-char", default="f557d96b-8284-4667-8793-b971c1deca2a",
                        help="RED-only registration-ID char for the ack; absent on legacy fw 2.30 (skipped)")
    parser.add_argument("--no-register", action="store_true", help="skip registration; just launch")
    parser.add_argument("--status-file", type=Path, default=None)
    args = parser.parse_args(argv)

    try:
        info = asyncio.run(run(args))
    except Exception as exc:  # noqa: BLE001 - report cleanly, never traceback-crash the flow
        info = {"error": repr(exc), "ap_state_label": "error"}
    launched = info.get("ap_state_label") == "launched"
    info["launched"] = launched
    if args.status_file:
        args.status_file.write_text(json.dumps(info, indent=2, sort_keys=True) + "\n")
    print(json.dumps(info, indent=2, sort_keys=True))
    return 0 if launched else 1


if __name__ == "__main__":
    raise SystemExit(main())
