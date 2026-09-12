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

stubborn_artifacts="$TMP_ROOT/stubborn"
stubborn_log="$TMP_ROOT/stubborn.log"
PTPSIM_REAL_BIN="$ROOT/target/debug/camera-sim-service" \
PTPSIM_BIN="$ROOT/scripts/test-fixtures/stubborn-camera-sim-service.sh" \
PTPSIM_ACCEPTANCE_TEST_SHUTDOWN=malformed-json \
PTPSIM_TRANSFER_SIZE=1000000 \
PTPSIM_TRANSFER_CASES=orderly \
PTPSIM_TRANSFER_ARTIFACT_ROOT="$stubborn_artifacts" \
  "$ROOT/scripts/acceptance-transfer-teardown.sh" >"$stubborn_log" 2>&1

python3 - "$normal_artifacts/results.json" "$stubborn_artifacts/results.json" <<'PY'
import json
import sys

for path in sys.argv[1:]:
    with open(path, encoding="utf-8") as stream:
        result = json.load(stream)
    assert result["status"] == "passed", result
PY

if ps ax | grep -E "[c]amera-sim-service.*$stubborn_artifacts/orderly/card" >/dev/null; then
    echo "stubborn simulator process survived cleanup" >&2
    exit 1
fi
if command -v lsof >/dev/null && lsof -t -- "$stubborn_artifacts/orderly/camera-sim-service.log" >/dev/null 2>&1; then
    echo "stubborn simulator log remained open after cleanup" >&2
    exit 1
fi

echo "OK: retained-artifact preflight and stubborn-service cleanup are bounded"
