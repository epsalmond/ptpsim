from __future__ import annotations

import argparse
import asyncio
from dataclasses import dataclass
from datetime import UTC, datetime
import json
from pathlib import Path
import shlex
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
from typing import Any

from rce.tools.fuji_ble_gps import ptpip, uuids
from rce.tools.fuji_ble_gps.ble_backend import BleakBackend, BleConnection
from rce.tools.fuji_ble_gps.camera import FujiCamera
from rce.tools.fuji_ble_gps.session import Session


DEFAULT_REMOTE_HOST = "eric@rpi4b.local"
DEFAULT_REMOTE_WORKDIR = "/home/eric/ptpsim-protocol-mapper-issue-52"
DEFAULT_REMOTE_PYTHON = "/home/eric/fuji/lab/.venv/bin/python"
DEFAULT_CAMERA_HOST = "192.168.0.1"
DEFAULT_WIFI_IFACE = "wlx00c0cab7f674"
DEFAULT_STATIC_IP_CIDR = "192.168.0.2/24"
DEFAULT_GUID = "f2e4538fada5485d87b27f0bd3d5ded0"
DEFAULT_FRIENDLY_NAME = "testhost"
DEFAULT_CON_NAME = "fuji-cam-ap"
REMOTE_SHUTTER_SEQUENCE = (
    ("S1", bytes.fromhex("0100")),
    ("S2", bytes.fromhex("0200")),
    ("S0", bytes.fromhex("0000")),
)
PHASES = ("a", "b", "c")
EVENT_NAMES = {
    0x4002: "ObjectAdded",
    0x4008: "DeviceInfoChanged",
    0x400D: "CaptureComplete",
    0xC001: "PostviewComplete",
    0xC004: "CaptureStart",
    0xC005: "AfComplete",
    0xC006: "PropListChanged",
}
POLL_PROPS = (0xD212, 0xD209, 0xD17C)


def utc_stamp() -> str:
    return datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")


def utc_iso() -> str:
    return datetime.now(UTC).isoformat().replace("+00:00", "Z")


def protocol_mapper_root() -> Path:
    return Path(__file__).resolve().parents[1]


def default_capture_root() -> Path:
    return protocol_mapper_root() / "captures"


def capture_dir_for(root: Path, stamp: str | None = None) -> Path:
    return root / f"issue-52-{stamp or utc_stamp()}"


def parse_phases(value: str) -> list[str]:
    phases = [part.strip().lower() for part in value.split(",") if part.strip()]
    invalid = [phase for phase in phases if phase not in PHASES]
    if invalid:
        raise argparse.ArgumentTypeError(f"invalid phase(s): {', '.join(invalid)}")
    return phases or list(PHASES)


def parse_pairing_key(value: str) -> str:
    cleaned = value.strip().replace(":", "").replace(" ", "")
    if not cleaned:
        return ""
    try:
        bytes.fromhex(cleaned)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("pairing key must be hex bytes") from exc
    if len(cleaned) % 2:
        raise argparse.ArgumentTypeError("pairing key must contain an even number of hex digits")
    return cleaned.lower()


def json_default(value: Any) -> Any:
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, bytes):
        return value.hex()
    raise TypeError(f"{type(value)!r} is not JSON serializable")


class Timeline:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._lock = threading.Lock()

    def event(self, event: str, **fields: Any) -> dict[str, Any]:
        record = {"ts": utc_iso(), "event": event, **fields}
        with self._lock:
            with self.path.open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(record, sort_keys=True, default=json_default) + "\n")
        return record


@dataclass
class CaptureProcess:
    argv: list[str]
    stdout_path: Path
    stderr_path: Path | None = None
    process: subprocess.Popen[bytes] | None = None

    def start(self) -> dict[str, Any]:
        self.stdout_path.parent.mkdir(parents=True, exist_ok=True)
        stderr_target = self.stderr_path or self.stdout_path.with_suffix(self.stdout_path.suffix + ".stderr")
        stdout = self.stdout_path.open("wb")
        stderr = stderr_target.open("wb")
        try:
            self.process = subprocess.Popen(self.argv, stdout=stdout, stderr=stderr)
        finally:
            stdout.close()
            stderr.close()
        return {"argv": self.argv, "pid": self.process.pid if self.process else None}

    def stop(self, timeout: float = 5.0) -> dict[str, Any]:
        if self.process is None:
            return {"stopped": False, "reason": "not_started"}
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=timeout)
        return {"stopped": True, "returncode": self.process.returncode}


