from __future__ import annotations

import asyncio
import argparse
from pathlib import Path
import types

import pytest

from protocol_mapper import app_capture_sweeps as probe


def ptp_string(value: str) -> bytes:
    encoded = (value + "\x00").encode("utf-16-le")
    return bytes([len(value) + 1]) + encoded


def test_capture_dir_for_uses_issue_number(tmp_path: Path) -> None:
    assert probe.capture_dir_for(tmp_path, 60, "20260614T120000Z") == tmp_path / "issue-60-20260614T120000Z"


def test_parse_sweeps_and_ap_modes_reject_unknown_values() -> None:
    assert probe.parse_sweeps("liveview,transfer") == ["liveview", "transfer"]
    assert probe.parse_ap_modes("take,get") == ["take", "get"]
    with pytest.raises(argparse.ArgumentTypeError):
        probe.parse_sweeps("liveview,restore")
    with pytest.raises(argparse.ArgumentTypeError):
        probe.parse_ap_modes("take,upload")


def test_parse_poll_specs_and_raf_preview_bounds() -> None:
    assert probe.parse_poll_specs("none,250ms") == ["none", "250ms"]
    assert probe.parse_raf_preview_bytes("999999") == probe.MAX_RAF_PREVIEW_BYTES
    with pytest.raises(argparse.ArgumentTypeError):
        probe.parse_raf_preview_bytes("-1")


def test_parse_ble_recovery_mode_rejects_unknown_value() -> None:
    assert probe.parse_ble_recovery_mode("reset-on-fail") == "reset-on-fail"
    with pytest.raises(argparse.ArgumentTypeError):
        probe.parse_ble_recovery_mode("remove-device")


def test_parse_handles_respects_declared_and_available_count() -> None:
    payload = (3).to_bytes(4, "little") + (0x10000001).to_bytes(4, "little")
    assert probe.parse_handles(payload) == [0x10000001]


def test_iw_static_handoff_does_not_require_passphrase(tmp_path: Path) -> None:
    parser = probe.build_parser()
    args = parser.parse_args(["--capture-root", str(tmp_path), "--wifi-join-method", "iw_static"])
    assert probe.should_read_ap_passphrase(probe.config_from_args(args)) is False

    args = parser.parse_args(["--capture-root", str(tmp_path), "--wifi-join-method", "wpa_cli"])
    assert probe.should_read_ap_passphrase(probe.config_from_args(args)) is True


def test_ble_connect_retries_with_attempt_records(tmp_path: Path) -> None:
    class FakeConn:
        def __init__(self, backend: FakeBackend) -> None:
            self.backend = backend

        async def __aenter__(self) -> "FakeConn":
            self.backend.enters += 1
            if self.backend.enters == 1:
                raise TimeoutError()
            return self

    class FakeBackend:
        def __init__(self) -> None:
            self.enters = 0
            self.timeouts: list[float] = []

        def connect(self, device: probe.DeviceInfo, *, timeout: float = 60.0) -> FakeConn:
            self.timeouts.append(timeout)
            return FakeConn(self)

    backend = FakeBackend()
    timeline = probe.base.Timeline(tmp_path / "timeline.jsonl")
    device = probe.DeviceInfo(address="38:7C:76:74:73:21", name="GFX100 II")

    async def run_probe() -> tuple[object, object, list[dict[str, object]]]:
        return await probe.connect_ble_with_retries(
            backend,
            device,
            timeline,
            attempts=2,
            timeout=1.5,
            retry_delay_s=0.0,
        )

    _conn_cm, conn, records = asyncio.run(run_probe())

    assert isinstance(conn, FakeConn)
    assert backend.timeouts == [1.5, 1.5]
    assert records[0]["error"] == "TimeoutError()"
    assert records[1]["connected"] is True
    timeline_text = (tmp_path / "timeline.jsonl").read_text(encoding="utf-8")
    assert '"event": "ble_connect_error"' in timeline_text
    assert '"event": "ble_connect_ok"' in timeline_text


