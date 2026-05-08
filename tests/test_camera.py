from __future__ import annotations

import asyncio
from datetime import UTC, datetime
import json
import stat
from zoneinfo import ZoneInfo

import pytest

from rce.tools.fuji_ble_gps.camera import (
    FujiCamera,
    _system_timezone_hhmm_and_dst,
    _timezone_hhmm_from_seconds,
    build_payload,
    build_utc_timezone_payload,
)
from rce.tools.fuji_ble_gps.ble_backend import DeviceInfo
from rce.tools.fuji_ble_gps.session import Session
from rce.tools.fuji_ble_gps import uuids


class FakeConn:
    def __init__(self) -> None:
        self.events: list[tuple[str, str]] = []
        self.writes: list[tuple[str, bytes, bool]] = []
        self.reads: list[str] = []
        self.sensitive_reads: list[str] = []
        self.notifications: list[str] = []
        self.characteristics = set(uuids.IDENTITY_READ_CHARS.values()) | {
            uuids.CHAR_CONNECTED_DEVICE_NAME,
            uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER,
            uuids.CHAR_LOCATION_SYNC_CYCLE,
            uuids.CHAR_LOCATION_AND_SPEED,
            uuids.CHAR_DATE_SYNC_STATE,
            uuids.CHAR_LOCATION_SYNC_STATE,
        }

    async def __aenter__(self):
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        pass

    async def services_json(self):
        return [{"uuid": "service", "characteristics": [{"uuid": uuid} for uuid in sorted(self.characteristics)]}]

    async def has_characteristic(self, uuid: str) -> bool:
        return uuid.lower() in self.characteristics

    async def read(self, uuid: str, *, sensitive: bool = False) -> bytes:
        self.events.append(("read", uuid.lower()))
        self.reads.append(uuid)
        if sensitive:
            self.sensitive_reads.append(uuid.lower())
        values = {
            uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER: bytes.fromhex("70df0500"),
            uuids.CHAR_GAP_DEVICE_NAME: b"GFX100 II\x00",
            uuids.CHAR_SERIAL_NUMBER_STRING: b"230829053020110C3E\x00",
            uuids.CHAR_CAMERA_MAC_ADDRESS: b"38-7C-76-74-73-20\x00",
            uuids.CHAR_FIRMWARE_REVISION_STRING: b"02.40\x00",
            uuids.CHAR_CAMERA_SERIAL_NUMBER: b"33E01721\x00",
            uuids.CHAR_CAMERA_SSID_NAME_STRING: b"FUJIFILM-GFX100II-0C3E\x00",
            uuids.CHAR_LOCATION_SYNC_STATE: b"\x01\x00",
            uuids.CHAR_DATE_SYNC_STATE: b"\x01\x00",
            uuids.CHAR_AP_STATE: b"\x00\x80",
        }
        return values.get(uuid.lower(), b"\x00")

    async def write(self, uuid: str, data: bytes, response: bool = True) -> None:
        self.events.append(("write", uuid.lower()))
        self.writes.append((uuid.lower(), data, response))

    async def start_notify(self, uuid: str, callback) -> None:
        self.notifications.append(uuid.lower())
        callback(uuid.lower(), b"\x01\x00")


class SparseConn(FakeConn):
    def __init__(self) -> None:
        super().__init__()
        self.characteristics = {uuids.CHAR_CONNECTED_DEVICE_NAME, uuids.CHAR_LOCATION_AND_SPEED}


class FlakyConn(FakeConn):
    async def read(self, uuid: str, *, sensitive: bool = False) -> bytes:
        self.reads.append(uuid)
        if uuid == uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER:
            raise RuntimeError("read blocked")
        if uuid == uuids.CHAR_GAP_DEVICE_NAME:
            raise UnicodeDecodeError("utf-8", b"\xff", 0, 1, "bad")
        values = {
            uuids.CHAR_LOCATION_SYNC_STATE: b"\x01\x00",
            uuids.CHAR_DATE_SYNC_STATE: b"\x01\x00",
        }
        return values.get(uuid.lower(), b"\x00")

    async def start_notify(self, uuid: str, callback) -> None:
        raise RuntimeError("notify blocked")


