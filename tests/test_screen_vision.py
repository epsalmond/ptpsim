from __future__ import annotations

import argparse
from datetime import datetime, timezone, timedelta
import json
from pathlib import Path
import shutil

import pytest

from rce.tools.fuji_ble_gps import screen_vision


FIXTURE_DIR = Path(__file__).parent / "fixtures" / "screen_vision"


def require_vision_stack() -> None:
    pytest.importorskip("cv2")
    pytest.importorskip("numpy")
    pytest.importorskip("pytesseract")
    if shutil.which("tesseract") is None:
        pytest.skip("tesseract executable is not installed")


def make_args(tmp_path, **kwargs):
    values = {
        "device_name": "iPhone",
        "image": None,
        "timeout": 0.1,
        "warmup": 2.0,
        "zoom": None,
        "capture_root": tmp_path / "captures",
        "labels_file": tmp_path / "labels.json",
        "lcd_box_file": tmp_path / "lcd_box.json",
        "state_file": tmp_path / "state.json",
        "name": "GFX100 II",
        "confidence_threshold": 0.75,
        "no_evidence": True,
    }
    values.update(kwargs)
    return argparse.Namespace(**values)


def test_local_timestamp_uses_local_offset_format() -> None:
    stamp = screen_vision.local_timestamp(
        datetime(2026, 5, 2, 16, 25, 30, tzinfo=timezone(timedelta(hours=-7)))
    )

    assert stamp.compact == "20260502T162530-0700"
    assert stamp.iso == "2026-05-02T16:25:30-07:00"
    assert stamp.timezone_name


def test_repair_commands_and_bbox_helpers(tmp_path) -> None:
    capture = tmp_path / "capture.json"
    artifact = {
        "state": {"label": "unknown"},
        "unknown_elements": [{"id": "unknown_001"}],
        "repair_command": "",
    }

    assert "pip install" in screen_vision.dependency_repair_command()
    assert screen_vision.capture_repair_command() == "scripts/request_macos_camera_permission.sh"
    assert screen_vision.detect_lcd_repair_command().startswith("scripts/detect_camera_lcd_box.sh")
    assert screen_vision.build_unknown_repair_command(capture).endswith(str(capture))
    assert screen_vision.screen_state_unknown_return_code(artifact) == 2
    screen_vision.ensure_unknown_state_repair_command(artifact, capture)
    assert artifact["repair_command"].endswith(str(capture))
    assert screen_vision.normalize_bbox({"x": 10, "y": 20, "w": 30, "h": 40}, 100, 200) == {
        "x": 0.1,
        "y": 0.1,
        "w": 0.3,
        "h": 0.2,
    }
    assert screen_vision.normalize_bbox({"x": 1, "y": 1, "w": 1, "h": 1}, 0, 0)["w"] == 0.0


def test_parse_tesseract_tsv_filters_invalid_rows() -> None:
    tsv = "\n".join(
        [
            "level\tleft\ttop\twidth\theight\tconf\ttext",
            "5\t10\t20\t30\t40\t95\tISO",
            "5\t50\t20\t30\t40\tbad\tBAD",
            "5\t50\t20\t30\t40\t-1\tSKIP",
            "5\t50\t20\t30\t40\t90\t",
            "broken\trow",
        ]
    )

    assert screen_vision.parse_tesseract_tsv("", image_width=100, image_height=100) == []
    items = screen_vision.parse_tesseract_tsv(tsv, image_width=100, image_height=200)

    assert items == [
        {
            "text": "ISO",
            "confidence": 0.95,
            "bbox": {"x": 10, "y": 20, "w": 30, "h": 40},
            "bbox_norm": {"x": 0.1, "y": 0.1, "w": 0.3, "h": 0.2},
        }
    ]


def ocr_words(*words: str) -> list[dict]:
    return [
        {
            "text": word,
            "confidence": 0.9,
            "bbox": {"x": index * 10, "y": 0, "w": 8, "h": 8},
            "bbox_norm": {"x": 0.0, "y": 0.0, "w": 0.1, "h": 0.1},
        }
        for index, word in enumerate(words)
    ]


