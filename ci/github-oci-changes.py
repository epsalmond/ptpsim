#!/usr/bin/env python3
"""Select OCI builds from changed paths or an authored [ALL] commit."""

import json
import os
from pathlib import Path
import re
import subprocess


EXACT_PATHS = {
    "Dockerfile",
    ".dockerignore",
    "ci/container-healthcheck",
    "ci/verify-oci-image.sh",
    "ci/smoke-oci-image.sh",
    "ci/github-oci-changes.py",
    "ci/test-github-oci-changes.py",
    ".github/workflows/oci-image.yml",
    ".woodpecker/oci-image.yml",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
}
PREFIXES = (
    "crates/",
    "services/",
    "packages/camera-config-data/",
    "packages/fixtures/",
    # The planner needs every workspace member for Cargo metadata.
    "tools/",
)


def git(*args):
    return subprocess.check_output(["git", *args], stderr=subprocess.DEVNULL)


def valid_revision(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-fA-F]{40}", value) is not None


def should_build(event_name, event):
    if event_name == "workflow_dispatch":
        return True
    if event_name == "pull_request":
        pr = event.get("pull_request", {})
        base = pr.get("base", {}).get("sha")
        head = pr.get("head", {}).get("sha")
    elif event_name == "push":
        base, head = event.get("before"), event.get("after")
    else:
        raise ValueError(f"Unsupported event: {event_name}")

    # New branches or unavailable history must not silently skip a build.
    if not valid_revision(base) or not valid_revision(head):
        return True
    try:
        authored = git("log", "--format=%B", f"{base}..{head}")
        if b"[ALL]" in authored:
            return True
        if event_name == "pull_request":
            base = git("merge-base", base, head).decode().strip()
        paths = git("diff", "--name-only", "--no-renames", "-z", base, head).split(b"\0")
    except subprocess.CalledProcessError:
        return True

    for raw_path in paths:
        path = os.fsdecode(raw_path)
        if path in EXACT_PATHS or path.startswith(PREFIXES):
            return True
    return False


def main():
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    build = should_build(os.environ["GITHUB_EVENT_NAME"], event)
    output = f"build={str(build).lower()}\n"
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as stream:
        stream.write(output)
    print(output, end="")


if __name__ == "__main__":
    main()
