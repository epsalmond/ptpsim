from __future__ import annotations

import argparse
import asyncio
from pathlib import Path
import struct

import pytest

from protocol_mapper import ble_ptpip_concurrency as probe
from rce.tools.fuji_ble_gps.ble_backend import BleBackend, DeviceInfo
from rce.tools.fuji_ble_gps.camera import FujiCamera
from rce.tools.fuji_ble_gps.session import Session


def test_decode_event_packet_capture_complete() -> None:
    packet = struct.pack(
        "<IHHIIIII",
        28,
        4,
        0x400D,
        1,
        0x10000001,
        0x10000001,
        0,
        0,
    )

    decoded = probe.decode_event_packet(packet)

    assert decoded["declared_length"] == 28
    assert decoded["packet_type"] == 4
    assert decoded["event_code"] == "0x400d"
    assert decoded["event_name"] == "CaptureComplete"
    assert decoded["params_hex"] == ["0x10000001", "0x10000001", "0x00000000", "0x00000000"]


def test_decode_event_packet_marks_length_mismatch() -> None:
    decoded = probe.decode_event_packet(b"\x1c\x00\x00\x00short")

    assert decoded["declared_length"] == 28
    assert decoded["length_mismatch"] is True
    assert decoded["decode_error"] == "short event body"


def test_parse_phases_rejects_unknown_phase() -> None:
    assert probe.parse_phases("a,c") == ["a", "c"]
    with pytest.raises(argparse.ArgumentTypeError):
        probe.parse_phases("a,d")


def test_parse_pairing_key_normalizes_hex() -> None:
    assert probe.parse_pairing_key("F6:31 32:80") == "f6313280"
    with pytest.raises(argparse.ArgumentTypeError):
        probe.parse_pairing_key("f631328")
    with pytest.raises(argparse.ArgumentTypeError):
        probe.parse_pairing_key("not-hex")


def test_capture_dir_for_uses_issue_52_prefix(tmp_path: Path) -> None:
    assert probe.capture_dir_for(tmp_path, "20260614T120000Z") == tmp_path / "issue-52-20260614T120000Z"


def test_phase_c_verdict_counts_55741_events() -> None:
    verdict = probe.phase_verdict(
        "c",
        {
            "ptp_initiate_capture": {"response_ok": True},
            "event_reader": {"events": 2},
        },
    )

    assert verdict == "ptp_capture_accepted_with_events"


def test_connect_ptpip_control_retries_refused(monkeypatch: pytest.MonkeyPatch) -> None:
    class FakeSocket:
        def __init__(self) -> None:
            self.timeout: float | None = None

        def settimeout(self, timeout: float) -> None:
            self.timeout = timeout

    class FakeTimeline:
        def __init__(self) -> None:
            self.events: list[tuple[str, dict[str, object]]] = []

        def event(self, event: str, **fields: object) -> None:
            self.events.append((event, fields))

    attempts = {"count": 0}
    fake_socket = FakeSocket()

    def fake_create_connection(address: tuple[str, int], timeout: float) -> FakeSocket:
        assert address == ("192.168.0.1", 55740)
        attempts["count"] += 1
        if attempts["count"] == 1:
            raise ConnectionRefusedError(111, "Connection refused")
        return fake_socket

    monkeypatch.setattr(probe.socket, "create_connection", fake_create_connection)
    monkeypatch.setattr(probe.time, "sleep", lambda seconds: None)
    timeline = FakeTimeline()

    sock = probe.connect_ptpip_control("192.168.0.1", 55740, 3.0, timeline)  # type: ignore[arg-type]

    assert sock is fake_socket
    assert fake_socket.timeout == 3.0
    assert attempts["count"] == 2
    assert timeline.events[0][0] == "ptpip_control_connect_failed"
    assert timeline.events[-1] == ("ptpip_control_connected", {"host": "192.168.0.1", "port": 55740, "attempts": 2})


def test_explicit_ble_address_target_keeps_advertisement_identity(tmp_path: Path) -> None:
    class FakeBackend(BleBackend):
        def __init__(self, session: Session) -> None:
            super().__init__(session)
            self.calls: list[tuple[str, str, float]] = []

        async def scan(self, timeout: float = 8.0) -> list[DeviceInfo]:
            return []

        async def find_device_by_address(self, address: str, name: str, timeout: float = 8.0) -> DeviceInfo:
            self.calls.append((address, name, timeout))
            return DeviceInfo(
                address=address,
                name="0C3EGFX100II-0C3E",
                rssi=-40,
                details={"fuji_manufacturer_identity": {"kind": "legacy_pairing_key", "payload_hex": "f6313280"}},
            )

    session = Session(root=tmp_path, label="target")
    backend = FakeBackend(session)
    camera = FujiCamera(backend, session)

    device = asyncio.run(camera._target(name="GFX100 II", timeout=12.0, address="38:7C:76:74:73:21"))

    assert backend.calls == [("38:7C:76:74:73:21", "GFX100 II", 12.0)]
    assert device.details["fuji_manufacturer_identity"]["payload_hex"] == "f6313280"


def test_remote_local_args_drop_remote_only_fields(tmp_path: Path) -> None:
    parser = probe.build_parser()
    args = parser.parse_args(
        [
            "--capture-root",
            str(tmp_path),
            "--phases",
            "a,b",
            "--remote-host",
            "eric@rpi4b.local",
            "--pairing-key",
            "f6313280",
            "--stamp",
            "fixed",
            "--preflight-only",
        ]
    )

    argv = probe.remote_local_args(args)

    assert "--run-local" in argv
    assert "--remote-host" not in argv
    assert argv[argv.index("--capture-root") + 1] == "captures"
    assert argv[argv.index("--phases") + 1] == "a,b"
    assert argv[argv.index("--pairing-key") + 1] == "f6313280"
    assert argv[argv.index("--stamp") + 1] == "fixed"
    assert "--preflight-only" in argv


def test_render_verdict_summarizes_phase_artifacts() -> None:
    text = probe.render_verdict(
        {
            "started_at": "2026-06-14T00:00:00Z",
            "finished_at": "2026-06-14T00:01:00Z",
            "capture_dir": "captures/issue-52-fixed",
            "rig": "rpi4b",
            "phases": [
                {
                    "phase": "a",
                    "verdict": "ble_shutter_write_accepted_no_55741_events_seen",
                    "event_reader": {"events": 0},
                    "liveview_reader": {"jpeg_frames": 25},
                }
            ],
        }
    )

    assert "# Issue #52 BLE/PTP-IP Concurrency Verdict" in text
    assert "| a | ble_shutter_write_accepted_no_55741_events_seen | 55741 events=0; 55742 jpeg=25 |" in text