def test_text_and_metadata_helpers() -> None:
    ocr = ocr_words("M", "ISO", "640", "F2.8", "1/125")
    symbols = [
        {"label": "gps_icon"},
        {"label": "external_power_indicator"},
        {"label": "autofocus_area"},
        {"label": "autofocus_touch_indicator"},
        {"label": "bluetooth_ready_not_connected_indicator"},
        {"label": "battery_percentage_indicator"},
    ]

    assert screen_vision.joined_text(ocr) == "M ISO 640 F2.8 1/125"
    assert screen_vision.token_after("ISO", ["M", "ISO", "640"]) == "640"
    assert screen_vision.token_after("ISO", ["M"]) is None
    assert screen_vision.extract_metadata(ocr, symbols) == {
        "aperture": "F2.8",
        "autofocus_area": "present",
        "autofocus_touch": "present",
        "battery_percentage_indicator": "present",
        "bluetooth_status": "ready_not_connected",
        "exposure_mode": "M",
        "external_power": "present",
        "gps_icon": "present",
        "iso": "640",
        "shutter_speed": "1/125",
    }


def test_region_metadata_helpers() -> None:
    ocr = [
        {
            "text": "A",
            "confidence": 0.9,
            "bbox": {"x": 0, "y": 100, "w": 8, "h": 8},
            "bbox_norm": {"x": 0.0, "y": 0.2, "w": 0.1, "h": 0.1},
        },
        {
            "text": "M",
            "confidence": 0.9,
            "bbox": {"x": 0, "y": 900, "w": 8, "h": 8},
            "bbox_norm": {"x": 0.0, "y": 0.9, "w": 0.1, "h": 0.1},
        },
    ]
    regions = [
        {"name": "top_counter", "texts": ["7257", "7357", "197357"]},
        {"name": "top_quality", "texts": ["RAW!"]},
        {"name": "bottom_left", "texts": ["AF-S", "| M |"]},
        {"name": "bottom_shutter", "texts": ["ss | 60", "ss 160"]},
        {"name": "bottom_aperture", "texts": ["= 4.0"]},
        {"name": "bottom_iso", "texts": ["m 301600", "5160", "31600", "so 1 600"]},
    ]

    assert screen_vision.extract_ocr_exposure_mode(ocr) == "M"
    assert screen_vision.choose_frame_count(["47257", "7257", "7357", "197357"]) == "7357"
    assert screen_vision.choose_iso_value(["m 301600", "5160", "31600", "so 1 600", "iso 301600"]) == "1600"
    assert screen_vision.extract_region_metadata(regions) == {
        "af_mode": "AF-S",
        "aperture": "F4.0",
        "exposure_mode": "M",
        "frames_remaining": "7357",
        "image_quality": "RAW",
        "iso": "1600",
        "shutter_speed": "1/60",
    }
    assert screen_vision.extract_metadata(ocr, [], regions)["exposure_mode"] == "M"
    assert screen_vision.region_ocr_text({"texts": "RAW"}) == "RAW"
    assert screen_vision.extract_region_metadata([{"name": "bottom_shutter", "texts": ["S160"]}]) == {
        "shutter_speed": "1/60"
    }


def test_blank_lcd_metrics_helper() -> None:
    assert screen_vision.is_blank_lcd_metrics(
        {"gray_mean": 27.0, "gray_stddev": 19.0, "bright_ratio": 0.0, "edge_ratio": 0.0}
    )
    assert not screen_vision.is_blank_lcd_metrics(
        {"gray_mean": 28.0, "gray_stddev": 36.0, "bright_ratio": 0.017, "edge_ratio": 0.02}
    )
    assert not screen_vision.is_blank_lcd_metrics({"gray_mean": "bad"})


