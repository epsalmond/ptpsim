from __future__ import annotations

import sys
import types
import builtins

import pytest

from rce.tools.fuji_ble_gps.ble_backend import BleBackend, BleConnection, BleakBackend, DeviceInfo
from rce.tools.fuji_ble_gps.session import Session
from rce.tools.fuji_ble_gps import uuids


class FakeDescriptor:
    uuid = "00002902-0000-1000-8000-00805f9b34fb"
    handle = 3


class FakeCharacteristic:
    uuid = "aaaaaaaa-0000-0000-0000-000000000001"
    description = "fake char"
    properties = ["read", "write", "notify"]
    handle = 2
    descriptors = [FakeDescriptor()]


class FakeService:
    uuid = "bbbbbbbb-0000-0000-0000-000000000001"
    description = "fake service"
    handle = 1
    characteristics = [FakeCharacteristic()]


class FakeClient:
    address = "device-uuid"

    def __init__(self) -> None:
        self.services = [FakeService()]
        self.connected = False
        self.disconnected = False
        self.writes: list[tuple[str, bytes, bool]] = []
        self.notify_callback = None

    async def connect(self) -> None:
        self.connected = True

    async def disconnect(self) -> None:
        self.disconnected = True

    async def read_gatt_char(self, uuid: str) -> bytes:
        return b"abc"

    async def write_gatt_char(self, uuid: str, data: bytes, response: bool = True) -> None:
        self.writes.append((uuid, data, response))

    async def start_notify(self, uuid: str, callback) -> None:
        self.notify_callback = callback
        callback(FakeCharacteristic(), bytearray(b"\x05"))


class FakeGetServicesClient(FakeClient):
    def __init__(self) -> None:
        super().__init__()
        self.services = None

    async def get_services(self):
        return [FakeService()]


@pytest.mark.asyncio
async def test_ble_connection_logs_services_reads_writes_and_notifications(tmp_path) -> None:
    session = Session(root=tmp_path)
    client = FakeClient()
    notifications: list[tuple[str, bytes]] = []

    async with BleConnection(client, session) as conn:
        services = await conn.services_json()
        assert services[0]["uuid"] == FakeService.uuid
        assert services[0]["characteristics"][0]["uuid"] == FakeCharacteristic.uuid
        assert await conn.has_characteristic(FakeCharacteristic.uuid)
        assert not await conn.has_characteristic("missing")
        assert await conn.read(FakeCharacteristic.uuid) == b"abc"
        await conn.write(FakeCharacteristic.uuid, b"xyz", response=False)
        await conn.start_notify(FakeCharacteristic.uuid, lambda uuid, data: notifications.append((uuid, data)))

    assert client.connected is True
    assert client.disconnected is True
    assert client.writes == [(FakeCharacteristic.uuid, b"xyz", False)]
    assert notifications == [(FakeCharacteristic.uuid, b"\x05")]
    assert (session.path / "reads.jsonl").exists()
    assert (session.path / "writes.jsonl").exists()
    assert (session.path / "notifications.jsonl").exists()


@pytest.mark.asyncio
async def test_ble_connection_redacts_sensitive_reads(tmp_path) -> None:
    session = Session(root=tmp_path)
    client = FakeClient()

    conn = BleConnection(client, session)
    assert await conn.read(FakeCharacteristic.uuid, sensitive=True) == b"abc"

    reads = (session.path / "reads.jsonl").read_text(encoding="utf-8")
    assert "<redacted>" in reads
    assert "616263" not in reads
    assert '"sensitive": true' in reads


@pytest.mark.asyncio
async def test_ble_connection_service_fallback_and_missing_services(tmp_path) -> None:
    session = Session(root=tmp_path)
    fallback = BleConnection(FakeGetServicesClient(), session)
    assert (await fallback.services_json())[0]["uuid"] == FakeService.uuid

    missing_client = FakeClient()
    missing_client.services = None
    missing = BleConnection(missing_client, session)
    with pytest.raises(RuntimeError, match="does not expose discovered services"):
        await missing.services_json()


