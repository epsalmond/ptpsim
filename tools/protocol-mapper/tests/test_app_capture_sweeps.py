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
                    "verdict": "transfer_metadata_probe_ok",
                    "metadata_probe": {"objects": [{"handle": "0x10000001"}]},
                },
            ],
        }
    )

    assert "# Issue #60 reference app Capture Sweep Verdict" in text
    assert "| liveview | liveview_status_sweep_ok | poll=250ms; 55742 jpeg=25; max_gap_s=0.2; d212_ok=3/3 |" in text
    assert "| ap | ap_launch_joined_ports_observed | mode=get; open_ports=55740 |" in text
    assert "| transfer | transfer_metadata_probe_ok | objects=1 |" in text