class FakeBackend:
    def __init__(self, conn: FakeConn) -> None:
        self.conn = conn
        self.device = DeviceInfo(address="corebluetooth-uuid", name="GFX100 II", rssi=-50)
        self.find_calls = 0

    async def scan(self, timeout: float = 8.0):
        return [self.device]

    async def find_device(self, name: str, timeout: float = 8.0):
        self.find_calls += 1
        return self.device

    def connect(self, device: DeviceInfo):
        return self.conn


class WifiConn(FakeConn):
    def __init__(self) -> None:
        super().__init__()
        self.characteristics.update(
            {
                uuids.CHAR_CAMERA_WIFI_PASSPHRASE_STRING,
                uuids.CHAR_FUNCTION_LAUNCH,
            }
        )
        self.launch_requested = False
        self.ap_state_reads_after_launch = 0

    async def read(self, uuid: str, *, sensitive: bool = False) -> bytes:
        if uuid == uuids.CHAR_CAMERA_WIFI_PASSPHRASE_STRING:
            self.events.append(("read", uuid.lower()))
            self.reads.append(uuid)
            if sensitive:
                self.sensitive_reads.append(uuid.lower())
            return b"CQAggA8AEEAVADjgIQA0\x00"
        if uuid == uuids.CHAR_AP_STATE and self.launch_requested:
            self.events.append(("read", uuid.lower()))
            self.reads.append(uuid)
            self.ap_state_reads_after_launch += 1
            if self.ap_state_reads_after_launch == 1:
                return b"\x00\x80"
            return b"\x01\x80"
        return await super().read(uuid, sensitive=sensitive)

    async def write(self, uuid: str, data: bytes, response: bool = True) -> None:
        await super().write(uuid, data, response=response)
        if uuid == uuids.CHAR_FUNCTION_LAUNCH:
            self.launch_requested = True


class StuckApConn(WifiConn):
    async def read(self, uuid: str, *, sensitive: bool = False) -> bytes:
        if uuid == uuids.CHAR_AP_STATE:
            self.events.append(("read", uuid.lower()))
            self.reads.append(uuid)
            return b"\x00\x80"
        return await super().read(uuid, sensitive=sensitive)


class EmptyPassphraseConn(WifiConn):
    async def read(self, uuid: str, *, sensitive: bool = False) -> bytes:
        if uuid == uuids.CHAR_CAMERA_WIFI_PASSPHRASE_STRING:
            self.events.append(("read", uuid.lower()))
            self.reads.append(uuid)
            if sensitive:
                self.sensitive_reads.append(uuid.lower())
            return b"\x00"
        return await super().read(uuid, sensitive=sensitive)


class FirmwarePrepareConn(WifiConn):
    def __init__(self) -> None:
        super().__init__()
        self.firmware_callback = None
        self.characteristics.update(
            {
                uuids.CHAR_FIRMWARE_UPDATE_REQUEST,
                uuids.CHAR_FIRMWARE_UPDATE_FILE_INFO,
                uuids.CHAR_FIRMWARE_UPDATE_STATE_NOTIFY,
            }
        )

    async def read(self, uuid: str, *, sensitive: bool = False) -> bytes:
        if uuid == uuids.CHAR_FIRMWARE_UPDATE_FILE_INFO:
            self.events.append(("read", uuid.lower()))
            self.reads.append(uuid)
            return b"\x00" * 29
        return await super().read(uuid, sensitive=sensitive)

    async def write(self, uuid: str, data: bytes, response: bool = True) -> None:
        await super().write(uuid, data, response=response)
        if uuid in {uuids.CHAR_FIRMWARE_UPDATE_REQUEST, uuids.CHAR_FUNCTION_LAUNCH} and self.firmware_callback:
            self.firmware_callback(uuids.CHAR_FIRMWARE_UPDATE_STATE_NOTIFY, b"\x01\x00")

    async def start_notify(self, uuid: str, callback) -> None:
        if uuid == uuids.CHAR_FIRMWARE_UPDATE_STATE_NOTIFY:
            self.notifications.append(uuid.lower())
            self.firmware_callback = callback
            return
        await super().start_notify(uuid, callback)


