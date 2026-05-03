from __future__ import annotations

import subprocess
import types

from rce.tools.fuji_ble_gps.device_identity import (
    camera_safe_device_name,
    default_device_name,
    stable_numeric_suffix,
    app_style_device_name,
)


def test_default_device_name_uses_macos_computer_name(monkeypatch) -> None:
    monkeypatch.setattr("platform.system", lambda: "Darwin")
    monkeypatch.setattr("socket.gethostname", lambda: "")
    calls = []

    def fake_run(command, **kwargs):
        calls.append(command)
        if command == ["scutil", "--get", "LocalHostName"]:
            return types.SimpleNamespace(returncode=1, stdout="")
        return types.SimpleNamespace(returncode=0, stdout="Eric's MacBook Pro\n")

    monkeypatch.setattr(subprocess, "run", fake_run)

    assert default_device_name() == app_style_device_name("Eric's MacBook Pro", seed="Eric's MacBook Pro")
    assert calls == [["scutil", "--get", "LocalHostName"], ["scutil", "--get", "ComputerName"]]


def test_default_device_name_prefers_macos_local_host_name(monkeypatch) -> None:
    monkeypatch.setattr("platform.system", lambda: "Darwin")


    def fake_run(command, **kwargs):
        if command == ["scutil", "--get", "LocalHostName"]:
            return types.SimpleNamespace(returncode=0, stdout="mbp\n")
        return types.SimpleNamespace(returncode=0, stdout="Eric's MacBook Pro\n")

    monkeypatch.setattr(subprocess, "run", fake_run)

    assert default_device_name() == app_style_device_name(
        "mbp",

    )


def test_default_device_name_sanitizes_macos_computer_name(monkeypatch) -> None:
    monkeypatch.setattr("platform.system", lambda: "Darwin")
    monkeypatch.setattr("socket.gethostname", lambda: "")

    def fake_run(command, **kwargs):
        if command == ["scutil", "--get", "LocalHostName"]:
            return types.SimpleNamespace(returncode=1, stdout="")
        return types.SimpleNamespace(returncode=0, stdout="eric’s MacBook Pro (2)\n")

    monkeypatch.setattr(subprocess, "run", fake_run)

    assert default_device_name() == app_style_device_name(
        "eric’s MacBook Pro (2)",
        seed="eric’s MacBook Pro (2)",
    )


def test_camera_safe_device_name_removes_non_ascii() -> None:
    assert camera_safe_device_name(" Fuji \u2603 \u2014 Laptop  ") == "Fuji-Laptop"
    assert camera_safe_device_name("Pixel-6-9405") == "Pixel-6-9405"


def test_app_style_device_name_adds_stable_numeric_suffix() -> None:
    assert app_style_device_name("Pixel 6", seed="seed") == app_style_device_name("Pixel 6", seed="seed")
    assert app_style_device_name("Pixel 6", seed="seed").startswith("Pixel-6-")
    assert len(stable_numeric_suffix("seed")) == 4


def test_default_device_name_falls_back_to_hostname(monkeypatch) -> None:
    monkeypatch.setattr("platform.system", lambda: "Linux")


    assert default_device_name() == app_style_device_name("mbp", seed="mbp")


def test_default_device_name_has_fallback(monkeypatch) -> None:
    monkeypatch.setattr("platform.system", lambda: "Linux")
    monkeypatch.setattr("socket.gethostname", lambda: "")

    assert default_device_name() == "Fuji-Laptop"


def test_default_device_name_uses_hostname_when_scutil_fails(monkeypatch) -> None:
    monkeypatch.setattr("platform.system", lambda: "Darwin")
    monkeypatch.setattr(
        subprocess,
        "run",
        lambda *args, **kwargs: types.SimpleNamespace(returncode=1, stdout=""),
    )


    assert default_device_name() == app_style_device_name("mbp", seed="mbp")


def test_default_device_name_uses_hostname_when_scutil_missing(monkeypatch) -> None:
    def raise_missing(*args, **kwargs):
        raise OSError("missing scutil")

    monkeypatch.setattr("platform.system", lambda: "Darwin")
    monkeypatch.setattr(subprocess, "run", raise_missing)


    assert default_device_name() == app_style_device_name("mbp", seed="mbp")
