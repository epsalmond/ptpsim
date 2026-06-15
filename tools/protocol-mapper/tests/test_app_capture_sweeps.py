from __future__ import annotations

import argparse
from pathlib import Path

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