@pytest.mark.asyncio
async def test_camera_register_runs_observed_sequence(tmp_path, monkeypatch) -> None:
    conn = FakeConn()
    conn.characteristics.add(uuids.CHAR_UTC_AND_TIMEZONE)
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))
    monkeypatch.setattr(
        "rce.tools.fuji_ble_gps.camera.build_utc_timezone_payload",
        lambda: bytes.fromhex("ea070501033708e0fcffff01"),
    )

    identity = await camera.register(device_name="Fuji-Laptop", ack_registration=True)

    assert conn.notifications == [uuids.CHAR_LOCATION_SYNC_STATE, uuids.CHAR_DATE_SYNC_STATE]
    assert conn.writes[0] == (uuids.CHAR_CONNECTED_DEVICE_NAME, b"Fuji-Laptop\x00", True)
    assert (uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER, bytes.fromhex("70df0520"), True) in conn.writes
    assert (
        uuids.CHAR_UTC_AND_TIMEZONE,
        bytes.fromhex("ea070501033708e0fcffff01"),
        True,
    ) in conn.writes
    assert (uuids.CHAR_LOCATION_SYNC_CYCLE, b"\x0a\x00", True) in conn.writes
    assert identity["gap_device_name"] == "GFX100 II"
    assert identity["firmware_revision"] == "02.40"


@pytest.mark.asyncio
async def test_camera_register_can_pair_trigger_before_registration(tmp_path) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    await camera.register(device_name="Fuji-Laptop", pair_trigger_first=True)

    assert conn.events[0] == ("read", uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER)
    assert ("write", uuids.CHAR_CONNECTED_DEVICE_NAME) in conn.events


@pytest.mark.asyncio
async def test_camera_set_location_writes_payload_repeatedly(tmp_path, monkeypatch) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))
    sleeps: list[float] = []

    async def fake_sleep(seconds: float) -> None:
        sleeps.append(seconds)

    monkeypatch.setattr("rce.tools.fuji_ble_gps.camera.asyncio.sleep", fake_sleep)
    payload = build_payload(37.8460286, -122.4806454, 33, 0)
    frozen = payload.__class__(
        latitude=payload.latitude,
        longitude=payload.longitude,
        altitude_m=payload.altitude_m,
        speed_mps=payload.speed_mps,
        utc=datetime(2026, 5, 1, 0, 26, 14, tzinfo=UTC),
    )

    await camera.set_location(frozen, repeat=2, interval=3.0, do_register=False)

    gps_writes = [write for write in conn.writes if write[0] == uuids.CHAR_LOCATION_AND_SPEED]
    assert len(gps_writes) == 2
    assert gps_writes[0][1].hex() == "7ed88e16caeffeb62100000000000000ea070501001a0e"
    assert sleeps == [3.0]


@pytest.mark.asyncio
async def test_camera_set_location_registers_by_default(tmp_path) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))
    payload = build_payload(1.0, 2.0, 3.0, 4.0)

    await camera.set_location(payload)

    write_uuids = [write[0] for write in conn.writes]
    assert uuids.CHAR_CONNECTED_DEVICE_NAME in write_uuids
    assert uuids.CHAR_LOCATION_AND_SPEED in write_uuids


@pytest.mark.asyncio
async def test_camera_set_location_can_pair_trigger_before_registration(tmp_path) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))
    payload = build_payload(1.0, 2.0, 3.0, 4.0)

    await camera.set_location(payload, pair_trigger_first=True)

    assert conn.events[0] == ("read", uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER)
    assert ("write", uuids.CHAR_CONNECTED_DEVICE_NAME) in conn.events


@pytest.mark.asyncio
async def test_camera_live_test_keeps_single_connection_and_can_skip_gps(tmp_path) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    identity = await camera.live_test(device_name="Fuji-Laptop")

    assert identity["gap_device_name"] == "GFX100 II"
    write_uuids = [write[0] for write in conn.writes]
    assert uuids.CHAR_CONNECTED_DEVICE_NAME in write_uuids
    assert uuids.CHAR_LOCATION_AND_SPEED not in write_uuids


@pytest.mark.asyncio
async def test_camera_live_test_can_pair_trigger_before_registration(tmp_path) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    await camera.live_test(device_name="Fuji-Laptop", pair_trigger_first=True)

    assert conn.events[0] == ("read", uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER)
    assert ("write", uuids.CHAR_CONNECTED_DEVICE_NAME) in conn.events


@pytest.mark.asyncio
async def test_pair_trigger_first_aborts_without_trigger_read(tmp_path) -> None:
    conn = SparseConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    with pytest.raises(RuntimeError, match="pair-trigger-first requested"):
        await camera.live_test(device_name="Fuji-Laptop", pair_trigger_first=True)

    assert conn.writes == []


