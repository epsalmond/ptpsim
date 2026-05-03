from __future__ import annotations

import json
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

    assert len(request) == ptpip.INIT_FIXED_LENGTH
    assert struct.unpack("<II", request[:8]) == (ptpip.INIT_FIXED_LENGTH, 1)
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


def test_decode_captured_init_command_requests() -> None:
    liveview = Path("rce/reference/ptp_decoded/liveview_payload_00000061.bin").read_bytes()
    get = Path("rce/reference/ptp_decoded/payload_00000059.bin").read_bytes()

    liveview_decoded = ptpip.decode_init_command_request(liveview)
    get_decoded = ptpip.decode_init_command_request(get)

    assert liveview_decoded["declared_length"] == 82
    assert liveview_decoded["packet_type_name"] == "InitCommandRequest"
    assert liveview_decoded["payload_length"] == 74
    assert liveview_decoded["initiator_guid_hex"] == "f2e4538fada5485d87b27f0bd3d5ded0"
    assert liveview_decoded["post_guid_unknown_u32"] == 0
    assert liveview_decoded["friendly_name"] == "Pixel-6-9405"
    assert liveview_decoded["friendly_name_terminator_unit"] == 12
    assert liveview_decoded["friendly_name_padding_hex"] == ""
    assert liveview_decoded["tail_profile"] == "liveview"
    assert liveview_decoded["tail_u16_le"] == [
        "0x008d",
        "0x002c",
        "0x0000",
        "0x0000",
        "0x0000",
        "0x0000",
        "0x0000",
        "0x00fa",
        "0x0005",
        "0x003d",
        "0x0000",
        "0x0000",
        "0x0000",
        "0x0000",
    ]
    assert get_decoded["friendly_name"] == "Pixel-6-9405"
    assert get_decoded["tail_profile"] == "get"


def test_decode_generated_init_command_request_short_name_padding() -> None:
    request = ptpip.build_init_command_request("mbp-7274", guid=bytes(range(16)))
    decoded = ptpip.decode_init_command_request(request)

    assert decoded["friendly_name"] == "mbp-7274"
    assert decoded["friendly_name_terminator_unit"] == 8
    assert decoded["friendly_name_padding_hex"] == "0000000000000000"
    assert decoded["tail_profile"] == "liveview"


def test_init_decoder_and_guid_parser_edge_cases() -> None:
    valid = ptpip.build_init_command_request("mbp", guid=bytes(range(16)))

    assert ptpip.tail_profile_name(b"not-a-known-tail") == "unknown"
    assert ptpip.parse_guid_hex("00:01:02:03:04:05:06:07:08:09:0a:0b:0c:0d:0e:0f") == bytes(range(16))
    assert ptpip.parse_guid_hex("00010203-04050607-08090a0b-0c0d0e0f") == bytes(range(16))

    with pytest.raises(ValueError, match="too short"):
        ptpip.decode_init_command_request(b"\x00")
    with pytest.raises(ValueError, match="declared length"):
        ptpip.decode_init_command_request(struct.pack("<II", 82, 1) + valid[8:-1])
    with pytest.raises(ValueError, match="packet type"):
        ptpip.decode_init_command_request(struct.pack("<II", 82, 2) + valid[8:])
    with pytest.raises(ValueError, match="Fuji Init_Command_Request shape"):
        ptpip.decode_init_command_request(struct.pack("<II", 84, 1) + valid[8:] + b"\x00\x00")
    with pytest.raises(ValueError, match="UTF-16LE field"):
        ptpip.decode_utf16le_nul_field(b"\x00")
    with pytest.raises(ValueError, match="tail must have even"):
        ptpip.tail_u16_le(b"\x00")
    with pytest.raises(ValueError, match="GUID must be 16 bytes"):
        ptpip.parse_guid_hex("not-hex")
    with pytest.raises(ValueError, match="got 1 bytes"):
        ptpip.parse_guid_hex("00")


def test_compare_init_command_requests_field_by_field() -> None:
    reference = Path("rce/reference/ptp_decoded/liveview_payload_00000061.bin").read_bytes()
    same = ptpip.compare_init_command_requests(reference, reference)
    candidate = ptpip.build_init_command_request(
        "mbp-7274",
        guid=bytes.fromhex("f2e4538fada5485d87b27f0bd3d5ded0"),
    )
    comparison = ptpip.compare_init_command_requests(reference, candidate)

    assert same["same"] is True
    assert comparison["same"] is False
    fields = {field["field"]: field for field in comparison["fields"]}
    assert fields["initiator_guid_hex"]["same"] is True
    assert fields["tail_hex"]["same"] is True
    assert fields["friendly_name"]["reference"] == "Pixel-6-9405"
    assert fields["friendly_name"]["candidate"] == "mbp-7274"
    assert fields["friendly_name_field_hex"]["same"] is False
    assert fields["friendly_name_padding_hex"]["candidate"] == "0000000000000000"