@pytest.mark.parametrize(
    ("ocr", "label", "confidence"),
    [
        (ocr_words("NOT", "FOUND", "PLEASE", "CHECK", "THE", "APP"), "app_function_not_found_retry", 0.98),
        (
            ocr_words("PLEASE", "CHECK", "THE", "APP", "AND", "SELECT", "THE", "FUNCTION", "AGAIN"),
            "app_function_not_found_retry",
            0.98,
        ),
        (ocr_words("WAITING", "FOR", "CONNECTED"), "waiting_for_connected", 0.96),
        (ocr_words("CONNECTION", "LOST"), "connection_lost", 0.95),
        (ocr_words("DEVICE", "NOT", "FOUND", "CONTINUE"), "device_not_found_continue_search", 0.92),
        (ocr_words("PAIRING"), "registration_mode", 0.82),
        (ocr_words("STBY"), "ready_to_shoot_video", 0.8),
        (ocr_words("ISO", "200", "1/60"), "ready_to_take_photo", 0.76),
        (ocr_words("ISO", "200", "F2.8"), "ready_to_take_photo", 0.76),
        (ocr_words("MENU"), "unknown", 0.0),
    ],
)
def test_classify_camera_state_rules(ocr, label, confidence) -> None:
    state = screen_vision.classify_camera_state(ocr, [])

    assert state["label"] == label
    assert state["confidence"] == confidence
    assert state["reasons"]


def test_classify_camera_state_blank_lcd_metrics() -> None:
    state = screen_vision.classify_camera_state(
        [],
        [],
        [],
        {"gray_mean": 27.0, "gray_stddev": 19.0, "bright_ratio": 0.0, "edge_ratio": 0.0},
    )

    assert state["label"] == "lcd_blank_or_sleep"
    assert state["confidence"] == 0.9
    assert state["reasons"] == ["screen metrics match blank LCD"]


def test_base_analysis_and_label_catalog_round_trip(tmp_path) -> None:
    labels = tmp_path / "labels.json"

    assert screen_vision.base_analysis("bad", "fix")["repair_command"] == "fix"
    assert "Fuji LCD fills most" in screen_vision.LCD_DETECTION_REPAIR
    lcd = screen_vision.lcd_detection_analysis()
    assert lcd["state"]["label"] == "unknown"
    assert lcd["repair_command"].startswith("scripts/detect_camera_lcd_box.sh")
    assert screen_vision.load_label_catalog(labels) == {"schema_version": 1, "labels": []}

    screen_vision.save_label_catalog(labels, {"labels": [{"label": "gps_icon"}]})

    assert screen_vision.load_label_catalog(labels) == {
        "schema_version": 1,
        "labels": [{"label": "gps_icon"}],
    }


def test_build_and_write_capture_artifact(tmp_path) -> None:
    raw = tmp_path / "raw.png"
    screen = tmp_path / "screen.png"
    capture_json = tmp_path / "capture.json"
    raw.write_bytes(b"raw")
    screen.write_bytes(b"screen")
    analysis = screen_vision.base_analysis()
    analysis["unknown_elements"] = [{"id": "unknown_001"}]
    stamp = screen_vision.LocalTimestamp(
        compact="20260502T162530-0700",
        iso="2026-05-02T16:25:30-07:00",
        timezone_name="PDT",
    )

    artifact = screen_vision.build_capture_artifact(
        timestamp=stamp,
        capture_dir=tmp_path,
        raw_image=raw,
        screen_image=screen,
        capture_json=capture_json,
        source={"device": "file"},
        analysis=analysis,
    )
    screen_vision.write_capture_artifact(capture_json, artifact)

    assert artifact["capture"]["timestamp_compact"] == "20260502T162530-0700"
    assert artifact["artifacts"]["normalized_screen_image"].endswith("screen.png")
    assert artifact["repair_command"].endswith("capture.json")
    assert json.loads(capture_json.read_text(encoding="utf-8"))["schema_version"] == 1


