from __future__ import annotations

import argparse
import asyncio
from datetime import UTC, datetime
import json
from pathlib import Path
import subprocess
import types

import pytest

from rce.tools.fuji_ble_gps import evidence, uuids
from rce.tools.fuji_ble_gps.ble_backend import DeviceInfo


def args(tmp_path, **kwargs):
    values = {
        "state_file": tmp_path / "state.json",
        "name": "GFX100 II",
        "timeout": 0.01,
        "match": ["FUJIFILM", "GFX"],
        "image": None,
        "note": "",
        "value": "unknown",
        "key": "camera_screen_state",
        "session_dir": None,
    }
    values.update(kwargs)
    return argparse.Namespace(**values)


def test_statefile_helpers_and_artifacts(tmp_path) -> None:
    state_file = tmp_path / "state.json"

    state = evidence.default_state("Camera")
    assert state["target_name"] == "Camera"
    assert evidence.load_state(state_file, "Camera")["target_name"] == "Camera"

    evidence.save_state(state_file, {"evidence": {}, "history": []})
    loaded = evidence.load_state(state_file, "Camera")
    assert loaded["schema_version"] == 1
    assert loaded["state_label"] == "unclassified"

    artifact = Path(evidence.write_artifact(state_file, "test/key", "hello world.txt", "payload"))
    assert artifact.exists()
    assert "hello_world.txt" in artifact.name


def test_run_command_success_and_failure(monkeypatch) -> None:
    monkeypatch.setattr(
        evidence.subprocess,
        "run",
        lambda *a, **kw: types.SimpleNamespace(returncode=3, stdout="out", stderr="err"),
    )
    result = evidence.run_command(["cmd"])
    assert result.returncode == 3
    assert result.text == "outerr"

    def raise_oserror(*_args, **_kwargs):
        raise OSError("missing")

    monkeypatch.setattr(evidence.subprocess, "run", raise_oserror)
    result = evidence.run_command(["cmd"])
    assert result.returncode == 127
    assert "missing" in result.text

    def raise_timeout(*_args, **_kwargs):
        raise subprocess.TimeoutExpired("cmd", 1)

    monkeypatch.setattr(evidence.subprocess, "run", raise_timeout)
    assert evidence.run_command(["cmd"]).returncode == 127


def test_record_and_classify_state_labels(tmp_path) -> None:
    state_file = tmp_path / "state.json"
    state = evidence.record_evidence(
        state_file,
        "ble_advertisement_scan",
        "present",
        source="test",
    )
    assert state["state_label"] == "camera_advertising_host_unknown"
    assert evidence.evidence_value(state, "missing") is None
    state["evidence"]["bad"] = "not-a-record"
    state["evidence"]["none"] = {"value": 1}
    assert evidence.evidence_value(state, "bad") is None
    assert evidence.evidence_value(state, "none") is None


def test_transient_screen_state_must_be_fresh() -> None:
    state = evidence.default_state()
    state["evidence"] = {
        "camera_screen_state": {
            "value": "waiting_for_connected",
            "observed_at": "2000-01-01T00:00:00Z",
        }
    }

    assert evidence.classify_state(state) == "unclassified"
    assert not evidence.evidence_is_fresh(
        state,
        "camera_screen_state",
        max_age_seconds=120,
        now=datetime(2000, 1, 1, 0, 3, 0, tzinfo=UTC),
    )
    assert evidence.fresh_evidence_value(
        state,
        "camera_screen_state",
        max_age_seconds=120,
    ) is None


def test_gps_sync_ready_must_be_fresh() -> None:
    state = evidence.default_state()
    state["evidence"] = {
        "gps_sync_ready": {
            "value": "present",
            "observed_at": "2000-01-01T00:00:00Z",
        }
    }

    assert evidence.classify_state(state) == "unclassified"


def test_malformed_observed_at_is_not_fresh() -> None:
    state = {
        "evidence": {
            "camera_screen_state": {
                "value": "waiting_for_connected",
                "observed_at": "not-a-date",
            }
        }
    }

    assert not evidence.evidence_is_fresh(
        state,
        "camera_screen_state",
        max_age_seconds=120,
        now=datetime(2026, 5, 2, tzinfo=UTC),
    )

    state["evidence"]["camera_screen_state"]["observed_at"] = "2026-05-02T00:00:00"
    assert evidence.evidence_is_fresh(
        state,
        "camera_screen_state",
        max_age_seconds=120,
        now=datetime(2026, 5, 2, 0, 1, 0, tzinfo=UTC),
    )


def test_print_evidence_summary_empty_invalid_and_populated(capsys) -> None:
    evidence.print_evidence_summary({"evidence": {}})
    assert "evidence: none" in capsys.readouterr().out

    evidence.print_evidence_summary(
        {
            "evidence": {
                "bad": "not-a-record",
                "good": {
                    "value": "present",
                    "source": "test",
                    "observed_at": "2026-05-02T00:00:00Z",
                    "details": {"address": "camera"},
                    "artifacts": ["artifact.txt"],
                },
            }
        }
    )
    out = capsys.readouterr().out
    assert "bad: invalid" in out
    assert "good: present" in out
    assert '"address": "camera"' in out
    assert '"artifact.txt"' in out