def test_inventory_init_command_requests(tmp_path, capsys) -> None:
    valid = tmp_path / "nested" / "init_command_request.bin"
    valid.parent.mkdir()
    valid.write_bytes(ptpip.build_init_command_request("Pixel-6-9405", "get", guid=bytes(range(16))))
    jsonl = tmp_path / "decoded.jsonl"
    jsonl.write_text(
        "\n".join(
            [
                "",
                "{not json",
                json.dumps({"code_name": "Other"}),
                json.dumps(
                    {
                        "code_name": "InitCommandRequest",
                        "container": "InitCommandRequest",
                        "data_preview": (
                            "000102030405060708090a0b0c0d0e0f00000000"
                            "50006900780065006c002d0036002d0039003400300035000000"
                            "92004700000000000000000000002f00"
                        ),
                        "guid": "000102030405060708090a0b0c0d0e0f",
                        "hint": "Pixel-6-9405",
                        "len": 82,
                        "source_payloads": "missing_payload.bin",
                    }
                ),
                json.dumps(
                    {
                        "code_name": "InitCommandRequest",
                        "data_preview": "00" * 50,
                        "guid": "feed",
                        "name": "BadPayload",
                        "source_payloads": "not_init.bin, nested/init_command_request.bin",
                    }
                ),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    (tmp_path / "not_init.bin").write_bytes(b"not an init packet")
    ignored = tmp_path / "ignored.txt"
    ignored.write_text("ignored", encoding="utf-8")

    records = ptpip.inventory_init_command_requests([tmp_path, ignored, tmp_path / "missing"])

    assert records[0] == {
        "source": str(jsonl) + ":4",
        "guid": "000102030405060708090a0b0c0d0e0f",
        "friendly_name": "Pixel-6-9405",
        "tail_profile": "get",
        "tail_hex": ptpip.TAIL_PROFILES["get"].hex(),
        "packet_length": 82,
        "post_guid_unknown_hex": "00000000",
    }
    assert records[1] == {
        "source": str(jsonl) + ":5",
        "guid": "feed",
        "friendly_name": "BadPayload",
        "tail_profile": "get",
        "tail_hex": ptpip.TAIL_PROFILES["get"].hex(),
        "packet_length": 0,
        "post_guid_unknown_hex": "00000000",
    }
    assert records[2] == {
        "source": str(valid),
        "guid": "000102030405060708090a0b0c0d0e0f",
        "friendly_name": "Pixel-6-9405",
        "tail_profile": "get",
        "tail_hex": ptpip.TAIL_PROFILES["get"].hex(),
        "packet_length": 82,
        "post_guid_unknown_hex": "00000000",
    }
    ptpip.print_init_inventory(records)
    out = capsys.readouterr().out
    assert f"source={valid}" in out
    assert "friendly_name=Pixel-6-9405" in out
    assert "tail_profile=get" in out
    assert ptpip.tail_profile_from_preview("not hex") == ("unknown", "")
    assert ptpip.tail_profile_from_preview(("00" * 46) + "1234") == ("unknown", "1234")


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
    assert ptpip.build_set_device_prop_value(0xDF01, 2).hex() == "10000000010016100200000001df0000"
    assert ptpip.build_ptp_data_container(0x1016, 2, bytes.fromhex("1400")).hex() == "0e00000002001610020000001400"
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


def test_probe_app_sdcard_browse_sequence(tmp_path) -> None:
    fake = FakeSocket(
        [
            packet(2),
            ptp_container(code=0x2001, transaction=1),
            ptp_container(container_type=2, code=0x1015, transaction=2),
            ptp_container(code=0x2001, transaction=2),
            ptp_container(code=0x2001, transaction=3),
            ptp_container(container_type=2, code=0x1015, transaction=4),
            ptp_container(code=0x2001, transaction=4),
            ptp_container(code=0x2001, transaction=5),
            ptp_container(code=0x2001, transaction=6),
            ptp_container(code=0x2001, transaction=7),
            ptp_container(container_type=2, code=0x1015, transaction=8),
            ptp_container(code=0x2001, transaction=8),
        ]
    )
    config = ptpip.ProbeConfig(
        session_dir=tmp_path,
        friendly_name="mbp-7274",
        open_session=True,
        app_sequence="sdcard-browse-bootstrap",
    )

    summary = ptpip.probe_ptpip(config, connector=lambda _target, _timeout: fake, clock=lambda: 0.0)

    assert summary["app_sequence_sent"] is True
    assert summary["app_sequence_completed"] is True
    assert [step["prop"] for step in summary["app_sequence_steps"]] == [
        "0xd212",
        "0xdf01",
        "0xdf28",
        "0xdf28",
        "0xd226",
        "0xd227",
        "0xd244",
    ]
    assert fake.sent[2] == ptpip.build_get_device_prop_value(0xD212, 2)
    assert fake.sent[3] == ptpip.build_set_device_prop_value(0xDF01, 3)
    assert fake.sent[4] == ptpip.build_ptp_data_container(0x1016, 3, bytes.fromhex("1400"))
    assert fake.sent[12] == ptpip.build_get_device_prop_value(0xD244, 8)
    assert (tmp_path / "app_sequence_07_get_d244_response.bin").exists()
    assert ptpip.exit_code_for_summary(summary, config) == 0


def test_probe_init_only_and_open_without_get_prop(tmp_path) -> None:
    guid = "000102030405060708090a0b0c0d0e0f"
    init_socket = FakeSocket([packet(2)])
    init_only = ptpip.probe_ptpip(
        ptpip.ProbeConfig(session_dir=tmp_path / "init-only", friendly_name="mbp", guid=guid),
        connector=lambda _target, _timeout: init_socket,
        clock=lambda: 0.0,
    )
    assert init_only["response_present"] is True
    assert init_only["open_session_sent"] is False
    assert init_only["guid"] == guid
    assert init_socket.sent[0][8:24] == bytes(range(16))

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

    sequence_timeout_config = ptpip.ProbeConfig(
        session_dir=tmp_path / "sequence-timeout",
        friendly_name="mbp",
        open_session=True,
        app_sequence="sdcard-browse-bootstrap",
    )
    sequence_timeout = ptpip.probe_ptpip(
        sequence_timeout_config,
        connector=lambda _target, _timeout: FakeSocket([packet(2), ptp_container(), b"", b""]),
        clock=lambda: 0.0,
    )
    assert sequence_timeout["app_sequence_completed"] is False
    assert ptpip.exit_code_for_summary(sequence_timeout, sequence_timeout_config) == 6

    with pytest.raises(ValueError, match="unknown reference app sequence"):
        ptpip.probe_ptpip(
            ptpip.ProbeConfig(
                session_dir=tmp_path / "unknown-sequence",
                friendly_name="mbp",
                open_session=True,
                app_sequence="missing",
            ),
            connector=lambda _target, _timeout: FakeSocket([packet(2), ptp_container()]),
            clock=lambda: 0.0,
        )


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
            "app_sequence_completed": bool(config.app_sequence),
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
            "--guid",
            "000102030405060708090a0b0c0d0e0f",
            "--get-prop",
            "0xd212",
            "--app-sequence",
            "sdcard-browse-bootstrap",
        ]
    )

    assert rc == 0
    assert calls[0].session_dir == tmp_path
    assert calls[0].open_session is True
    assert calls[0].tail_profile == "get"
    assert calls[0].guid == "000102030405060708090a0b0c0d0e0f"
    assert calls[0].app_sequence == "sdcard-browse-bootstrap"
    assert '"tcp_connect": "present"' in capsys.readouterr().out


def test_cli_decode_and_compare_init(tmp_path, capsys) -> None:
    reference = Path("rce/reference/ptp_decoded/liveview_payload_00000061.bin")
    same_candidate = tmp_path / "same.bin"
    same_candidate.write_bytes(reference.read_bytes())

    assert ptpip.main(["decode-init", "--payload", str(reference)]) == 0
    decoded = json.loads(capsys.readouterr().out)
    assert decoded["friendly_name"] == "Pixel-6-9405"

    assert (
        ptpip.main(
            [
                "compare-init",
                "--reference",
                str(reference),
                "--candidate",
                str(same_candidate),
            ]
        )
        == 0
    )
    same = json.loads(capsys.readouterr().out)
    assert same["same"] is True

    assert (
        ptpip.main(
            [
                "compare-init",
                "--reference",
                str(reference),
                "--friendly-name",
                "mbp-7274",
                "--guid",
                "f2e4538fada5485d87b27f0bd3d5ded0",
            ]
        )
        == 1
    )
    different = json.loads(capsys.readouterr().out)
    assert different["candidate"]["friendly_name"] == "mbp-7274"

    valid = tmp_path / "init.bin"
    valid.write_bytes(reference.read_bytes())
    assert ptpip.main(["inventory-init", "--json", str(tmp_path)]) == 0
    inventory = json.loads(capsys.readouterr().out)
    assert inventory[0]["friendly_name"] == "Pixel-6-9405"

    assert ptpip.main(["inventory-init", str(tmp_path)]) == 0
    assert "friendly_name=Pixel-6-9405" in capsys.readouterr().out