def test_lcd_box_artifact_and_calibration_round_trip(tmp_path, capsys) -> None:
    raw = tmp_path / "raw.png"
    screen = tmp_path / "screen.png"
    lcd_box_json = tmp_path / "lcd_box_artifact.json"
    lcd_box_file = tmp_path / "lcd_box.json"
    raw.write_bytes(b"raw")
    screen.write_bytes(b"screen")
    lcd_box = {
        "corners": [[1, 2], [3, 4], [5, 6], [7, 8]],
        "detected": True,
        "normalized_size": [100, 80],
        "raw_size": [200, 160],
        "transform": [[1, 0, 0], [0, 1, 0], [0, 0, 1]],
    }
    stamp = screen_vision.LocalTimestamp(
        compact="20260502T162530-0700",
        iso="2026-05-02T16:25:30-07:00",
        timezone_name="PDT",
    )
    artifact = screen_vision.build_lcd_box_artifact(
        timestamp=stamp,
        capture_dir=tmp_path,
        raw_image=raw,
        screen_image=screen,
        lcd_box_json=lcd_box_json,
        lcd_box_file=lcd_box_file,
        source={"device": "file"},
        lcd_box=lcd_box,
    )
    screen_vision.write_json_artifact(lcd_box_json, artifact)
    screen_vision.write_json_artifact(lcd_box_file, screen_vision.build_lcd_box_calibration(artifact))

    loaded_lcd_box, loaded = screen_vision.load_lcd_box_file(lcd_box_file)
    screen_vision.print_lcd_box_key_values(artifact)

    assert loaded_lcd_box == lcd_box
    assert loaded["updated_at_local"] == "2026-05-02T16:25:30-07:00"
    assert "lcd_box_detected=true" in capsys.readouterr().out


def test_lcd_box_file_errors(tmp_path) -> None:
    assert screen_vision.empty_lcd_box([10, 20])["raw_size"] == [10, 20]
    assert screen_vision.resolve_artifact_path("README.md", capture_path=tmp_path / "capture.json").name == "README.md"
    with pytest.raises(screen_vision.VisionError):
        screen_vision.load_lcd_box_file(tmp_path / "missing.json")

    bad = tmp_path / "bad.json"
    bad.write_text(json.dumps({"lcd_box": {"detected": False}}), encoding="utf-8")
    with pytest.raises(screen_vision.VisionError):
        screen_vision.load_lcd_box_file(bad)

    unparsable = tmp_path / "unparsable.json"
    unparsable.write_text(json.dumps({"lcd_box": []}), encoding="utf-8")
    with pytest.raises(screen_vision.VisionError):
        screen_vision.load_lcd_box_file(unparsable)


def test_copy_input_image_success_and_failure(tmp_path) -> None:
    source = tmp_path / "source.png"
    destination = tmp_path / "destination.png"
    source.write_bytes(b"image")

    assert screen_vision.copy_input_image(source, destination)["device"] == "file"
    assert destination.read_bytes() == b"image"
    with pytest.raises(screen_vision.VisionError):
        screen_vision.copy_input_image(tmp_path / "missing.png", destination)


def test_boxes_overlap() -> None:
    assert screen_vision.boxes_overlap({"x": 0, "y": 0, "w": 5, "h": 5}, {"x": 4, "y": 4, "w": 5, "h": 5})
    assert not screen_vision.boxes_overlap(
        {"x": 0, "y": 0, "w": 5, "h": 5},
        {"x": 6, "y": 6, "w": 5, "h": 5},
    )


def test_record_artifact_evidence_records_capture_state_and_gps(monkeypatch, tmp_path) -> None:
    calls = []

    def fake_record(*args, **kwargs):
        calls.append((args, kwargs))

    monkeypatch.setattr(screen_vision, "record_evidence", fake_record)
    artifact = {
        "artifacts": {
            "capture_json": "capture.json",
            "raw_image": "raw.png",
            "normalized_screen_image": "screen.png",
        },
        "screen": {"detected": True},
        "unknown_elements": [],
        "state": {
            "label": "ready_to_take_photo",
            "confidence": 0.8,
            "metadata": {"gps_icon": "present", "bluetooth_status": "ready_not_connected"},
            "reasons": ["found ISO metadata"],
        },
        "error": "",
    }

    screen_vision.record_artifact_evidence(
        state_file=tmp_path / "state.json",
        name="GFX",
        artifact=artifact,
        capture_json=tmp_path / "capture.json",
        threshold=0.75,
    )

    assert [call[0][1] for call in calls] == [
        "camera_screen_capture",
        "camera_screen_state",
        "camera_gps_icon",
        "camera_bluetooth_status",
    ]


