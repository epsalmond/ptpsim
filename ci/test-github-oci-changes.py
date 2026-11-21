#!/usr/bin/env python3
"""Exercise OCI change selection with actual commit histories."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("github-oci-changes.py")
SPEC = importlib.util.spec_from_file_location("oci_changes", SCRIPT)
CHANGES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHANGES)


class ChangesTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.previous = Path.cwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, self.previous)
        self.git("init", "-q", "--initial-branch=main")
        self.git("config", "user.name", "OCI tests")
        self.git("config", "user.email", "oci-tests@example.com")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "core.hooksPath", "/dev/null")
        self.base = self.commit("README.md")

    def git(self, *args):
        return subprocess.check_output(["git", *args], stderr=subprocess.DEVNULL).decode().strip()

    def commit(self, path, message="Update fixture"):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(target.read_text() + "change\n" if target.exists() else "change\n")
        self.git("add", "--", path)
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def select(self, head, base=None, event_name="pull_request"):
        base = self.base if base is None else base
        if event_name == "pull_request":
            event = {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}
        else:
            event = {"before": base, "after": head}
        return CHANGES.should_build(event_name, event)

    def test_docs_skip(self):
        self.assertFalse(self.select(self.commit("docs/guide.md")))

    def test_build_paths(self):
        for path in sorted(CHANGES.EXACT_PATHS) + [prefix + "nested/input" for prefix in CHANGES.PREFIXES]:
            with self.subTest(path=path):
                base = self.git("rev-parse", "HEAD")
                self.assertTrue(self.select(self.commit(path), base))

    def test_earlier_all_commit_forces_docs_build(self):
        self.commit("docs/guide.md", "Refresh fixture [ALL]")
        self.assertTrue(self.select(self.commit("docs/other.md")))

    def test_base_all_commit_is_excluded(self):
        base = self.commit("docs/base.md", "Refresh base [ALL]")
        self.assertFalse(self.select(self.commit("docs/pr.md"), base))

    def test_unmerged_base_changes_do_not_select_pr(self):
        self.git("checkout", "-qb", "feature")
        head = self.commit("docs/pr.md")
        self.git("checkout", "-q", "main")
        base = self.commit("Dockerfile", "Refresh base [ALL]")
        self.assertFalse(self.select(head, base))

    def test_deleted_build_path_selects(self):
        base = self.commit("crates/example/Cargo.toml")
        self.git("rm", "crates/example/Cargo.toml")
        self.git("commit", "-qm", "Remove fixture")
        self.assertTrue(self.select(self.git("rev-parse", "HEAD"), base))

    def test_rename_out_of_build_path_selects(self):
        base = self.commit("Dockerfile")
        self.git("mv", "Dockerfile", "example.txt")
        self.git("commit", "-qm", "Move fixture")
        self.assertTrue(self.select(self.git("rev-parse", "HEAD"), base))

    def test_push_range(self):
        head = self.commit("docs/guide.md")
        self.assertFalse(self.select(head, event_name="push"))
        self.assertTrue(self.select(self.commit("Dockerfile"), event_name="push"))

    def test_missing_history_selects(self):
        self.assertTrue(self.select(self.base, "0" * 40))
        self.assertTrue(self.select(self.base, "f" * 40))

    def test_manual_always_selects(self):
        self.assertTrue(CHANGES.should_build("workflow_dispatch", {}))

    def test_payload_strings_are_not_executed(self):
        marker = self.root / "executed"
        head = self.commit("docs/guide.md", "Refresh $(touch executed) `touch executed`")
        self.assertFalse(self.select(head))
        self.assertTrue(self.select(head, "$(touch executed)"))
        self.assertFalse(marker.exists())

    def test_event_file_outputs_literal_boolean(self):
        head = self.commit("docs/guide.md")
        event_file = self.root / "event.json"
        event_file.write_text(json.dumps({"pull_request": {"base": {"sha": self.base}, "head": {"sha": head}}}))
        output_file = self.root / "output"
        with patch.dict(os.environ, GITHUB_EVENT_NAME="pull_request", GITHUB_EVENT_PATH=str(event_file), GITHUB_OUTPUT=str(output_file)):
            CHANGES.main()
        self.assertEqual(output_file.read_text(), "build=false\n")


if __name__ == "__main__":
    unittest.main()
