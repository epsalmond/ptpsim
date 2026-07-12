#!/usr/bin/env bash
#
# Build CameraProtocolFFI for Android — three cdylib ABIs + the uniffi Kotlin
# bindings compiled to a real Android Archive (.aar) consumers add as a normal
# Gradle file dependency.
#
# Outputs (under $OUT_DIR, default `out/`):
#
#     android/jniLibs/<abi>/libcamera_protocol_ffi.so                (intermediate)
#     android/kotlin/uniffi/camera_protocol_ffi/camera_protocol_ffi.kt (intermediate)
#     android/classes.jar                                            (intermediate)
#     dist/CameraProtocolFFI-<sha8>.aar              (the publishable artifact)
#     dist/CameraProtocolFFI-<sha8>.checksum         (SHA-256 of the .aar)
#
# Requires:
#   - cargo + rustup (3 Android targets installed)
#   - cargo-ndk (wraps cargo build with the right NDK toolchain per ABI)
#   - Android NDK (ANDROID_NDK_HOME set, OR auto-discovered via cargo-ndk)
#   - kotlinc + zip (compile the .kt to classes.jar, assemble the .aar)
#   - JNA ($JNA_JAR), kotlinx-coroutines ($KOTLINX_COROUTINES_JAR), and
#     android.jar (from $ANDROID_HOME/$ANDROID_SDK_ROOT)
#
# The .aar (#74) wraps classes.jar + AndroidManifest.xml + jni/<abi>/*.so in an
# Android-Archive zip. The ci-android image carries kotlinc + a JNA jar for this;
# android.jar comes from the cimg Android SDK already in the base image.
#
# Consumer-side: docs/ANDROID_INTEGRATION.md.

set -euo pipefail

# ---------------------------------------------------------------------------
# Knobs
# ---------------------------------------------------------------------------

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-${ROOT}/out}"
PROFILE="${PROFILE:-release}"
# Set to 1 for the PR seam: generate and compile host Kotlin bindings without
# building Android native libraries or assembling a release AAR.
KOTLIN_BINDINGS_CHECK_ONLY="${KOTLIN_BINDINGS_CHECK_ONLY:-0}"
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
require kotlinc

case "${KOTLIN_BINDINGS_CHECK_ONLY}" in
    0 | 1) ;;
    *)
        echo "FATAL: KOTLIN_BINDINGS_CHECK_ONLY must be 0 or 1" >&2
        exit 1
        ;;
esac

if [[ "${KOTLIN_BINDINGS_CHECK_ONLY}" == 0 ]]; then
    require rustup
    require cargo-ndk
    require zip
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
fi

# .aar wrapping needs a JNA jar to compile the JNA-based uniffi bindings against.
# The ci-android image sets JNA_JAR; fall back to a search so this runs locally.
if [[ -z "${JNA_JAR:-}" || ! -f "${JNA_JAR:-}" ]]; then
    JNA_JAR="$(find / -name 'jna-*.jar' 2>/dev/null | sort -V | tail -1)"