@pytest.mark.parametrize(
    ("entries", "label"),
    [
        ({"camera_screen_state": "pair_prompt_pending"}, "host_pair_prompt_pending"),
        (
            {"camera_screen_state": "app_function_not_found_retry"},
            "camera_ap_launched_app_function_not_found",
        ),
        (
            {"camera_screen_state": "waiting_for_connected"},
            "camera_ap_waiting_for_ptpip_connection",
        ),
        (
            {"camera_ap_wifi_association": "present"},
            "camera_ap_wifi_associated_ethernet_default",
        ),
        (
            {"camera_ap_ble_launch": "not_launched"},
            "camera_ap_ble_launch_not_launched",
        ),
        (
            {"camera_ap_ble_launch": "launched"},
            "camera_ap_ble_launch_launched",
        ),
        (
            {"camera_ap_ptpip_probe": "get_prop_d212_ok"},
            "camera_ap_ptpip_get_prop_d212_ok",
        ),
        (
            {"camera_ap_ptpip_probe": "get_prop_ok"},
            "camera_ap_ptpip_get_prop_ok",
        ),
        (
            {"camera_ap_ptpip_probe": "open_session_ok"},
            "camera_ap_ptpip_open_session_ok",
        ),
        (
            {"camera_ap_ptpip_probe": "init_ack_present"},
            "camera_ap_ptpip_init_ack_present",
        ),
        (
            {"camera_ap_ptpip_probe": "tcp_connected_init_timeout"},
            "camera_ap_ptpip_tcp_connected_init_timeout",
        ),
        (
            {"camera_ap_ptpip_probe": "tcp_connected_init_no_response"},
            "camera_ap_ptpip_tcp_connected_init_timeout",
        ),
        (
            {"camera_ap_ptpip_probe": "tcp_connect_absent"},
            "camera_ap_ptpip_tcp_connect_absent",
        ),
        (
            {"camera_ap_ptpip_probe": "route_failed"},
            "camera_ap_ptpip_route_failed",
        ),
        (
            {"camera_screen_state": "registration_mode"},
            "camera_pairing_registration_screen",
        ),
        (
            {"camera_screen_state": "device_not_found_continue_search"},
            "camera_pairing_registration_timeout",
        ),
        (
            {"camera_screen_state": "connection_lost"},
            "camera_connection_lost_screen",
        ),
        (
            {
                "session_gps_payload_written": "present",
                "camera_gps_icon": "absent",
                "camera_registered_name_display": "empty",
            },
            "gps_payload_written_camera_icon_absent_name_empty",
        ),
        (
            {"session_gps_payload_written": "present", "camera_gps_icon": "absent"},
            "gps_payload_written_camera_icon_absent",
        ),
        (
            {"session_gps_payload_written": "present", "camera_gps_icon": "present"},
            "gps_payload_written_camera_icon_present",
        ),
        ({"gps_sync_ready": "present"}, "gps_sync_ready"),
        (
            {
                "gps_sync_ready": "present",
                "host_registered_in_camera_menu": "present",
                "camera_registered_name_display": "empty",
            },
            "gps_sync_ready_camera_name_empty",
        ),
        (
            {"host_registered_in_camera_menu": "present", "macos_ioreg_device": "present"},
            "host_and_camera_registered_connected",
        ),
        (
            {
                "host_registered_in_camera_menu": "present",
                "ble_direct_connect_probe": "present",
            },
            "camera_registered_host_direct_connectable",
        ),
        (
            {
                "host_registered_in_camera_menu": "present",
                "system_profiler_device": "not_connected",
            },
            "host_and_camera_registered_not_connected",
        ),
        (
            {
                "host_registered_in_camera_menu": "present",
                "camera_registered_name_display": "empty",
                "macos_bluetooth_settings": "not_connected",
            },
            "camera_registered_empty_name_host_not_connected",
        ),
        (
            {
                "host_registered_in_camera_menu": "present",
                "macos_bluetooth_settings": "connected",
            },
            "host_and_camera_registered_connected",
        ),
        (
            {
                "host_registered_in_camera_menu": "present",
                "macos_bluetooth_settings": "not_connected",
            },
            "host_and_camera_registered_not_connected",
        ),
        (
            {
                "host_registered_in_camera_menu": "present",
                "ble_advertisement_scan": "present",
                "blueutil_paired_device": "absent",
                "system_profiler_device": "absent",
                "macos_ioreg_device": "absent",
            },
            "camera_registered_host_unlisted_advertising",
        ),
        (
            {
                "host_registered_in_camera_menu": "present",
                "session_registration_ack_written": "present",
                "session_disconnect_after_ack": "absent",
                "ble_advertisement_scan": "absent",
                "blueutil_paired_device": "absent",
                "system_profiler_device": "absent",
                "macos_ioreg_device": "absent",
            },
            "camera_registered_host_unlisted_not_advertising",
        ),
        (
            {
                "session_registration_name_written": "present",
                "session_registration_ack_written": "present",
                "session_disconnect_after_ack": "present",
            },
            "host_connected_registration_ack_written_camera_disconnects",
        ),
        (
            {
                "session_registration_name_written": "present",
                "session_registration_id_read": "present",
                "session_registration_ack_written": "absent",
            },
            "host_connected_registration_name_written_ack_skipped",
        ),
        (
            {"session_pair_trigger_read": "present", "host_registered_in_camera_menu": "absent"},
            "host_orphaned_gatt_access_camera_not_registered",
        ),
        (
            {"session_pair_trigger_read": "present"},
            "host_pair_only_complete_camera_not_registered",
        ),
        (
            {
                "blueutil_paired_device": "present",
                "host_registered_in_camera_menu": "absent",
                "camera_pairing_mode": "absent",
            },
            "host_trusted_camera_unknown_not_pairing",
        ),
        (
            {
                "system_profiler_device": "not_connected",
                "macos_ioreg_device": "absent",
                "ble_advertisement_scan": "absent",
            },
            "host_remembers_camera_camera_unknown_not_advertising",
        ),
        (
            {
                "macos_known_device_plist": "absent",
                "macos_ioreg_device": "absent",
                "system_profiler_device": "absent",
                "ble_advertisement_scan": "absent",
            },
            "host_unknown_camera_unknown_not_advertising",
        ),
        (
            {
                "camera_screen_state": "ready_to_take_photo",
                "camera_bluetooth_status": "ready_not_connected",
            },
            "camera_ready_bluetooth_ready_not_connected",
        ),
        (
            {"camera_screen_state": "ready_to_take_photo"},
            "camera_ready_to_take_photo",
        ),
        (
            {"camera_screen_state": "ready_to_shoot_video"},
            "camera_ready_to_shoot_video",
        ),
        ({}, "unclassified"),
    ],
)
def test_classify_state_all_labels(entries, label) -> None:
    state = evidence.default_state()
    state["evidence"] = {
        key: {"value": value}
        for key, value in entries.items()
    }
    assert evidence.classify_state(state) == label


