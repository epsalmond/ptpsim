from __future__ import annotations

import json

from rce.tools.fuji_ble_gps.session import Session, project_root


def test_project_root_points_to_workspace() -> None:
    assert (project_root() / "pyproject.toml").exists()


def test_session_writes_artifacts(tmp_path) -> None:
    session = Session(root=tmp_path)

    session.jsonl("events.jsonl", {"event": "hello", "value": 7})
    session.write_json("nested/data.json", {"ok": True})
    payload_path = session.write_payload("sample", b"\x01\x02")
    session.write_summary(["# Summary", "", "done"])

    event = json.loads((session.path / "events.jsonl").read_text(encoding="utf-8"))
    assert event["event"] == "hello"
    assert event["value"] == 7
    assert "ts" in event
    assert json.loads((session.path / "nested/data.json").read_text(encoding="utf-8")) == {"ok": True}
    assert payload_path.read_bytes() == b"\x01\x02"
    assert (session.path / "summary.md").read_text(encoding="utf-8") == "# Summary\n\ndone\n"
