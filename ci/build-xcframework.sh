#!/usr/bin/env bash
#
# Build CameraProtocolFFI.xcframework — verified recipe per plan §11.11.
#
# Runs on macOS with: Xcode (≥ 14 for xcframework), rustup with these targets
# installed:
#
#     rustup target add aarch64-apple-ios aarch64-apple-ios-sim \
#                       aarch64-apple-darwin x86_64-apple-darwin
#
# Outputs (under $OUT_DIR, default `out/`):
#
#     swift/CameraProtocolFFI.swift           — Swift module file
#     swift/CameraProtocolFFIFFI.h            — C bridging header
#     swift/CameraProtocolFFIFFI.modulemap    — modulemap
#     xcf/CameraProtocolFFI.xcframework/      — the xcframework (three slices)
#     dist/CameraProtocolFFI-<sha8>.tar.gz    — registry-pushable tarball
#

# rustc 1.95.0.

set -euo pipefail

# ---------------------------------------------------------------------------
# Knobs
# ---------------------------------------------------------------------------

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-${ROOT}/out}"
PROFILE="${PROFILE:-release}"  # debug rebuilds faster for local iteration

# Short git SHA (8 chars) for the tarball name; falls back to "dev" if not in
# a git checkout (e.g. tarball-extracted builds).
SHA8="$(cd "${ROOT}" && git rev-parse --short=8 HEAD 2>/dev/null || echo dev)"

# Targets — keep in sync with plan §11.11.
IOS_DEVICE_TARGET="aarch64-apple-ios"
IOS_SIM_TARGET="aarch64-apple-ios-sim"
MACOS_ARM_TARGET="aarch64-apple-darwin"
MACOS_X86_TARGET="x86_64-apple-darwin"

CRATE="camera-protocol-ffi"
STATICLIB="libcamera_protocol_ffi.a"
XCF_NAME="CameraProtocolFFI"

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

if [[ "$(uname)" != "Darwin" ]]; then
    echo "FATAL: this script must run on macOS (need Xcode + Apple Rust targets)." >&2
    exit 1
fi

require() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FATAL: missing required tool: $1" >&2
        exit 1
    }
}
require cargo
require rustup
require xcodebuild
require lipo
require tar
require zip
require shasum

# Verify all four targets are installed before building any of them — a
# missing target only fails partway through and wastes minutes.
for t in "${IOS_DEVICE_TARGET}" "${IOS_SIM_TARGET}" "${MACOS_ARM_TARGET}" "${MACOS_X86_TARGET}"; do
    if ! rustup target list --installed | grep -qx "${t}"; then
        echo "FATAL: rustup target '${t}' not installed." >&2
        echo "       Run: rustup target add ${IOS_DEVICE_TARGET} ${IOS_SIM_TARGET} ${MACOS_ARM_TARGET} ${MACOS_X86_TARGET}" >&2
        exit 1
    fi
done

# ---------------------------------------------------------------------------
# Build all four staticlibs (~6 min each cold; cached cargo state is fast)
# ---------------------------------------------------------------------------

cd "${ROOT}"

# Honour an externally-provided CARGO_TARGET_DIR (CI sets a persistent one so
# re-cloned workspaces still get warm incremental builds); default to the
# in-tree target/ for local runs.
TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT}/target}"

echo "==> Building ${CRATE} for 4 Apple targets (profile: ${PROFILE})"
for t in "${IOS_DEVICE_TARGET}" "${IOS_SIM_TARGET}" "${MACOS_ARM_TARGET}" "${MACOS_X86_TARGET}"; do
    echo "  - ${t}"
    cargo build -p "${CRATE}" --"${PROFILE}" --target "${t}"
done

# ---------------------------------------------------------------------------
# Fat-combine the two macOS arches — xcframework wants one .a per platform
# ---------------------------------------------------------------------------

LIPO_DIR="${OUT_DIR}/lipo/macos"
mkdir -p "${LIPO_DIR}"