@pytest.mark.asyncio
async def test_base_backend_errors_and_no_match(tmp_path) -> None:
    backend = BleBackend(Session(root=tmp_path))
    with pytest.raises(NotImplementedError):
        await backend.scan()
    with pytest.raises(NotImplementedError):
        backend.connect(DeviceInfo(address="x", name="x"))

    class EmptyBackend(BleBackend):
        async def scan(self, timeout: float = 8.0):
            return [DeviceInfo(address="x", name=None), DeviceInfo(address="y", name="Other")]

    with pytest.raises(RuntimeError, match="no BLE device matching"):
        await EmptyBackend(Session(root=tmp_path)).find_device("GFX")


@pytest.mark.asyncio
async def test_base_backend_selects_strongest_rssi_match(tmp_path) -> None:
    class MultiBackend(BleBackend):
        async def scan(self, timeout: float = 8.0):
            return [
                DeviceInfo(address="weak", name="GFX100 II", rssi=-80),
                DeviceInfo(address="strong", name="GFX100 II", rssi=-40),
                DeviceInfo(address="unknown-rssi", name="GFX100 II", rssi=None),
            ]

    found = await MultiBackend(Session(root=tmp_path)).find_device("gfx100")

    assert found.address == "strong"


@pytest.mark.asyncio
async def test_base_backend_can_match_fuji_service_uuid_without_name(tmp_path) -> None:
    class ServiceBackend(BleBackend):
        async def scan(self, timeout: float = 8.0):
            return [
                DeviceInfo(address="other", name=None, rssi=-30, details={"service_uuids": []}),
                DeviceInfo(
                    address="camera",
                    name=None,
                    rssi=-70,
                    details={"service_uuids": [uuids.SERVICE_FUJI_CAMERA.upper()]},
                ),
            ]

    found = await ServiceBackend(Session(root=tmp_path)).find_device("GFX")

    assert found.address == "camera"


@pytest.mark.asyncio
async def test_bleak_backend_find_device_and_connect_with_mocked_bleak(tmp_path, monkeypatch) -> None:
    class FakeAdv:
        local_name = "GFX100 II"
        rssi = -41
        service_uuids = ["svc"]
        manufacturer_data = {1240: b"fuji"}

    class FakeDevice:
        address = "corebluetooth-uuid"
        name = None
        metadata = {"platform": "mac"}

    class FakeScanner:
        @staticmethod
        async def discover(timeout: float, return_adv: bool = False):
            assert timeout == 1.5
            assert return_adv is True
            return {"x": (FakeDevice(), FakeAdv())}

    made_clients = []

    class FakeBleakClient:
        def __init__(self, address: str, timeout: float) -> None:
            self.address = address
            self.timeout = timeout
            made_clients.append(self)

    monkeypatch.setitem(
        sys.modules,
        "bleak",
        types.SimpleNamespace(BleakScanner=FakeScanner, BleakClient=FakeBleakClient),
    )

    session = Session(root=tmp_path)
    backend = BleakBackend(session)
    devices = await backend.scan(timeout=1.5)
    assert devices == [
        DeviceInfo(
            address="corebluetooth-uuid",
            name="GFX100 II",
            rssi=-41,
            details={
                "metadata": {"platform": "mac"},
                "service_uuids": ["svc"],
                "manufacturer_data_keys": [1240],
            },
        )
    ]
    found = await backend.find_device("gfx100", timeout=1.5)
    conn = backend.connect(found)
    assert conn.client.address == "corebluetooth-uuid"
    assert made_clients[0].timeout == 60.0


