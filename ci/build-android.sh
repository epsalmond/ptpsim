#!/usr/bin/env bash
#
# Build CameraProtocolFFI for Android — three cdylib ABIs + Kotlin bindings,
# packaged as a source-distribution tarball consumers integrate into their
# Android Gradle module's `src/main/{jniLibs, java}` trees.
#
# Outputs (under $OUT_DIR, default `out/`):
#
#     android/jniLibs/arm64-v8a/libcamera_protocol_ffi.so
#     android/jniLibs/armeabi-v7a/libcamera_protocol_ffi.so
#     android/jniLibs/x86_64/libcamera_protocol_ffi.so
#     android/kotlin/uniffi/camera_protocol_ffi/camera_protocol_ffi.kt
#     dist/CameraProtocolFFI-<sha8>-android.tar.gz   (the publishable artifact)
#
# Requires:
#   - cargo + rustup (3 Android targets installed)
#   - cargo-ndk (wraps cargo build with the right NDK toolchain per ABI)
#   - Android NDK (ANDROID_NDK_HOME set, OR auto-discovered via cargo-ndk)
#
# Real .aar wrapping (classes.jar + AndroidManifest.xml in an Android-Archive
# zip) is task #43 — needs kotlinc + android.jar in CI which doubles the
# Docker image size. The .so + .kt this script ships are exactly the bytes
# that would go INSIDE the .aar, so consumers can integrate today without
# waiting for the wrapping work.
#
# Consumer-side: docs/ANDROID_INTEGRATION.md.

set -euo pipefail

# ---------------------------------------------------------------------------
# Knobs
# ---------------------------------------------------------------------------

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-${ROOT}/out}"
PROFILE="${PROFILE:-release}"
SHA8="$(cd "${ROOT}" && git rev-parse --short=8 HEAD 2>/dev/null || echo dev)"

CRATE="camera-protocol-ffi"
SO_NAME="libcamera_protocol_ffi.so"
XCF_NAME="CameraProtocolFFI"

# Rust target → Android ABI directory name (what jniLibs/<abi> expects).
declare -A ABIS=(
    [aarch64-linux-android]="arm64-v8a"
    [armv7-linux-androideabi]="armeabi-v7a"
    [x86_64-linux-android]="x86_64"
)

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

require() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FATAL: missing required tool: $1" >&2
        exit 1
    }
}
require cargo
require rustup
require cargo-ndk
require tar
require shasum

for t in "${!ABIS[@]}"; do
    if ! rustup target list --installed | grep -qx "${t}"; then
        echo "FATAL: rustup target '${t}' not installed." >&2
        echo "       Run: rustup target add ${!ABIS[*]}" >&2
        exit 1
    fi
done

if [[ -z "${ANDROID_NDK_HOME:-}" && -z "${ANDROID_NDK_ROOT:-}" ]]; then
    echo "WARN: ANDROID_NDK_HOME / ANDROID_NDK_ROOT not set — cargo-ndk will" >&2
    echo "      try to auto-discover. If it can't, set ANDROID_NDK_HOME and rerun." >&2
fi

cd "${ROOT}"

# ---------------------------------------------------------------------------
# Build cdylibs
# ---------------------------------------------------------------------------

ANDROID_DIR="${OUT_DIR}/android"
JNILIBS_DIR="${ANDROID_DIR}/jniLibs"
rm -rf "${ANDROID_DIR}"
mkdir -p "${JNILIBS_DIR}"

echo "==> Building ${CRATE} for ${#ABIS[@]} Android ABIs (profile: ${PROFILE})"
# cargo-ndk does the per-target cargo build with the right NDK linker per ABI.
# -o stages the .so files into <out>/<abi>/<so> exactly as jniLibs expects.
cargo ndk \
    -t arm64-v8a \
    -t armeabi-v7a \
    -t x86_64 \
    -o "${JNILIBS_DIR}" \
    build --"${PROFILE}" -p "${CRATE}"

# Sanity-check: each ABI dir should contain the .so.
for abi in "${ABIS[@]}"; do
    if [[ ! -f "${JNILIBS_DIR}/${abi}/${SO_NAME}" ]]; then
        echo "FATAL: missing ${JNILIBS_DIR}/${abi}/${SO_NAME}" >&2
        exit 1
    fi
done

# ---------------------------------------------------------------------------
# Kotlin bindings
# ---------------------------------------------------------------------------

KOTLIN_DIR="${ANDROID_DIR}/kotlin"
rm -rf "${KOTLIN_DIR}"
mkdir -p "${KOTLIN_DIR}"

echo "==> Generating Kotlin bindings (uniffi 0.31)"
# Point uniffi at a HOST dev-profile staticlib, NOT the built android .so:
# the workspace [profile.release] sets strip = true, which removes the
# uniffi metadata section from release cdylibs (same reason
# build-python-wheel.sh feeds bindgen the .a). The .kt output is arch- and
# profile-independent. The dev host build is nearly free — the
# uniffi-bindgen binary was just compiled in the same profile.
cargo build -p "${CRATE}"
HOST_LIB="${CARGO_TARGET_DIR:-${ROOT}/target}/debug/libcamera_protocol_ffi.a"
test -f "${HOST_LIB}" || { echo "FATAL: ${HOST_LIB} not produced" >&2; exit 1; }
cargo run --bin uniffi-bindgen -- generate \
    -l kotlin \
    -o "${KOTLIN_DIR}" \
    "${HOST_LIB}"

# uniffi writes to <out>/uniffi/<crate>/<crate>.kt. Sanity-check it landed.
KOTLIN_FILE="${KOTLIN_DIR}/uniffi/camera_protocol_ffi/camera_protocol_ffi.kt"
test -f "${KOTLIN_FILE}" || {
    echo "FATAL: expected ${KOTLIN_FILE}; got:" >&2
    find "${KOTLIN_DIR}" -type f >&2
    exit 1
}

# ---------------------------------------------------------------------------
# Tarball + checksum (matches the xcframework convention)
# ---------------------------------------------------------------------------

DIST_DIR="${OUT_DIR}/dist"
mkdir -p "${DIST_DIR}"
TARBALL="${DIST_DIR}/${XCF_NAME}-${SHA8}-android.tar.gz"
CHECKSUM="${DIST_DIR}/${XCF_NAME}-${SHA8}-android.checksum"

echo "==> Tarball ${TARBALL}"
# Tarball contains a top-level `android/` dir mirroring the layout consumers
# integrate into their Gradle module: android/jniLibs/<abi>/*.so +
# android/kotlin/uniffi/camera_protocol_ffi/*.kt.
tar -czf "${TARBALL}" -C "${OUT_DIR}" android

echo "==> Checksum ${CHECKSUM}"
shasum -a 256 "${TARBALL}" | awk '{print $1}' > "${CHECKSUM}"

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------

echo
echo "Build complete:"
echo "  jniLibs:    ${JNILIBS_DIR}"
echo "  kotlin:     ${KOTLIN_DIR}"
echo "  tarball:    ${TARBALL}"
echo "  checksum:   ${CHECKSUM} ($(cat "${CHECKSUM}"))"
echo
du -sh "${JNILIBS_DIR}" "${KOTLIN_DIR}" "${TARBALL}" 2>/dev/null || true
