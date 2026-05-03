from __future__ import annotations

from pathlib import Path
import socket
import struct

import pytest

from rce.tools.fuji_ble_gps import ptpip


class FakeSocket:
    def __init__(self, responses=None) -> None:
        self.responses = list(responses or [])
        self.sent: list[bytes] = []
        self.timeout = None

    def __enter__(self):
        return self

    def __exit__(self, _exc_type, _exc, _traceback) -> None:
        return None

    def settimeout(self, value: float) -> None:
        self.timeout = value

    def sendall(self, data: bytes) -> None:
        self.sent.append(data)

    def recv(self, size: int) -> bytes:
        if not self.responses:
            return b""
        current = self.responses[0]
        if isinstance(current, BaseException):
            self.responses.pop(0)
            raise current
        if current == b"":
            self.responses.pop(0)
            return b""
        chunk = current[:size]
        rest = current[size:]
        if rest:
            self.responses[0] = rest
        else:
            self.responses.pop(0)
        return chunk


def packet(packet_type: int = 2, payload: bytes = b"") -> bytes:
    return struct.pack("<II", 8 + len(payload), packet_type) + payload


def ptp_container(container_type: int = 3, code: int = 0x2001, transaction: int = 1) -> bytes:
    return struct.pack("<IHHI", 12, container_type, code, transaction)


def test_build_init_command_request_deterministic(monkeypatch) -> None:
    guid = bytes(range(16))
    request = ptpip.build_init_command_request("mbp-7274", guid=guid)

    assert len(request) == 82
    assert struct.unpack("<II", request[:8]) == (82, 1)
    assert request[8:24] == guid
    assert request[24:28] == b"\x00\x00\x00\x00"
    assert request[28:44].startswith("mbp-7274".encode("utf-16le"))
    assert request[-28:] == ptpip.TAIL_PROFILES["liveview"]

    with pytest.raises(ValueError, match="unknown tail profile"):
        ptpip.build_init_command_request("mbp", "missing", guid=guid)
    with pytest.raises(ValueError, match="initiator GUID must be 16 bytes"):
        ptpip.build_init_command_request("mbp", guid=b"short")

    monkeypatch.setitem(ptpip.TAIL_PROFILES, "bad", b"\x00")
    with pytest.raises(ValueError, match="expected 28"):
        ptpip.build_init_command_request("mbp", "bad", guid=guid)


def test_headers_and_packet_reading() -> None:
    assert ptpip.packet_header(b"\x00") == {"length": 1, "packet_type": "short"}
    assert ptpip.packet_header(packet(7)) == {"length": 8, "packet_type": 7}
    assert ptpip.ptp_container_header(b"\x00") == {"length": 1, "container_type": "short"}
    assert ptpip.ptp_container_header(ptp_container(code=0x201E)) == {
        "length": 12,
        "container_type": 3,
        "code": 0x201E,
        "transaction_id": 1,
    }

    assert ptpip.read_exact(FakeSocket([b"ab", b"cd"]), 4) == b"abcd"
    assert ptpip.read_exact(FakeSocket([b"ab", b""]), 4) == b"ab"
    assert ptpip.recv_packet(FakeSocket([packet(2, b"abc")])) == packet(2, b"abc")
    assert ptpip.recv_packet(FakeSocket([b""])) == b""
    with pytest.raises(RuntimeError, match="short packet length header"):
        ptpip.recv_packet(FakeSocket([b"\x01\x00"]))
    with pytest.raises(RuntimeError, match="invalid packet length"):
        ptpip.recv_packet(FakeSocket([struct.pack("<I", 4)]))
    with pytest.raises(RuntimeError, match="invalid packet length"):
        ptpip.recv_packet(FakeSocket([struct.pack("<I", ptpip.MAX_PACKET_LENGTH + 1)]))


def test_ptp_request_builders_and_property_parser() -> None:
    assert ptpip.build_open_session().hex() == "10000000010002100100000001000000"
    assert ptpip.parse_u16_or_hex("0xd212") == 0xD212
    assert ptpip.parse_u16_or_hex("123") == 123
    assert ptpip.build_get_device_prop_value(0xD212).hex() == "10000000010015100200000012d20000"
    with pytest.raises(ValueError, match="out of uint16 range"):
        ptpip.parse_u16_or_hex("0x10000")


def test_probe_connect_only_writes_summary(tmp_path) -> None:
    fake = FakeSocket()
    ticks = iter([10.0, 10.1234])

    summary = ptpip.probe_ptpip(
        ptpip.ProbeConfig(
            session_dir=tmp_path,
            host="192.168.0.1",
            friendly_name="mbp-7274",
            connect_only=True,
        ),
        connector=lambda _target, _timeout: fake,
        clock=lambda: next(ticks),
    )

    assert summary["tcp_connect"] == "present"
    assert summary["connect_elapsed_ms"] == 123
    assert summary["init_sent"] is False
    assert ptpip.exit_code_for_summary(summary, ptpip.ProbeConfig(tmp_path, connect_only=True)) == 0
    assert (tmp_path / "summary.json").exists()
    assert not fake.sent