fi
[[ -n "${JNA_JAR}" && -f "${JNA_JAR}" ]] || {
    echo "FATAL: no JNA jar found; set JNA_JAR to a jna-<ver>.jar" >&2
    exit 1
}
# Kotlin's compiler distribution carries the coroutine runtime used by uniffi's
# generated async glue. Prefer an explicit override, then locate that sibling of
# kotlinc so the build remains compatible with the selected compiler.
if [[ -z "${KOTLINX_COROUTINES_JAR:-}" || ! -f "${KOTLINX_COROUTINES_JAR:-}" ]]; then
    KOTLINC_BIN="$(command -v kotlinc)"
    while [[ -L "${KOTLINC_BIN}" ]]; do
        KOTLINC_LINK="$(readlink "${KOTLINC_BIN}")"
        if [[ "${KOTLINC_LINK}" == /* ]]; then
            KOTLINC_BIN="${KOTLINC_LINK}"
        else
            KOTLINC_BIN="$(dirname "${KOTLINC_BIN}")/${KOTLINC_LINK}"
        fi
    done
    KOTLIN_HOME="$(cd -P "$(dirname "${KOTLINC_BIN}")/.." && pwd)"
    KOTLINX_COROUTINES_JAR="${KOTLIN_HOME}/lib/kotlinx-coroutines-core-jvm.jar"
fi
[[ -f "${KOTLINX_COROUTINES_JAR}" ]] || {
    echo "FATAL: no kotlinx-coroutines jar found; set KOTLINX_COROUTINES_JAR" >&2
    exit 1
}
# android.jar is optional for these pure-JVM bindings — include it if the SDK
# carries one (it does in the cimg base) so the compile matches the Android API.
ANDROID_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
ANDROID_JAR=""
if [[ -n "${ANDROID_HOME}" && -d "${ANDROID_HOME}/platforms" ]]; then
    ANDROID_JAR="$(find "${ANDROID_HOME}/platforms" -maxdepth 2 -name android.jar 2>/dev/null | sort -V | tail -1)"
fi

cd "${ROOT}"

# ---------------------------------------------------------------------------
# Build cdylibs
# ---------------------------------------------------------------------------

ANDROID_DIR="${OUT_DIR}/android"
JNILIBS_DIR="${ANDROID_DIR}/jniLibs"
rm -rf "${ANDROID_DIR}"
if [[ "${KOTLIN_BINDINGS_CHECK_ONLY}" == 0 ]]; then
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
fi

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
# Compile bindings → classes.jar
# ---------------------------------------------------------------------------

echo "==> Compiling Kotlin bindings → classes.jar (kotlinc)"
CLASSES_JAR="${ANDROID_DIR}/classes.jar"
# uniffi 0.31 emits JNA-based bindings (import com.sun.jna.*), so JNA is on the
# compile classpath; its async glue also imports kotlinx.coroutines. android.jar
# is included when present. No -include-runtime: consumers supply these runtime
# dependencies, so classes.jar holds only our generated classes.
KOTLINC_CP="${JNA_JAR}:${KOTLINX_COROUTINES_JAR}${ANDROID_JAR:+:${ANDROID_JAR}}"

if [[ "${KOTLIN_BINDINGS_CHECK_ONLY}" == 1 ]]; then
    CHECK_JAR="${ANDROID_DIR}/kotlin-bindings-check.jar"
    STUB="${ROOT}/ci/kotlin/ExecutorBindingsStub.kt"
    test -f "${STUB}" || { echo "FATAL: ${STUB} not found" >&2; exit 1; }
    kotlinc "${KOTLIN_FILE}" "${STUB}" -classpath "${KOTLINC_CP}" -d "${CHECK_JAR}"
    test -f "${CHECK_JAR}" || { echo "FATAL: kotlinc did not produce ${CHECK_JAR}" >&2; exit 1; }
    echo "Kotlin executor binding check complete: ${CHECK_JAR}"
    exit 0
fi

kotlinc "${KOTLIN_FILE}" -classpath "${KOTLINC_CP}" -d "${CLASSES_JAR}"
test -f "${CLASSES_JAR}" || { echo "FATAL: kotlinc did not produce ${CLASSES_JAR}" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Assemble the .aar + checksum
# ---------------------------------------------------------------------------

# Android-Archive layout (a plain zip): AndroidManifest.xml + classes.jar +
# jni/<abi>/*.so (note: INSIDE an .aar it is jni/, not jniLibs/) + an empty
# R.txt. Consumed as `implementation files('…/CameraProtocolFFI-<sha8>.aar')`.
AAR_STAGE="${ANDROID_DIR}/aar"
rm -rf "${AAR_STAGE}"
mkdir -p "${AAR_STAGE}/jni"
cp "${CLASSES_JAR}" "${AAR_STAGE}/classes.jar"
for abi in "${ABIS[@]}"; do
    mkdir -p "${AAR_STAGE}/jni/${abi}"
    cp "${JNILIBS_DIR}/${abi}/${SO_NAME}" "${AAR_STAGE}/jni/${abi}/${SO_NAME}"
done
: > "${AAR_STAGE}/R.txt"
cat > "${AAR_STAGE}/AndroidManifest.xml" <<'MANIFEST'
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="uniffi.camera_protocol_ffi">
    <uses-sdk android:minSdkVersion="21" />
</manifest>
MANIFEST

DIST_DIR="${OUT_DIR}/dist"
mkdir -p "${DIST_DIR}"
AAR="${DIST_DIR}/${XCF_NAME}-${SHA8}.aar"
CHECKSUM="${DIST_DIR}/${XCF_NAME}-${SHA8}.checksum"
rm -f "${AAR}"

echo "==> Assembling ${AAR}"
# -X strips extra file attributes for a more reproducible archive.
( cd "${AAR_STAGE}" && zip -q -r -X "${AAR}" AndroidManifest.xml classes.jar R.txt jni )

echo "==> Checksum ${CHECKSUM}"
shasum -a 256 "${AAR}" | awk '{print $1}' > "${CHECKSUM}"

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------

echo
echo "Build complete:"
echo "  aar:        ${AAR}"
echo "  checksum:   ${CHECKSUM} ($(cat "${CHECKSUM}"))"
echo
unzip -l "${AAR}" 2>/dev/null || true
du -sh "${AAR}" 2>/dev/null || true