def test_record_artifact_evidence_skips_low_confidence_state(monkeypatch, tmp_path) -> None:
    calls = []
    monkeypatch.setattr(screen_vision, "record_evidence", lambda *args, **kwargs: calls.append((args, kwargs)))
    artifact = {
        "artifacts": {"capture_json": "capture.json", "raw_image": "raw.png", "normalized_screen_image": ""},
        "screen": {"detected": False},
        "unknown_elements": [{"id": "unknown_001"}],
        "state": {"label": "unknown", "confidence": 0.0, "metadata": {}, "reasons": []},
        "error": "missing vision dependency",
    }

    screen_vision.record_artifact_evidence(
        state_file=tmp_path / "state.json",
        name="GFX",
        artifact=artifact,
        capture_json=tmp_path / "capture.json",
        threshold=0.75,
    )

    assert len(calls) == 1
    assert calls[0][0][2] == "unavailable"


def test_run_read_state_with_existing_image_and_fake_analyzer(tmp_path, capsys) -> None:
    source = tmp_path / "source.png"
    source.write_bytes(b"image")

    def fake_analyzer(raw, screen, capture_dir, labels):
        screen.write_bytes(b"screen")
        analysis = screen_vision.base_analysis()
        analysis["state"] = {
            "label": "ready_to_take_photo",
            "confidence": 0.8,
            "metadata": {"iso": "200"},
            "reasons": ["found ISO metadata"],
        }
        return analysis

    rc = screen_vision.run_read_state(
        make_args(tmp_path, image=str(source)),
        analyzer=fake_analyzer,
    )

    out = capsys.readouterr().out
    captures = list((tmp_path / "captures").glob("*/capture.json"))
    assert rc == 0
    assert len(captures) == 1
    assert "camera_screen_state=ready_to_take_photo" in out
    assert "iso=200" in out


def test_run_read_state_requires_lcd_box_without_fake_analyzer(tmp_path, capsys) -> None:
    source = tmp_path / "source.png"
    source.write_bytes(b"image")

    rc = screen_vision.run_read_state(make_args(tmp_path, image=str(source)))

    out = capsys.readouterr().out
    assert rc == 1
    assert "missing LCD box calibration" in out
    assert "repair_command=scripts/detect_camera_lcd_box.sh" in out


def test_run_detect_lcd_with_existing_image_and_fake_detector(tmp_path, capsys) -> None:
    source = tmp_path / "source.png"
    source.write_bytes(b"image")

    def fake_detector(_raw, screen):
        screen.write_bytes(b"screen")
        return {
            "corners": [[1, 2], [3, 4], [5, 6], [7, 8]],
            "detected": True,
            "normalized_size": [100, 80],
            "raw_size": [200, 160],
            "transform": [[1, 0, 0], [0, 1, 0], [0, 0, 1]],
        }

    rc = screen_vision.run_detect_lcd(
        make_args(tmp_path, image=str(source), no_save=False),
        detector=fake_detector,
    )

    out = capsys.readouterr().out
    captures = list((tmp_path / "captures").glob("*/lcd_box.json"))
    assert rc == 0
    assert len(captures) == 1
    assert (tmp_path / "lcd_box.json").exists()
    assert "lcd_box_detected=true" in out


def test_run_detect_lcd_reports_failed_detection(tmp_path, capsys) -> None:
    source = tmp_path / "source.png"
    source.write_bytes(b"image")

    rc = screen_vision.run_detect_lcd(
        make_args(tmp_path, image=str(source), no_save=True),
        detector=lambda _raw, _screen: screen_vision.empty_lcd_box([1, 1]),
    )

    out = capsys.readouterr().out
    assert rc == 1
    assert "lcd_box_detected=false" in out
    assert "repair_command=scripts/detect_camera_lcd_box.sh" in out


