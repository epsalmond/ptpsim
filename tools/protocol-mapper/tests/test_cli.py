from __future__ import annotations

from pathlib import Path
import types

import pytest

from rce.tools.fuji_ble_gps import cli


def test_cli_decode_payload(capsys) -> None:
    rc = cli.main(["decode-payload", "7ed88e16caeffeb62100000000000000ea070501001a0e"])
    out = capsys.readouterr().out
    assert rc == 0
    assert "lat=37.8460286" in out
    assert "utc=2026-05-01T00:26:14Z" in out


def test_cli_decode_payload_file(tmp_path, capsys) -> None:
    payload = tmp_path / "payload.bin"
    payload.write_bytes(bytes.fromhex("7ed88e16caeffeb62100000000000000ea070501001a0e"))

    rc = cli.main(["decode-payload", str(payload)])

    assert rc == 0
    assert "lon=-122.4806454" in capsys.readouterr().out


def test_cli_dry_run_writes_session(tmp_path, capsys) -> None:
    rc = cli.main(
        [
            "--session-root",
            str(tmp_path),
            "set-location",
            "--lat",
            "37.8460286",
            "--lon",
            "-122.4806454",
            "--alt",
            "33",
            "--speed",
            "0",
            "--dry-run",
        ]
    )

    out = capsys.readouterr().out
    assert rc == 0
    assert "lat=37.8460286" in out
    sessions = list(Path(tmp_path).glob("laptop_ble_gps_*"))
    assert len(sessions) == 1
    assert (sessions[0] / "payloads" / "location_dry_run.bin").exists()


def test_cli_returns_error_for_bad_payload(capsys) -> None:
    rc = cli.main(["decode-payload", "00"])
    err = capsys.readouterr().err
    assert rc == 1
    assert "GPS payload must be 23 bytes" in err


class FakeCamera:
    calls: list[tuple[str, dict]]

    def __init__(self, backend, session) -> None:
        self.calls = []
        FakeCamera.calls = self.calls

    async def scan(self, **kwargs):
        self.calls.append(("scan", kwargs))

    async def pair(self, **kwargs):
        self.calls.append(("pair", kwargs))

    async def discover(self, **kwargs):
        self.calls.append(("discover", kwargs))

    async def register(self, **kwargs):
        self.calls.append(("register", kwargs))

    async def set_location(self, payload, **kwargs):
        self.calls.append(("set_location", {"payload": payload, **kwargs}))

    async def live_test(self, **kwargs):
        self.calls.append(("live_test", kwargs))

    async def wifi_info(self, **kwargs):
        self.calls.append(("wifi_info", kwargs))
        return {
            "ssid": "FUJIFILM-GFX100II-0C3E",
            "bssid": "38-7C-76-74-73-20",
            "ap_state": "0180",
            "ap_state_label": "launched",
            "launch_ap": "get",
            "credentials_path": "/tmp/session/wifi_credentials.json",
            "passphrase_present": True,
        }

    async def firmware_update_prepare(self, **kwargs):
        self.calls.append(("firmware_update_prepare", kwargs))
        return {
            "ssid": "FUJIFILM-GFX100II-0C3E",
            "bssid": "38-7C-76-74-73-20",
            "ap_state": "0180",
            "ap_state_label": "launched",
            "launch_ap": "fw_transfer",
            "firmware_file_info_hex": "00" * 29,
            "firmware_request_notify_hex": "0100",
            "firmware_launch_notify_hex": "0100",
            "credentials_path": "/tmp/session/wifi_credentials.json",
            "passphrase_present": True,
        }


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("args", "expected"),
    [
        (types.SimpleNamespace(command="scan", session_root=None, timeout=1.0), "scan"),
        (
            types.SimpleNamespace(
                command="pair",
                session_root=None,
                name="GFX",
                address=None,
                timeout=1.0,
            ),
            "pair",
        ),
        (
            types.SimpleNamespace(
                command="discover",
                session_root=None,
                name="GFX",
                address=None,
                timeout=1.0,
            ),
            "discover",
        ),
        (
            types.SimpleNamespace(
                command="register",
                session_root=None,
                name="GFX",
                address=None,
                device_name="Laptop",
                timeout=1.0,
                write_registration_ack=False,
                skip_registration_ack=False,
                pair_trigger_first=False,
            ),
            "register",
        ),
        (
            types.SimpleNamespace(
                command="set-location",
                session_root=None,
                lat=1.0,
                lon=2.0,
                alt=3.0,
                speed=4.0,
                name="GFX",
                address=None,
                device_name="Laptop",
                timeout=1.0,
                skip_register=True,
                write_registration_ack=False,
                skip_registration_ack=False,
                pair_trigger_first=False,
                repeat=2,
                interval=0.5,
            ),
            "set_location",
        ),
        (
            types.SimpleNamespace(
                command="live-test",
                session_root=None,
                name="GFX",
                address=None,
                device_name="Laptop",
                timeout=1.0,
                lat=None,
                lon=None,
                alt=3.0,
                speed=4.0,
                repeat=2,
                interval=0.5,
                write_registration_ack=False,
                skip_registration_ack=False,
                pair_trigger_first=False,
            ),
            "live_test",
        ),
        (
            types.SimpleNamespace(
                command="live-test",
                session_root=None,
                name="GFX",
                address=None,
                device_name="Laptop",
                timeout=1.0,
                lat=1.0,
                lon=2.0,
                alt=3.0,
                speed=4.0,
                repeat=2,
                interval=0.5,
                write_registration_ack=True,
                skip_registration_ack=False,
                pair_trigger_first=True,
            ),
            "live_test",
        ),
        (
            types.SimpleNamespace(
                command="wifi-info",
                session_root=None,
                name="GFX",
                address=None,
                device_name="Laptop",
                timeout=1.0,
                skip_register=False,
                write_registration_ack=True,
                skip_registration_ack=False,
                pair_trigger_first=True,
                read_passphrase=True,
                launch_ap="get",
                ap_state_timeout=2.0,
                hold_after_launch=0.0,
            ),
            "wifi_info",
        ),
        (
            types.SimpleNamespace(
                command="firmware-prepare",
                session_root=None,
                name="GFX",
                address=None,
                device_name="Laptop",
                timeout=1.0,
                dat=Path("rce/reference/firmware_update_20260508/sendobjectinfo_payload.bin"),
                claim_version="2.41",
                product_name="GFX100 II",
                request_file_name="GXUP0006.DAT",
                ap_state_timeout=2.0,
                notify_timeout=3.0,
                skip_register=False,
                write_registration_ack=True,
                pair_trigger_first=True,
                no_read_passphrase=False,
            ),
            "firmware_update_prepare",
        ),
    ],
)
async def test_run_async_dispatches_commands(monkeypatch, args, expected) -> None:
    monkeypatch.setattr(cli, "FujiCamera", FakeCamera)

    assert await cli.run_async(args) == 0

    assert FakeCamera.calls[0][0] == expected


