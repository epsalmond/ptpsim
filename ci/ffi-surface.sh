#!/usr/bin/env bash
#
# Check or update the committed snapshot of the camera-protocol-ffi public
# UniFFI surface (the items the generated Swift and Kotlin bindings expose).
#
#     ci/ffi-surface.sh          # check: fail when the baseline differs from the current surface
#     ci/ffi-surface.sh update   # rewrite the baseline from the current surface
#
# The snapshot is read from the metadata UniFFI embeds in a host dev-profile
# staticlib. ci/build-android.sh generates Kotlin from the same kind of build;
# ci/build-xcframework.sh generates Swift from the iOS device release build.
# The two agree only while the FFI crate has no target- or profile-dependent
# cfg, so both modes first run `ffi-surface cfg-scan` and fail if it finds one.
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

TOOL="${TARGET_DIR}/debug/ffi-surface"
scan=0
"${TOOL}" cfg-scan --crate-dir crates/camera-protocol-ffi || scan=$?

inputs=(--library "${LIBRARY}" --config "${CONFIG}" --lockfile Cargo.lock)
if [[ "${MODE}" == update ]]; then
    if [[ "${scan}" != 0 ]]; then
        echo "FATAL: not updating the baseline while the FFI crate has a build-dependent cfg" >&2
        exit "${scan}"
    fi
    exec "${TOOL}" snapshot "${inputs[@]}" --output "${BASELINE}"
fi
check=0
"${TOOL}" check "${inputs[@]}" --baseline "${BASELINE}" || check=$?
# Report both results; fail with the more severe status.
exit $((scan > check ? scan : check))
