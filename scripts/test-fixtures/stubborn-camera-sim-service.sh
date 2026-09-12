#!/usr/bin/env bash
set -euo pipefail

: "${PTPSIM_REAL_BIN:?PTPSIM_REAL_BIN is required}"
trap '' TERM
exec "$PTPSIM_REAL_BIN" "$@"
