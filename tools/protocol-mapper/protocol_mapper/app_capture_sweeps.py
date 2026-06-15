from __future__ import annotations

import argparse
import asyncio
from dataclasses import dataclass
import json
from pathlib import Path
import shlex
import socket
import struct
import subprocess
import sys
import time
from typing import Any

from rce.tools.fuji_ble_gps import ptpip, uuids
from rce.tools.fuji_ble_gps.ble_backend import BleakBackend, BleConnection, DeviceInfo
from rce.tools.fuji_ble_gps.camera import FujiCamera
from rce.tools.fuji_ble_gps.session import Session

from protocol_mapper import ble_ptpip_concurrency as base


DEFAULT_ISSUE_NUMBER = 60
DEFAULT_REMOTE_WORKDIR = "/home/eric/ptpsim-protocol-mapper-capture-sweeps"
SWEEPS = ("liveview", "ap", "transfer")
AP_MODES = ("take", "get", "fw_transfer")
POLL_SPECS = {
    "none": None,
    "1s": 1.0,
    "250ms": 0.25,
}
PTP_GET_OBJECT_HANDLES = 0x1007
PTP_GET_EXTENSION_OBJECT_INFO = 0x9054
PTP_GET_PARTIAL_OBJECT = 0x101B
PROP_OBJECT_COUNT = 0xD620
PROP_OBJECT_HANDLES = 0xD621
MAX_RAF_PREVIEW_BYTES = 64 * 1024


def capture_dir_for(root: Path, issue_number: int = DEFAULT_ISSUE_NUMBER, stamp: str | None = None) -> Path:
    return root / f"issue-{issue_number}-{stamp or base.utc_stamp()}"


def parse_csv(value: str, allowed: tuple[str, ...], label: str) -> list[str]:
    selected = [part.strip().lower() for part in value.split(",") if part.strip()]
    invalid = [part for part in selected if part not in allowed]
    if invalid:
        raise argparse.ArgumentTypeError(f"invalid {label}: {', '.join(invalid)}")
    return selected or list(allowed)


def parse_sweeps(value: str) -> list[str]:
    return parse_csv(value, SWEEPS, "sweep")


def parse_ap_modes(value: str) -> list[str]:
    return parse_csv(value, AP_MODES, "AP mode")


def parse_poll_specs(value: str) -> list[str]:
    return parse_csv(value, tuple(POLL_SPECS), "poll spec")


def parse_positive_int(value: str) -> int:
    parsed = int(value, 10)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be positive")
    return parsed


def parse_raf_preview_bytes(value: str) -> int:
    parsed = int(value, 10)
    if parsed < 0:
        raise argparse.ArgumentTypeError("RAF preview byte count must be non-negative")
    return min(parsed, MAX_RAF_PREVIEW_BYTES)


def json_default(value: Any) -> Any:
    return base.json_default(value)


def payload_bytes(result: dict[str, Any]) -> bytes:
    try:
        return bytes.fromhex(str(result.get("payload_hex", "")))
    except ValueError:
        return b""


def parse_handles(payload: bytes) -> list[int]:
    if len(payload) < 4:
        return []
    count = struct.unpack_from("<I", payload, 0)[0]
    available = (len(payload) - 4) // 4
    return [struct.unpack_from("<I", payload, 4 + 4 * index)[0] for index in range(min(count, available))]


def read_ptp_string(payload: bytes, offset: int) -> tuple[str, int]:
    if offset >= len(payload):
        return "", offset
    chars = payload[offset]
    offset += 1
    if chars == 0:
        return "", offset
    end = min(offset + chars * 2, len(payload))
    return payload[offset:end].decode("utf-16-le", "replace").rstrip("\x00"), end


def parse_object_info(payload: bytes) -> dict[str, Any]:
    if len(payload) < 12:
        return {"decode_error": "short object info", "payload_bytes": len(payload)}
    storage_id, obj_format, protection, size = struct.unpack_from("<IHHI", payload, 0)
    filename, _ = read_ptp_string(payload, 52)
    return {
        "storage_id": f"0x{storage_id:08x}",
        "format": f"0x{obj_format:04x}",
        "protection": f"0x{protection:04x}",
        "compressed_size": size,
        "filename": filename,
    }


def is_raf_object(info: dict[str, Any], header: bytes) -> bool:
    if str(info.get("format", "")).lower() in {"0xb103", "0xb901"}:
        return True
    name = str(info.get("filename", "")).lower()
    return name.endswith(".raf") or header.startswith(b"FUJI") or header.startswith(b"II")


def redact_wifi_info(info: dict[str, Any]) -> dict[str, Any]:
    redacted = dict(info)
    passphrase = redacted.pop("passphrase", None)
    if passphrase is not None:
        redacted["passphrase_present"] = True
        redacted["passphrase_length"] = len(str(passphrase))
    return redacted