@pytest.mark.asyncio
async def test_run_async_passes_pair_trigger_first(monkeypatch) -> None:
    monkeypatch.setattr(cli, "FujiCamera", FakeCamera)

    assert await cli.run_async(
        types.SimpleNamespace(
            command="register",
            session_root=None,
            name="GFX",
            address=None,
            device_name="Laptop",
            timeout=1.0,
            write_registration_ack=True,
            skip_registration_ack=False,
            pair_trigger_first=True,
        )
    ) == 0

    assert FakeCamera.calls == [
        (
            "register",
            {
                "name": "GFX",
                "device_name": "Laptop",
                "timeout": 1.0,
                "ack_registration": True,
                "address": None,
                "pair_trigger_first": True,
            },
        )
    ]


@pytest.mark.asyncio
async def test_run_async_tui(monkeypatch) -> None:
    called = []
    monkeypatch.setitem(
        __import__("sys").modules,
        "rce.tools.fuji_ble_gps.tui",
        types.SimpleNamespace(run_tui=lambda: called.append(True)),
    )

    assert await cli.run_async(types.SimpleNamespace(command="tui", session_root=None)) == 0
    assert called == [True]


@pytest.mark.asyncio
async def test_run_async_unknown_command() -> None:
    with pytest.raises(RuntimeError, match="unknown command"):
        await cli.run_async(types.SimpleNamespace(command="nope", session_root=None))


@pytest.mark.asyncio
async def test_run_async_live_test_requires_lat_lon_together(monkeypatch) -> None:
    monkeypatch.setattr(cli, "FujiCamera", FakeCamera)

    with pytest.raises(RuntimeError, match="--lat and --lon must be provided together"):
        await cli.run_async(
            types.SimpleNamespace(
                command="live-test",
                session_root=None,
                name="GFX",
                address=None,
                device_name="Laptop",
                timeout=1.0,
                lat=1.0,
                lon=None,
                alt=0.0,
                speed=0.0,
                repeat=1,
                interval=1.0,
                write_registration_ack=False,
                skip_registration_ack=False,
                pair_trigger_first=False,
            )
        )


@pytest.mark.asyncio
async def test_run_async_firmware_prepare_requires_existing_dat(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(cli, "FujiCamera", FakeCamera)

    with pytest.raises(RuntimeError, match="DAT file not found"):
        await cli.run_async(
            types.SimpleNamespace(
                command="firmware-prepare",
                session_root=None,
                name="GFX",
                address=None,
                device_name="Laptop",
                timeout=1.0,
                dat=tmp_path / "missing.DAT",
                claim_version="2.41",
                product_name="GFX100 II",
                request_file_name="GXUP0006.DAT",
                ap_state_timeout=2.0,
                notify_timeout=3.0,
                skip_register=False,
                write_registration_ack=True,
                pair_trigger_first=True,
                no_read_passphrase=False,
            )
        )


def test_main_keyboard_interrupt(monkeypatch) -> None:
    def raise_keyboard_interrupt(_coro):
        raise KeyboardInterrupt

    monkeypatch.setattr(cli, "run_async", lambda _args: object())
    monkeypatch.setattr(cli.asyncio, "run", raise_keyboard_interrupt)

    assert cli.main(["scan"]) == 130
