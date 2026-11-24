#!/usr/bin/env bash
#
# Check or update the committed snapshot of the camera-protocol-ffi public
# UniFFI surface (the items the generated Swift and Kotlin bindings expose).
#
#     ci/ffi-surface.sh                    # check the host dev build against the baseline
#     ci/ffi-surface.sh update             # rewrite the baseline from the host dev build
#     ci/ffi-surface.sh check --target T   # check a release staticlib built for target T
#     ci/ffi-surface.sh check --library L  # check an existing staticlib L
#
# check fails when the baseline differs from the surface in any way.
#
# The surface is read from the metadata UniFFI embeds in a staticlib. Kotlin
# bindings are generated from a host build and Swift bindings from the iOS
# device release build, so the CI job checks both the host build and an
# aarch64-apple-ios release build, and ci/build-xcframework.sh checks the
# exact library it generates the Swift bindings from. The host modes also run
# `ffi-surface cfg-scan`, a source heuristic that points at build-dependent
# cfg early. Policy and workflow: docs/APPLE_FFI_RELEASES.md, "FFI surface
# baseline".

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASELINE="crates/camera-protocol-ffi/ffi-surface.txt"
CONFIG="crates/camera-protocol-ffi/uniffi.toml"
CRATE="camera-protocol-ffi"
STATICLIB="libcamera_protocol_ffi.a"

usage() {
    echo "usage: ci/ffi-surface.sh [check [--target <triple> | --library <staticlib>] | update]" >&2
    exit 2
}

MODE="${1:-check}"
[[ $# -gt 0 ]] && shift
TARGET=""
LIBRARY=""
case "${MODE}:$#:${1:-}" in
    check:0: | update:0:) ;;
    check:2:--target) TARGET="$2" ;;
    check:2:--library) LIBRARY="$2" ;;
    *) usage ;;
esac
# --target and --library compare one given build exactly; cfg-scan belongs to
# the host modes.
EXACT=0
[[ -n "${TARGET}${LIBRARY}" ]] && EXACT=1

cd "${ROOT}"
TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT}/target}"

if [[ -n "${TARGET}" ]]; then
    # A staticlib needs no linker, so this builds for an Apple target on any
    # host that has the target's standard library installed.
    cargo rustc --locked -p "${CRATE}" --lib --release --target "${TARGET}" --crate-type staticlib
    LIBRARY="${TARGET_DIR}/${TARGET}/release/${STATICLIB}"
elif [[ -z "${LIBRARY}" ]]; then
    cargo build --locked -p "${CRATE}"
    LIBRARY="${TARGET_DIR}/debug/${STATICLIB}"
fi
cargo build --locked -p ptpsim-uniffi-bindgen --bin ffi-surface
test -f "${LIBRARY}" || {
    echo "FATAL: ${LIBRARY} not found" >&2
    exit 1
}

TOOL="${TARGET_DIR}/debug/ffi-surface"
inputs=(--library "${LIBRARY}" --config "${CONFIG}" --lockfile Cargo.lock)
if [[ "${EXACT}" == 1 ]]; then
    exact=0
    "${TOOL}" check "${inputs[@]}" --baseline "${BASELINE}" || exact=$?
    if [[ "${exact}" == 1 ]]; then
        cat <<'NOTE'

This library is a target or release build. If `ci/ffi-surface.sh` (the host
build) reports no difference, the code that differs depends on the target or
build profile: make the FFI surface target-independent instead of updating
the baseline. See docs/APPLE_FFI_RELEASES.md, "FFI surface baseline".
NOTE
    fi
    exit "${exact}"
fi

scan=0
"${TOOL}" cfg-scan --crate-dir "crates/${CRATE}" || scan=$?
if [[ "${MODE}" == update ]]; then
    if [[ "${scan}" != 0 ]]; then
        echo "FATAL: not updating the baseline while cfg-scan reports a finding" >&2
        exit "${scan}"
    fi
    exec "${TOOL}" snapshot "${inputs[@]}" --output "${BASELINE}"
fi
check=0
"${TOOL}" check "${inputs[@]}" --baseline "${BASELINE}" || check=$?
# Report both results; fail with the more severe status.
exit $((scan > check ? scan : check))
