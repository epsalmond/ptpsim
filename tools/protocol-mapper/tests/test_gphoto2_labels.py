"""Tests for the gphoto2 config-dump label parser (curation-aid)."""
import importlib.util
import pathlib

_spec = importlib.util.spec_from_file_location(
    "gphoto2_labels",
    pathlib.Path(__file__).parent.parent / "scripts" / "gphoto2_labels.py",
)
g = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(g)

SAMPLE = """\
/main/capturesettings/shutterspeed
Label: Shutter Speed
Readonly: 0
Type: RADIO
Current: 1/100
Choice: 0 1/4000
Choice: 16 1/100
Choice: 61 bulb
END
/main/imgsettings/iso
Label: ISO Speed
Readonly: 0
Type: RADIO
Current: 80
Choice: 1 80
END
/main/other/500d
Label: Exposure Time
Readonly: 0
Type: MENU
Current: 9843
Choice: 16 9843
Step: 0
END
"""


def props():
    return list(g.parse(SAMPLE))


def test_parses_named_prop_with_choices():
    shutter = props()[0]
    assert shutter["path"].endswith("/shutterspeed")
    assert shutter["label"] == "Shutter Speed"
    assert shutter["readonly"] is False
    assert shutter["current"] == "1/100"
    # Choice index -> label (index is gphoto2's, not necessarily the wire value).
    labels = {c["index"]: c["label"] for c in shutter["choices"]}
    assert labels[16] == "1/100"
    assert labels[61] == "bulb"


def test_extracts_hex_code_for_other_paths():
    # /main/other/500d carries a raw PTP code; the unit (microseconds) leaks via Current.
    exptime = next(p for p in props() if p["path"].endswith("/500d"))
    assert exptime["hexCode"] == "0x500d"
    assert exptime["label"] == "Exposure Time"
    assert exptime["current"] == "9843"  # ~1/100s -> confirms 0x500D is microseconds


def test_all_blocks_parsed():
    assert len(props()) == 3
