#!/usr/bin/env bash
# Focused harness tests for retained-artifact preflight and process cleanup.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ptpsim-transfer-teardown-test.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT INT TERM

normal_artifacts="$TMP_ROOT/normal"
normal_log="$TMP_ROOT/normal.log"
PTPSIM_TRANSFER_SIZE=1000000 \
PTPSIM_TRANSFER_CASES=orderly \
PTPSIM_TRANSFER_ARTIFACT_ROOT="$normal_artifacts" \
  "$ROOT/scripts/acceptance-transfer-teardown.sh" >"$normal_log" 2>&1

rerun_log="$TMP_ROOT/rerun.log"
if PTPSIM_TRANSFER_SIZE=1000000 \
    PTPSIM_TRANSFER_CASES=orderly \
    PTPSIM_TRANSFER_ARTIFACT_ROOT="$normal_artifacts" \
    "$ROOT/scripts/acceptance-transfer-teardown.sh" >"$rerun_log" 2>&1; then
    echo "expected retained artifact preflight to reject an existing case" >&2
    exit 1
fi
grep -F "artifact case directory already exists" "$rerun_log" >/dev/null
if grep -F "FileExistsError" "$rerun_log" >/dev/null; then
    echo "retained artifact preflight exposed a Python traceback" >&2
    exit 1
fi

small_log="$TMP_ROOT/small-fixture.log"
if PTPSIM_TRANSFER_SIZE=30 PTPSIM_TRANSFER_CASES=orderly \
    "$ROOT/scripts/acceptance-transfer-teardown.sh" >"$small_log" 2>&1; then
    echo "expected a fixture smaller than its prefix and tail to be rejected" >&2
    exit 1
fi
grep -F "MOV prefix and tail marker" "$small_log" >/dev/null

malformed_artifacts="$TMP_ROOT/malformed"
malformed_log="$TMP_ROOT/malformed.log"
PTPSIM_ACCEPTANCE_TEST_SHUTDOWN=malformed-json \
PTPSIM_TRANSFER_SIZE=1000000 \
PTPSIM_TRANSFER_CASES=orderly \
PTPSIM_TRANSFER_ARTIFACT_ROOT="$malformed_artifacts" \
  "$ROOT/scripts/acceptance-transfer-teardown.sh" >"$malformed_log" 2>&1

python3 - "$normal_artifacts/results.json" "$malformed_artifacts/results.json" <<'PY'
import json
import sys

for path in sys.argv[1:]:
    with open(path, encoding="utf-8") as stream:
        result = json.load(stream)
    assert result["status"] == "passed", result
PY

python3 - "$TMP_ROOT/stubborn-child.log" "$ROOT" <<'PY'
import signal
import subprocess
import sys
import time

root = sys.argv[2]
sys.path.insert(0, root + "/scripts")
from transfer_teardown_cleanup import reap_process

log_path = sys.argv[1]
with open(log_path, "wb") as log_stream:
    process = subprocess.Popen(
        [sys.executable, "-c", "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"],
        stdout=log_stream,
        stderr=subprocess.STDOUT,
    )
    reap_process(process, log_stream)
assert process.returncode == -signal.SIGKILL, process.returncode
assert process.poll() is not None
PY

if command -v lsof >/dev/null && lsof -t -- "$malformed_artifacts/orderly/camera-sim-service.log" >/dev/null 2>&1; then
    echo "malformed-shutdown simulator log remained open after cleanup" >&2
    exit 1
fi

echo "OK: retained-artifact preflight and stubborn-service cleanup are bounded"