@pytest.mark.asyncio
async def test_camera_live_test_can_write_gps(tmp_path, monkeypatch) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))
    payload = build_payload(37.8460286, -122.4806454, 33, 0)
    sleeps: list[float] = []

    async def fake_sleep(seconds: float) -> None:
        sleeps.append(seconds)

    monkeypatch.setattr("rce.tools.fuji_ble_gps.camera.asyncio.sleep", fake_sleep)

    await camera.live_test(payload=payload, repeat=2, interval=4.0)

    gps_writes = [write for write in conn.writes if write[0] == uuids.CHAR_LOCATION_AND_SPEED]
    assert len(gps_writes) == 2
    assert sleeps == [4.0]


@pytest.mark.asyncio
async def test_pair_reads_first_available_pairing_trigger(tmp_path) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    await camera.pair()

    assert uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER in conn.reads


@pytest.mark.asyncio
async def test_camera_scan_and_discover(tmp_path) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    assert (await camera.scan())[0].name == "GFX100 II"
    services = await camera.discover()

    assert services[0]["uuid"] == "service"


@pytest.mark.asyncio
async def test_camera_can_use_explicit_address_without_scan(tmp_path) -> None:
    conn = FakeConn()
    backend = FakeBackend(conn)
    camera = FujiCamera(backend, Session(root=tmp_path))

    await camera.discover(address="known-corebluetooth-uuid")

    assert backend.find_calls == 0


@pytest.mark.asyncio
async def test_register_continues_without_registration_id(tmp_path, monkeypatch) -> None:
    conn = SparseConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))
    sleeps: list[float] = []

    async def fake_sleep(seconds: float) -> None:
        sleeps.append(seconds)

    monkeypatch.setattr("rce.tools.fuji_ble_gps.camera.asyncio.sleep", fake_sleep)

    identity = await camera.register(device_name="Fuji-Laptop")

    assert identity == {}
    assert conn.writes == [(uuids.CHAR_CONNECTED_DEVICE_NAME, b"Fuji-Laptop\x00", True)]
    assert sleeps == []


@pytest.mark.asyncio
async def test_register_skips_ack_by_default(tmp_path) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    await camera.register(device_name="Fuji-Laptop")

    assert (uuids.CHAR_CONNECTED_DEVICE_NAME, b"Fuji-Laptop\x00", True) in conn.writes
    assert not any(
        write[0] == uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER for write in conn.writes
    )


@pytest.mark.asyncio
async def test_pair_skips_missing_then_logs_read_error(tmp_path) -> None:
    conn = FlakyConn()
    conn.characteristics.remove(uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER)
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    await camera.pair()

    assert uuids.CHAR_GAP_DEVICE_NAME in conn.reads


@pytest.mark.asyncio
async def test_prepare_and_identity_tolerate_errors(tmp_path, monkeypatch) -> None:
    conn = FlakyConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))
    monkeypatch.setattr("rce.tools.fuji_ble_gps.camera.asyncio.sleep", lambda _seconds: _noop())

    identity = await camera.register()

    assert uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER not in [
        write[0] for write in conn.writes
    ]
    assert "gap_device_name" not in identity


def test_decode_read_value_binary_fallback(tmp_path) -> None:
    camera = FujiCamera(FakeBackend(FakeConn()), Session(root=tmp_path))

    assert camera._decode_read_value(b"\xff\x00") == "ff00"


def test_build_utc_timezone_payload_matches_app_format() -> None:
    payload = build_utc_timezone_payload(
        datetime(2026, 4, 30, 20, 55, 8, tzinfo=ZoneInfo("America/Los_Angeles"))
    )

    assert payload.hex() == "ea070501033708e0fcffff01"


def test_build_utc_timezone_payload_defaults_to_system_time() -> None:
    assert len(build_utc_timezone_payload()) == 12


def test_build_utc_timezone_payload_rejects_naive_datetime() -> None:
    with pytest.raises(ValueError, match="timezone-aware"):
        build_utc_timezone_payload(datetime(2026, 5, 1, 3, 55, 8))


def test_system_timezone_uses_standard_offset_and_dst_flag(monkeypatch) -> None:
    class LocalTime:
        tm_isdst = 1

    monkeypatch.setattr("rce.tools.fuji_ble_gps.camera.time.timezone", 28800)
    monkeypatch.setattr("rce.tools.fuji_ble_gps.camera.time.localtime", lambda: LocalTime())

    assert _system_timezone_hhmm_and_dst() == (-800, 1)


