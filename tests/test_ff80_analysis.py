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


def test_persona_selector_pattern_scan_and_formatting(tmp_path):
    def movz_w(imm: int, register: int = 0) -> int:
        return 0x52800000 | (imm << 5) | register

    def movn_w(imm: int, register: int = 0, shift: int = 0) -> int:
        return 0x12800000 | ((shift // 16) << 21) | (imm << 5) | register

    def bl_from_to(address: int, target: int) -> int:
        return 0x94000000 | (((target - address) >> 2) & 0x03FFFFFF)

    base = 0x00100000
    data = bytearray(0x60)
    struct.pack_into("<I", data, 0x00, movz_w(0xFF80, 3))
    struct.pack_into("<I", data, 0x04, movz_w(0x02FE, 4))
    struct.pack_into("<I", data, 0x08, movz_w(0x1234, 5))
    struct.pack_into("<I", data, 0x0C, movn_w(0xFF80, 2, shift=16))
    struct.pack_into("<I", data, 0x10, movz_w(0x00D8, 0))
    struct.pack_into("<I", data, 0x18, bl_from_to(base + 0x18, 0x0158BFC8))
    struct.pack_into("<I", data, 0x30, movz_w(0x00D8, 1))
    struct.pack_into("<I", data, 0x34, bl_from_to(base + 0x34, 0x0158C000))
    struct.pack_into("<I", data, 0x38, movz_w(0x00D8, 2))
    struct.pack_into("<I", data, 0x3C, bl_from_to(base + 0x3C, 0x015C5FC0))
    dump = tmp_path / "persona_test_00100000_00100060.bin"
    dump.write_bytes(data)

    summary = ff80_analysis.analyze_dump_file(dump)
    text = ff80_analysis.format_summary_text({"files": [summary]})

    assert ff80_analysis.decode_move_wide_w(0x32800000) is None
    assert summary["persona_selector_patterns"] == [
        {
            "offset": 0,
            "address": base,
            "kind": "ff80_pid_ff80_load",
            "instruction": "movz_w_imm",
            "register": 3,
            "immediate": 0xFF80,
            "shift": 0,
            "word": movz_w(0xFF80, 3),
        },
        {
            "offset": 4,
            "address": base + 4,
            "kind": "normal_pid_02fe_load",
            "instruction": "movz_w_imm",
            "register": 4,
            "immediate": 0x02FE,
            "shift": 0,
            "word": movz_w(0x02FE, 4),
        },
        {
            "offset": 0x0C,
            "address": base + 0x0C,
            "kind": "ff80_pid_ff80_load",
            "instruction": "movn_w_imm",
            "register": 2,
            "immediate": 0xFF80,
            "shift": 16,
            "word": movn_w(0xFF80, 2, shift=16),
        },
        {
            "offset": 0x10,
            "address": base + 0x10,
            "kind": "cfgdata_0d8_getter_candidate",
            "instruction": "movz_w_imm",
            "register": 0,
            "immediate": 0x00D8,
            "shift": 0,
            "word": movz_w(0x00D8, 0),
            "following_bl_calls": [
                {
                    "offset": 0x18,
                    "address": base + 0x18,
                    "target": 0x0158BFC8,
                    "match": "cfgdata_getter_0158bfc8",
                }
            ],
        },
        {
            "offset": 0x30,
            "address": base + 0x30,
            "kind": "cfgdata_0d8_getter_candidate",
            "instruction": "movz_w_imm",
            "register": 1,
            "immediate": 0x00D8,
            "shift": 0,
            "word": movz_w(0x00D8, 1),
            "following_bl_calls": [
                {
                    "offset": 0x34,
                    "address": base + 0x34,
                    "target": 0x0158C000,
                    "match": "cfgdata_getter_range_0158xxxx",
                }
            ],
        },
    ]
    assert "persona_patterns:" in text
    assert "0x00100010:cfgdata_0d8_getter_candidate:w0->[0x0158bfc8:cfgdata_getter_0158bfc8]" in text


def test_cfgdata_getter_caller_and_usb_evidence_scan(tmp_path):
    def movz_w(imm: int, register: int = 0) -> int:
        return 0x52800000 | (imm << 5) | register

    def bl_from_to(address: int, target: int) -> int:
        return 0x94000000 | (((target - address) >> 2) & 0x03FFFFFF)

    base = 0x00200000
    data = bytearray(0x100)
    struct.pack_into("<I", data, 0x20, movz_w(0x00D8, 0))
    struct.pack_into("<I", data, 0x28, bl_from_to(base + 0x28, 0x0158BFC8))
    data[0x50 : 0x58] = b"USB_TSK\x00"
    data[0x70 : 0x70 + 14] = bytes.fromhex("1201000280730740cb0480ff0001")
    dump = tmp_path / "persona_scan_00200000_00200100.bin"
    dump.write_bytes(data)

    evidence = ff80_analysis.build_persona_evidence([dump])
    text = ff80_analysis.format_persona_evidence_text(evidence)

    assert evidence["cfgdata_getter"]["direct_call_count"] == 1
    assert evidence["cfgdata_getter"]["tag_0d8_candidate_count"] == 1
    caller = evidence["cfgdata_getter"]["tag_0d8_candidates"][0]
    assert caller["address"] == base + 0x28
    assert caller["nearest_arg_tag_load"]["tag"] == "persona_selector_candidate_00d8"
    assert evidence["usb_runtime"]["text_hit_count"] == 1
    assert evidence["usb_runtime"]["raw_hit_count"] == 3
    assert "tag_0d8_candidates=1" in text
    assert "USB_TSK" in text
    assert "ff80_device_descriptor" in text


def test_persona_evidence_edge_branches(tmp_path):
    def movz_w(imm: int, register: int = 0) -> int:
        return 0x52800000 | (imm << 5) | register

    def bl_from_to(address: int, target: int) -> int:
        return 0x94000000 | (((target - address) >> 2) & 0x03FFFFFF)

    assert ff80_analysis.decode_interesting_move_wide(movz_w(0x1234)) is None
    assert ff80_analysis.scan_usb_runtime_evidence(b"plain ascii\x00", 0)["text_hits"] == []
    assert ff80_analysis.scan_pointer_references([], []) == []

    unknown = tmp_path / "unknown.bin"
    unknown.write_bytes(struct.pack("<Q", 0x1000))
    assert ff80_analysis.scan_pointer_references([unknown], [0x1000]) == []

    many_refs = tmp_path / "many_refs_00002000_00003000.bin"
    many_targets = [0x12345000 + index for index in range(16)]
    many_refs.write_bytes(b"".join(struct.pack("<Q", target) * 20 for target in many_targets))
    assert len(ff80_analysis.scan_pointer_references([many_refs], many_targets)) == 256

    base = 0x00300000
    no_arg = bytearray(0x40)
    struct.pack_into("<I", no_arg, 0x20, bl_from_to(base + 0x20, 0x0158BFC8))
    no_arg_dump = tmp_path / "no_arg_00300000_00300040.bin"
    no_arg_dump.write_bytes(no_arg)

    long_usb = tmp_path / "long_usb_00400000_00400100.bin"
    long_usb.write_bytes((b"USB_" + (b"A" * 100) + b"\x00").ljust(0x100, b"\x00"))

    pointer = tmp_path / "pointer_00500000_00500020.bin"
    pointer.write_bytes(struct.pack("<Q", 0x00400000))

    evidence = ff80_analysis.build_persona_evidence([no_arg_dump, long_usb, pointer, unknown])
    text = ff80_analysis.format_persona_evidence_text(evidence)

    assert evidence["files_scanned"] == 3
    assert evidence["cfgdata_getter"]["direct_call_count"] == 1
    assert "no_nearby_known_tag_load" in text
    assert "USB_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA..." in text
    assert "u64 -> 0x00400000" in text


def test_session_root_persona_evidence_main(tmp_path, capsys):
    session = tmp_path / "sessions" / "ff80_test" / "dumps"
    session.mkdir(parents=True)
    dump = session / "usb_00001000_00001020.bin"
    dump.write_bytes(b"USB_PC\x00" + bytes(0x19))
    output = tmp_path / "evidence.json"

    assert (
        ff80_analysis.main(
            [
                "--persona-evidence",
                "--session-root",
                str(tmp_path / "sessions"),
                "--output-json",
                str(output),
            ]
        )
        == 0
    )

    captured = capsys.readouterr()
    assert "FF80 persona-selector evidence scan" in captured.out
    assert "usb_runtime=text_hits=1" in captured.out
    assert json.loads(output.read_text())["files_scanned"] == 1


def test_cfgdata_summary_and_formatting(tmp_path):
    data = bytearray(0x140)
    data[0xA2] = 0x02
    data[0xF7] = 0x01
    struct.pack_into("<H", data, 0x20, 0x04CB)
    struct.pack_into("<H", data, 0x24, 0x02FE)
    struct.pack_into("<H", data, 0x28, 0xFF80)
    data[0x40:0x48] = b"FUJIFILM"
    data[0x50:0x59] = b"GFX100 II"
    data[0x70:0x75] = b"_FUJI"
    data[0x80:0x84] = b"DSCF"
    cfgdata = tmp_path / "cfgdata.bin"
    cfgdata.write_bytes(data)

    summary = ff80_analysis.analyze_dump_file(cfgdata)
    text = ff80_analysis.format_summary_text({"files": [summary]})

    assert summary["cfgdata"]["service_offsets"] == [
        {"offset": 0xA2, "value": 0x02},
        {"offset": 0xF7, "value": 0x01},
    ]
    assert summary["cfgdata"]["known_word_hits"] == [
        {"offset": 0x20, "value": 0x04CB, "label": "fuji_usb_vendor_id"},
        {"offset": 0x24, "value": 0x02FE, "label": "gfx100ii_normal_ptp_product_id"},
        {"offset": 0x28, "value": 0xFF80, "label": "ff80_jig_product_id"},
    ]
    assert summary["cfgdata"]["ascii_string_count"] == 4
    assert {"start": 0x40, "end": 0x48, "length": 8} in summary["cfgdata"][
        "largest_nonzero_ranges"
    ]
    assert "cfgdata_service_offsets: +0xa2=0x02, +0xf7=0x01" in text
    assert "cfgdata_word_hits: +0x20:fuji_usb_vendor_id=0x04cb" in text
    assert "cfgdata_strings: +0x40:FUJIFILM" in text


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