def test_pre_phase_ble_recovery_records_bluez_commands(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    parser = probe.build_parser()
    config = probe.config_from_args(
        parser.parse_args(
            [
                "--capture-root",
                str(tmp_path),
                "--ble-address",
                "38:7C:76:74:73:21",
                "--ble-recovery-mode",
                "gate",
                "--ble-recovery-settle-s",
                "0",
            ]
        )
    )
    commands: list[list[str]] = []

    def fake_run_cmd(argv: list[str], **kwargs: object) -> dict[str, object]:
        commands.append(argv)
        return {"argv": argv, "returncode": 0, "stdout": "ok", "stderr": "", "elapsed_s": 0.01}

    monkeypatch.setattr(probe.base, "run_cmd", fake_run_cmd)
    timeline = probe.base.Timeline(tmp_path / "timeline.jsonl")

    summary = asyncio.run(probe.run_pre_phase_ble_recovery(config, timeline))

    assert summary["mode"] == "gate"
    assert commands == [
        ["bluetoothctl", "show"],
        ["bluetoothctl", "scan", "off"],
        ["bluetoothctl", "disconnect", "38:7C:76:74:73:21"],
    ]
    timeline_text = (tmp_path / "timeline.jsonl").read_text(encoding="utf-8")
    assert '"event": "ble_recovery_command"' in timeline_text
    assert '"label": "disconnect_target"' in timeline_text


def test_connect_and_launch_ap_resets_adapter_after_initial_connect_failure(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    parser = probe.build_parser()
    config = probe.config_from_args(
        parser.parse_args(["--capture-root", str(tmp_path), "--ble-recovery-settle-s", "0"])
    )
    cycles: list[str] = []
    resets: list[str] = []

    class FakeConnCM:
        async def __aexit__(self, *args: object) -> None:
            return None

    class FakeCamera:
        def __init__(self, backend: object, session: object) -> None:
            self.backend = backend
            self.session = session

        async def _launch_ap(self, conn: object, mode: str, *, timeout: float) -> str:
            return "0180"

    async def fake_pre_phase_recovery(config: probe.SweepConfig, timeline: probe.base.Timeline) -> dict[str, object]:
        return {"trigger": "pre_phase", "mode": config.ble_recovery_mode, "commands": []}

    async def fake_adapter_reset(
        config: probe.SweepConfig,
        timeline: probe.base.Timeline,
        *,
        trigger: str,
    ) -> dict[str, object]:
        resets.append(trigger)
        return {"trigger": trigger, "mode": config.ble_recovery_mode, "adapter_reset": True, "ok": True}

    async def fake_find_ready_ble_device(
        camera: object,
        config: probe.SweepConfig,
        timeline: probe.base.Timeline,
        *,
        cycle: str,
    ) -> probe.DeviceInfo:
        cycles.append(cycle)
        return probe.DeviceInfo(address="38:7C:76:74:73:21", name="GFX100 II")

    async def fake_connect_ble_with_retries(
        backend: object,
        device: probe.DeviceInfo,
        timeline: probe.base.Timeline,
        *,
        attempts: int,
        timeout: float,
        retry_delay_s: float,
        cycle: str | None = None,
    ) -> tuple[FakeConnCM, object, list[dict[str, object]]]:
        if cycle == "initial":
            raise probe.PhaseBlockedError(
                "BLE connect failed after 3 attempt(s)",
                partial_summary={"ble_connect": [{"attempt": 1, "cycle": cycle, "error": "TimeoutError()"}]},
            )
        return FakeConnCM(), object(), [{"attempt": 1, "cycle": cycle, "connected": True}]

    async def fake_prepare_ble_connection(*args: object, **kwargs: object) -> dict[str, object]:
        return {"notifications": [], "wifi_info": {"ssid": "FUJIFILM-GFX100II-0C3E"}}

    monkeypatch.setattr(probe, "BleakBackend", lambda session: object())
    monkeypatch.setattr(probe, "FujiCamera", FakeCamera)
    monkeypatch.setattr(probe, "run_pre_phase_ble_recovery", fake_pre_phase_recovery)
    monkeypatch.setattr(probe, "run_ble_adapter_reset", fake_adapter_reset)
    monkeypatch.setattr(probe, "find_ready_ble_device", fake_find_ready_ble_device)
    monkeypatch.setattr(probe, "connect_ble_with_retries", fake_connect_ble_with_retries)
    monkeypatch.setattr(probe, "prepare_ble_connection", fake_prepare_ble_connection)

    summary, _backend, _camera, _conn = asyncio.run(
        probe.connect_and_launch_ap(
            config,
            tmp_path,
            probe.base.Timeline(tmp_path / "timeline.jsonl"),
            mode="get",
            read_passphrase=False,
        )
    )

    assert cycles == ["initial", "post_adapter_reset"]
    assert resets == ["ble_connect_failure"]
    assert summary["ble_connect"] == [
        {"attempt": 1, "cycle": "initial", "error": "TimeoutError()"},
        {"attempt": 1, "cycle": "post_adapter_reset", "connected": True},
    ]
    assert summary["ap_state"] == "0180"


def test_transfer_stops_before_protocol_claims_when_reset_required(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    parser = probe.build_parser()
    config = probe.config_from_args(
        parser.parse_args(["--capture-root", str(tmp_path), "--sweeps", "transfer", "--no-pcss-fallback"])
    )

    async def fake_connect_and_launch_ap(*args: object, **kwargs: object) -> object:
        raise probe.PhaseBlockedError(
            "BLE connect failed after adapter reset",
            partial_summary={
                "ble_connect": [{"attempt": 1, "cycle": "post_adapter_reset", "error": "TimeoutError()"}],
                "reset_required": True,
                "reset_reason": "ble_connect_failed_after_adapter_reset",
            },
        )

    monkeypatch.setattr(probe, "connect_and_launch_ap", fake_connect_and_launch_ap)
    monkeypatch.setattr(probe, "join_after_launch", lambda *args, **kwargs: pytest.fail("join should be skipped"))
    monkeypatch.setattr(probe.base, "cleanup_wifi", lambda _config: {"ok": True})

    result = asyncio.run(
        probe.run_transfer_sweep(config, tmp_path, types.SimpleNamespace(process=types.SimpleNamespace(pid=7)))
    )

    assert result["existing_ap_reuse"]["attempted"] is False
    assert result["existing_ap_reuse"]["skipped"] == "reset_required"
    assert result["verdict"] == "blocked: reset_required: ble_connect_failed_after_adapter_reset"
    assert "metadata_probe" not in result


def test_transfer_reuses_existing_ap_after_ble_launch_failure(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    parser = probe.build_parser()
    config = probe.config_from_args(
        parser.parse_args(["--capture-root", str(tmp_path), "--sweeps", "transfer", "--no-pcss-fallback"])
    )

    async def fake_connect_and_launch_ap(*args: object, **kwargs: object) -> object:
        raise probe.PhaseBlockedError(
            "BLE connect failed after 3 attempt(s)",
            partial_summary={"ble_connect": [{"attempt": 1, "error": "TimeoutError()"}]},
        )

    def fake_join_after_launch(
        config: probe.SweepConfig,
        phase_dir: Path,
        timeline: probe.base.Timeline,
        summary: dict[str, object],
    ) -> bool:
        summary["wifi_join"] = {"ok": True}
        return True

    class FakeCapture:
        argv = ["tcpdump"]
        process = types.SimpleNamespace(pid=42)

        def stop(self) -> dict[str, object]:
            return {"stopped": True}

    class FakePtpSession:
        alive = True
        steps: list[dict[str, object]] = []

        def close(self) -> dict[str, object]:
            self.alive = False
            return {"closed": True}

    monkeypatch.setattr(probe, "connect_and_launch_ap", fake_connect_and_launch_ap)
    monkeypatch.setattr(probe, "join_after_launch", fake_join_after_launch)
    monkeypatch.setattr(probe.base, "start_tcpdump", lambda _config, _phase_dir: FakeCapture())
    monkeypatch.setattr(probe.base.PtpControlSession, "open", staticmethod(lambda *args, **kwargs: FakePtpSession()))
    monkeypatch.setattr(probe, "run_object_metadata_probe", lambda *args, **kwargs: {"objects": [{"handle": "x"}]})
    monkeypatch.setattr(probe.base, "cleanup_wifi", lambda _config: {"ok": True})

    result = asyncio.run(
        probe.run_transfer_sweep(config, tmp_path, types.SimpleNamespace(process=types.SimpleNamespace(pid=7)))
    )

    assert result["existing_ap_reuse"]["attempted"] is True
    assert result["ble_connect"] == [{"attempt": 1, "error": "TimeoutError()"}]
    assert result["metadata_probe"]["objects"] == [{"handle": "x"}]
    assert result["verdict"] == "transfer_metadata_probe_ok"


def test_ap_sweep_reuses_existing_ap_after_ble_launch_failure(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    parser = probe.build_parser()
    config = probe.config_from_args(
        parser.parse_args(["--capture-root", str(tmp_path), "--sweeps", "ap", "--ap-modes", "get"])
    )

    async def fake_connect_and_launch_ap(*args: object, **kwargs: object) -> object:
        raise probe.PhaseBlockedError(
            "BLE connect failed after 1 attempt(s)",
            partial_summary={"ble_connect": [{"attempt": 1, "error": "TimeoutError()"}]},
        )

    def fake_join_after_launch(
        config: probe.SweepConfig,
        phase_dir: Path,
        timeline: probe.base.Timeline,
        summary: dict[str, object],
    ) -> bool:
        summary["wifi_join"] = {"ok": True}
        return True

    monkeypatch.setattr(probe, "connect_and_launch_ap", fake_connect_and_launch_ap)
    monkeypatch.setattr(probe, "join_after_launch", fake_join_after_launch)
    monkeypatch.setattr(
        probe,
        "port_probe",
        lambda host, ports, timeout: {
            "55740": {"open": True},
            "55741": {"open": False},
            "55742": {"open": False},
        },
    )
    monkeypatch.setattr(probe.base, "cleanup_wifi", lambda _config: {"ok": True})

    result = asyncio.run(
        probe.run_ap_mode_sweep(config, tmp_path, "get", types.SimpleNamespace(process=types.SimpleNamespace(pid=7)))
    )

    assert result["existing_ap_reuse"]["attempted"] is True
    assert result["open_ports"] == ["55740"]
    assert result["verdict"] == "ap_existing_ap_ports_observed"


def test_ap_port_probe_skips_command_socket_before_transfer(tmp_path: Path) -> None:
    parser = probe.build_parser()
    config = probe.config_from_args(
        parser.parse_args(["--capture-root", str(tmp_path), "--sweeps", "ap,transfer"])
    )

    assert probe.ap_port_probe_ports(config) == [55741, 55742]

    ap_only = probe.config_from_args(parser.parse_args(["--capture-root", str(tmp_path), "--sweeps", "ap"]))
    assert probe.ap_port_probe_ports(ap_only) == [55740, 55741, 55742]


def test_parse_object_info_decodes_core_fields() -> None:
    payload = bytearray(52)
    payload[0:4] = (1).to_bytes(4, "little")
    payload[4:6] = (0x3801).to_bytes(2, "little")
    payload[6:8] = (0).to_bytes(2, "little")
    payload[8:12] = (123456).to_bytes(4, "little")
    payload.extend(ptp_string("DSCF0001.JPG"))

    decoded = probe.parse_object_info(bytes(payload))

    assert decoded["storage_id"] == "0x00000001"
    assert decoded["format"] == "0x3801"
    assert decoded["compressed_size"] == 123456
    assert decoded["filename"] == "DSCF0001.JPG"


def test_remote_local_args_drop_remote_only_fields(tmp_path: Path) -> None:
    parser = probe.build_parser()
    args = parser.parse_args(
        [
            "--capture-root",
            str(tmp_path),
            "--issue-number",
            "60",
            "--sweeps",
            "liveview,ap",
            "--ap-modes",
            "take,get",
            "--poll-specs",
            "none,1s",
            "--remote-host",
            "eric@rpi4b.local",
            "--pairing-key",
            "f6313280",
            "--pcss-timeout",
            "12",
            "--stamp",
            "fixed",
            "--preflight-only",
        ]
    )

    argv = probe.remote_local_args(args)

    assert argv[0] == "scripts/probe_app_capture_sweeps.py"
    assert "--run-local" in argv
    assert "--remote-host" not in argv
    assert argv[argv.index("--capture-root") + 1] == "captures"
    assert argv[argv.index("--issue-number") + 1] == "60"
    assert argv[argv.index("--sweeps") + 1] == "liveview,ap"
    assert argv[argv.index("--ap-modes") + 1] == "take,get"
    assert argv[argv.index("--poll-specs") + 1] == "none,1s"
    assert argv[argv.index("--pairing-key") + 1] == "f6313280"
    assert argv[argv.index("--ble-connect-timeout") + 1] == "20.0"
    assert argv[argv.index("--ble-connect-attempts") + 1] == "3"
    assert argv[argv.index("--ble-connect-retry-delay") + 1] == "2.0"
    assert argv[argv.index("--ble-recovery-mode") + 1] == "reset-on-fail"
    assert argv[argv.index("--ble-recovery-timeout") + 1] == "8.0"
    assert argv[argv.index("--ble-recovery-settle-s") + 1] == "2.0"
    assert argv[argv.index("--pcss-timeout") + 1] == "12.0"
    assert argv[argv.index("--stamp") + 1] == "fixed"
    assert "--preflight-only" in argv


def test_render_verdict_summarizes_all_sweep_types() -> None:
    text = probe.render_verdict(
        {
            "issue_number": 60,
            "started_at": "2026-06-14T00:00:00Z",
            "finished_at": "2026-06-14T00:01:00Z",
            "capture_dir": "captures/issue-60-fixed",
            "rig": "rpi4b",
            "results": [
                {
                    "sweep": "liveview",
                    "poll": "250ms",
                    "verdict": "liveview_status_sweep_ok",
                    "liveview_reader": {"jpeg_frames": 25, "max_gap_s": 0.2},
                    "status_poll": {"ok": 3, "polls": 3},
                },
                {
                    "sweep": "ap",
                    "mode": "get",
                    "verdict": "ap_launch_joined_ports_observed",
                    "open_ports": ["55740"],
                },
                {
                    "sweep": "transfer",
                    "verdict": "blocked: reset_required: ble_connect_failed_after_adapter_reset",
                    "reset_required": True,
                    "reset_reason": "ble_connect_failed_after_adapter_reset",
                    "existing_ap_reuse": {"attempted": False, "skipped": "reset_required"},
                },
            ],
        }
    )

    assert "# Issue #60 reference app Capture Sweep Verdict" in text
    assert "| liveview | liveview_status_sweep_ok | poll=250ms; 55742 jpeg=25; max_gap_s=0.2; d212_ok=3/3 |" in text
    assert "| ap | ap_launch_joined_ports_observed | mode=get; open_ports=55740 |" in text
    assert (
        "| transfer | blocked: reset_required: ble_connect_failed_after_adapter_reset | "
        "reset_required=ble_connect_failed_after_adapter_reset; objects=0; "
        "existing_ap_reuse=skipped:reset_required |"
    ) in text