def test_run_detect_lcd_reports_capture_error(tmp_path, capsys) -> None:
    def broken_capture(_output, _device, _timeout, _warmup, _zoom):
        raise screen_vision.VisionError("camera blocked", repair_command="fix camera")

    rc = screen_vision.run_detect_lcd(
        make_args(tmp_path, no_save=True),
        capture=broken_capture,
    )

    out = capsys.readouterr().out
    assert rc == 1
    assert "error=camera blocked" in out
    assert "repair_command=fix camera" in out


def test_run_reclassify_capture_from_existing_screen(tmp_path, capsys) -> None:
    screen = tmp_path / "screen.png"
    screen.write_bytes(b"screen")
    capture = tmp_path / "capture.json"
    original = {
        "artifacts": {
            "capture_json": str(capture),
            "normalized_screen_image": str(screen),
            "raw_image": "raw.png",
        },
        "screen": {
            "corners": [[1, 2], [3, 4], [5, 6], [7, 8]],
            "detected": True,
            "normalized_size": [100, 80],
            "raw_size": [200, 160],
            "transform": [[1, 0, 0], [0, 1, 0], [0, 0, 1]],
        },
        "ocr": [],
        "ocr_regions": [],
        "symbols": [],
        "unknown_elements": [],
        "state": {"label": "unknown", "confidence": 0.0, "metadata": {}, "reasons": []},
        "error": "",
        "repair_command": "",
    }
    capture.write_text(json.dumps(original), encoding="utf-8")

    def fake_analyzer(screen_image, capture_dir, labels, lcd_box):
        assert screen_image == screen
        assert capture_dir == tmp_path
        assert labels == tmp_path / "labels.json"
        assert lcd_box["detected"] is True
        analysis = screen_vision.base_analysis()
        analysis["screen"] = lcd_box
        analysis["state"] = {
            "label": "ready_to_take_photo",
            "confidence": 0.8,
            "metadata": {"iso": "1600"},
            "reasons": ["found ISO metadata"],
        }
        analysis["unknown_elements"] = [{"id": "unknown_001"}]
        return analysis

    rc = screen_vision.run_reclassify_capture(
        argparse.Namespace(capture=str(capture), labels_file=tmp_path / "labels.json", write=False),
        analyzer=fake_analyzer,
    )

    out = capsys.readouterr().out
    assert rc == 0
    assert "camera_screen_state=ready_to_take_photo" in out
    assert "iso=1600" in out
    assert "unknown_count=1" in out
    assert "repair_command=scripts/identify_unknown_elements.sh" in out
    assert json.loads(capture.read_text(encoding="utf-8"))["state"]["label"] == "unknown"


def test_run_reclassify_capture_writes_and_reports_missing_screen(tmp_path, capsys) -> None:
    screen = tmp_path / "screen.png"
    screen.write_bytes(b"screen")
    capture = tmp_path / "capture.json"
    capture.write_text(
        json.dumps(
            {
                "artifacts": {"capture_json": str(capture), "normalized_screen_image": "screen.png"},
                "screen": {"detected": True},
                "unknown_elements": [],
                "state": {"label": "unknown", "confidence": 0.0, "metadata": {}, "reasons": []},
            }
        ),
        encoding="utf-8",
    )

    def fake_analyzer(_screen_image, _capture_dir, _labels, lcd_box):
        analysis = screen_vision.base_analysis()
        analysis["screen"] = lcd_box
        analysis["state"] = {
            "label": "waiting_for_connected",
            "confidence": 0.96,
            "metadata": {},
            "reasons": ["matched WAITING FOR CONNECTED text"],
        }
        return analysis

    assert screen_vision.run_reclassify_capture(
        argparse.Namespace(capture=str(capture), labels_file=tmp_path / "labels.json", write=True),
        analyzer=fake_analyzer,
    ) == 0
    assert json.loads(capture.read_text(encoding="utf-8"))["state"]["label"] == "waiting_for_connected"

    missing = tmp_path / "missing_capture.json"
    missing.write_text(json.dumps({"artifacts": {"normalized_screen_image": "missing.png"}}), encoding="utf-8")
    assert screen_vision.run_reclassify_capture(
        argparse.Namespace(capture=str(missing), labels_file=tmp_path / "labels.json", write=False),
        analyzer=fake_analyzer,
    ) == 1
    no_screen = tmp_path / "no_screen_capture.json"
    no_screen.write_text(json.dumps({"artifacts": {}}), encoding="utf-8")
    assert screen_vision.run_reclassify_capture(
        argparse.Namespace(capture=str(no_screen), labels_file=tmp_path / "labels.json", write=False),
        analyzer=fake_analyzer,
    ) == 1
    out = capsys.readouterr().out
    assert "normalized screen image not found" in out
    assert "capture has no normalized_screen_image" in out


