#!/usr/bin/env python3
"""Select hosted Linux jobs from the event's Git history."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys


JOBS = ("docs", "workspace", "kotlin", "android", "python", "manifest")
SHA = re.compile(r"[0-9a-f]{40}\Z")


def git(*args):
    return subprocess.check_output(["git", *args], stderr=subprocess.DEVNULL)


def commit(value):
    if not isinstance(value, str) or not SHA.fullmatch(value):
        raise ValueError("event must contain a full commit SHA")
    return value


def selection(event_name, event):
    if event_name == "pull_request":
        base = commit(event["pull_request"]["base"]["sha"])
        head = commit(event["pull_request"]["head"]["sha"])
        lint_range = f"{base}..{head}"
    elif event_name == "push":
        base = commit(event["before"])
        head = commit(event["after"])
        lint_range = f"{base}..{head}" if base != "0" * 40 else head
    else:
        raise ValueError(f"unsupported event: {event_name}")

    selected = dict.fromkeys(JOBS, False)
    try:
        git("cat-file", "-e", f"{base}^{{commit}}")
        git("cat-file", "-e", f"{head}^{{commit}}")
        diff_base = (
            git("merge-base", base, head).decode().strip()
            if event_name == "pull_request"
            else base
        )
        paths = git("diff", "--name-only", "--no-renames", "-z", diff_base, head, "--").decode().split("\0")
        messages = git("log", "--format=%B", lint_range).decode()
    except (subprocess.CalledProcessError, UnicodeError):
        # A force push or unavailable base must not silently skip checks.
        selected = dict.fromkeys(JOBS, True)
        if event_name == "push":
            lint_range = head
        return {**selected, "lint_range": lint_range}

    all_jobs = "[ALL]" in messages
    for path in paths:
        if not path:
            continue
        if (
            path.startswith((".github/workflows/", "ci/github-linux-"))
            or path in ("ci/ensure-github-release", "ci/test-ensure-github-release")
        ):
            all_jobs = True
        selected["docs"] |= path.endswith(".md") or path == "scripts/lint-docs.sh"
        common = path in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")
        ffi = path.startswith(("crates/", "tools/uniffi-bindgen/"))
        kotlin = path.startswith("ci/kotlin/") or path == "ci/build-android.sh"
        selected["kotlin"] |= common or ffi or kotlin
        selected["android"] |= common or ffi or kotlin
        selected["python"] |= (
            common or ffi or path.startswith("packages/camera-protocol-python/")
            or path == "ci/build-python-wheel.sh"
        )
        selected["manifest"] |= path.startswith("packages/camera-config-data/")
        selected["workspace"] |= (
            common or ffi or kotlin
            or path.startswith((
                "services/", "tools/camera-initiator/", "tools/camera-simctl/",
                "tools/camera-sim-tui/", "packages/camera-config-data/",
                "packages/camera-protocol-python/", "packages/fixtures/", ".cargo/",
            ))
            or path in ("Dockerfile", "scripts/rustc-wrapper", "scripts/test-rustc-wrapper.sh",
                        "ci/build-python-wheel.sh")
        )
    if all_jobs:
        selected = dict.fromkeys(JOBS, True)
    # Publication and seam jobs depend on workspace validation.
    selected["workspace"] |= selected["kotlin"] or selected["android"] or selected["python"]
    return {**selected, "lint_range": lint_range}


def main():
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    result = selection(os.environ["GITHUB_EVENT_NAME"], event)
    lines = [f"{name}={str(value).lower() if isinstance(value, bool) else value}"
             for name, value in result.items()]
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    try:
        main()
    except (KeyError, ValueError) as error:
        sys.exit(f"invalid GitHub event: {error}")