def port_probe(host: str, ports: list[int], timeout: float) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for port in ports:
        started = time.monotonic()
        record: dict[str, Any] = {"port": port}
        try:
            sock = socket.create_connection((host, port), timeout)
        except OSError as exc:
            record.update({"open": False, "error": repr(exc)})
        else:
            record["open"] = True
            sock.close()
        record["elapsed_s"] = round(time.monotonic() - started, 3)
        out[str(port)] = record
    return out


@dataclass
class SweepConfig:
    issue_number: int
    capture_root: Path
    sweeps: list[str]
    ap_modes: list[str]
    poll_specs: list[str]
    camera_name: str
    ble_address: str | None
    friendly_name: str
    guid: str
    camera_host: str
    pcss_camera_host: str
    command_port: int
    event_port: int
    liveview_port: int
    wifi_iface: str
    wifi_join_method: str
    static_ip_cidr: str
    ap_ssid: str
    con_name: str
    scan_timeout: float
    ap_timeout: float
    ptp_timeout: float
    live_duration_s: float
    settle_s: float
    save_frames: int
    object_limit: int
    partial_bytes: int
    raf_preview_bytes: int
    pcss_fallback: bool
    pcss_timeout: float
    register: bool
    cleanup_wifi: bool
    preflight_only: bool
    pairing_key: bytes | None = None
    stamp: str | None = None


def base_compatible_config(config: SweepConfig) -> SweepConfig:
    return config


async def prepare_ble_connection(
    config: SweepConfig,
    camera: FujiCamera,
    conn: BleConnection,
    device: DeviceInfo,
    timeline: base.Timeline,
    *,
    read_passphrase: bool,
) -> dict[str, Any]:
    out: dict[str, Any] = {}
    if config.register:
        advertisement_pairing_identity = camera._pairing_identity_from_device(device)
        pairing_identity = config.pairing_key or advertisement_pairing_identity
        if config.pairing_key is not None:
            out["pairing_identity"] = {"source": "cli", "hex": config.pairing_key.hex()}
        elif advertisement_pairing_identity is not None:
            out["pairing_identity"] = {"source": "advertisement", "hex": advertisement_pairing_identity.hex()}
        out["registration"] = await camera._register_connected(
            conn,
            device_name=config.friendly_name,
            ack_registration=True,
            pairing_identity=pairing_identity,
        )
    await camera._prepare_connection(conn)
    await camera._prepare_wifi_handoff(conn)
    out["notifications"] = await base.subscribe_all_notifications(conn, timeline)
    out["wifi_info"] = redact_wifi_info(
        await camera._read_wifi_info_connected(conn, read_passphrase=read_passphrase)
    )
    return out


async def connect_and_launch_ap(
    config: SweepConfig,
    phase_dir: Path,
    timeline: base.Timeline,
    *,
    mode: str,
    read_passphrase: bool,
) -> tuple[dict[str, Any], BleakBackend, FujiCamera, BleConnection]:
    ble_session = Session(root=phase_dir, label=f"ble_{mode}")
    backend = BleakBackend(ble_session)
    camera = FujiCamera(backend, ble_session)
    device = await camera._target(name=config.camera_name, timeout=config.scan_timeout, address=config.ble_address)
    summary: dict[str, Any] = {"device": device.to_log_dict(), "launch_mode": mode}
    timeline.event("ble_target", **summary["device"])
    conn_cm = backend.connect(device)
    conn = await conn_cm.__aenter__()
    summary["_conn_cm"] = conn_cm
    try:
        summary.update(
            await prepare_ble_connection(
                config,
                camera,
                conn,
                device,
                timeline,
                read_passphrase=read_passphrase,
            )
        )
        summary["function_launch"] = {
            "mode": mode,
            "uuid": uuids.CHAR_FUNCTION_LAUNCH,
            "hex": uuids.FUNCTION_LAUNCH_VALUES[mode].hex(),
        }
        summary["ap_state"] = await camera._launch_ap(conn, mode, timeout=config.ap_timeout)
        return summary, backend, camera, conn
    except Exception:
        await conn_cm.__aexit__(*sys.exc_info())
        raise


async def close_ble_connection(summary: dict[str, Any]) -> None:
    conn_cm = summary.pop("_conn_cm", None)
    if conn_cm is not None:
        try:
            await conn_cm.__aexit__(None, None, None)
        except Exception as exc:  # noqa: BLE001 - capture cleanup should be visible
            summary["ble_disconnect_error"] = repr(exc)


def join_after_launch(config: SweepConfig, phase_dir: Path, timeline: base.Timeline, summary: dict[str, Any]) -> bool:
    ssid = str(summary.get("wifi_info", {}).get("ssid") or config.ap_ssid)
    join = base.join_camera_ap(base_compatible_config(config), ssid, phase_dir, timeline)
    summary["wifi_join"] = join
    return bool(join.get("ok"))