def test_run_read_state_records_evidence_when_enabled(monkeypatch, tmp_path) -> None:
    source = tmp_path / "source.png"
    source.write_bytes(b"image")
    calls = []
    monkeypatch.setattr(screen_vision, "record_artifact_evidence", lambda **kwargs: calls.append(kwargs))

    def fake_analyzer(raw, screen, capture_dir, labels):
        analysis = screen_vision.base_analysis()
        analysis["state"] = {
            "label": "waiting_for_connected",
            "confidence": 0.96,
            "metadata": {},
            "reasons": ["matched WAITING FOR CONNECTED text"],
        }
        return analysis

    assert screen_vision.run_read_state(
        make_args(tmp_path, image=str(source), no_evidence=False),
        analyzer=fake_analyzer,
    ) == 0
    assert calls[0]["name"] == "GFX100 II"


def test_run_read_state_with_fake_capture_and_capture_error(tmp_path, capsys) -> None:
    calls = []

    def fake_capture(output, device, timeout, warmup, zoom):
        output.write_bytes(b"image")
        calls.append((device, timeout, warmup, zoom))
        return {"device": device, "timeout": timeout, "warmup": warmup, "zoom": zoom}

    def fake_analyzer(_raw, _screen, _capture_dir, _labels):
        return screen_vision.base_analysis()

    assert screen_vision.run_read_state(
        make_args(tmp_path, warmup=3.0, zoom=2.0),
        capture=fake_capture,
        analyzer=fake_analyzer,
    ) == 2
    assert calls == [("iPhone", 0.1, 3.0, 2.0)]
    out = capsys.readouterr().out
    assert "camera_screen_state=unknown" in out
    assert "repair_command=scripts/detect_camera_lcd_box.sh" in out

    def broken_capture(_output, _device, _timeout, _warmup, _zoom):
        raise screen_vision.VisionError("camera blocked", repair_command="fix camera")

    rc = screen_vision.run_read_state(
        make_args(tmp_path),
        capture=broken_capture,
        analyzer=fake_analyzer,
    )

    out = capsys.readouterr().out
    assert rc == 1
    assert "repair_command=fix camera" in out
    assert "error=camera blocked" in out


def test_print_key_values_includes_repair_and_error(capsys) -> None:
    artifact = screen_vision.base_analysis("bad", "fix")
    artifact.update(
        {
            "artifacts": {"capture_json": "capture.json"},
            "unknown_elements": [{"id": "unknown_001"}],
            "repair_command": "fix",
        }
    )

    screen_vision.print_key_values(artifact)

    out = capsys.readouterr().out
    assert "unknown_count=1" in out
    assert "repair_command=fix" in out
    assert "error=bad" in out


