import json
import struct

from rce.tools.fuji_ble_gps import ff80_analysis


def test_entropy_and_strings():
    assert ff80_analysis.shannon_entropy(b"") == 0.0
    assert ff80_analysis.shannon_entropy(b"\x00\x00\x00") == 0.0
    assert ff80_analysis.ascii_strings(b"\x00abcd\x00efg\x00wxyz") == [
        {"offset": 1, "text": "abcd"},
        {"offset": 10, "text": "wxyz"},
    ]
    assert ff80_analysis.find_all(b"aaaa", b"aa") == [0, 1, 2]


def test_dump_range_from_name():
    assert ff80_analysis.dump_range_from_name(
        ff80_analysis.Path("msg_pool_508000_00508000_00528000.bin")
    ) == (0x00508000, 0x00528000)
    assert ff80_analysis.dump_range_from_name(ff80_analysis.Path("unknown.bin")) == (None, None)


def test_message_pool_summary():
    data = bytearray(0x80)
    struct.pack_into("<5I", data, 0, 0x0004D59A, 0x752E, 0x0004D59B, 0x752C, 4)
    data[0x20 : 0x20 + ff80_analysis.MESSAGE_RECORD_STRIDE] = b"\x00\xff" * 10
    data[0x40 : 0x54] = b"syslog Ver 3.0     \x00"
    struct.pack_into("<6H", data, 0x54, 0x1234, 0x012D, 0x0043, 0x000A, 0x7530, 0x1995)

    summary = ff80_analysis.summarize_message_pool(bytes(data), 0x508000)

    assert summary["nonempty_records"] == 3
    assert summary["sample_records"][0]["address"] == 0x508000
    assert summary["sample_records"][0]["words"][1] == 0x752E
    assert summary["syslog_offsets"] == [0x40]
    assert summary["syslog_headers"] == [
        {"offset": 0x54, "u16": [0x1234, 0x012D, 0x0043, 0x000A, 0x7530, 0x1995]}
    ]


def test_byte_pool_summary():
    data = bytearray(0x120)
    offset = 0x20
    struct.pack_into("<I", data, offset, ff80_analysis.BYTE_POOL_MAGIC_LE)
    struct.pack_into("<Q", data, offset + 0x08, 0xA0DF8)
    struct.pack_into("<Q", data, offset + 0x18, 0x2610D20)
    struct.pack_into("<Q", data, offset + 0x20, 0x26FECE8)
    struct.pack_into("<Q", data, offset + 0x30, 0x180000)
    struct.pack_into("<Q", data, offset + 0x38, 0x8D4D0)
    data[offset + 0x60 : offset + 0x68] = b"uiMPL001"

    assert ff80_analysis.summarize_byte_pools(bytes(data), 0x9E000) == [
        {
            "address": 0x9E020,
            "next": 0xA0DF8,
            "pool_start": 0x2610D20,
            "pool_search": 0x26FECE8,
            "pool_size": 0x180000,
            "owner": 0x8D4D0,
            "name": "uiMPL001",
        }
    ]


def test_task_record_summary_uses_named_end_and_extra_bytes():
    record = bytearray(ff80_analysis.TASK_RECORD_STRIDE)
    record[0x30:0x34] = b"\x01\x02\x03\x04"
    record_two = bytearray(ff80_analysis.TASK_RECORD_STRIDE)
    record_two[0x40:0x44] = b"\x05\x06\x07\x08"
    data = bytes(ff80_analysis.TASK_RECORD_STRIDE) + bytes(record) + bytes(record_two) + (b"\xff" * 0xC0)

    summary = ff80_analysis.summarize_task_records(
        data,
        base_address=0xB7320,
        end_address=0xB7320 + (3 * ff80_analysis.TASK_RECORD_STRIDE),
    )

    assert summary["record_count"] == 3
    assert summary["nonempty_count"] == 2
    assert summary["nonempty_runs"] == [{"start": 2, "end": 3}]
    assert summary["sample_nonempty_records"][0]["address"] == 0xB7320 + ff80_analysis.TASK_RECORD_STRIDE
    assert summary["extra_bytes"] == 0xC0


def test_qword_table_summary():
    data = struct.pack("<QQQQ", 0, 0x57000, 0xFFFFFFFFFFFFFFFF, 0x123456789)
    summary = ff80_analysis.summarize_qword_table(data, 0xED000)

    assert summary["nonzero_qwords"] == 2
    assert summary["pointer_like_qwords"] == 1
    assert summary["sample_pointer_like"] == [{"offset": 8, "address": 0xED008, "value": 0x57000}]


def test_analyze_file_and_format_summary(tmp_path):
    scheduler = tmp_path / "threadx_scheduler_globals_0009e000_000a1000.bin"
    scheduler_data = bytearray(0x120)
    struct.pack_into("<I", scheduler_data, 0x20, ff80_analysis.BYTE_POOL_MAGIC_LE)
    scheduler_data[0x80:0x88] = b"uiMPL001"
    scheduler.write_bytes(scheduler_data)
    message_pool = tmp_path / "msg_pool_508000_00508000_00528000.bin"
    message_pool.write_bytes(b"syslog Ver 3.0     \x00" + bytes(0x40))
    task_records = tmp_path / "threadx_task_records_000b7320_000b79b0.bin"
    task_record_data = bytearray(ff80_analysis.TASK_RECORD_STRIDE)
    task_record_data[0] = 1
    task_records.write_bytes(task_record_data)

    summary = ff80_analysis.build_summary([scheduler, message_pool, task_records])
    text = ff80_analysis.format_summary_text(summary)

    assert summary["files"][0]["base_address"] == 0x9E000
    assert "byte_pools: uiMPL001@0x9e020" in text
    assert "message_pool: records=" in text
    assert "task_records: count=3 nonempty=1" in text


def test_dump_paths_from_session_and_main(tmp_path, capsys):
    session = tmp_path / "session"
    dumps = session / "dumps"
    dumps.mkdir(parents=True)
    dump = dumps / "dispatch_table_00057000_00059000.bin"
    dump.write_bytes(struct.pack("<Q", 0x57000))
    output = tmp_path / "analysis.json"

    assert ff80_analysis.dump_paths_from_session(session) == [dump]
    assert ff80_analysis.dump_paths_from_session(tmp_path / "missing") == []
    assert ff80_analysis.main(["--session-dir", str(session), "--output-json", str(output)]) == 0

    captured = capsys.readouterr()
    assert "dispatch_table_00057000_00059000.bin" in captured.out
    assert json.loads(output.read_text())["files"][0]["qword_table"]["nonzero_qwords"] == 1


def test_main_accepts_multiple_session_dirs(tmp_path, capsys):
    session_a = tmp_path / "session-a" / "dumps"
    session_b = tmp_path / "session-b" / "dumps"
    session_a.mkdir(parents=True)
    session_b.mkdir(parents=True)
    (session_a / "dispatch_table_00057000_00059000.bin").write_bytes(struct.pack("<Q", 0x57000))
    (session_b / "dispatch_table_000ea000_000eb000.bin").write_bytes(struct.pack("<Q", 0xEA000))

    assert ff80_analysis.main(
        [
            "--session-dir",
            str(session_a.parent),
            "--session-dir",
            str(session_b.parent),
        ]
    ) == 0

    captured = capsys.readouterr()
    assert "dispatch_table_00057000_00059000.bin" in captured.out
    assert "dispatch_table_000ea000_000eb000.bin" in captured.out


def test_main_rejects_empty_args(capsys):
    assert ff80_analysis.main([]) == 2
    assert "no dump paths provided" in capsys.readouterr().err