def test_timezone_hhmm_from_seconds_handles_fractional_hour_offsets() -> None:
    assert _timezone_hhmm_from_seconds(19800) == 530
    assert _timezone_hhmm_from_seconds(-12600) == -330


@pytest.mark.asyncio
async def test_sensitive_identity_values_are_skipped(tmp_path, monkeypatch) -> None:
    conn = FakeConn()
    conn.characteristics.add(uuids.CHAR_CAMERA_WIFI_PASSPHRASE_STRING)
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))
    monkeypatch.setitem(
        uuids.IDENTITY_READ_CHARS,
        "camera_wifi_passphrase",
        uuids.CHAR_CAMERA_WIFI_PASSPHRASE_STRING,
    )

    identity = await camera._read_identity(conn)

    assert "camera_wifi_passphrase" not in identity
    assert uuids.CHAR_CAMERA_WIFI_PASSPHRASE_STRING not in conn.reads


@pytest.mark.asyncio
async def test_wifi_info_reads_sensitive_credentials_and_launches_ap(tmp_path, monkeypatch) -> None:
    conn = WifiConn()
    conn.characteristics.add(uuids.CHAR_IMAGE_TRANSFER_SETTING_EX)
    session = Session(root=tmp_path)
    camera = FujiCamera(FakeBackend(conn), session)
    sleeps: list[float] = []

    async def fake_sleep(seconds: float) -> None:
        sleeps.append(seconds)

    monkeypatch.setattr("rce.tools.fuji_ble_gps.camera.asyncio.sleep", fake_sleep)

    info = await camera.wifi_info(
        device_name="Fuji-Laptop",
        ack_registration=True,
        pair_trigger_first=True,
        read_passphrase=True,
        launch_ap="get",
        ap_state_timeout=2.0,
    )

    assert "passphrase" not in info
    assert info["ssid"] == "FUJIFILM-GFX100II-0C3E"
    assert info["bssid"] == "38-7C-76-74-73-20"
    assert info["ap_state"] == "0180"
    assert info["ap_state_label"] == "launched"
    assert info["passphrase_present"] is True
    assert sleeps == [1.0]
    assert uuids.CHAR_CAMERA_WIFI_PASSPHRASE_STRING in conn.sensitive_reads
    assert (uuids.CHAR_IMAGE_TRANSFER_SETTING_EX, b"\x01", True) in conn.writes
    assert (uuids.CHAR_FUNCTION_LAUNCH, b"\x03\x00", True) in conn.writes

    credentials_path = session.path / "wifi_credentials.json"
    credentials = json.loads(credentials_path.read_text(encoding="utf-8"))
    assert credentials["passphrase"] == "CQAggA8AEEAVADjgIQA0"
    assert stat.S_IMODE(credentials_path.stat().st_mode) == 0o600

    redacted = (session.path / "wifi_info_redacted.json").read_text(encoding="utf-8")
    log = (session.path / "session.log").read_text(encoding="utf-8")
    assert "CQAggA8AEEAVADjgIQA0" not in redacted
    assert "CQAggA8AEEAVADjgIQA0" not in log
    assert "credentials_path" in redacted


@pytest.mark.asyncio
async def test_firmware_update_prepare_writes_request_and_launches_fw_ap(tmp_path, monkeypatch) -> None:
    conn = FirmwarePrepareConn()
    conn.characteristics.add(uuids.CHAR_IMAGE_TRANSFER_SETTING_EX)
    session = Session(root=tmp_path)
    camera = FujiCamera(FakeBackend(conn), session)
    sleeps: list[float] = []

    async def fake_sleep(seconds: float) -> None:
        sleeps.append(seconds)

    monkeypatch.setattr("rce.tools.fuji_ble_gps.camera.asyncio.sleep", fake_sleep)

    info = await camera.firmware_update_prepare(
        product_name="GFX100 II",
        request_file_name="GXUP0006.DAT",
        file_size=163_184_655,
        version="2.41",
        device_name="Fuji-Laptop",
        do_register=True,
        ack_registration=True,
        pair_trigger_first=True,
        read_passphrase=True,
    )

    assert info["ssid"] == "FUJIFILM-GFX100II-0C3E"
    assert info["ap_state"] == "0180"
    assert info["launch_ap"] == "fw_transfer"
    assert info["firmware_file_info_hex"] == "00" * 29
    assert info["firmware_request_notify_hex"] == "0100"
    assert info["firmware_launch_notify_hex"] == "0100"
    assert "passphrase" not in info
    assert uuids.CHAR_FIRMWARE_UPDATE_STATE_NOTIFY in conn.notifications
    assert conn.events[0] == ("read", uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER)
    assert (uuids.CHAR_CONNECTED_DEVICE_NAME, b"Fuji-Laptop\x00", True) in conn.writes
    assert any(write[0] == uuids.CHAR_FIRMWARE_UPDATE_REQUEST and len(write[1]) == 92 for write in conn.writes)
    assert (uuids.CHAR_FUNCTION_LAUNCH, b"\x05\x00", True) in conn.writes
    assert (session.path / "firmware_update_prepare.json").exists()
    assert (session.path / "payloads" / "firmware_update_request.bin").exists()
    assert sleeps == [1.0]


