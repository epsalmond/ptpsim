#!/usr/bin/env python3
"""Linux/BlueZ BLE launcher for the Fuji camera AP (GFX100 II).

Connects over BLE (bleak), pairs/bonds if needed, writes FUNCTION_LAUNCH = 0x0300
("get") to trigger the camera's (open) Wi-Fi AP, and polls AP_STATE until it
reports launched (0x0180). Optionally holds the BLE link open so the AP persists
while a separate process joins the AP and runs the PTP/IP probe.

Reuses only the pure UUID constants from rce.tools.fuji_ble_gps.uuids.

Status JSON is written to --status-file; a launched AP yields exit 0.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from bleak import BleakClient, BleakScanner  # noqa: E402

from rce.tools.fuji_ble_gps import uuids  # noqa: E402

AP_LAUNCHED = b"\x01\x80"
AP_NOT_LAUNCHED = b"\x00\x80"

# Registration-ID characteristic differs by firmware. The mbp/RED firmware
# exposes connected-device service 123d8f06 with f557d96b; current GFX100 II
# firmware exposes 91f1de68 with 7ede1988 instead. Try f557 first, then 7ede1988.
REG_ID_CHARS = (
    uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER,  # f557d96b (mbp/RED)
    "7ede1988-b27e-43fc-80f4-6fec994f0552",             # current GFX100 II fw
)


def _registration_ack(raw: bytes) -> bytes:
    return (int.from_bytes(raw, "little") | 0x20000000).to_bytes(4, "little")


def _label(data: bytes) -> str:
    return {AP_NOT_LAUNCHED: "not_launched", AP_LAUNCHED: "launched"}.get(data, "unknown")


async def _resolve_device(address: str, name_fragment: str, timeout: float):
    """Scan and return a BLEDevice matching address (preferred) or name fragment.

    Connecting to a freshly-scanned BLEDevice avoids BlueZ "device not found".
    """
    addr_lc = address.lower() if address else ""
    name_lc = name_fragment.lower()

    def match(dev, adv) -> bool:
        if addr_lc and dev.address.lower() == addr_lc:
            return True
        name = (adv.local_name or dev.name or "").lower()
        return bool(name_lc) and name_lc in name

    return await BleakScanner.find_device_by_filter(match, timeout=timeout)


async def _safe_read(client: BleakClient, uuid: str) -> str:
    try:
        return (await client.read_gatt_char(uuid)).hex()
    except Exception as exc:  # noqa: BLE001 - evidence only, never fatal
        return f"<read_error: {exc!r}>"


async def run(args: argparse.Namespace) -> dict:
    status: dict = {
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "address": args.address,
        "launched": False,
    }
    device = await _resolve_device(args.address, args.name, args.scan_timeout)
    if not device:
        status["error"] = f"camera (addr={args.address!r} name~{args.name!r}) not found in BLE scan"
        return status
    status["address"] = device.address

    # pair=True bonds during connect so encrypted Fuji GATT services resolve.
    client = BleakClient(device, timeout=args.connect_timeout, pair=args.pair)
    await client.connect()
    status["connected"] = client.is_connected
    status["paired"] = args.pair
    try:
        status["gatt_chars"] = sorted(
            c.uuid.lower() for s in client.services for c in s.characteristics
        )
        has_launch = uuids.CHAR_FUNCTION_LAUNCH in status["gatt_chars"]
        status["has_function_launch_char"] = has_launch
        status["has_fuji_service"] = any(
            s.uuid.lower() == uuids.SERVICE_FUJI_CAMERA for s in client.services
        )
        status["ap_state_before"] = await _safe_read(client, uuids.CHAR_AP_STATE)
        status["ssid"] = await _safe_read(client, uuids.CHAR_CAMERA_SSID_NAME_STRING)
        if not has_launch:
            status["error"] = "FUNCTION_LAUNCH characteristic absent (camera may need registration)"
            return status

        # Replicate the known-good warm-reconnect pre-launch sequence:
        # name -> read registration id + write ack (id|0x20000000) -> sync cycle.
        # The ack is required for FUNCTION_LAUNCH to bring the AP up.
        await client.write_gatt_char(
            uuids.CHAR_CONNECTED_DEVICE_NAME, args.device_name.encode("utf-8") + b"\x00", response=True
        )
        status["device_name_written"] = args.device_name
        id_char = next((c for c in REG_ID_CHARS if c in status["gatt_chars"]), None)
        status["reg_id_char"] = id_char
        if id_char:
            try:
                raw_id = bytes(await client.read_gatt_char(id_char))
                status["reg_id"] = raw_id.hex()
                if len(raw_id) == 4 and any(raw_id):
                    ack = _registration_ack(raw_id)
                    await client.write_gatt_char(id_char, ack, response=True)
                    status["reg_ack"] = ack.hex()
            except Exception as exc:  # noqa: BLE001
                status["reg_ack_error"] = repr(exc)
        if uuids.CHAR_LOCATION_SYNC_CYCLE in status["gatt_chars"]:
            try:
                await client.write_gatt_char(uuids.CHAR_LOCATION_SYNC_CYCLE, b"\x0a\x00", response=True)
            except Exception as exc:  # noqa: BLE001
                status["sync_cycle_error"] = repr(exc)

        # Match the known-good AP-handoff precondition: image-transfer setting on.
        if uuids.CHAR_IMAGE_TRANSFER_SETTING_EX in status["gatt_chars"]:
            try:
                await client.write_gatt_char(uuids.CHAR_IMAGE_TRANSFER_SETTING_EX, b"\x01", response=True)
                status["image_transfer_setting_ex"] = "01"
            except Exception as exc:  # noqa: BLE001
                status["image_transfer_setting_ex_error"] = repr(exc)

        launch_value = uuids.FUNCTION_LAUNCH_VALUES[args.function]
        await client.write_gatt_char(uuids.CHAR_FUNCTION_LAUNCH, launch_value, response=True)
        status["function"] = args.function
        status["function_launch_written"] = launch_value.hex()

        deadline = time.monotonic() + args.ap_timeout
        last = ""
        while time.monotonic() < deadline:
            try:
                raw = bytes(await client.read_gatt_char(uuids.CHAR_AP_STATE))
            except Exception as exc:  # noqa: BLE001
                # Camera may drop BLE as it switches to AP mode; let the Wi-Fi
                # step verify whether the AP actually came up.
                status["ap_state_read_error"] = repr(exc)
                break
            last = raw.hex()
            if raw == AP_LAUNCHED:
                status["launched"] = True
                break
            await asyncio.sleep(1.0)
        status["ap_state_after"] = last
        status["ap_state_label"] = _label(bytes.fromhex(last)) if last else "unknown"

        if status["launched"] and args.hold > 0:
            status["holding_seconds"] = args.hold
            _write_status(args.status_file, status)
            await asyncio.sleep(args.hold)
    finally:
        try:
            await client.disconnect()
        except Exception:  # noqa: BLE001
            pass
    return status


def _write_status(path: Path | None, status: dict) -> None:
    if path:
        path.write_text(json.dumps(status, indent=2, sort_keys=True) + "\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="launch-ap-linux")
    parser.add_argument("--address", default="38:7C:76:74:73:21", help="camera BLE BD_ADDR")
    parser.add_argument("--name", default="GFX100II", help="name fragment if address unset")
    parser.add_argument("--scan-timeout", type=float, default=12.0)
    parser.add_argument("--connect-timeout", type=float, default=30.0)
    parser.add_argument("--ap-timeout", type=float, default=25.0)
    parser.add_argument("--device-name", default="testhost", help="registered device name to (re)write")
    parser.add_argument("--function", default="take", choices=sorted(uuids.FUNCTION_LAUNCH_VALUES),
                        help="FUNCTION_LAUNCH value: take=0400 (live-view control), get=0300 (SD browse), fw_transfer=0500")
    parser.add_argument("--no-pair", dest="pair", action="store_false")
    parser.add_argument("--hold", type=float, default=0.0, help="seconds to hold BLE open after launch")
    parser.add_argument("--status-file", type=Path, default=None)
    args = parser.parse_args(argv)

    status = asyncio.run(run(args))
    _write_status(args.status_file, status)
    print(json.dumps(status, indent=2, sort_keys=True))
    return 0 if status.get("launched") else 1


if __name__ == "__main__":
    raise SystemExit(main())