def poll_d212_loop(
    ptp_session: base.PtpControlSession,
    phase_dir: Path,
    timeline: base.Timeline,
    *,
    label: str,
    interval_s: float,
    duration_s: float,
) -> dict[str, Any]:
    path = phase_dir / "status_polls.jsonl"
    started = time.monotonic()
    deadline = started + duration_s
    count = 0
    ok = 0
    errors = 0
    max_elapsed = 0.0
    with path.open("a", encoding="utf-8") as handle:
        while ptp_session.alive and time.monotonic() < deadline:
            poll_started = time.monotonic()
            record = ptp_session.get_prop(0xD212, f"{label}_d212_{count:04d}")
            elapsed = time.monotonic() - poll_started
            max_elapsed = max(max_elapsed, elapsed)
            record["poll_index"] = count
            record["poll_elapsed_s"] = round(elapsed, 3)
            handle.write(json.dumps({"ts": base.utc_iso(), **record}, sort_keys=True, default=json_default) + "\n")
            handle.flush()
            timeline.event(
                "status_poll",
                label=label,
                index=count,
                response_code=record.get("response_code"),
                response_ok=record.get("response_ok"),
                elapsed_s=round(elapsed, 3),
            )
            count += 1
            if record.get("response_ok"):
                ok += 1
            else:
                errors += 1
            remaining_sleep = interval_s - elapsed
            if remaining_sleep > 0:
                time.sleep(min(remaining_sleep, max(deadline - time.monotonic(), 0.0)))
    return {
        "prop": "0xd212",
        "label": label,
        "interval_s": interval_s,
        "duration_s": round(time.monotonic() - started, 3),
        "polls": count,
        "ok": ok,
        "errors": errors,
        "max_poll_elapsed_s": round(max_elapsed, 3),
        "jsonl": str(path),
        "session_alive": ptp_session.alive,
    }