@pytest.mark.asyncio
async def test_firmware_helpers_report_missing_characteristics_and_timeout(tmp_path) -> None:
    camera = FujiCamera(FakeBackend(FakeConn()), Session(root=tmp_path))
    conn = FakeConn()

    assert await camera._read_firmware_file_info(conn) is None
    with pytest.raises(RuntimeError, match="state notify characteristic"):
        await camera._start_firmware_notify_queue(conn)
    with pytest.raises(RuntimeError, match="timed out"):
        await camera._wait_for_firmware_notify(asyncio.Queue(), timeout=0.0, label="test")


@pytest.mark.asyncio
async def test_wifi_info_can_skip_passphrase_and_registration(tmp_path) -> None:
    conn = WifiConn()
    session = Session(root=tmp_path)
    camera = FujiCamera(FakeBackend(conn), session)

    info = await camera.wifi_info(do_register=False)

    assert info["passphrase_present"] is False
    assert not (session.path / "wifi_credentials.json").exists()
    assert not any(write[0] == uuids.CHAR_CONNECTED_DEVICE_NAME for write in conn.writes)


@pytest.mark.asyncio
async def test_wifi_info_can_hold_ble_after_launch(tmp_path, monkeypatch) -> None:
    conn = WifiConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))
    sleeps: list[float] = []

    async def fake_sleep(seconds: float) -> None:
        sleeps.append(seconds)

    monkeypatch.setattr("rce.tools.fuji_ble_gps.camera.asyncio.sleep", fake_sleep)

    await camera.wifi_info(do_register=False, launch_ap="take", hold_after_launch=2.5)

    assert sleeps == [1.0, 2.5]


@pytest.mark.asyncio
async def test_wifi_info_rejects_unknown_launch_mode(tmp_path) -> None:
    camera = FujiCamera(FakeBackend(FakeConn()), Session(root=tmp_path))

    with pytest.raises(ValueError, match="unknown launch_ap"):
        await camera.wifi_info(launch_ap="movie")


@pytest.mark.asyncio
async def test_wifi_info_requires_passphrase_characteristic(tmp_path) -> None:
    conn = SparseConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    with pytest.raises(RuntimeError, match="passphrase characteristic"):
        await camera.wifi_info(do_register=False, read_passphrase=True)


@pytest.mark.asyncio
async def test_wifi_info_rejects_empty_passphrase(tmp_path) -> None:
    conn = EmptyPassphraseConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    with pytest.raises(RuntimeError, match="passphrase was empty"):
        await camera.wifi_info(do_register=False, read_passphrase=True)


@pytest.mark.asyncio
async def test_launch_ap_requires_launch_characteristic(tmp_path) -> None:
    conn = FakeConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    with pytest.raises(RuntimeError, match="AP launch characteristic"):
        await camera._launch_ap(conn, "get", timeout=0.0)


@pytest.mark.asyncio
async def test_launch_ap_requires_launched_state(tmp_path) -> None:
    conn = StuckApConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    with pytest.raises(RuntimeError, match="did not reach launched state"):
        await camera._launch_ap(conn, "get", timeout=0.0)


@pytest.mark.asyncio
async def test_read_ap_state_requires_state_characteristic(tmp_path) -> None:
    conn = SparseConn()
    camera = FujiCamera(FakeBackend(conn), Session(root=tmp_path))

    with pytest.raises(RuntimeError, match="AP state characteristic"):
        await camera._read_ap_state(conn)


async def _noop() -> None:
    return None
