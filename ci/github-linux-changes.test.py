#!/usr/bin/env python3
"""Exercise selection against real PR and push histories without credentials."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("github-linux-changes.py")
LINTER = SCRIPT.parent.parent / "scripts/lint-commit-messages.sh"
SPEC = importlib.util.spec_from_file_location("changes", SCRIPT)
CHANGES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHANGES)


class SelectionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.previous = Path.cwd()
        os.chdir(self.temp.name)
        self.addCleanup(os.chdir, self.previous)
        self.git("init", "--quiet", "--initial-branch=main")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        # Keep fixture commits independent of host hooks and signing policy.
        self.git("config", "core.hooksPath", "/dev/null")
        self.git("config", "commit.gpgsign", "false")
        self.base = self.add("README.md", "Seed repository")

    def git(self, *args):
        return subprocess.check_output(["git", *args], stderr=subprocess.DEVNULL).decode().strip()

    def add(self, path, message="Add fixture"):
        file = Path(path)
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(file.read_text() + "fixture\n" if file.exists() else "fixture\n")
        self.git("add", "--", path)
        self.git("commit", "--quiet", "-m", message)
        return self.git("rev-parse", "HEAD")

    def pr(self, head=None, base=None):
        return {"pull_request": {
            "base": {"sha": base or self.base, "repo": {"full_name": "example/project"}},
            "head": {"sha": head or self.git("rev-parse", "HEAD"),
                     "repo": {"full_name": "contributor/project"}},
            "title": "$(touch injected)\nworkspace=true",
        }}

    def select(self, event=None):
        return CHANGES.selection("pull_request", event or self.pr())

    def assert_jobs(self, result, *enabled):
        for job in CHANGES.JOBS:
            self.assertEqual(result[job], job in enabled, job)

    def assert_lint_success(self, result):
        lint = subprocess.run([str(LINTER), result["lint_range"]],
                              capture_output=True, text=True)
        self.assertEqual(lint.returncode, 0, lint.stderr)
        self.assertIn("lint-commit-messages: ok", lint.stdout)

    def test_docs_only_fork_pr(self):
        self.add("docs/reference.md")
        self.assert_jobs(self.select(), "docs")

    def test_ffi_change_runs_workspace_seams_and_release_build_selection(self):
        self.add("crates/camera-protocol-ffi/src/lib.rs")
        self.assert_jobs(self.select(), "workspace", "kotlin", "android", "python")

    def test_manifest_change_runs_workspace_and_data_publication_selection(self):
        self.add("packages/camera-config-data/example/index.yaml")
        self.assert_jobs(self.select(), "workspace", "manifest")

    def test_fixture_change_runs_workspace(self):
        self.add("packages/fixtures/capture.json")
        self.assert_jobs(self.select(), "workspace")

    def test_kotlin_stub_change_runs_workspace_and_android(self):
        self.add("ci/kotlin/ExecutorBindingsStub.kt")
        self.assert_jobs(self.select(), "workspace", "kotlin", "android")

    def test_python_package_change_runs_workspace_and_wheel(self):
        self.add("packages/camera-protocol-python/pyproject.toml")
        self.assert_jobs(self.select(), "workspace", "python")

    def test_workflow_and_selector_changes_run_all_jobs(self):
        for path in (".github/workflows/linux.yml", ".github/workflows/oci-image.yml",
                     "ci/github-linux-changes.py", "ci/github-linux-setup.sh",
                     "ci/ensure-github-release", "ci/test-ensure-github-release"):
            with self.subTest(path=path):
                self.git("checkout", "--quiet", "--detach", self.base)
                self.add(path)
                self.assert_jobs(self.select(), *CHANGES.JOBS)

    def test_all_marker_in_earlier_pr_commit(self):
        self.add("README.md", "Exercise all jobs [ALL]")
        self.add("docs/reference.md", "Add documentation")
        self.assert_jobs(self.select(), *CHANGES.JOBS)

    def test_pr_diff_excludes_changes_added_on_base(self):
        self.git("checkout", "--quiet", "-b", "feature")
        head = self.add("docs/reference.md")
        self.git("checkout", "--quiet", "main")
        advanced_base = self.add("crates/ptp-core/src/lib.rs")
        self.git("checkout", "--quiet", "feature")
        self.git("merge", "--quiet", "--no-ff", "main", "-m", "Synthetic merge")
        result = self.select(self.pr(head=head, base=advanced_base))
        self.assert_jobs(result, "docs")
        self.assertEqual(result["lint_range"], f"{advanced_base}..{head}")

    def test_push_uses_entire_before_after_range(self):
        self.add("crates/ptp-core/src/lib.rs")
        head = self.add("docs/reference.md")
        result = CHANGES.selection("push", {"before": self.base, "after": head})
        self.assert_jobs(result, "docs", "workspace", "kotlin", "android", "python")
        self.assertEqual(result["lint_range"], f"{self.base}..{head}")

    def test_renamed_source_runs_builds_for_pr_and_push(self):
        before = self.add("crates/example/src/lib.rs")
        self.git("config", "diff.renames", "true")
        Path("docs").mkdir()
        self.git("mv", "crates/example/src/lib.rs", "docs/lib.rs")
        self.git("commit", "--quiet", "-m", "Move fixture")
        head = self.git("rev-parse", "HEAD")
        for event_name, event in (
            ("pull_request", self.pr(base=before, head=head)),
            ("push", {"before": before, "after": head}),
        ):
            with self.subTest(event_name=event_name):
                result = CHANGES.selection(event_name, event)
                self.assert_jobs(result, "workspace", "kotlin", "android", "python")

    def test_main_push_all_marker(self):
        head = self.add("README.md", "Exercise all jobs [ALL]")
        result = CHANGES.selection("push", {"before": self.base, "after": head})
        self.assert_jobs(result, *CHANGES.JOBS)

    def test_missing_base_runs_all_jobs(self):
        self.add("README.md")
        self.assert_jobs(self.select(self.pr(base="1" * 40)), *CHANGES.JOBS)

    def test_initial_and_force_push_run_all_jobs(self):
        head = self.add("README.md")
        for before in ("0" * 40, "1" * 40):
            with self.subTest(before=before):
                result = CHANGES.selection("push", {"before": before, "after": head})
                self.assert_jobs(result, *CHANGES.JOBS)
                self.assert_lint_success(result)
                self.assertEqual(result["lint_range"], head)

    def test_initial_and_missing_history_push_lint_root_commit(self):
        for before in ("0" * 40, "1" * 40):
            with self.subTest(before=before):
                result = CHANGES.selection("push", {"before": before, "after": self.base})
                self.assert_jobs(result, *CHANGES.JOBS)
                self.assert_lint_success(result)
                self.assertEqual(result["lint_range"], self.base)

    def test_payload_is_read_as_data_and_outputs_are_fixed(self):
        self.add("docs/$(touch injected)\nworkspace=true.md")
        event = Path("event.json")
        event.write_text(json.dumps(self.pr()))
        output = Path("output")
        subprocess.run(["python3", str(SCRIPT)], check=True, stdout=subprocess.DEVNULL, env={
            **os.environ, "GITHUB_EVENT_NAME": "pull_request", "GITHUB_EVENT_PATH": str(event),
            "GITHUB_OUTPUT": str(output),
        })
        self.assertFalse(Path("injected").exists())
        values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        self.assertEqual(values["workspace"], "false")
        self.assertEqual(values["docs"], "true")
        self.assertEqual(len(values), len(CHANGES.JOBS) + 1)

    def test_invalid_sha_and_event_rejected(self):
        event = self.pr(head="HEAD; touch injected")
        with self.assertRaises(ValueError):
            self.select(event)
        with self.assertRaises(ValueError):
            CHANGES.selection("pull_request_target", self.pr())


if __name__ == "__main__":
    unittest.main()