def test_parse_system_profiler_device_state() -> None:
    text = """
Bluetooth:
  Connected:
    GFX100 II:
      Address: 38:7C:76:74:73:21
"""
    assert evidence.parse_system_profiler_device_state(text, "GFX100 II") == (
        "connected",
        {"address": "38:7C:76:74:73:21"},
    )
    text = """
Bluetooth:
  Not Connected:
    GFX100 II:
"""
    assert evidence.parse_system_profiler_device_state(text, "GFX100 II") == ("not_connected", {})
    assert evidence.parse_system_profiler_device_state("GFX100 II:\n", "GFX100 II") == (
        "present_unknown",
        {},
    )
    assert evidence.parse_system_profiler_device_state("Other", "GFX100 II") == ("absent", {})


def test_macos_collectors(monkeypatch, tmp_path, capsys) -> None:
    calls = []

    def fake_run(command, timeout=20.0):
        calls.append(command)
        text = "GFX100 II" if command[0] == "plutil" else "Other"
        return evidence.CommandResult(command, 0, text, "")

    monkeypatch.setattr(evidence, "run_command", fake_run)

    assert evidence.collect_macos_known_device_plist(args(tmp_path)) == 0
    assert "macos_known_device_plist=present" in capsys.readouterr().out

    assert evidence.collect_macos_ioreg_device(args(tmp_path)) == 0
    assert evidence.load_state(tmp_path / "state.json")["evidence"]["macos_ioreg_device"]["value"] == "absent"

    monkeypatch.setattr(
        evidence,
        "run_command",
        lambda command, timeout=20.0: evidence.CommandResult(command, 127, "", "missing"),
    )
    evidence.collect_macos_ioreg_device(args(tmp_path))
    evidence.collect_system_profiler_device(args(tmp_path))
    state = evidence.load_state(tmp_path / "state.json")
    assert state["evidence"]["macos_ioreg_device"]["value"] == "unknown"
    assert state["evidence"]["system_profiler_device"]["value"] == "unknown"


def test_system_profiler_and_usb_collectors(monkeypatch, tmp_path) -> None:
    def fake_run(command, timeout=20.0):
        if "SPBluetoothDataType" in command:
            return evidence.CommandResult(command, 0, "Not Connected:\n  GFX100 II:\n", "")
        return evidence.CommandResult(command, 0, '{"name":"FUJIFILM"}', "")

    monkeypatch.setattr(evidence, "run_command", fake_run)
    evidence.collect_system_profiler_device(args(tmp_path))
    evidence.collect_camera_usb_probe(args(tmp_path))
    state = evidence.load_state(tmp_path / "state.json")
    assert state["evidence"]["system_profiler_device"]["value"] == "not_connected"
    assert state["evidence"]["camera_usb_device"]["value"] == "present"

    monkeypatch.setattr(
        evidence,
        "run_command",
        lambda command, timeout=20.0: evidence.CommandResult(command, 127, "", "missing"),
    )
    evidence.collect_camera_usb_probe(args(tmp_path))
    assert evidence.load_state(tmp_path / "state.json")["evidence"]["camera_usb_device"]["value"] == "unknown"


