#!/usr/bin/env bash
#
# Check or update the committed snapshot of the camera-protocol-ffi public
# UniFFI surface (the items the generated Swift and Kotlin bindings expose).
#
#     ci/ffi-surface.sh          # check: fail when a baseline item was removed or changed
#     ci/ffi-surface.sh update   # rewrite the baseline from the current surface
#
# The snapshot is read from the metadata UniFFI embeds in a host dev-profile
# staticlib, the same input ci/build-android.sh and ci/build-xcframework.sh
# give the binding generator. Release cdylibs are stripped of that metadata.
# Policy and workflow: docs/APPLE_FFI_RELEASES.md, "FFI surface baseline".

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="${1:-check}"
BASELINE="crates/camera-protocol-ffi/ffi-surface.txt"
CONFIG="crates/camera-protocol-ffi/uniffi.toml"

case "${MODE}" in
    check | update) ;;
    *)
        echo "usage: ci/ffi-surface.sh [check|update]" >&2
        exit 2
        ;;
esac

cd "${ROOT}"
TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT}/target}"
LIBRARY="${TARGET_DIR}/debug/libcamera_protocol_ffi.a"

cargo build --locked -p camera-protocol-ffi
cargo build --locked -p ptpsim-uniffi-bindgen --bin ffi-surface
test -f "${LIBRARY}" || {
    echo "FATAL: ${LIBRARY} not produced" >&2
    exit 1
}

inputs=(--library "${LIBRARY}" --config "${CONFIG}" --lockfile Cargo.lock)
if [[ "${MODE}" == update ]]; then
    exec "${TARGET_DIR}/debug/ffi-surface" snapshot "${inputs[@]}" --output "${BASELINE}"
fi
exec "${TARGET_DIR}/debug/ffi-surface" check "${inputs[@]}" --baseline "${BASELINE}"