def test_probe_full_success_with_captured_init_payload(tmp_path) -> None:
    init_payload = tmp_path / "captured_init.bin"
    init_payload.write_bytes(b"captured")
    fake = FakeSocket(
        [
            packet(2),
            ptp_container(code=0x201E),
            ptp_container(container_type=2, code=0xD212, transaction=2),
            ptp_container(code=0x2001, transaction=2),
        ]
    )
    config = ptpip.ProbeConfig(
        session_dir=tmp_path,
        friendly_name="mbp-7274",
        init_payload=init_payload,
        open_session=True,
        get_prop="0xd212",
    )

    summary = ptpip.probe_ptpip(config, connector=lambda _target, _timeout: fake, clock=lambda: 0.0)

    assert summary["response_present"] is True
    assert summary["response_header"] == {"length": 8, "packet_type": 2}
    assert summary["open_session_response_header"]["code"] == 0x201E
    assert summary["get_prop_data_header"]["code"] == 0xD212
    assert summary["get_prop_response_header"]["code"] == 0x2001
    assert fake.sent[0] == b"captured"
    assert fake.sent[1] == ptpip.build_open_session()
    assert fake.sent[2] == ptpip.build_get_device_prop_value(0xD212)
    assert (tmp_path / "get_prop_response.bin").exists()
    assert ptpip.exit_code_for_summary(summary, config) == 0


def test_probe_init_only_and_open_without_get_prop(tmp_path) -> None:
    init_only = ptpip.probe_ptpip(
        ptpip.ProbeConfig(session_dir=tmp_path / "init-only", friendly_name="mbp"),
        connector=lambda _target, _timeout: FakeSocket([packet(2)]),
        clock=lambda: 0.0,
    )
    assert init_only["response_present"] is True
    assert init_only["open_session_sent"] is False

    open_only = ptpip.probe_ptpip(
        ptpip.ProbeConfig(session_dir=tmp_path / "open-only", friendly_name="mbp", open_session=True),
        connector=lambda _target, _timeout: FakeSocket([packet(2), ptp_container()]),
        clock=lambda: 0.0,
    )
    assert open_only["open_session_response_present"] is True
    assert open_only["get_prop_sent"] is False


def test_probe_timeout_and_missing_response_branches(tmp_path) -> None:
    init_timeout = ptpip.probe_ptpip(
        ptpip.ProbeConfig(session_dir=tmp_path / "init-timeout", friendly_name="mbp"),
        connector=lambda _target, _timeout: FakeSocket([socket.timeout("slow")]),
        clock=lambda: 0.0,
    )
    assert init_timeout["response_error"] == "timeout"
    assert ptpip.exit_code_for_summary(
        init_timeout,
        ptpip.ProbeConfig(session_dir=tmp_path / "init-timeout"),
    ) == 2

    open_timeout_config = ptpip.ProbeConfig(
        session_dir=tmp_path / "open-timeout",
        friendly_name="mbp",
        open_session=True,
    )
    open_timeout = ptpip.probe_ptpip(
        open_timeout_config,
        connector=lambda _target, _timeout: FakeSocket([packet(2), socket.timeout("slow")]),
        clock=lambda: 0.0,
    )
    assert open_timeout["open_session_response_error"] == "timeout"
    assert ptpip.exit_code_for_summary(open_timeout, open_timeout_config) == 4

    get_timeout_config = ptpip.ProbeConfig(
        session_dir=tmp_path / "get-timeout",
        friendly_name="mbp",
        open_session=True,
        get_prop="0xd212",
    )
    get_timeout = ptpip.probe_ptpip(
        get_timeout_config,
        connector=lambda _target, _timeout: FakeSocket([packet(2), ptp_container(), b"", socket.timeout("slow")]),
        clock=lambda: 0.0,
    )
    assert get_timeout["get_prop_response_error"] == "timeout"
    assert ptpip.exit_code_for_summary(get_timeout, get_timeout_config) == 5


def test_probe_socket_error_and_default_display_name(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(ptpip, "default_device_name", lambda: "host-default")

    def fail_connect(_target, _timeout):
        raise OSError("no route")

    config = ptpip.ProbeConfig(session_dir=tmp_path, friendly_name="")
    summary = ptpip.probe_ptpip(config, connector=fail_connect, clock=lambda: 0.0)

    assert summary["friendly_name"] == "host-default"
    assert "no route" in summary["error"]
    assert ptpip.exit_code_for_summary(summary, config) == 1


def test_cli_main_builds_config_and_returns_probe_exit(monkeypatch, tmp_path, capsys) -> None:
    calls = []

    def fake_probe(config):
        calls.append(config)
        return {
            "tcp_connect": "present",
            "response_present": True,
            "open_session_response_present": True,
            "get_prop_response_present": True,
        }

    monkeypatch.setattr(ptpip, "probe_ptpip", fake_probe)

    rc = ptpip.main(
        [
            "probe",
            "--session-dir",
            str(tmp_path),
            "--host",
            "192.168.0.1",
            "--port",
            "55740",
            "--friendly-name",
            "mbp-7274",
            "--timeout",
            "12",
            "--tail-profile",
            "get",
            "--get-prop",
            "0xd212",
        ]
    )

    assert rc == 0
    assert calls[0].session_dir == tmp_path
    assert calls[0].open_session is True
    assert calls[0].tail_profile == "get"
    assert '"tcp_connect": "present"' in capsys.readouterr().out