def test_identify_unknown_elements_round_trip(tmp_path, capsys) -> None:
    crop = tmp_path / "unknown.png"
    crop.write_bytes(b"png")
    capture = tmp_path / "capture.json"
    capture.write_text(
        json.dumps(
            {
                "unknown_elements": [
                    {"id": "unknown_001", "kind": "symbol", "crop": str(crop)},
                    {"id": "unknown_002", "kind": "symbol", "crop": "relative_unknown.png"},
                ]
            }
        ),
        encoding="utf-8",
    )
    opened = []

    rc = screen_vision.run_identify_unknown_elements(
        argparse.Namespace(capture=str(capture), labels_file=tmp_path / "labels.json", open=True),
        input_func=lambda _prompt, answers=iter(["gps_icon", ""]): next(answers),
        opener=lambda path: opened.append(path),
    )

    out = capsys.readouterr().out
    labels = json.loads((tmp_path / "labels.json").read_text(encoding="utf-8"))["labels"]
    assert rc == 0
    assert len(opened) == 2
    assert labels[0]["label"] == "gps_icon"
    assert labels[0]["source_crop"] == str(crop)
    assert labels[0]["crop"].endswith("screen_element_templates/gps_icon_test_identify_unknown_elements0_unknown_001.png")
    assert (tmp_path / "screen_element_templates" / "gps_icon_test_identify_unknown_elements0_unknown_001.png").exists()
    assert "labels_added=1" in out


def test_identify_unknown_elements_no_unknowns(tmp_path, capsys) -> None:
    capture = tmp_path / "capture.json"
    capture.write_text(json.dumps({"unknown_elements": []}), encoding="utf-8")

    assert screen_vision.run_identify_unknown_elements(
        argparse.Namespace(capture=str(capture), labels_file=tmp_path / "labels.json", open=False),
    ) == 0
    assert "unknown_count=0" in capsys.readouterr().out


def test_main_success_and_error(tmp_path, capsys) -> None:
    capture = tmp_path / "capture.json"
    capture.write_text(json.dumps({"unknown_elements": []}), encoding="utf-8")

    assert screen_vision.main(["identify-unknown-elements", "--capture", str(capture), "--no-open"]) == 0
    assert screen_vision.main(["identify-unknown-elements", "--capture", str(tmp_path / "missing.json")]) == 1
    assert "error:" in capsys.readouterr().err


def test_detect_lcd_box_handles_glare_capture_fixture(tmp_path) -> None:
    require_vision_stack()
    raw_image = FIXTURE_DIR / "ready_to_take_photo_glare_raw.png"
    screen_image = tmp_path / "screen.png"

    lcd_box = screen_vision.detect_lcd_box(raw_image, screen_image)

    assert lcd_box["detected"] is True
    assert lcd_box["method"] == "blue_lcd_color"
    assert lcd_box["raw_size"] == [1920, 1080]
    assert lcd_box["normalized_size"][0] >= 900
    assert lcd_box["normalized_size"][1] >= 700
    assert screen_image.exists()

    analysis = screen_vision.analyze_normalized_screen(
        screen_image,
        tmp_path,
        screen_vision.DEFAULT_LABELS_FILE,
        lcd_box,
    )
    assert analysis["state"]["label"] == "ready_to_take_photo"
    assert analysis["state"]["metadata"]["bluetooth_status"] == "ready_not_connected"


@pytest.mark.parametrize(
    ("fixture_name", "expected_label"),
    [
        ("app_function_not_found_retry_screen.png", "app_function_not_found_retry"),
        ("device_not_found_continue_search_screen.png", "device_not_found_continue_search"),
        ("lcd_blank_or_sleep_screen.png", "lcd_blank_or_sleep"),
        ("ready_to_take_photo_screen.png", "ready_to_take_photo"),
        ("registration_mode_screen.png", "registration_mode"),
        ("waiting_for_connected_screen.png", "waiting_for_connected"),
    ],
)
def test_classifies_representative_capture_fixtures(tmp_path, fixture_name, expected_label) -> None:
    require_vision_stack()
    screen_image = FIXTURE_DIR / fixture_name
    capture_dir = tmp_path / fixture_name.removesuffix(".png")
    capture_dir.mkdir()

    analysis = screen_vision.analyze_normalized_screen(
        screen_image,
        capture_dir,
        screen_vision.DEFAULT_LABELS_FILE,
        {
            "detected": True,
            "corners": [],
            "normalized_size": [1, 1],
            "raw_size": [1, 1],
            "transform": [],
        },
    )

    assert analysis["state"]["label"] == expected_label
    assert analysis["state"]["confidence"] >= screen_vision.STATE_CONFIDENCE_THRESHOLD