def run_cmd(argv: list[str], *, cwd: Path | None = None, timeout: float | None = None) -> dict[str, Any]:
    started = time.monotonic()
    try:
        proc = subprocess.run(
            argv,
            cwd=cwd,
            timeout=timeout,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        return {
            "argv": argv,
            "returncode": proc.returncode,
            "stdout": proc.stdout.strip(),
            "stderr": proc.stderr.strip(),
            "elapsed_s": round(time.monotonic() - started, 3),
        }
    except FileNotFoundError as exc:
        return {"argv": argv, "returncode": 127, "error": str(exc)}
    except subprocess.TimeoutExpired as exc:
        return {
            "argv": argv,
            "returncode": 124,
            "stdout": (exc.stdout or "").strip() if isinstance(exc.stdout, str) else "",
            "stderr": (exc.stderr or "").strip() if isinstance(exc.stderr, str) else "",
            "error": "timeout",
        }


def preflight(config: "LocalConfig") -> dict[str, Any]:
    checks: dict[str, Any] = {
        "host": socket.gethostname(),
        "capture_root": str(config.capture_root),
        "wifi_iface": config.wifi_iface,
        "commands": {},
    }
    for command in ("bluetoothctl", "btmon", "nmcli", "wpa_cli", "iw", "networkctl", "dhclient", "tcpdump", "sudo", "ip"):
        checks["commands"][command] = shutil.which(command)
    checks["bluetoothctl_show"] = run_cmd(["bluetoothctl", "show"], timeout=5)
    checks["wifi_link"] = run_cmd(["ip", "link", "show", "dev", config.wifi_iface], timeout=5)
    checks["sudo_noninteractive"] = run_cmd(["sudo", "-n", "true"], timeout=5)
    try:
        import bleak  # noqa: F401

        checks["python_bleak"] = {"ok": True}
    except Exception as exc:  # noqa: BLE001 - artifact should capture exact import failure
        checks["python_bleak"] = {"ok": False, "error": repr(exc)}
    required = ("bluetoothctl", "btmon", "tcpdump", "sudo", "ip")
    missing = [name for name in required if not checks["commands"].get(name)]
    failures = []
    if missing:
        failures.append(f"missing commands: {', '.join(missing)}")
    wifi_join_available = bool(checks["commands"].get("nmcli")) or bool(checks["commands"].get("iw"))
    checks["wifi_join_available"] = wifi_join_available
    if not wifi_join_available:
        failures.append("missing Wi-Fi join tools: need nmcli or iw")
    if checks["bluetoothctl_show"]["returncode"] != 0 or "No default controller" in checks["bluetoothctl_show"].get("stdout", ""):
        failures.append("BlueZ default controller unavailable")
    if checks["wifi_link"]["returncode"] != 0:
        failures.append(f"Wi-Fi interface {config.wifi_iface} unavailable")
    if checks["sudo_noninteractive"]["returncode"] != 0:
        failures.append("sudo -n is not available for capture commands")
    if not checks["python_bleak"]["ok"]:
        failures.append("Python bleak import failed")
    checks["ok"] = not failures
    checks["failures"] = failures
    return checks


def decode_event_packet(packet: bytes) -> dict[str, Any]:
    out: dict[str, Any] = {"raw_hex": packet.hex(), "bytes": len(packet)}
    if len(packet) < 4:
        out["decode_error"] = "short length"
        return out
    total = struct.unpack_from("<I", packet, 0)[0]
    out["declared_length"] = total
    if total != len(packet):
        out["length_mismatch"] = True
    body = packet[4:]
    if len(body) < 24:
        out["decode_error"] = "short event body"
        return out
    packet_type, event_code, tid, p1, p2, p3, p4 = struct.unpack_from("<HHIIIII", body, 0)
    out.update(
        {
            "packet_type": packet_type,
            "event_code": f"0x{event_code:04x}",
            "event_name": EVENT_NAMES.get(event_code, "Unknown"),
            "transaction_id": tid,
            "params": [p1, p2, p3, p4],
            "params_hex": [f"0x{param:08x}" for param in (p1, p2, p3, p4)],
        }
    )
    return out


class EventRecorder(threading.Thread):
    def __init__(self, host: str, port: int, out_dir: Path, timeline: Timeline, timeout: float) -> None:
        super().__init__(daemon=True)
        self.host = host
        self.port = port
        self.out_dir = out_dir
        self.timeline = timeline
        self.timeout = timeout
        self.stop_event = threading.Event()
        self.summary: dict[str, Any] = {"port": port, "connected": False, "events": 0, "errors": []}

    def run(self) -> None:
        raw_path = self.out_dir / f"port_{self.port}_events.raw"
        jsonl_path = self.out_dir / f"port_{self.port}_events.jsonl"
        try:
            sock = socket.create_connection((self.host, self.port), self.timeout)
        except OSError as exc:
            self.summary["errors"].append(repr(exc))
            self.timeline.event("ptpip_event_socket_connect_failed", port=self.port, error=repr(exc))
            return
        self.summary["connected"] = True
        self.timeline.event("ptpip_event_socket_connected", port=self.port)
        sock.settimeout(0.5)
        with sock, raw_path.open("ab") as raw, jsonl_path.open("a", encoding="utf-8") as out:
            while not self.stop_event.is_set():
                try:
                    header = ptpip.read_exact(sock, 4)
                    if not header:
                        break
                    if len(header) != 4:
                        self.summary["errors"].append("short length header")
                        break
                    total = struct.unpack("<I", header)[0]
                    if total < 4 or total > 1024 * 1024:
                        self.summary["errors"].append(f"bad event length {total}")
                        break
                    body = ptpip.read_exact(sock, total - 4)
                    if len(body) != total - 4:
                        self.summary["errors"].append("short event body")
                        break
                except socket.timeout:
                    continue
                except OSError as exc:
                    self.summary["errors"].append(repr(exc))
                    break
                packet = header + body
                raw.write(packet)
                decoded = decode_event_packet(packet)
                self.summary["events"] += 1
                out.write(json.dumps({"ts": utc_iso(), **decoded}, sort_keys=True) + "\n")
                out.flush()
                self.timeline.event("ptpip_event", port=self.port, **decoded)

    def stop(self) -> dict[str, Any]:
        self.stop_event.set()
        self.join(timeout=2.0)
        return dict(self.summary)


class LiveViewRecorder(threading.Thread):
    def __init__(self, host: str, port: int, out_dir: Path, timeline: Timeline, timeout: float, save_frames: int) -> None:
        super().__init__(daemon=True)
        self.host = host
        self.port = port
        self.out_dir = out_dir
        self.timeline = timeline
        self.timeout = timeout
        self.save_frames = save_frames
        self.stop_event = threading.Event()
        self.summary: dict[str, Any] = {
            "port": port,
            "connected": False,
            "frames": 0,
            "jpeg_frames": 0,
            "bytes_min": None,
            "bytes_max": None,
            "errors": [],
        }

    def run(self) -> None:
        try:
            sock = socket.create_connection((self.host, self.port), self.timeout)
        except OSError as exc:
            self.summary["errors"].append(repr(exc))
            self.timeline.event("ptpip_liveview_socket_connect_failed", port=self.port, error=repr(exc))
            return
        self.summary["connected"] = True
        self.timeline.event("ptpip_liveview_socket_connected", port=self.port)
        sock.settimeout(0.5)
        saved = 0
        with sock:
            while not self.stop_event.is_set():
                try:
                    header = ptpip.read_exact(sock, 4)
                    if not header:
                        break
                    if len(header) != 4:
                        self.summary["errors"].append("short frame length")
                        break
                    total = int.from_bytes(header, "little")
                    if total < 4 or total > 64 * 1024 * 1024:
                        self.summary["errors"].append(f"bad frame length {total}")
                        break
                    body = ptpip.read_exact(sock, total - 4)
                    if len(body) != total - 4:
                        self.summary["errors"].append("short frame body")
                        break
                except socket.timeout:
                    continue
                except OSError as exc:
                    self.summary["errors"].append(repr(exc))
                    break
                self.summary["frames"] += 1
                soi = body.find(b"\xff\xd8")
                if soi < 0:
                    continue
                eoi = body.find(b"\xff\xd9", soi)
                jpeg = body[soi:eoi + 2] if eoi > 0 else body[soi:]
                size = len(jpeg)
                self.summary["jpeg_frames"] += 1
                self.summary["bytes_min"] = size if self.summary["bytes_min"] is None else min(self.summary["bytes_min"], size)
                self.summary["bytes_max"] = size if self.summary["bytes_max"] is None else max(self.summary["bytes_max"], size)
                if saved < self.save_frames and eoi > 0:
                    target = self.out_dir / f"liveview_{saved:03d}.jpg"
                    target.write_bytes(jpeg)
                    saved += 1
                if self.summary["jpeg_frames"] <= 5 or self.summary["jpeg_frames"] % 100 == 0:
                    self.timeline.event("ptpip_liveview_frame", port=self.port, bytes=size)
        self.summary["jpegs_saved"] = saved

    def stop(self) -> dict[str, Any]:
        self.stop_event.set()
        self.join(timeout=2.0)
        return dict(self.summary)


def connect_ptpip_control(host: str, port: int, timeout: float, timeline: Timeline) -> socket.socket:
    deadline = time.monotonic() + timeout
    attempt = 0
    last_exc: OSError | None = None
    while True:
        attempt += 1
        remaining = max(deadline - time.monotonic(), 0.1)
        try:
            sock = socket.create_connection((host, port), min(1.0, remaining))
        except OSError as exc:
            last_exc = exc
            timeline.event("ptpip_control_connect_failed", host=host, port=port, attempt=attempt, error=repr(exc))
            if time.monotonic() >= deadline:
                raise
            time.sleep(min(0.5, max(deadline - time.monotonic(), 0.0)))
            continue
        sock.settimeout(timeout)
        timeline.event("ptpip_control_connected", host=host, port=port, attempts=attempt)
        return sock
    raise RuntimeError(f"unreachable PTP/IP control connect loop last_exc={last_exc!r}")


class PtpControlSession:
    def __init__(self, sock: socket.socket, out_dir: Path, timeline: Timeline) -> None:
        self.sock = sock
        self.out_dir = out_dir
        self.timeline = timeline
        self.tid = 2
        self.alive = True
        self.steps: list[dict[str, Any]] = []

    @classmethod
    def open(
        cls,
        host: str,
        port: int,
        guid: str,
        friendly_name: str,
        timeout: float,
        out_dir: Path,
        timeline: Timeline,
    ) -> "PtpControlSession":
        out_dir.mkdir(parents=True, exist_ok=True)
        sock = connect_ptpip_control(host, port, timeout, timeline)
        init = ptpip.build_init_command_request(friendly_name, "liveview", guid=ptpip.parse_guid_hex(guid))
        (out_dir / "init_command_request.bin").write_bytes(init)
        sock.sendall(init)
        init_resp = ptpip.recv_packet(sock)
        (out_dir / "init_command_response.bin").write_bytes(init_resp)
        open_req = ptpip.build_open_session()
        (out_dir / "open_session_request.bin").write_bytes(open_req)
        sock.sendall(open_req)
        open_resp = ptpip.recv_packet(sock)
        (out_dir / "open_session_response.bin").write_bytes(open_resp)
        ohdr = ptpip.ptp_container_header(open_resp)
        if ohdr.get("code") not in (ptpip.PTP_RESPONSE_OK, ptpip.PTP_RESPONSE_SESSION_ALREADY_OPEN):
            raise RuntimeError(f"OpenSession failed: {ohdr}")
        timeline.event("ptpip_open_session_ok", response_code=f"0x{int(ohdr.get('code', 0)):04x}")
        return cls(sock, out_dir, timeline)

    def close(self) -> dict[str, Any]:
        if not self.alive:
            return {"closed": False, "reason": "not_alive"}
        result = self.transaction(ptpip.PTP_CLOSE_SESSION, (), prefix="close_session")
        try:
            self.sock.close()
        finally:
            self.alive = False
        return result

    def _recv(self) -> tuple[bytes, str]:
        try:
            packet = ptpip.recv_packet(self.sock)
        except socket.timeout:
            self.alive = False
            return b"", "timeout"
        except (OSError, RuntimeError) as exc:
            self.alive = False
            return b"", repr(exc)
        if not packet:
            self.alive = False
            return b"", "socket_closed"
        return packet, ""

    def transaction(
        self,
        code: int,
        params: tuple[int, ...] = (),
        *,
        data: bytes | None = None,
        prefix: str | None = None,
    ) -> dict[str, Any]:
        tid = self.tid
        self.tid += 1
        prefix = prefix or f"op_{code:04x}_{tid:08x}"
        command = ptpip.build_ptp_command(code, tid, *params)
        result: dict[str, Any] = {
            "code": f"0x{code:04x}",
            "transaction_id": tid,
            "params": [f"0x{param:08x}" for param in params],
            "prefix": prefix,
        }
        (self.out_dir / f"{prefix}_request.bin").write_bytes(command)
        try:
            self.sock.sendall(command)
            if data is not None:
                container = ptpip.build_ptp_data_container(code, tid, data)
                (self.out_dir / f"{prefix}_data_request.bin").write_bytes(container)
                self.sock.sendall(container)
        except OSError as exc:
            self.alive = False
            result["send_error"] = repr(exc)
            self.steps.append(result)
            return result
        first, err = self._recv()
        if err:
            result["recv_error"] = err
            self.steps.append(result)
            return result
        header = ptpip.ptp_container_header(first)
        response = first
        if header.get("container_type") == ptpip.PTP_CONTAINER_DATA:
            payload = ptpip.ptp_data_payload(first)
            (self.out_dir / f"{prefix}_data_response.bin").write_bytes(first)
            result["payload_hex"] = payload.hex()
            result["payload_bytes"] = len(payload)
            response, err = self._recv()
            if err:
                result["recv_error"] = err
                self.steps.append(result)
                return result
        (self.out_dir / f"{prefix}_response.bin").write_bytes(response)
        response_header = ptpip.ptp_container_header(response)
        result["response_code"] = f"0x{int(response_header.get('code', 0)):04x}"
        result["response_ok"] = response_header.get("code") == ptpip.PTP_RESPONSE_OK
        self.timeline.event("ptpip_transaction", **result)
        self.steps.append(result)
        return result

    def get_prop(self, prop: int, prefix: str | None = None) -> dict[str, Any]:
        result = self.transaction(ptpip.PTP_GET_DEVICE_PROP_VALUE, (prop,), prefix=prefix or f"get_{prop:04x}")
        payload = bytes.fromhex(result.get("payload_hex", ""))
        if payload:
            result["value"] = {"raw_hex": payload.hex(), "uint_le": int.from_bytes(payload, "little")}
        return result

    def set_prop(self, prop: int, value: bytes, prefix: str | None = None) -> dict[str, Any]:
        return self.transaction(ptpip.PTP_SET_DEVICE_PROP_VALUE, (prop,), data=value, prefix=prefix or f"set_{prop:04x}")


def ptp_liveview_handshake(ptp_session: PtpControlSession) -> dict[str, Any]:
    out: dict[str, Any] = {}
    out["df00_set6"] = ptp_session.set_prop(0xDF00, struct.pack("<H", 6), "lv_df00_set6")
    out["df01_set22"] = ptp_session.set_prop(0xDF01, struct.pack("<H", 22), "lv_df01_set22")
    version = ptp_session.get_prop(0xDF2A, "lv_df2a_get")
    out["df2a_get"] = version
    payload = bytes.fromhex(version.get("payload_hex", ""))
    if payload:
        target = min(int.from_bytes(payload, "little"), 4)
        out["df2a_set"] = ptp_session.set_prop(0xDF2A, target.to_bytes(len(payload), "little"), "lv_df2a_set")
    out["initiate_open_capture"] = ptp_session.transaction(0x101C, (0, 0), prefix="initiate_open_capture")
    return out


def poll_props(ptp_session: PtpControlSession, label: str) -> dict[str, Any]:
    return {f"0x{prop:04x}": ptp_session.get_prop(prop, f"{label}_get_{prop:04x}") for prop in POLL_PROPS if ptp_session.alive}


def join_camera_ap(config: "LocalConfig", ssid: str, phase_dir: Path, timeline: Timeline) -> dict[str, Any]:
    if config.wifi_join_method == "nmcli" or (
        config.wifi_join_method == "auto" and shutil.which("nmcli")
    ):
        return join_camera_ap_nmcli(config, ssid, phase_dir, timeline)
    if config.wifi_join_method == "wpa_cli":
        return join_camera_ap_wpa_cli(config, ssid, phase_dir, timeline)
    return join_camera_ap_iw_static(config, ssid, phase_dir, timeline)


def join_camera_ap_nmcli(config: "LocalConfig", ssid: str, phase_dir: Path, timeline: Timeline) -> dict[str, Any]:
    result: dict[str, Any] = {"ssid": ssid, "wifi_iface": config.wifi_iface, "con_name": config.con_name, "steps": []}
    commands = [
        ["sudo", "-n", "nmcli", "con", "delete", config.con_name],
        [
            "sudo",
            "-n",
            "nmcli",
            "con",
            "add",
            "type",
            "wifi",
            "con-name",
            config.con_name,
            "ifname",
            config.wifi_iface,
            "ssid",
            ssid,
            "ipv4.never-default",
            "yes",
            "ipv6.never-default",
            "yes",
            "ipv4.route-metric",
            "9999",
        ],
        ["sudo", "-n", "nmcli", "con", "up", config.con_name],
    ]
    for index, command in enumerate(commands):
        step = run_cmd(command, timeout=20)
        if index == 0 and step["returncode"] != 0:
            step["ignored"] = True
        elif step["returncode"] != 0:
            result["error"] = f"Wi-Fi join failed at step {index}: {step.get('stderr') or step.get('stdout')}"
            result["steps"].append(step)
            (phase_dir / "wifi_join.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
            timeline.event("wifi_join_failed", step=index, result=step)
            return result
        result["steps"].append(step)
    time.sleep(2.0)
    route = run_cmd(["ip", "route", "get", config.camera_host], timeout=5)
    result["route"] = route
    result["ok"] = route["returncode"] == 0 and config.wifi_iface in route.get("stdout", "")
    (phase_dir / "wifi_join.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    timeline.event("wifi_join_complete", ok=result["ok"], route=route.get("stdout", ""))
    return result


def _wpa_cli(config: "LocalConfig", *args: str) -> list[str]:
    return ["sudo", "-n", "wpa_cli", "-i", config.wifi_iface, *args]


def join_camera_ap_wpa_cli(config: "LocalConfig", ssid: str, phase_dir: Path, timeline: Timeline) -> dict[str, Any]:
    result: dict[str, Any] = {
        "ssid": ssid,
        "wifi_iface": config.wifi_iface,
        "method": "wpa_cli",
        "steps": [],
    }
    commands = [
        ["sudo", "-n", "ip", "link", "set", "dev", config.wifi_iface, "up"],
        _wpa_cli(config, "disconnect"),
        _wpa_cli(config, "remove_network", "all"),
    ]
    for command in commands:
        step = run_cmd(command, timeout=10)
        result["steps"].append(step)
    add = run_cmd(_wpa_cli(config, "add_network"), timeout=10)
    result["steps"].append(add)
    if add["returncode"] != 0 or not add.get("stdout", "").strip().isdigit():
        result["error"] = f"wpa_cli add_network failed: {add.get('stderr') or add.get('stdout')}"
        (phase_dir / "wifi_join.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
        timeline.event("wifi_join_failed", result=add)
        return result
    net_id = add["stdout"].strip()
    quoted_ssid = json.dumps(ssid)
    for command in (
        _wpa_cli(config, "set_network", net_id, "ssid", quoted_ssid),
        _wpa_cli(config, "set_network", net_id, "key_mgmt", "NONE"),
        _wpa_cli(config, "enable_network", net_id),
        _wpa_cli(config, "select_network", net_id),
        _wpa_cli(config, "reassociate"),
    ):
        step = run_cmd(command, timeout=10)
        result["steps"].append(step)
        if step["returncode"] != 0:
            result["error"] = f"wpa_cli join failed: {step.get('stderr') or step.get('stdout')}"
            (phase_dir / "wifi_join.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
            timeline.event("wifi_join_failed", result=step)
            return result
    time.sleep(5.0)
    renew_commands = []
    if shutil.which("networkctl"):
        renew_commands.append(["sudo", "-n", "networkctl", "renew", config.wifi_iface])
    if shutil.which("dhclient"):
        renew_commands.append(["sudo", "-n", "dhclient", "-v", config.wifi_iface])
    for command in renew_commands:
        result["steps"].append(run_cmd(command, timeout=20))
    time.sleep(2.0)
    status = run_cmd(_wpa_cli(config, "status"), timeout=10)
    route = run_cmd(["ip", "route", "get", config.camera_host], timeout=5)
    result["wpa_status"] = status
    result["route"] = route
    result["ok"] = route["returncode"] == 0 and config.wifi_iface in route.get("stdout", "")
    (phase_dir / "wifi_join.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    timeline.event("wifi_join_complete", ok=result["ok"], route=route.get("stdout", ""))
    return result


def join_camera_ap_iw_static(config: "LocalConfig", ssid: str, phase_dir: Path, timeline: Timeline) -> dict[str, Any]:
    result: dict[str, Any] = {
        "ssid": ssid,
        "wifi_iface": config.wifi_iface,
        "method": "iw_static",
        "static_ip_cidr": config.static_ip_cidr,
        "steps": [],
    }
    commands = [
        ["sudo", "-n", "ip", "link", "set", "dev", config.wifi_iface, "up"],
        ["sudo", "-n", "iw", "dev", config.wifi_iface, "disconnect"],
        ["sudo", "-n", "ip", "addr", "flush", "dev", config.wifi_iface],
        ["sudo", "-n", "iw", "dev", config.wifi_iface, "connect", "-w", ssid],
        ["sudo", "-n", "ip", "addr", "add", config.static_ip_cidr, "dev", config.wifi_iface],
    ]
    for index, command in enumerate(commands):
        step = run_cmd(command, timeout=30)
        if index == 1 and step["returncode"] != 0:
            step["ignored"] = True
        elif step["returncode"] != 0:
            result["error"] = f"iw_static join failed at step {index}: {step.get('stderr') or step.get('stdout')}"
            result["steps"].append(step)
            (phase_dir / "wifi_join.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
            timeline.event("wifi_join_failed", result=step)
            return result
        result["steps"].append(step)
    time.sleep(1.0)
    link = run_cmd(["iw", "dev", config.wifi_iface, "link"], timeout=5)
    route = run_cmd(["ip", "route", "get", config.camera_host], timeout=5)
    result["iw_link"] = link
    result["route"] = route
    result["ok"] = (
        link["returncode"] == 0
        and "Not connected" not in link.get("stdout", "")
        and route["returncode"] == 0
        and config.wifi_iface in route.get("stdout", "")
    )
    (phase_dir / "wifi_join.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    timeline.event("wifi_join_complete", ok=result["ok"], route=route.get("stdout", ""))
    return result


def cleanup_wifi(config: "LocalConfig") -> dict[str, Any]:
    if config.wifi_join_method == "nmcli" or (
        config.wifi_join_method == "auto" and shutil.which("nmcli")
    ):
        return run_cmd(["sudo", "-n", "nmcli", "con", "delete", config.con_name], timeout=10)
    if config.wifi_join_method == "wpa_cli":
        return run_cmd(_wpa_cli(config, "disconnect"), timeout=10)
    disconnect = run_cmd(["sudo", "-n", "iw", "dev", config.wifi_iface, "disconnect"], timeout=10)
    flush = run_cmd(["sudo", "-n", "ip", "addr", "flush", "dev", config.wifi_iface], timeout=10)
    return {"disconnect": disconnect, "flush": flush}


async def subscribe_all_notifications(conn: BleConnection, timeline: Timeline) -> dict[str, Any]:
    services = await conn.services_json()
    enabled: list[str] = []
    failed: list[dict[str, str]] = []

    def callback(uuid: str, data: bytes) -> None:
        timeline.event("ble_notify", uuid=uuid, hex=data.hex(), length=len(data))

    for service in services:
        for char in service.get("characteristics", []):
            uuid = str(char.get("uuid", "")).lower()
            props = set(char.get("properties", []))
            if not uuid or not (props & {"notify", "indicate"}):
                continue
            try:
                await conn.start_notify(uuid, callback)
                enabled.append(uuid)
            except Exception as exc:  # noqa: BLE001 - artifact should record exact failure
                failed.append({"uuid": uuid, "error": repr(exc)})
    return {"enabled": enabled, "failed": failed, "services": services}


async def write_remote_shutter(conn: BleConnection, timeline: Timeline, hold_s: float) -> list[dict[str, Any]]:
    if not await conn.has_characteristic(uuids.CHAR_SHOOTING_REQUEST):
        result = [{"error": "shooting request characteristic missing", "uuid": uuids.CHAR_SHOOTING_REQUEST}]
        timeline.event("ble_remote_shutter_missing", uuid=uuids.CHAR_SHOOTING_REQUEST)
        return result
    out: list[dict[str, Any]] = []
    for label, payload in REMOTE_SHUTTER_SEQUENCE:
        started = time.monotonic()
        record = {"label": label, "uuid": uuids.CHAR_SHOOTING_REQUEST, "hex": payload.hex()}
        try:
            await conn.write(uuids.CHAR_SHOOTING_REQUEST, payload, response=True)
            record["ok"] = True
        except Exception as exc:  # noqa: BLE001 - artifact should capture exact write failure
            record["ok"] = False
            record["error"] = repr(exc)
        record["elapsed_s"] = round(time.monotonic() - started, 3)
        out.append(record)
        timeline.event("ble_remote_shutter_write", **record)
        await asyncio.sleep(hold_s if label == "S2" else 0.15)
    return out


def start_btmon(run_dir: Path) -> CaptureProcess:
    proc = CaptureProcess(["sudo", "-n", "btmon", "-w", str(run_dir / "hci.btsnoop")], run_dir / "btmon.stdout")
    proc.start()
    time.sleep(1.0)
    return proc


def start_tcpdump(config: "LocalConfig", phase_dir: Path) -> CaptureProcess:
    argv = [
        "sudo",
        "-n",
        "tcpdump",
        "-i",
        config.wifi_iface,
        "-w",
        str(phase_dir / "ptpip.pcap"),
        "host",
        config.camera_host,
        "and",
        "(",
        "tcp",
        "port",
        "55740",
        "or",
        "tcp",
        "port",
        "55741",
        "or",
        "tcp",
        "port",
        "55742",
        ")",
    ]
    proc = CaptureProcess(argv, phase_dir / "tcpdump.stdout")
    proc.start()
    time.sleep(1.0)
    return proc


async def run_ble_phase(config: "LocalConfig", run_dir: Path, phase: str, btmon: CaptureProcess) -> dict[str, Any]:
    phase_dir = run_dir / f"phase-{phase}"
    phase_dir.mkdir(parents=True, exist_ok=True)
    timeline = Timeline(phase_dir / "timeline.jsonl")
    timeline.event("phase_start", phase=phase)
    summary: dict[str, Any] = {"phase": phase, "phase_dir": str(phase_dir), "started_at": utc_iso()}
    ble_session = Session(root=phase_dir, label=f"ble_{phase}")
    backend = BleakBackend(ble_session)
    camera = FujiCamera(backend, ble_session)
    tcpdump: CaptureProcess | None = None
    event_reader: EventRecorder | None = None
    liveview_reader: LiveViewRecorder | None = None
    ptp_session: PtpControlSession | None = None
    try:
        device = await camera._target(name=config.camera_name, timeout=config.scan_timeout, address=config.ble_address)
        summary["device"] = device.to_log_dict()
        timeline.event("ble_target", **summary["device"])
        async with backend.connect(device) as conn:
            if config.register:
                advertisement_pairing_identity = camera._pairing_identity_from_device(device)
                pairing_identity = config.pairing_key or advertisement_pairing_identity
                if config.pairing_key is not None:
                    summary["pairing_identity"] = {"source": "cli", "hex": config.pairing_key.hex()}
                elif advertisement_pairing_identity is not None:
                    summary["pairing_identity"] = {
                        "source": "advertisement",
                        "hex": advertisement_pairing_identity.hex(),
                    }
                summary["registration"] = await camera._register_connected(
                    conn,
                    device_name=config.friendly_name,
                    ack_registration=True,
                    pairing_identity=pairing_identity,
                )
            await camera._prepare_connection(conn)
            await camera._prepare_wifi_handoff(conn)
            summary["notifications"] = await subscribe_all_notifications(conn, timeline)
            summary["wifi_info"] = await camera._read_wifi_info_connected(conn, read_passphrase=False)
            launch_state = await camera._launch_ap(conn, "take", timeout=config.ap_timeout)
            summary["ap_state"] = launch_state
            ssid = str(summary["wifi_info"].get("ssid") or config.ap_ssid)
            join = join_camera_ap(config, ssid, phase_dir, timeline)
            summary["wifi_join"] = join
            if not join.get("ok"):
                summary["verdict"] = "blocked: wifi join failed"
                return summary
            tcpdump = start_tcpdump(config, phase_dir)
            summary["tcpdump"] = {"argv": tcpdump.argv, "pid": tcpdump.process.pid if tcpdump.process else None}

            if phase == "b":
                summary["ble_remote_shutter_before_ptp"] = await write_remote_shutter(conn, timeline, config.shutter_hold_s)

            ptp_dir = phase_dir / "ptpip"
            ptp_session = PtpControlSession.open(
                config.camera_host,
                config.command_port,
                config.guid,
                config.friendly_name,
                config.ptp_timeout,
                ptp_dir,
                timeline,
            )
            summary["live_view_handshake"] = ptp_liveview_handshake(ptp_session)
            event_reader = EventRecorder(config.camera_host, config.event_port, phase_dir, timeline, config.ptp_timeout)
            liveview_reader = LiveViewRecorder(
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

            if phase == "a":
                summary["props_before_ble_shutter"] = poll_props(ptp_session, "a_before")
                summary["ble_remote_shutter"] = await write_remote_shutter(conn, timeline, config.shutter_hold_s)
                await asyncio.sleep(config.observe_s)
                summary["props_after_ble_shutter"] = poll_props(ptp_session, "a_after")
            elif phase == "b":
                summary["props_after_ptp_bringup"] = poll_props(ptp_session, "b_after")
                await asyncio.sleep(config.observe_s)
            elif phase == "c":
                summary["props_before_ptp_capture"] = poll_props(ptp_session, "c_before")
                summary["ptp_initiate_capture"] = ptp_session.transaction(0x100E, (), prefix="ptp_initiate_capture")
                await asyncio.sleep(config.observe_s)
                summary["props_after_ptp_capture"] = poll_props(ptp_session, "c_after")
            summary["ptp_steps"] = ptp_session.steps
    except Exception as exc:  # noqa: BLE001 - live artifact should preserve exact failure
        summary["error"] = repr(exc)
        summary["verdict"] = f"blocked: {exc!r}"
        timeline.event("phase_error", phase=phase, error=repr(exc))
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
            summary["wifi_cleanup"] = cleanup_wifi(config)
        if "verdict" not in summary or not str(summary["verdict"]).startswith("blocked"):
            summary["verdict"] = phase_verdict(phase, summary)
        summary["btmon_pid"] = btmon.process.pid if btmon.process else None
        summary["finished_at"] = utc_iso()
        (phase_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
        timeline.event("phase_finish", phase=phase, verdict=summary.get("verdict", "unknown"))
    return summary


def phase_verdict(phase: str, summary: dict[str, Any]) -> str:
    if summary.get("error"):
        return "blocked"
    if phase == "a":
        writes = summary.get("ble_remote_shutter", [])
        events = summary.get("event_reader", {}).get("events", 0)
        if writes and all(record.get("ok") for record in writes) and events:
            return "ble_shutter_accepted_with_ptpip_events"
        if writes and all(record.get("ok") for record in writes):
            return "ble_shutter_write_accepted_no_55741_events_seen"
        return "ble_shutter_not_accepted"
    if phase == "b":
        if summary.get("live_view_handshake", {}).get("initiate_open_capture", {}).get("response_ok"):
            return "ptpip_liveview_bringup_succeeded_after_ble_remote_trigger"
        return "ptpip_liveview_bringup_failed_after_ble_remote_trigger"
    if phase == "c":
        capture = summary.get("ptp_initiate_capture", {})
        events = summary.get("event_reader", {}).get("events", 0)
        if capture.get("response_ok") and events:
            return "ptp_capture_accepted_with_events"
        if capture.get("response_ok"):
            return "ptp_capture_accepted_no_55741_events_seen"
        return "ptp_capture_not_accepted"
    return "unknown_phase"


def render_verdict(run_summary: dict[str, Any]) -> str:
    lines = [
        "# Issue #52 BLE/PTP-IP Concurrency Verdict",
        "",
        f"- Started: {run_summary.get('started_at', '')}",
        f"- Finished: {run_summary.get('finished_at', '')}",
        f"- Capture dir: `{run_summary.get('capture_dir', '')}`",
        f"- Rig: `{run_summary.get('rig', '')}`",
        "",
        "| Phase | Verdict | Notes |",
        "|---|---|---|",
    ]
    for phase in run_summary.get("phases", []):
        notes = []
        if phase.get("event_reader"):
            notes.append(f"55741 events={phase['event_reader'].get('events', 0)}")
        if phase.get("liveview_reader"):
            notes.append(f"55742 jpeg={phase['liveview_reader'].get('jpeg_frames', 0)}")
        if phase.get("error"):
            notes.append(str(phase["error"]))
        lines.append(f"| {phase.get('phase')} | {phase.get('verdict', 'unknown')} | {'; '.join(notes)} |")
    lines.append("")
    lines.append("Raw artifacts include `hci.btsnoop`, per-phase `ptpip.pcap`, decoded BLE JSONL, 55741 event JSONL, and per-phase summaries.")
    return "\n".join(lines) + "\n"


@dataclass
class LocalConfig:
    capture_root: Path
    phases: list[str]
    camera_name: str
    ble_address: str | None
    friendly_name: str
    guid: str
    camera_host: str
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
    observe_s: float
    settle_s: float
    shutter_hold_s: float
    save_frames: int
    register: bool
    cleanup_wifi: bool
    preflight_only: bool
    pairing_key: bytes | None = None
    stamp: str | None = None


def config_from_args(args: argparse.Namespace) -> LocalConfig:
    return LocalConfig(
        capture_root=args.capture_root,
        phases=args.phases,
        camera_name=args.camera_name,
        ble_address=args.ble_address or None,
        friendly_name=args.friendly_name,
        guid=args.guid,
        camera_host=args.camera_host,
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
        observe_s=args.observe_s,
        settle_s=args.settle_s,
        shutter_hold_s=args.shutter_hold_s,
        save_frames=args.save_frames,
        register=not args.no_register,
        cleanup_wifi=not args.keep_wifi,
        preflight_only=args.preflight_only,
        pairing_key=bytes.fromhex(args.pairing_key) if args.pairing_key else None,
        stamp=args.stamp,
    )


async def run_local_async(config: LocalConfig) -> dict[str, Any]:
    run_dir = capture_dir_for(config.capture_root, config.stamp)
    run_dir.mkdir(parents=True, exist_ok=True)
    summary: dict[str, Any] = {
        "started_at": utc_iso(),
        "capture_dir": str(run_dir),
        "rig": socket.gethostname(),
        "phases_requested": config.phases,
    }
    checks = preflight(config)
    summary["preflight"] = checks
    (run_dir / "preflight.json").write_text(json.dumps(checks, indent=2, sort_keys=True) + "\n")
    if not checks["ok"]:
        summary["finished_at"] = utc_iso()
        summary["verdict"] = "blocked: preflight failed"
        (run_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
        (run_dir / "VERDICT.md").write_text(render_verdict({**summary, "phases": []}), encoding="utf-8")
        return summary
    if config.preflight_only:
        summary["finished_at"] = utc_iso()
        summary["verdict"] = "preflight ok"
        (run_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
        (run_dir / "VERDICT.md").write_text(render_verdict({**summary, "phases": []}), encoding="utf-8")
        return summary
    btmon = start_btmon(run_dir)
    summary["btmon"] = {"argv": btmon.argv, "pid": btmon.process.pid if btmon.process else None}
    phases: list[dict[str, Any]] = []
    try:
        for phase in config.phases:
            phases.append(await run_ble_phase(config, run_dir, phase, btmon))
    finally:
        summary["btmon_stop"] = btmon.stop()
    summary["phases"] = phases
    summary["finished_at"] = utc_iso()
    summary["verdict"] = "complete" if all(not str(p.get("verdict", "")).startswith("blocked") for p in phases) else "blocked"
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, default=json_default) + "\n")
    (run_dir / "VERDICT.md").write_text(render_verdict(summary), encoding="utf-8")
    return summary


def run_local(args: argparse.Namespace) -> int:
    summary = asyncio.run(run_local_async(config_from_args(args)))
    print("RUN_RESULT_JSON " + json.dumps(summary, sort_keys=True, default=json_default))
    return 0 if not str(summary.get("verdict", "")).startswith("blocked") else 1


def remote_local_args(args: argparse.Namespace) -> list[str]:
    argv = [
        "scripts/probe_ble_ptpip_concurrency.py",
        "--run-local",
        "--capture-root",
        "captures",
        "--phases",
        ",".join(args.phases),
        "--camera-name",
        args.camera_name,
        "--friendly-name",
        args.friendly_name,
        "--guid",
        args.guid,
        "--camera-host",
        args.camera_host,
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
        "--observe-s",
        str(args.observe_s),
        "--settle-s",
        str(args.settle_s),
        "--shutter-hold-s",
        str(args.shutter_hold_s),
        "--save-frames",
        str(args.save_frames),
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
    if args.stamp:
        argv.extend(["--stamp", args.stamp])
    return argv


def parse_run_result(stdout: str) -> dict[str, Any]:
    for line in reversed(stdout.splitlines()):
        if line.startswith("RUN_RESULT_JSON "):
            return json.loads(line.removeprefix("RUN_RESULT_JSON "))
    raise RuntimeError("remote output did not contain RUN_RESULT_JSON")


def run_remote(args: argparse.Namespace) -> int:
    root = protocol_mapper_root()
    args.capture_root.mkdir(parents=True, exist_ok=True)
    remote_workdir = args.remote_workdir.rstrip("/")
    ssh_prefix = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", args.remote_host]
    ssh_check = run_cmd(ssh_prefix + ["hostname"], timeout=12)
    if ssh_check["returncode"] != 0:
        print(json.dumps({"error": "ssh preflight failed", "result": ssh_check}, indent=2, sort_keys=True))
        return 1
    mkdir = run_cmd(ssh_prefix + [f"mkdir -p {shlex.quote(remote_workdir)}"], timeout=12)
    if mkdir["returncode"] != 0:
        print(json.dumps({"error": "remote mkdir failed", "result": mkdir}, indent=2, sort_keys=True))
        return 1
    rsync_push = run_cmd(
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
    remote_run = run_cmd(ssh_prefix + [command], timeout=args.remote_timeout)
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
        rsync_pull = run_cmd(
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
    parser = argparse.ArgumentParser(prog="probe-ble-ptpip-concurrency")
    parser.add_argument("--remote-host", default=DEFAULT_REMOTE_HOST)
    parser.add_argument("--remote-workdir", default=DEFAULT_REMOTE_WORKDIR)
    parser.add_argument("--remote-python", default=DEFAULT_REMOTE_PYTHON)
    parser.add_argument("--remote-timeout", type=float, default=1800)
    parser.add_argument("--run-local", action="store_true", help="run on the host with attached BLE/Wi-Fi hardware")
    parser.add_argument("--capture-root", type=Path, default=default_capture_root())
    parser.add_argument("--stamp", default="")
    parser.add_argument("--phases", type=parse_phases, default=list(PHASES))
    parser.add_argument("--camera-name", default="GFX100 II")
    parser.add_argument("--ble-address", default="")
    parser.add_argument(
        "--pairing-key",
        type=parse_pairing_key,
        default="",
        help="hex payload to write to the Fuji PAIRING_KEY characteristic when advertisements omit it",
    )
    parser.add_argument("--friendly-name", default=DEFAULT_FRIENDLY_NAME)
    parser.add_argument("--guid", default=DEFAULT_GUID)
    parser.add_argument("--camera-host", default=DEFAULT_CAMERA_HOST)
    parser.add_argument("--command-port", type=int, default=55740)
    parser.add_argument("--event-port", type=int, default=55741)
    parser.add_argument("--liveview-port", type=int, default=55742)
    parser.add_argument("--wifi-iface", default=DEFAULT_WIFI_IFACE)
    parser.add_argument("--wifi-join-method", choices=["auto", "nmcli", "wpa_cli", "iw_static"], default="auto")
    parser.add_argument("--static-ip-cidr", default=DEFAULT_STATIC_IP_CIDR)
    parser.add_argument("--ap-ssid", default="FUJIFILM-GFX100II-0C3E")
    parser.add_argument("--con-name", default=DEFAULT_CON_NAME)
    parser.add_argument("--scan-timeout", type=float, default=40.0)
    parser.add_argument("--ap-timeout", type=float, default=25.0)
    parser.add_argument("--ptp-timeout", type=float, default=6.0)
    parser.add_argument("--observe-s", type=float, default=4.0)
    parser.add_argument("--settle-s", type=float, default=1.0)
    parser.add_argument("--shutter-hold-s", type=float, default=0.5)
    parser.add_argument("--save-frames", type=int, default=3)
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