echo "==> lipo macOS arm64 + x86_64 -> ${LIPO_DIR}/${STATICLIB}"
lipo -create \
    -output "${LIPO_DIR}/${STATICLIB}" \
    "${TARGET_DIR}/${MACOS_ARM_TARGET}/${PROFILE}/${STATICLIB}" \
    "${TARGET_DIR}/${MACOS_X86_TARGET}/${PROFILE}/${STATICLIB}"

# ---------------------------------------------------------------------------
# Swift bindings
# ---------------------------------------------------------------------------

SWIFT_DIR="${OUT_DIR}/swift"
rm -rf "${SWIFT_DIR}"
mkdir -p "${SWIFT_DIR}"

echo "==> Generating Swift bindings (uniffi 0.31, single binary)"
cargo run --bin uniffi-bindgen -- generate \
    -l swift \
    -o "${SWIFT_DIR}" \
    "${TARGET_DIR}/${IOS_DEVICE_TARGET}/${PROFILE}/${STATICLIB}"

# Sanity-check the module name override took effect (config in
# crates/camera-protocol-ffi/uniffi.toml).
test -f "${SWIFT_DIR}/${XCF_NAME}.swift" || {
    echo "FATAL: expected ${SWIFT_DIR}/${XCF_NAME}.swift; got:" >&2
    ls "${SWIFT_DIR}" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# Assemble xcframework
# ---------------------------------------------------------------------------

XCF_DIR="${OUT_DIR}/xcf"
rm -rf "${XCF_DIR}"
mkdir -p "${XCF_DIR}"

echo "==> xcodebuild -create-xcframework -> ${XCF_DIR}/${XCF_NAME}.xcframework"
xcodebuild -create-xcframework \
    -library "${TARGET_DIR}/${IOS_DEVICE_TARGET}/${PROFILE}/${STATICLIB}" \
    -headers "${SWIFT_DIR}" \
    -library "${TARGET_DIR}/${IOS_SIM_TARGET}/${PROFILE}/${STATICLIB}" \
    -headers "${SWIFT_DIR}" \
    -library "${LIPO_DIR}/${STATICLIB}" \
    -headers "${SWIFT_DIR}" \
    -output "${XCF_DIR}/${XCF_NAME}.xcframework"

# ---------------------------------------------------------------------------
# Tarball for the registry push
# ---------------------------------------------------------------------------

DIST_DIR="${OUT_DIR}/dist"
mkdir -p "${DIST_DIR}"
TARBALL="${DIST_DIR}/${XCF_NAME}-${SHA8}.tar.gz"
ZIP="${DIST_DIR}/${XCF_NAME}-${SHA8}.xcframework.zip"
CHECKSUM="${DIST_DIR}/${XCF_NAME}-${SHA8}.checksum"

echo "==> Tarball ${TARBALL}"
tar -czf "${TARBALL}" -C "${XCF_DIR}" "${XCF_NAME}.xcframework"

# SPM's binaryTarget(url:, checksum:) requires .zip, not .tar.gz, with a
# SHA-256 hex of the zip file. Ship both so existing consumers of the
# tarball aren't broken, and SPM-via-binaryTarget consumers get the zip.
echo "==> Zip ${ZIP}"
(cd "${XCF_DIR}" && zip -qry "${ZIP}" "${XCF_NAME}.xcframework")

echo "==> Checksum ${CHECKSUM}"
shasum -a 256 "${ZIP}" | awk '{print $1}' > "${CHECKSUM}"

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------

echo
echo "Build complete:"
echo "  xcframework: ${XCF_DIR}/${XCF_NAME}.xcframework"
echo "  tarball:     ${TARBALL}"
echo "  zip:         ${ZIP}"
echo "  checksum:    ${CHECKSUM} ($(cat "${CHECKSUM}"))"
echo "  swift dir:   ${SWIFT_DIR}"
echo
du -sh "${XCF_DIR}/${XCF_NAME}.xcframework" "${TARBALL}" "${ZIP}" 2>/dev/null || true