async def run_liveview_variant(
    config: SweepConfig,
    run_dir: Path,
    poll_label: str,
    interval_s: float | None,
    btmon: base.CaptureProcess,
) -> dict[str, Any]:
    phase_dir = run_dir / f"liveview-{poll_label}"
    phase_dir.mkdir(parents=True, exist_ok=True)
    timeline = base.Timeline(phase_dir / "timeline.jsonl")
    timeline.event("sweep_start", sweep="liveview", poll=poll_label)
    summary: dict[str, Any] = {
        "sweep": "liveview",
        "poll": poll_label,
        "phase_dir": str(phase_dir),
        "started_at": base.utc_iso(),
        "btmon_pid": btmon.process.pid if btmon.process else None,
    }
    tcpdump: base.CaptureProcess | None = None
    event_reader: base.EventRecorder | None = None
    liveview_reader: base.LiveViewRecorder | None = None
    ptp_session: base.PtpControlSession | None = None
    try:
        launch_summary, _backend, _camera, _conn = await connect_and_launch_ap(
            config,
            phase_dir,
            timeline,
            mode="take",
            read_passphrase=False,
        )
        summary.update(launch_summary)
        if not join_after_launch(config, phase_dir, timeline, summary):
            summary["verdict"] = "blocked: wifi join failed"
            if config.pcss_fallback:
                summary["pcss_fallback"] = run_pcss_fallback(config, phase_dir)
            return summary
        tcpdump = base.start_tcpdump(base_compatible_config(config), phase_dir)
        summary["tcpdump"] = {"argv": tcpdump.argv, "pid": tcpdump.process.pid if tcpdump.process else None}
        ptp_session = base.PtpControlSession.open(
            config.camera_host,
            config.command_port,
            config.guid,
            config.friendly_name,
            config.ptp_timeout,
            phase_dir / "ptpip",
            timeline,
            tail_profile="liveview",
        )
        summary["live_view_handshake"] = base.ptp_liveview_handshake(ptp_session)
        event_reader = base.EventRecorder(config.camera_host, config.event_port, phase_dir, timeline, config.ptp_timeout)
        liveview_reader = base.LiveViewRecorder(
            config.camera_host,
            config.liveview_port,
            phase_dir,
            timeline,
            config.ptp_timeout,
            config.save_frames,
        )
        event_reader.start()
        liveview_reader.start()
        await asyncio.sleep(config.settle_s)
        if interval_s is None:
            await asyncio.sleep(config.live_duration_s)
        else:
            summary["status_poll"] = poll_d212_loop(
                ptp_session,
                phase_dir,
                timeline,
                label=poll_label,
                interval_s=interval_s,
                duration_s=config.live_duration_s,
            )
        summary["ptp_steps"] = ptp_session.steps
    except Exception as exc:  # noqa: BLE001 - live artifact should preserve exact failure
        summary["error"] = repr(exc)
        summary["verdict"] = f"blocked: {exc!r}"
        timeline.event("sweep_error", sweep="liveview", error=repr(exc))
    finally:
        if event_reader is not None:
            summary["event_reader"] = event_reader.stop()
        if liveview_reader is not None:
            summary["liveview_reader"] = liveview_reader.stop()
        if ptp_session is not None and ptp_session.alive:
            summary["ptp_close"] = ptp_session.close()
        if tcpdump is not None:
            summary["tcpdump_stop"] = tcpdump.stop()
        if config.cleanup_wifi:
            summary["wifi_cleanup"] = base.cleanup_wifi(base_compatible_config(config))
        await close_ble_connection(summary)
        if "verdict" not in summary:
            frames = summary.get("liveview_reader", {}).get("jpeg_frames", 0)
            errors = summary.get("liveview_reader", {}).get("errors", [])
            poll = summary.get("status_poll", {})
            if frames and not errors and (not poll or poll.get("errors", 0) == 0):
                summary["verdict"] = "liveview_status_sweep_ok"
            elif frames:
                summary["verdict"] = "liveview_streamed_with_errors"
            else:
                summary["verdict"] = "liveview_not_streaming"
        summary["finished_at"] = base.utc_iso()
        (phase_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
        timeline.event("sweep_finish", sweep="liveview", verdict=summary.get("verdict", "unknown"))
    return summary


async def run_ap_mode_sweep(
    config: SweepConfig,
    run_dir: Path,
    mode: str,
    btmon: base.CaptureProcess,
) -> dict[str, Any]:
    phase_dir = run_dir / f"ap-{mode}"
    phase_dir.mkdir(parents=True, exist_ok=True)
    timeline = base.Timeline(phase_dir / "timeline.jsonl")
    timeline.event("sweep_start", sweep="ap", mode=mode)
    summary: dict[str, Any] = {
        "sweep": "ap",
        "mode": mode,
        "phase_dir": str(phase_dir),
        "started_at": base.utc_iso(),
        "btmon_pid": btmon.process.pid if btmon.process else None,
    }
    try:
        launch_summary, _backend, _camera, _conn = await connect_and_launch_ap(
            config,
            phase_dir,
            timeline,
            mode=mode,
            read_passphrase=True,
        )
        summary.update(launch_summary)
        if join_after_launch(config, phase_dir, timeline, summary):
            summary["port_probe"] = port_probe(
                config.camera_host,
                [config.command_port, config.event_port, config.liveview_port],
                min(config.ptp_timeout, 3.0),
            )
        else:
            summary["port_probe"] = {"skipped": "wifi join failed"}
    except Exception as exc:  # noqa: BLE001 - live artifact should preserve exact failure
        summary["error"] = repr(exc)
        summary["verdict"] = f"blocked: {exc!r}"
        timeline.event("sweep_error", sweep="ap", mode=mode, error=repr(exc))
    finally:
        if config.cleanup_wifi:
            summary["wifi_cleanup"] = base.cleanup_wifi(base_compatible_config(config))
        await close_ble_connection(summary)
        if "verdict" not in summary:
            joined = bool(summary.get("wifi_join", {}).get("ok"))
            open_ports = [
                port for port, record in summary.get("port_probe", {}).items()
                if isinstance(record, dict) and record.get("open")
            ]
            summary["verdict"] = "ap_launch_joined_ports_observed" if joined else "ap_launch_no_wifi_join"
            summary["open_ports"] = open_ports
        summary["finished_at"] = base.utc_iso()
        (phase_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
        timeline.event("sweep_finish", sweep="ap", mode=mode, verdict=summary.get("verdict", "unknown"))
    return summary


def run_object_metadata_probe(
    config: SweepConfig,
    phase_dir: Path,
    timeline: base.Timeline,
    ptp_session: base.PtpControlSession,
) -> dict[str, Any]:
    summary: dict[str, Any] = {
        "object_limit": config.object_limit,
        "partial_bytes": config.partial_bytes,
        "raf_preview_bytes": config.raf_preview_bytes,
    }
    count = ptp_session.get_prop(PROP_OBJECT_COUNT, "transfer_get_d620_count")
    summary["object_count"] = count
    handles_result = ptp_session.get_prop(PROP_OBJECT_HANDLES, "transfer_get_d621_handles")
    handles = parse_handles(payload_bytes(handles_result))
    summary["handles_d621"] = handles_result
    if not handles:
        standard = ptp_session.transaction(
            PTP_GET_OBJECT_HANDLES,
            (0xFFFFFFFF, 0, 0),
            prefix="transfer_get_object_handles",
        )
        handles = parse_handles(payload_bytes(standard))
        summary["handles_1007"] = standard
    selected = handles[: config.object_limit]
    summary["handles_selected"] = [f"0x{handle:08x}" for handle in selected]
    objects: list[dict[str, Any]] = []
    for index, handle in enumerate(selected):
        prefix = f"transfer_obj_{index:02d}_{handle:08x}"
        record: dict[str, Any] = {"handle": f"0x{handle:08x}"}
        info = ptp_session.transaction(ptpip.PTP_GET_OBJECT_INFO, (handle,), prefix=f"{prefix}_info")
        record["object_info"] = info
        info_payload = payload_bytes(info)
        record["object_info_decoded"] = parse_object_info(info_payload) if info_payload else {}
        ext = ptp_session.transaction(PTP_GET_EXTENSION_OBJECT_INFO, (handle,), prefix=f"{prefix}_ext9054")
        record["extension_object_info"] = ext
        partial = ptp_session.transaction(
            PTP_GET_PARTIAL_OBJECT,
            (handle, 0, config.partial_bytes),
            prefix=f"{prefix}_partial_header",
        )
        record["partial_header"] = partial
        header = payload_bytes(partial)
        record["partial_header_signature"] = header[:16].hex(" ") if header else ""
        if config.raf_preview_bytes and is_raf_object(record["object_info_decoded"], header):
            preview = ptp_session.transaction(
                PTP_GET_PARTIAL_OBJECT,
                (handle, 0, config.raf_preview_bytes),
                prefix=f"{prefix}_raf_preview_scan",
            )
            record["raf_preview_scan"] = preview
        objects.append(record)
        timeline.event(
            "transfer_object_probe",
            handle=f"0x{handle:08x}",
            info_ok=info.get("response_ok"),
            ext9054_ok=ext.get("response_ok"),
            partial_ok=partial.get("response_ok"),
        )
    summary["objects"] = objects
    summary["ptp_steps"] = ptp_session.steps
    return summary


def run_pcss_fallback(config: SweepConfig, phase_dir: Path) -> dict[str, Any]:
    fallback_dir = phase_dir / "pcss_fallback"
    fallback_dir.mkdir(parents=True, exist_ok=True)
    return base.run_cmd(
        [
            sys.executable,
            "scripts/probe_partial_header.py",
            config.pcss_camera_host,
            "--bytes",
            str(config.partial_bytes),
            "--session-dir",
            str(fallback_dir),
        ],
        cwd=base.protocol_mapper_root(),
        timeout=config.pcss_timeout,
    )


async def run_transfer_sweep(config: SweepConfig, run_dir: Path, btmon: base.CaptureProcess) -> dict[str, Any]:
    phase_dir = run_dir / "transfer-metadata"
    phase_dir.mkdir(parents=True, exist_ok=True)
    timeline = base.Timeline(phase_dir / "timeline.jsonl")
    timeline.event("sweep_start", sweep="transfer")
    summary: dict[str, Any] = {
        "sweep": "transfer",
        "phase_dir": str(phase_dir),
        "started_at": base.utc_iso(),
        "btmon_pid": btmon.process.pid if btmon.process else None,
    }
    tcpdump: base.CaptureProcess | None = None
    ptp_session: base.PtpControlSession | None = None
    try:
        launch_summary, _backend, _camera, _conn = await connect_and_launch_ap(
            config,
            phase_dir,
            timeline,
            mode="get",
            read_passphrase=True,
        )
        summary.update(launch_summary)
        if not join_after_launch(config, phase_dir, timeline, summary):
            summary["verdict"] = "blocked: wifi join failed"
            return summary
        tcpdump = base.start_tcpdump(base_compatible_config(config), phase_dir)
        summary["tcpdump"] = {"argv": tcpdump.argv, "pid": tcpdump.process.pid if tcpdump.process else None}
        ptp_session = base.PtpControlSession.open(
            config.camera_host,
            config.command_port,
            config.guid,
            config.friendly_name,
            config.ptp_timeout,
            phase_dir / "ptpip",
            timeline,
            tail_profile="get",
        )
        summary["metadata_probe"] = run_object_metadata_probe(config, phase_dir, timeline, ptp_session)
    except Exception as exc:  # noqa: BLE001 - live artifact should preserve exact failure
        summary["error"] = repr(exc)
        summary["verdict"] = f"blocked: {exc!r}"
        timeline.event("sweep_error", sweep="transfer", error=repr(exc))
        if config.pcss_fallback:
            summary["pcss_fallback"] = run_pcss_fallback(config, phase_dir)
    finally:
        if ptp_session is not None and ptp_session.alive:
            summary["ptp_close"] = ptp_session.close()
        if tcpdump is not None:
            summary["tcpdump_stop"] = tcpdump.stop()
        if config.cleanup_wifi:
            summary["wifi_cleanup"] = base.cleanup_wifi(base_compatible_config(config))
        await close_ble_connection(summary)
        if "verdict" not in summary:
            objects = summary.get("metadata_probe", {}).get("objects", [])
            summary["verdict"] = "transfer_metadata_probe_ok" if objects else "transfer_metadata_no_objects"
        summary["finished_at"] = base.utc_iso()
        (phase_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
        timeline.event("sweep_finish", sweep="transfer", verdict=summary.get("verdict", "unknown"))
    return summary


def render_verdict(run_summary: dict[str, Any]) -> str:
    lines = [
        f"# Issue #{run_summary.get('issue_number', DEFAULT_ISSUE_NUMBER)} reference app Capture Sweep Verdict",
        "",
        f"- Started: {run_summary.get('started_at', '')}",
        f"- Finished: {run_summary.get('finished_at', '')}",
        f"- Capture dir: `{run_summary.get('capture_dir', '')}`",
        f"- Rig: `{run_summary.get('rig', '')}`",
        "",
        "| Sweep | Verdict | Notes |",
        "|---|---|---|",
    ]
    for result in run_summary.get("results", []):
        notes: list[str] = []
        if result.get("sweep") == "liveview":
            live = result.get("liveview_reader", {})
            notes.append(f"poll={result.get('poll')}")
            notes.append(f"55742 jpeg={live.get('jpeg_frames', 0)}")
            if live.get("max_gap_s") is not None:
                notes.append(f"max_gap_s={live.get('max_gap_s')}")
            if result.get("status_poll"):
                poll = result["status_poll"]
                notes.append(f"d212_ok={poll.get('ok', 0)}/{poll.get('polls', 0)}")
        elif result.get("sweep") == "ap":
            notes.append(f"mode={result.get('mode')}")
            notes.append(f"open_ports={','.join(result.get('open_ports', []))}")
        elif result.get("sweep") == "transfer":
            objects = result.get("metadata_probe", {}).get("objects", [])
            notes.append(f"objects={len(objects)}")
            if result.get("pcss_fallback"):
                notes.append(f"pcss_fallback_rc={result['pcss_fallback'].get('returncode')}")
        if result.get("error"):
            notes.append(str(result["error"]))
        lines.append(f"| {result.get('sweep')} | {result.get('verdict', 'unknown')} | {'; '.join(notes)} |")
    lines.append("")
    lines.append("Raw artifacts include run-level `hci.btsnoop`, per-sweep `ptpip.pcap`, BLE JSONL, timelines, summaries, and bounded object metadata blobs.")
    return "\n".join(lines) + "\n"


def config_from_args(args: argparse.Namespace) -> SweepConfig:
    return SweepConfig(
        issue_number=args.issue_number,
        capture_root=args.capture_root,
        sweeps=args.sweeps,
        ap_modes=args.ap_modes,
        poll_specs=args.poll_specs,
        camera_name=args.camera_name,
        ble_address=args.ble_address or None,
        friendly_name=args.friendly_name,
        guid=args.guid,
        camera_host=args.camera_host,
        pcss_camera_host=args.pcss_camera_host,
        command_port=args.command_port,
        event_port=args.event_port,
        liveview_port=args.liveview_port,
        wifi_iface=args.wifi_iface,
        wifi_join_method=args.wifi_join_method,
        static_ip_cidr=args.static_ip_cidr,
        ap_ssid=args.ap_ssid,
        con_name=args.con_name,
        scan_timeout=args.scan_timeout,
        ap_timeout=args.ap_timeout,
        ptp_timeout=args.ptp_timeout,
        live_duration_s=args.live_duration_s,
        settle_s=args.settle_s,
        save_frames=args.save_frames,
        object_limit=args.object_limit,
        partial_bytes=args.partial_bytes,
        raf_preview_bytes=args.raf_preview_bytes,
        pcss_fallback=not args.no_pcss_fallback,
        pcss_timeout=args.pcss_timeout,
        register=not args.no_register,
        cleanup_wifi=not args.keep_wifi,
        preflight_only=args.preflight_only,
        pairing_key=bytes.fromhex(args.pairing_key) if args.pairing_key else None,
        stamp=args.stamp,
    )


async def run_local_async(config: SweepConfig) -> dict[str, Any]:
    run_dir = capture_dir_for(config.capture_root, config.issue_number, config.stamp)
    run_dir.mkdir(parents=True, exist_ok=True)
    summary: dict[str, Any] = {
        "issue_number": config.issue_number,
        "started_at": base.utc_iso(),
        "capture_dir": str(run_dir),
        "rig": socket.gethostname(),
        "sweeps_requested": config.sweeps,
    }
    checks = base.preflight(base_compatible_config(config))
    summary["preflight"] = checks
    (run_dir / "preflight.json").write_text(json.dumps(checks, indent=2, sort_keys=True) + "\n")
    if not checks["ok"]:
        summary["finished_at"] = base.utc_iso()
        summary["verdict"] = "blocked: preflight failed"
        (run_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
        (run_dir / "VERDICT.md").write_text(render_verdict({**summary, "results": []}), encoding="utf-8")
        return summary
    if config.preflight_only:
        summary["finished_at"] = base.utc_iso()
        summary["verdict"] = "preflight ok"
        (run_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
        (run_dir / "VERDICT.md").write_text(render_verdict({**summary, "results": []}), encoding="utf-8")
        return summary
    btmon = base.start_btmon(run_dir)
    summary["btmon"] = {"argv": btmon.argv, "pid": btmon.process.pid if btmon.process else None}
    results: list[dict[str, Any]] = []
    try:
        if "liveview" in config.sweeps:
            for poll_label in config.poll_specs:
                results.append(await run_liveview_variant(config, run_dir, poll_label, POLL_SPECS[poll_label], btmon))
        if "ap" in config.sweeps:
            for mode in config.ap_modes:
                results.append(await run_ap_mode_sweep(config, run_dir, mode, btmon))
        if "transfer" in config.sweeps:
            results.append(await run_transfer_sweep(config, run_dir, btmon))
    finally:
        summary["btmon_stop"] = btmon.stop()
    summary["results"] = results
    summary["finished_at"] = base.utc_iso()
    summary["verdict"] = "complete" if all(not str(r.get("verdict", "")).startswith("blocked") for r in results) else "blocked"
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
    (run_dir / "VERDICT.md").write_text(render_verdict(summary), encoding="utf-8")
    return summary


def run_local(args: argparse.Namespace) -> int:
    summary = asyncio.run(run_local_async(config_from_args(args)))
    print("RUN_RESULT_JSON " + json.dumps(summary, sort_keys=True, default=json_default))
    return 0 if not str(summary.get("verdict", "")).startswith("blocked") else 1


def remote_local_args(args: argparse.Namespace) -> list[str]:
    argv = [
        "scripts/probe_app_capture_sweeps.py",
        "--run-local",
        "--issue-number",
        str(args.issue_number),
        "--capture-root",
        "captures",
        "--sweeps",
        ",".join(args.sweeps),
        "--ap-modes",
        ",".join(args.ap_modes),
        "--poll-specs",
        ",".join(args.poll_specs),
        "--camera-name",
        args.camera_name,
        "--friendly-name",
        args.friendly_name,
        "--guid",
        args.guid,
        "--camera-host",
        args.camera_host,
        "--pcss-camera-host",
        args.pcss_camera_host,
        "--wifi-iface",
        args.wifi_iface,
        "--wifi-join-method",
        args.wifi_join_method,
        "--static-ip-cidr",
        args.static_ip_cidr,
        "--ap-ssid",
        args.ap_ssid,
        "--con-name",
        args.con_name,
        "--scan-timeout",
        str(args.scan_timeout),
        "--ap-timeout",
        str(args.ap_timeout),
        "--ptp-timeout",
        str(args.ptp_timeout),
        "--live-duration-s",
        str(args.live_duration_s),
        "--settle-s",
        str(args.settle_s),
        "--save-frames",
        str(args.save_frames),
        "--object-limit",
        str(args.object_limit),
        "--partial-bytes",
        str(args.partial_bytes),
        "--raf-preview-bytes",
        str(args.raf_preview_bytes),
        "--pcss-timeout",
        str(args.pcss_timeout),
    ]
    if args.ble_address:
        argv.extend(["--ble-address", args.ble_address])
    if args.pairing_key:
        argv.extend(["--pairing-key", args.pairing_key])
    if args.no_register:
        argv.append("--no-register")
    if args.keep_wifi:
        argv.append("--keep-wifi")
    if args.preflight_only:
        argv.append("--preflight-only")
    if args.no_pcss_fallback:
        argv.append("--no-pcss-fallback")
    if args.stamp:
        argv.extend(["--stamp", args.stamp])
    return argv


def parse_run_result(stdout: str) -> dict[str, Any]:
    return base.parse_run_result(stdout)


def run_remote(args: argparse.Namespace) -> int:
    root = base.protocol_mapper_root()
    args.capture_root.mkdir(parents=True, exist_ok=True)
    remote_workdir = args.remote_workdir.rstrip("/")
    ssh_prefix = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", args.remote_host]
    ssh_check = base.run_cmd(ssh_prefix + ["hostname"], timeout=12)
    if ssh_check["returncode"] != 0:
        print(json.dumps({"error": "ssh preflight failed", "result": ssh_check}, indent=2, sort_keys=True))
        return 1
    mkdir = base.run_cmd(ssh_prefix + [f"mkdir -p {shlex.quote(remote_workdir)}"], timeout=12)
    if mkdir["returncode"] != 0:
        print(json.dumps({"error": "remote mkdir failed", "result": mkdir}, indent=2, sort_keys=True))
        return 1
    rsync_push = base.run_cmd(
        [
            "rsync",
            "-az",
            "--delete",
            "--exclude",
            ".venv/",
            "--exclude",
            "captures/",
            "--exclude",
            "__pycache__/",
            "--exclude",
            ".pytest_cache/",
            f"{root}/",
            f"{args.remote_host}:{remote_workdir}/",
        ],
        timeout=120,
    )
    if rsync_push["returncode"] != 0:
        print(json.dumps({"error": "rsync push failed", "result": rsync_push}, indent=2, sort_keys=True))
        return 1
    remote_args = " ".join(shlex.quote(part) for part in remote_local_args(args))
    command = f"cd {shlex.quote(remote_workdir)} && PYTHONPATH=. {args.remote_python} {remote_args}"
    remote_run = base.run_cmd(ssh_prefix + [command], timeout=args.remote_timeout)
    print(remote_run.get("stdout", ""))
    if remote_run.get("stderr"):
        print(remote_run["stderr"], file=sys.stderr)
    try:
        result = parse_run_result(remote_run.get("stdout", ""))
    except RuntimeError as exc:
        print(json.dumps({"error": str(exc), "remote_run": remote_run}, indent=2, sort_keys=True), file=sys.stderr)
        return 1
    remote_capture = str(result.get("capture_dir", ""))
    if remote_capture:
        local_target = args.capture_root / Path(remote_capture).name
        local_target.parent.mkdir(parents=True, exist_ok=True)
        local_target.mkdir(parents=True, exist_ok=True)
        remote_capture_for_rsync = (
            remote_capture
            if remote_capture.startswith("/")
            else f"{remote_workdir}/{remote_capture}"
        )
        rsync_pull = base.run_cmd(
            ["rsync", "-az", f"{args.remote_host}:{remote_capture_for_rsync}/", f"{local_target}/"],
            timeout=120,
        )
        (local_target / "remote_result.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
        if rsync_pull["returncode"] != 0:
            print(json.dumps({"error": "rsync pull failed", "result": rsync_pull}, indent=2, sort_keys=True))
            return 1
        print(f"local_capture_dir={local_target}")
    return remote_run["returncode"]


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="probe-app-capture-sweeps")
    parser.add_argument("--remote-host", default=base.DEFAULT_REMOTE_HOST)
    parser.add_argument("--remote-workdir", default=DEFAULT_REMOTE_WORKDIR)
    parser.add_argument("--remote-python", default=base.DEFAULT_REMOTE_PYTHON)
    parser.add_argument("--remote-timeout", type=float, default=3600)
    parser.add_argument("--run-local", action="store_true", help="run on the host with attached BLE/Wi-Fi hardware")
    parser.add_argument("--issue-number", type=int, default=DEFAULT_ISSUE_NUMBER)
    parser.add_argument("--capture-root", type=Path, default=base.default_capture_root())
    parser.add_argument("--stamp", default="")
    parser.add_argument("--sweeps", type=parse_sweeps, default=list(SWEEPS))
    parser.add_argument("--ap-modes", type=parse_ap_modes, default=list(AP_MODES))
    parser.add_argument("--poll-specs", type=parse_poll_specs, default=list(POLL_SPECS))
    parser.add_argument("--camera-name", default="GFX100 II")
    parser.add_argument("--ble-address", default="")
    parser.add_argument(
        "--pairing-key",
        type=base.parse_pairing_key,
        default="",
        help="hex payload to write to the Fuji PAIRING_KEY characteristic when advertisements omit it",
    )
    parser.add_argument("--friendly-name", default=base.DEFAULT_FRIENDLY_NAME)
    parser.add_argument("--guid", default=base.DEFAULT_GUID)
    parser.add_argument("--camera-host", default=base.DEFAULT_CAMERA_HOST)
    parser.add_argument("--pcss-camera-host", default=base.DEFAULT_CAMERA_HOST)
    parser.add_argument("--command-port", type=int, default=55740)
    parser.add_argument("--event-port", type=int, default=55741)
    parser.add_argument("--liveview-port", type=int, default=55742)
    parser.add_argument("--wifi-iface", default=base.DEFAULT_WIFI_IFACE)
    parser.add_argument("--wifi-join-method", choices=["auto", "nmcli", "wpa_cli", "iw_static"], default="auto")
    parser.add_argument("--static-ip-cidr", default=base.DEFAULT_STATIC_IP_CIDR)
    parser.add_argument("--ap-ssid", default="FUJIFILM-GFX100II-0C3E")
    parser.add_argument("--con-name", default=base.DEFAULT_CON_NAME)
    parser.add_argument("--scan-timeout", type=float, default=40.0)
    parser.add_argument("--ap-timeout", type=float, default=25.0)
    parser.add_argument("--ptp-timeout", type=float, default=6.0)
    parser.add_argument("--live-duration-s", type=float, default=15.0)
    parser.add_argument("--settle-s", type=float, default=1.0)
    parser.add_argument("--save-frames", type=int, default=3)
    parser.add_argument("--object-limit", type=parse_positive_int, default=5)
    parser.add_argument("--partial-bytes", type=parse_positive_int, default=4096)
    parser.add_argument("--raf-preview-bytes", type=parse_raf_preview_bytes, default=0)
    parser.add_argument("--no-pcss-fallback", action="store_true")
    parser.add_argument("--pcss-timeout", type=float, default=90.0)
    parser.add_argument("--no-register", action="store_true")
    parser.add_argument("--keep-wifi", action="store_true")
    parser.add_argument("--preflight-only", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.run_local:
        return run_local(args)
    return run_remote(args)


if __name__ == "__main__":
    raise SystemExit(main())