def test_blueutil_collectors(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(
        evidence,
        "run_command",
        lambda command, timeout=20.0: evidence.CommandResult(command, 0, "GFX100 II\n", ""),
    )
    evidence.collect_blueutil_paired_device(args(tmp_path))
    evidence.collect_blueutil_connected_device(args(tmp_path))
    state = evidence.load_state(tmp_path / "state.json")
    assert state["evidence"]["blueutil_paired_device"]["value"] == "present"
    assert state["evidence"]["blueutil_connected_device"]["value"] == "present"

    monkeypatch.setattr(
        evidence,
        "run_command",
        lambda command, timeout=20.0: evidence.CommandResult(command, 127, "", "missing"),
    )
    evidence.collect_blueutil_paired_device(args(tmp_path))
    assert evidence.load_state(tmp_path / "state.json")["evidence"]["blueutil_paired_device"]["value"] == "unknown"


@pytest.mark.asyncio
async def test_ble_advertisement_scan_present_absent_and_error(monkeypatch, tmp_path) -> None:
    class FakeSession:
        path = tmp_path / "ble-session"

        def __init__(self):
            self.path.mkdir(exist_ok=True)

    class FakeBackend:
        def __init__(self, session):
            self.session = session

        async def scan(self, timeout: float):
            return [
                DeviceInfo(address="other", name="Other", rssi=-80),
                DeviceInfo(
                    address="camera",
                    name=None,
                    rssi=-60,
                    details={"service_uuids": [uuids.SERVICE_FUJI_CAMERA]},
                ),
            ]

    monkeypatch.setattr(evidence, "Session", FakeSession)
    monkeypatch.setattr(evidence, "BleakBackend", FakeBackend)

    await evidence.collect_ble_advertisement_scan_async(args(tmp_path))
    state = evidence.load_state(tmp_path / "state.json")
    assert state["evidence"]["ble_advertisement_scan"]["value"] == "present"
    assert state["state_label"] == "camera_advertising_host_unknown"

    class EmptyBackend(FakeBackend):
        async def scan(self, timeout: float):
            return []

    monkeypatch.setattr(evidence, "BleakBackend", EmptyBackend)
    await evidence.collect_ble_advertisement_scan_async(args(tmp_path))
    assert evidence.load_state(tmp_path / "state.json")["evidence"]["ble_advertisement_scan"]["value"] == "absent"

    class ErrorBackend(FakeBackend):
        async def scan(self, timeout: float):
            raise RuntimeError("no bluetooth")

    monkeypatch.setattr(evidence, "BleakBackend", ErrorBackend)
    await evidence.collect_ble_advertisement_scan_async(args(tmp_path))
    assert evidence.load_state(tmp_path / "state.json")["evidence"]["ble_advertisement_scan"]["value"] == "unknown"


@pytest.mark.asyncio
async def test_ble_direct_connect_probe_present_and_absent(monkeypatch, tmp_path) -> None:
    class FakeSession:
        path = tmp_path / "direct-session"

        def __init__(self):
            self.path.mkdir(exist_ok=True)

        def write_json(self, name, _payload):
            (self.path / name).write_text("[]", encoding="utf-8")

    class FakeConn:
        async def __aenter__(self):
            return self

        async def __aexit__(self, exc_type, exc, tb):
            return None

        async def services_json(self):
            return [{"uuid": "service"}]

    class FakeBackend:
        fail = False

        def __init__(self, session):
            self.session = session

        def connect(self, device):
            assert device.address == "corebluetooth-id"
            if self.fail:
                raise RuntimeError("not reachable")
            return FakeConn()

    monkeypatch.setattr(evidence, "Session", FakeSession)
    monkeypatch.setattr(evidence, "BleakBackend", FakeBackend)

    await evidence.collect_ble_direct_connect_probe_async(args(tmp_path, address="corebluetooth-id"))
    state = evidence.load_state(tmp_path / "state.json")
    assert state["evidence"]["ble_direct_connect_probe"]["value"] == "present"

    FakeBackend.fail = True
    await evidence.collect_ble_direct_connect_probe_async(args(tmp_path, address="corebluetooth-id"))
    state = evidence.load_state(tmp_path / "state.json")
    assert state["evidence"]["ble_direct_connect_probe"]["value"] == "absent"


def test_ble_advertisement_scan_sync_wrapper(monkeypatch, tmp_path) -> None:
    called = []

    async def fake_collect(args):
        called.append(args.timeout)
        return 0

    monkeypatch.setattr(evidence, "collect_ble_advertisement_scan_async", fake_collect)
    assert evidence.collect_ble_advertisement_scan(args(tmp_path, timeout=3.0)) == 0
    assert called == [3.0]


def test_ble_direct_connect_probe_sync_wrapper(monkeypatch, tmp_path) -> None:
    called = []

    async def fake_collect(args):
        called.append(args.address)
        return 0

    monkeypatch.setattr(evidence, "collect_ble_direct_connect_probe_async", fake_collect)
    assert evidence.collect_ble_direct_connect_probe(args(tmp_path, address="corebluetooth-id")) == 0
    assert called == ["corebluetooth-id"]


def test_camera_screen_capture_branches(monkeypatch, tmp_path) -> None:
    image = tmp_path / "screen.jpg"
    image.write_bytes(b"jpg")
    evidence.collect_camera_screen_capture(args(tmp_path, image=str(image)))
    state = evidence.load_state(tmp_path / "state.json")
    assert state["evidence"]["camera_screen_capture"]["value"] == "captured"

    evidence.collect_camera_screen_capture(args(tmp_path, image=str(tmp_path / "missing.jpg")))
    assert evidence.load_state(tmp_path / "state.json")["evidence"]["camera_screen_capture"]["value"] == "unavailable"

    monkeypatch.setattr(evidence.shutil, "which", lambda name: None)
    evidence.collect_camera_screen_capture(args(tmp_path, image=None))
    assert evidence.load_state(tmp_path / "state.json")["evidence"]["camera_screen_capture"]["value"] == "unavailable"

    destination_holder = {}

    def fake_run(command, timeout=20.0):
        destination = Path(command[-1])
        destination_holder["path"] = destination
        destination.write_bytes(b"jpg")
        return evidence.CommandResult(command, 0, "ok", "")

    monkeypatch.setattr(evidence.shutil, "which", lambda name: "/usr/local/bin/imagesnap")
    monkeypatch.setattr(evidence, "run_command", fake_run)
    evidence.collect_camera_screen_capture(args(tmp_path, image=None))
    assert destination_holder["path"].exists()

    monkeypatch.setattr(
        evidence,
        "run_command",
        lambda command, timeout=20.0: evidence.CommandResult(command, 1, "", ""),
    )
    evidence.collect_camera_screen_capture(args(tmp_path, image=None))
    assert evidence.load_state(tmp_path / "state.json")["evidence"]["camera_screen_capture"]["value"] == "unavailable"


def test_manual_and_evaluate_commands(monkeypatch, tmp_path, capsys) -> None:
    state_file = tmp_path / "state.json"
    assert evidence.main(
        [
            "camera-screen-manual",
            "--state-file",
            str(state_file),
            "--value",
            "pair_prompt_pending",
        ]
    ) == 0
    assert "host_pair_prompt_pending" in capsys.readouterr().out
    assert evidence.main(["evaluate", "--state-file", str(state_file)]) == 0
    assert evidence.load_state(state_file)["state_label"] == "host_pair_prompt_pending"
    assert evidence.main(["evaluate", "--state-file", str(state_file), "--verbose"]) == 0
    assert "camera_screen_state: pair_prompt_pending" in capsys.readouterr().out

    refresh_calls = []

    def fake_refresh(refresh_args):
        refresh_calls.append(refresh_args)
        evidence.record_evidence(
            refresh_args.state_file,
            "camera_screen_state",
            "registration_mode",
            target_name=refresh_args.name,
            source="fake_camera_screen_vision",
        )
        return 0

    monkeypatch.setattr(evidence, "refresh_camera_screen_evidence", fake_refresh)
    assert evidence.main(
        [
            "evaluate",
            "--state-file",
            str(state_file),
            "--refresh-screen",
            "--screen-device-name",
            "iPhone",
            "--screen-warmup",
            "5",
            "--screen-zoom",
            "2",
        ]
    ) == 0
    assert refresh_calls[-1].screen_device_name == "iPhone"
    assert evidence.load_state(state_file)["state_label"] == "camera_pairing_registration_screen"

    assert evidence.collect_manual_value(
        args(tmp_path, key="camera_pairing_mode", value="present", note="camera menu")
    ) == 0

    assert evidence.main(
        [
            "reset",
            "--state-file",
            str(state_file),
            "--reason",
            "start over",
        ]
    ) == 0
    reset_state = evidence.load_state(state_file)
    assert reset_state["state_label"] == "unclassified"
    assert reset_state["evidence"] == {}
    assert reset_state["history"][0]["details"]["reason"] == "start over"


def test_refresh_camera_screen_evidence_builds_screen_args(monkeypatch, tmp_path) -> None:
    from rce.tools.fuji_ble_gps import screen_vision

    calls = []

    def fake_run_read_state(screen_args):
        calls.append(screen_args)
        return 7

    monkeypatch.setattr(screen_vision, "run_read_state", fake_run_read_state)

    rc = evidence.refresh_camera_screen_evidence(
        argparse.Namespace(
            state_file=tmp_path / "state.json",
            name="GFX100 II",
            screen_device_name="iPhone",
            screen_timeout=11.0,
            screen_warmup=5.0,
            screen_zoom=2.0,
        )
    )

    assert rc == 7
    assert calls[0].device_name == "iPhone"
    assert calls[0].timeout == 11.0
    assert calls[0].warmup == 5.0
    assert calls[0].zoom == 2.0
    assert calls[0].no_evidence is False
    assert calls[0].lcd_box_file == screen_vision.DEFAULT_LCD_BOX_FILE


def make_camera_ap_wifi_session(tmp_path: Path, **overrides: str) -> Path:
    session = tmp_path / "camera_ap_wifi_20260502T000000Z"
    session.mkdir()
    values = {
        "session": str(session),
        "associated": "present",
        "wifi_interface": "en0",
        "ssid": "FUJIFILM-AP",
        "bssid": "00:11:22:33:44:55",
        "local_ip": "192.168.0.136",
        "target_ip": "192.168.0.1",
        "default_route": "en7",
        "internet_route": "en7",
        "camera_route": "en0",
    }
    values.update(overrides)
    (session / "summary.txt").write_text(
        "# camera AP Wi-Fi summary\n" + "\n".join(f"{key}={value}" for key, value in values.items()) + "\n",
        encoding="utf-8",
    )
    return session


def test_camera_ap_wifi_summary_helpers(tmp_path) -> None:
    session = make_camera_ap_wifi_session(tmp_path)
    summary = evidence.parse_key_value_file(session / "summary.txt")

    assert summary["local_ip"] == "192.168.0.136"
    assert evidence.evaluate_camera_ap_wifi_summary(summary) == (
        "present",
        "Wi-Fi associated to camera AP with Ethernet default/internet route preserved",
    )

    assert evidence.evaluate_camera_ap_wifi_summary({})[0] == "absent"
    assert evidence.evaluate_camera_ap_wifi_summary({**summary, "associated": "absent"}) == (
        "absent",
        "associated is not present",
    )
    assert evidence.evaluate_camera_ap_wifi_summary({**summary, "camera_route": "en7"}) == (
        "absent",
        "camera endpoint route is not on Wi-Fi",
    )
    assert evidence.evaluate_camera_ap_wifi_summary({**summary, "default_route": "en0"}) == (
        "absent",
        "default route moved to Wi-Fi",
    )
    assert evidence.evaluate_camera_ap_wifi_summary({**summary, "internet_route": "en0"}) == (
        "absent",
        "internet route moved to Wi-Fi",
    )


def test_camera_ap_wifi_session_collector(monkeypatch, tmp_path) -> None:
    session = make_camera_ap_wifi_session(tmp_path)
    state_file = tmp_path / "state.json"

    assert evidence.collect_camera_ap_wifi_session(
        args(tmp_path, state_file=state_file, session_dir=str(session))
    ) == 0
    state = evidence.load_state(state_file)
    assert state["evidence"]["camera_ap_wifi_association"]["value"] == "present"
    assert state["evidence"]["camera_ap_wifi_association"]["details"]["internet_route"] == "en7"
    assert state["state_label"] == "camera_ap_wifi_associated_ethernet_default"

    missing_summary = tmp_path / "camera_ap_wifi_missing"
    missing_summary.mkdir()
    evidence.collect_camera_ap_wifi_session(
        args(tmp_path, state_file=tmp_path / "missing-state.json", session_dir=str(missing_summary))
    )
    missing_state = evidence.load_state(tmp_path / "missing-state.json")
    assert missing_state["evidence"]["camera_ap_wifi_association"]["value"] == "unavailable"

    root = tmp_path / "sessions"
    root.mkdir()
    assert evidence.latest_camera_ap_wifi_session(root) is None
    (root / "camera_ap_wifi_a").mkdir()
    (root / "camera_ap_wifi_b").mkdir()
    assert evidence.latest_camera_ap_wifi_session(root).name == "camera_ap_wifi_b"

    monkeypatch.setattr(evidence, "latest_camera_ap_wifi_session", lambda: session)
    assert evidence.resolve_camera_ap_wifi_session_dir(args(tmp_path, session_dir=None)) == session

    monkeypatch.setattr(evidence, "latest_camera_ap_wifi_session", lambda: None)
    evidence.collect_camera_ap_wifi_session(args(tmp_path, state_file=tmp_path / "none-state.json"))
    none_state = evidence.load_state(tmp_path / "none-state.json")
    assert none_state["evidence"]["camera_ap_wifi_association"]["value"] == "unavailable"


def make_camera_ap_ble_session(tmp_path: Path, log_text: str) -> Path:
    session = tmp_path / "laptop_ble_gps_20260502T000000Z"
    session.mkdir()
    (session / "session.log").write_text(log_text, encoding="utf-8")
    return session


@pytest.mark.parametrize(
    ("log_text", "value", "last_label"),
    [
        (
            "launching camera AP mode=take value=0400\n"
            "ap_state=0080 label=not_launched\n"
            "ap_state=0180 label=launched\n",
            "launched",
            "launched",
        ),
        (
            "launching camera AP mode=take value=0400\n"
            "ap_state=0080 label=not_launched\n"
            "ap_state=0080 label=not_launched\n",
            "not_launched",
            "not_launched",
        ),
        ("connected camera\n", "not_requested", ""),
        ("launching camera AP mode=take value=0400\n", "unknown", ""),
    ],
)
def test_camera_ap_ble_log_evaluation(log_text, value, last_label) -> None:
    evaluated, _reason, details = evidence.evaluate_camera_ap_ble_log(log_text)

    assert evaluated == value
    assert details["last_ap_state_label"] == last_label


def test_camera_ap_ble_session_collector(monkeypatch, tmp_path) -> None:
    session = make_camera_ap_ble_session(
        tmp_path,
        "launching camera AP mode=take value=0400\n"
        "ap_state=0080 label=not_launched\n"
        "ap_state=0080 label=not_launched\n",
    )
    state_file = tmp_path / "state.json"

    assert evidence.collect_camera_ap_ble_session(
        args(tmp_path, state_file=state_file, session_dir=str(session))
    ) == 0
    state = evidence.load_state(state_file)
    record = state["evidence"]["camera_ap_ble_launch"]
    assert record["value"] == "not_launched"
    assert record["details"]["last_ap_state"] == "0080"
    assert state["state_label"] == "camera_ap_ble_launch_not_launched"

    missing_log = tmp_path / "laptop_ble_gps_missing_log"
    missing_log.mkdir()
    evidence.collect_camera_ap_ble_session(
        args(tmp_path, state_file=tmp_path / "missing-log-state.json", session_dir=str(missing_log))
    )
    missing_state = evidence.load_state(tmp_path / "missing-log-state.json")
    assert missing_state["evidence"]["camera_ap_ble_launch"]["value"] == "unavailable"

    monkeypatch.setattr(evidence, "latest_laptop_session", lambda: session)
    evidence.collect_camera_ap_ble_session(args(tmp_path, state_file=tmp_path / "latest-state.json"))
    latest_state = evidence.load_state(tmp_path / "latest-state.json")
    assert latest_state["evidence"]["camera_ap_ble_launch"]["value"] == "not_launched"

    monkeypatch.setattr(evidence, "latest_laptop_session", lambda: None)
    evidence.collect_camera_ap_ble_session(args(tmp_path, state_file=tmp_path / "none-state.json"))
    none_state = evidence.load_state(tmp_path / "none-state.json")
    assert none_state["evidence"]["camera_ap_ble_launch"]["value"] == "unavailable"


def ptpip_summary(**overrides) -> dict:
    values = {
        "host": "192.168.0.1",
        "port": 55740,
        "friendly_name": "mbp-7274",
        "route_check": "passed",
        "tcp_connect": "present",
        "init_sent": True,
        "response_present": True,
        "response_header": {"length": 68, "packet_type": 2},
        "open_session_sent": True,
        "open_session_response_present": True,
        "open_session_response_header": {"length": 12, "container_type": 3, "code": 0x201E, "transaction_id": 1},
        "get_prop": "0xd212",
        "get_prop_sent": True,
        "get_prop_data_header": {"length": 50, "container_type": 2, "code": 0x1015, "transaction_id": 2},
        "get_prop_response_present": True,
        "get_prop_response_header": {"length": 12, "container_type": 3, "code": 0x2001, "transaction_id": 2},
    }
    values.update(overrides)
    return values


def make_ptpip_probe_session(tmp_path: Path, summary: dict | None = None) -> Path:
    session = tmp_path / "ptpip_probe_20260502T000000Z"
    session.mkdir()
    (session / "summary.json").write_text(json.dumps(summary or ptpip_summary()) + "\n", encoding="utf-8")
    (session / "init_command_request.bin").write_bytes(b"init")
    (session / "get_prop_response.bin").write_bytes(b"ok")
    (session / "app_sequence_01_get_d212_response.bin").write_bytes(b"ok")
    return session


@pytest.mark.parametrize(
    ("summary", "value"),
    [
        (ptpip_summary(route_check="failed"), "route_failed"),
        (ptpip_summary(tcp_connect="absent"), "tcp_connect_absent"),
        (ptpip_summary(init_sent=False), "tcp_connected"),
        (
            ptpip_summary(response_present=False, response_error="timeout"),
            "tcp_connected_init_timeout",
        ),
        (
            ptpip_summary(response_present=False, response_error=""),
            "tcp_connected_init_no_response",
        ),
        (ptpip_summary(open_session_sent=False), "init_ack_present"),
        (
            ptpip_summary(open_session_response_present=False),
            "open_session_no_response",
        ),
        (
            ptpip_summary(open_session_response_header={"code": 0x2005}),
            "open_session_rejected",
        ),
        (
            ptpip_summary(app_sequence="sdcard-browse-bootstrap", app_sequence_completed=True),
            "app_sequence_sdcard_browse_bootstrap_ok",
        ),
        (
            ptpip_summary(app_sequence="sdcard-current-object-info", app_sequence_completed=True),
            "app_sequence_sdcard_current_object_info_ok",
        ),
        (
            ptpip_summary(app_sequence="sdcard-current-object-thumbnail", app_sequence_completed=True),
            "app_sequence_sdcard_current_object_thumbnail_ok",
        ),
        (
            ptpip_summary(app_sequence="sdcard-browse-bootstrap", app_sequence_completed=False),
            "app_sequence_incomplete",
        ),
        (ptpip_summary(get_prop_sent=False), "open_session_ok"),
        (
            ptpip_summary(get_prop_response_present=False),
            "get_prop_no_response",
        ),
        (ptpip_summary(get_prop="0xd212"), "get_prop_d212_ok"),
        (ptpip_summary(get_prop="4660"), "get_prop_ok"),
        (
            ptpip_summary(get_prop_data_header={"code": 0x9999}),
            "get_prop_unexpected_response",
        ),
        (
            ptpip_summary(get_prop="not-a-prop"),
            "get_prop_ok",
        ),
    ],
)
def test_evaluate_ptpip_summary(summary, value) -> None:
    assert evidence.evaluate_ptpip_summary(summary)[0] == value


def test_ptpip_summary_helper_edge_cases() -> None:
    assert evidence.nested_header_code({"header": []}, "header") is None
    assert evidence.parse_optional_u16(None) is None


def test_ptpip_probe_session_collector(monkeypatch, tmp_path) -> None:
    session = make_ptpip_probe_session(tmp_path)
    state_file = tmp_path / "state.json"

    assert evidence.collect_ptpip_probe_session(
        args(tmp_path, state_file=state_file, session_dir=str(session))
    ) == 0
    state = evidence.load_state(state_file)
    record = state["evidence"]["camera_ap_ptpip_probe"]
    assert record["value"] == "get_prop_d212_ok"
    assert record["details"]["get_prop_response_header"]["code"] == 0x2001
    assert any(path.endswith("app_sequence_01_get_d212_response.bin") for path in record["artifacts"])
    assert any(path.endswith("summary.json") for path in record["artifacts"])
    assert state["state_label"] == "camera_ap_ptpip_get_prop_d212_ok"

    sequence_session = tmp_path / "ptpip_probe_sequence"
    sequence_session.mkdir()
    (sequence_session / "summary.json").write_text(
        json.dumps(
            ptpip_summary(
                app_sequence="sdcard-browse-bootstrap",
                app_sequence_completed=True,
                app_sequence_steps=[{"action": "get"}],
            )
        )
        + "\n",
        encoding="utf-8",
    )
    evidence.collect_ptpip_probe_session(
        args(tmp_path, state_file=tmp_path / "sequence-state.json", session_dir=str(sequence_session))
    )
    sequence_state = evidence.load_state(tmp_path / "sequence-state.json")
    assert sequence_state["evidence"]["camera_ap_ptpip_probe"]["details"]["app_sequence_step_count"] == 1
    assert sequence_state["state_label"] == "camera_ap_ptpip_sdcard_browse_bootstrap_ok"

    object_info_session = tmp_path / "ptpip_probe_object_info"
    object_info_session.mkdir()
    (object_info_session / "summary.json").write_text(
        json.dumps(
            ptpip_summary(
                app_sequence="sdcard-current-object-info",
                app_sequence_completed=True,
                app_sequence_steps=[{"action": "vendor_get"}],
            )
        )
        + "\n",
        encoding="utf-8",
    )
    evidence.collect_ptpip_probe_session(
        args(tmp_path, state_file=tmp_path / "object-info-state.json", session_dir=str(object_info_session))
    )
    object_info_state = evidence.load_state(tmp_path / "object-info-state.json")
    assert object_info_state["state_label"] == "camera_ap_ptpip_sdcard_current_object_info_ok"

    thumbnail_session = tmp_path / "ptpip_probe_thumbnail"
    thumbnail_session.mkdir()
    (thumbnail_session / "summary.json").write_text(
        json.dumps(
            ptpip_summary(
                app_sequence="sdcard-current-object-thumbnail",
                app_sequence_completed=True,
                app_sequence_steps=[{"action": "vendor_get"}],
            )
        )
        + "\n",
        encoding="utf-8",
    )
    evidence.collect_ptpip_probe_session(
        args(tmp_path, state_file=tmp_path / "thumbnail-state.json", session_dir=str(thumbnail_session))
    )
    thumbnail_state = evidence.load_state(tmp_path / "thumbnail-state.json")
    assert thumbnail_state["state_label"] == "camera_ap_ptpip_sdcard_current_object_thumbnail_ok"

    missing_summary = tmp_path / "ptpip_probe_missing"
    missing_summary.mkdir()
    evidence.collect_ptpip_probe_session(
        args(tmp_path, state_file=tmp_path / "missing-state.json", session_dir=str(missing_summary))
    )
    missing_state = evidence.load_state(tmp_path / "missing-state.json")
    assert missing_state["evidence"]["camera_ap_ptpip_probe"]["value"] == "unavailable"

    root = tmp_path / "sessions"
    root.mkdir()
    assert evidence.latest_ptpip_probe_session(root) is None
    (root / "ptpip_probe_a").mkdir()
    (root / "ptpip_probe_b").mkdir()
    assert evidence.latest_ptpip_probe_session(root).name == "ptpip_probe_b"

    monkeypatch.setattr(evidence, "latest_ptpip_probe_session", lambda: session)
    assert evidence.resolve_ptpip_probe_session_dir(args(tmp_path, session_dir=None)) == session

    monkeypatch.setattr(evidence, "latest_ptpip_probe_session", lambda: None)
    evidence.collect_ptpip_probe_session(args(tmp_path, state_file=tmp_path / "none-state.json"))
    none_state = evidence.load_state(tmp_path / "none-state.json")
    assert none_state["evidence"]["camera_ap_ptpip_probe"]["value"] == "unavailable"


def make_session(tmp_path: Path) -> Path:
    session = tmp_path / "laptop_ble_gps_20260502T000000Z"
    session.mkdir()
    (session / "session.log").write_text(
        "pair-trigger read uuid=f557 hex=01000000\n"
        "registration id=c5880600 ack=c5880620\n"
        "disconnected device\n",
        encoding="utf-8",
    )
    (session / "writes.jsonl").write_text(
        "\n".join(
            [
                '{"uuid":"%s","hex":"6572696300","length":5}' % uuids.CHAR_CONNECTED_DEVICE_NAME,
                '{"uuid":"%s","hex":"c5880620","length":4}' % uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER,
                '{"uuid":"%s","hex":"7ed88e16caeffeb62100000000000000ea070502080001","length":23,"ts":"2026-05-02T08:00:01Z"}'
                % uuids.CHAR_LOCATION_AND_SPEED,
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    (session / "reads.jsonl").write_text(
        '{"uuid":"%s","hex":"c5880600","length":4}\n'
        % uuids.CHAR_CONNECTED_DEVICE_IDENTIFICATION_NUMBER,
        encoding="utf-8",
    )
    (session / "identity.json").write_text('{"location_sync_state": "0100"}\n', encoding="utf-8")
    (session / "notifications.jsonl").write_text(
        '{"uuid":"%s","hex":"0100","length":2}\n' % uuids.CHAR_LOCATION_SYNC_STATE,
        encoding="utf-8",
    )
    return session


def test_session_evidence_collectors(tmp_path, monkeypatch) -> None:
    session = make_session(tmp_path)
    state_file = tmp_path / "state.json"
    common = args(tmp_path, state_file=state_file, session_dir=str(session))

    evidence.collect_session_pair_trigger_read(common)
    evidence.collect_session_registration_name_written(common)
    evidence.collect_session_registration_id_read(common)
    evidence.collect_session_registration_ack_written(common)
    evidence.collect_session_disconnect_after_ack(common)

    state = evidence.load_state(state_file)
    assert state["evidence"]["session_pair_trigger_read"]["value"] == "present"
    assert state["evidence"]["session_registration_name_written"]["details"]["payload_text"] == "eric"
    assert state["evidence"]["session_registration_id_read"]["details"]["registration_id_hex"] == "c5880600"
    assert state["evidence"]["session_registration_ack_written"]["details"]["ack_hex"] == "c5880620"
    assert state["state_label"] == "host_connected_registration_ack_written_camera_disconnects"

    empty = tmp_path / "empty"
    empty.mkdir()
    (empty / "session.log").write_text("registration id=x ack=y\nidentity ok\ndisconnected\n", encoding="utf-8")
    (empty / "writes.jsonl").write_text("", encoding="utf-8")
    (empty / "reads.jsonl").write_text("", encoding="utf-8")
    empty_args = args(tmp_path, state_file=tmp_path / "empty-state.json", session_dir=str(empty))
    evidence.collect_session_pair_trigger_read(empty_args)
    evidence.collect_session_registration_name_written(empty_args)
    evidence.collect_session_registration_id_read(empty_args)
    evidence.collect_session_registration_ack_written(empty_args)
    evidence.collect_session_disconnect_after_ack(empty_args)
    empty_state = evidence.load_state(tmp_path / "empty-state.json")
    assert empty_state["evidence"]["session_pair_trigger_read"]["value"] == "absent"
    assert empty_state["evidence"]["session_disconnect_after_ack"]["value"] == "unknown"

    root = tmp_path / "sessions"
    root.mkdir()
    assert evidence.latest_laptop_session(root) is None
    (root / "laptop_ble_gps_a").mkdir()
    (root / "laptop_ble_gps_b").mkdir()
    assert evidence.latest_laptop_session(root).name == "laptop_ble_gps_b"

    monkeypatch.setattr(evidence, "latest_laptop_session", lambda: session)
    assert evidence.resolve_session_dir(args(tmp_path, session_dir=None)) == session


def test_session_gps_sync_ready_collector(tmp_path) -> None:
    session = make_session(tmp_path)
    state_file = tmp_path / "state.json"

    evidence.collect_session_gps_sync_ready(args(tmp_path, state_file=state_file, session_dir=str(session)))

    state = evidence.load_state(state_file)
    assert state["evidence"]["gps_sync_ready"]["value"] == "present"
    assert state["state_label"] == "gps_sync_ready"

    empty = tmp_path / "empty"
    empty.mkdir()
    (empty / "identity.json").write_text('{"location_sync_state": "0000"}\n', encoding="utf-8")
    evidence.collect_session_gps_sync_ready(
        args(tmp_path, state_file=tmp_path / "empty-state.json", session_dir=str(empty))
    )
    assert evidence.load_state(tmp_path / "empty-state.json")["evidence"]["gps_sync_ready"]["value"] == "absent"


def test_session_gps_payload_written_and_camera_icon_collectors(tmp_path) -> None:
    session = make_session(tmp_path)
    state_file = tmp_path / "state.json"

    evidence.collect_session_gps_payload_written(
        args(tmp_path, state_file=state_file, session_dir=str(session))
    )
    evidence.collect_manual_value(
        args(
            tmp_path,
            state_file=state_file,
            key="camera_gps_icon",
            value="absent",
            note="no icon visible after repeated GPS writes",
            source="manual_camera_screen_observation",
        )
    )

    state = evidence.load_state(state_file)
    assert state["evidence"]["session_gps_payload_written"]["value"] == "present"
    assert state["evidence"]["session_gps_payload_written"]["details"]["write_count"] == 1
    assert state["state_label"] == "gps_payload_written_camera_icon_absent"

    empty = tmp_path / "empty"
    empty.mkdir()
    (empty / "writes.jsonl").write_text("", encoding="utf-8")
    evidence.collect_session_gps_payload_written(
        args(tmp_path, state_file=tmp_path / "empty-state.json", session_dir=str(empty))
    )
    assert (
        evidence.load_state(tmp_path / "empty-state.json")["evidence"]["session_gps_payload_written"]["value"]
        == "absent"
    )


def test_jsonl_and_decode_helpers(tmp_path) -> None:
    assert evidence.read_jsonl(tmp_path / "missing.jsonl") == []
    path = tmp_path / "rows.jsonl"
    path.write_text('\n{"a": 1}\n', encoding="utf-8")
    assert evidence.read_jsonl(path) == [{"a": 1}]
    assert evidence.decode_text_payload("6572696300") == "eric"
    assert evidence.decode_text_payload("not-hex") == ""
    assert evidence.decode_text_payload("ff") == ""


def test_main_keyboard_interrupt_and_error(monkeypatch, tmp_path, capsys) -> None:
    parser = evidence.build_parser()
    assert parser.prog == "fuji-evidence"

    def raise_keyboard_interrupt(_args):
        raise KeyboardInterrupt

    def raise_error(_args):
        raise RuntimeError("boom")

    monkeypatch.setattr(evidence, "collect_macos_ioreg_device", raise_keyboard_interrupt)
    monkeypatch.setattr(
        evidence,
        "build_parser",
        lambda: types.SimpleNamespace(
            parse_args=lambda argv: types.SimpleNamespace(func=raise_keyboard_interrupt)
        ),
    )
    assert evidence.main(["ignored"]) == 130

    monkeypatch.setattr(
        evidence,
        "build_parser",
        lambda: types.SimpleNamespace(parse_args=lambda argv: types.SimpleNamespace(func=raise_error)),
    )
    assert evidence.main(["ignored"]) == 1
    assert "boom" in capsys.readouterr().err
