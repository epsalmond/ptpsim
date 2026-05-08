from __future__ import annotations

import struct
from pathlib import Path

import pytest

from rce.tools.fuji_ble_gps import firmware_update as fw


def test_build_and_decode_firmware_update_request() -> None:
    info = fw.FirmwareUpdateRequestInfo(
        product_name="GFX100 II",
        file_name="GXUP0006.DAT",
        file_size=163_184_655,
        version="2.41",
    )

    payload = fw.build_firmware_update_request(info)
    decoded = fw.decode_firmware_update_request(payload)

    assert len(payload) == 92
    assert payload[:2] == b"\x00\x05"
    assert payload[0x02:0x42].startswith(b"GFX100 II\x00")
    assert payload[0x42:0x51].startswith(b"GXUP0006.DAT\x00")
    assert struct.unpack_from("<I", payload, 0x51)[0] == 163_184_655
    assert payload[0x55:0x5C].startswith(b"2.41\x00")
    assert decoded == info


def test_firmware_update_request_rejects_bad_fields() -> None:
    base = fw.FirmwareUpdateRequestInfo("GFX100 II", "GXUP0006.DAT", 1, "2.41")

    with pytest.raises(ValueError, match="product_name"):
        fw.build_firmware_update_request(base.__class__("é", base.file_name, base.file_size, base.version))
    with pytest.raises(ValueError, match="file_name is"):
        fw.build_firmware_update_request(base.__class__(base.product_name, "X" * 16, base.file_size, base.version))
    with pytest.raises(ValueError, match="version is"):
        fw.build_firmware_update_request(base.__class__(base.product_name, base.file_name, base.file_size, "12345678"))
    with pytest.raises(ValueError, match="file_size"):
        fw.build_firmware_update_request(base.__class__(base.product_name, base.file_name, 0x1_0000_0000, base.version))
    with pytest.raises(ValueError, match="firmware_type"):
        fw.build_firmware_update_request(
            base.__class__(base.product_name, base.file_name, base.file_size, base.version, 0x1_0000)
        )
    with pytest.raises(ValueError, match="92 bytes"):
        fw.decode_firmware_update_request(b"\x00")


def test_build_firmware_send_object_info_matches_capture_fixture() -> None:
    fixture = Path("rce/reference/firmware_update_20260508/sendobjectinfo_payload.bin").read_bytes()
    payload = fw.build_firmware_send_object_info("FUP_FILE.DAT", 163_184_655)
    decoded = fw.decode_firmware_send_object_info(payload)

    assert payload == fixture
    assert len(payload) == 839
    assert decoded == fw.FirmwareSendObjectInfo(
        file_size=163_184_655,
        filename="FUP_FILE.DAT",
        filename_code_units=13,
        trailing_nonzero_bytes=0,
    )


def test_firmware_send_object_info_rejects_bad_inputs() -> None:
    with pytest.raises(ValueError, match="file_size"):
        fw.build_firmware_send_object_info("FUP_FILE.DAT", -1)
    with pytest.raises(ValueError, match="filename including NUL"):
        fw.build_firmware_send_object_info("X" * 13, 1)
    with pytest.raises(ValueError, match="PTP string"):
        fw.build_firmware_send_object_info("X" * 300, 1)
    with pytest.raises(ValueError, match="839 bytes"):
        fw.decode_firmware_send_object_info(b"\x00")

    malformed = bytearray(fw.build_firmware_send_object_info("FUP_FILE.DAT", 1))
    malformed[0x2C] = 14
    with pytest.raises(ValueError, match="filename field declares"):
        fw.decode_firmware_send_object_info(bytes(malformed))


def test_firmware_chunk_plan_matches_successful_capture() -> None:
    chunks = fw.firmware_chunk_plan(163_184_655)

    assert len(chunks) == 156
    assert chunks[0] == fw.FirmwareChunk(offset=0, length=0x100000)
    assert chunks[-1] == fw.FirmwareChunk(offset=0x09B00000, length=0x000A000F)
    assert sum(chunk.length for chunk in chunks) == 163_184_655
    assert fw.firmware_chunk_plan(0) == []

    with pytest.raises(ValueError, match="chunk_size"):
        fw.firmware_chunk_plan(1, 0)