@pytest.mark.asyncio
async def test_bleak_backend_connects_first_live_detection_with_native_device(
    tmp_path, monkeypatch
) -> None:
    class FakeAdv:
        local_name = None
        rssi = -47
        service_uuids = [uuids.SERVICE_FUJI_CAMERA.upper()]
        manufacturer_data = {1240: b"fuji"}

    class FakeDevice:
        address = "live-corebluetooth-uuid"
        name = None
        metadata = {"platform": "mac"}

    live_device = FakeDevice()

    class FakeScanner:
        def __init__(self, detection_callback=None) -> None:
            self.detection_callback = detection_callback

        async def __aenter__(self):
            self.detection_callback(live_device, FakeAdv())
            return self

        async def __aexit__(self, exc_type, exc, tb) -> None:
            pass

    made_clients = []

    class FakeBleakClient:
        def __init__(self, address, timeout: float) -> None:
            self.address = address
            self.timeout = timeout
            made_clients.append(self)

    monkeypatch.setitem(
        sys.modules,
        "bleak",
        types.SimpleNamespace(BleakScanner=FakeScanner, BleakClient=FakeBleakClient),
    )

    session = Session(root=tmp_path)
    backend = BleakBackend(session)
    found = await backend.find_device("GFX100 II", timeout=1.5)
    conn = backend.connect(found)

    assert found.address == "live-corebluetooth-uuid"
    assert found.details["match_reason"] == "fuji_service_uuid"
    assert found.details["connect_strategy"] == "connect_on_detection"
    assert conn.client.address is live_device
    assert made_clients[0].timeout == 60.0
    assert "connect_on_detection" in (session.path / "devices.jsonl").read_text(encoding="utf-8")


@pytest.mark.asyncio
async def test_bleak_backend_live_detection_times_out_without_match(tmp_path, monkeypatch) -> None:
    class FakeAdv:
        local_name = "Other"
        rssi = -47
        service_uuids = []
        manufacturer_data = {}

    class FakeDevice:
        address = "not-camera"
        name = "Other"
        metadata = {}

    class FakeScanner:
        def __init__(self, detection_callback=None) -> None:
            self.detection_callback = detection_callback

        async def __aenter__(self):
            self.detection_callback(FakeDevice(), FakeAdv())
            return self

        async def __aexit__(self, exc_type, exc, tb) -> None:
            pass

    monkeypatch.setitem(
        sys.modules,
        "bleak",
        types.SimpleNamespace(BleakScanner=FakeScanner),
    )

    with pytest.raises(RuntimeError, match="no live BLE advertisement"):
        await BleakBackend(Session(root=tmp_path)).find_device("GFX100 II", timeout=0.01)


@pytest.mark.asyncio
async def test_bleak_backend_scan_falls_back_for_old_bleak_api(tmp_path, monkeypatch) -> None:
    class FakeDevice:
        address = "legacy-address"
        name = "Legacy GFX100 II"
        rssi = -55
        metadata = {"legacy": True}

    class FakeScanner:
        @staticmethod
        async def discover(*, timeout: float, return_adv: bool = False):
            if return_adv:
                raise TypeError("old bleak")
            return [FakeDevice()]

    monkeypatch.setitem(sys.modules, "bleak", types.SimpleNamespace(BleakScanner=FakeScanner))

    devices = await BleakBackend(Session(root=tmp_path)).scan(timeout=2.0)

    assert devices == [
        DeviceInfo(
            address="legacy-address",
            name="Legacy GFX100 II",
            rssi=-55,
            details={"metadata": {"legacy": True}},
        )
    ]


@pytest.mark.asyncio
async def test_bleak_backend_import_errors_are_clear(tmp_path, monkeypatch) -> None:
    real_import = builtins.__import__

    def fake_import(name, *args, **kwargs):
        if name == "bleak":
            raise ImportError("no bleak")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", fake_import)
    backend = BleakBackend(Session(root=tmp_path))

    with pytest.raises(RuntimeError, match="live BLE commands require `bleak`"):
        await backend.scan()
    with pytest.raises(RuntimeError, match="live BLE commands require `bleak`"):
        await backend.find_device("GFX100 II")
    with pytest.raises(RuntimeError, match="live BLE commands require `bleak`"):
        backend.connect(DeviceInfo(address="x", name="x"))
