#!/usr/bin/env bash
# Exercise Android packaging without installing an NDK or compiling bindings.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "${TEST_ROOT}"' EXIT
mkdir -p "${TEST_ROOT}/bin" "${TEST_ROOT}/out/dist" "${TEST_ROOT}/target/debug"
: > "${TEST_ROOT}/jna.jar"
: > "${TEST_ROOT}/coroutines.jar"
cat > "${TEST_ROOT}/bin/cargo" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
    ndk)
        while [[ "$1" != -o ]]; do shift; done
        for abi in arm64-v8a armeabi-v7a x86_64; do
            mkdir -p "$2/$abi"
            printf 'native %s\n' "$abi" > "$2/$abi/libcamera_protocol_ffi.so"
        done
        ;;
    build)
        : > "${CARGO_TARGET_DIR}/debug/libcamera_protocol_ffi.a"
        ;;
    run)
        while [[ "$1" != -o ]]; do shift; done
        mkdir -p "$2/uniffi/camera_protocol_ffi"
        printf 'bindings\n' > "$2/uniffi/camera_protocol_ffi/camera_protocol_ffi.kt"
        ;;
    *) exit 2 ;;
esac
MOCK
cat > "${TEST_ROOT}/bin/rustup" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == 'target list --installed' ]]
printf '%s\n' aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
MOCK
cat > "${TEST_ROOT}/bin/cargo-ndk" <<'MOCK'
#!/usr/bin/env bash
exit 0
MOCK
cat > "${TEST_ROOT}/bin/kotlinc" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
while [[ "$1" != -d ]]; do shift; done
printf 'compiled bindings\n' > "$2"
MOCK
chmod +x "${TEST_ROOT}/bin/"*
SHA8="$(git -C "${ROOT}" rev-parse --short=8 HEAD)"
APPLE_CHECKSUM="${TEST_ROOT}/out/dist/CameraProtocolFFI-${SHA8}.checksum"
printf 'existing Apple ZIP checksum\n' > "${APPLE_CHECKSUM}"
PATH="${TEST_ROOT}/bin:${PATH}" \
OUT_DIR="${TEST_ROOT}/out" CARGO_TARGET_DIR="${TEST_ROOT}/target" \
JNA_JAR="${TEST_ROOT}/jna.jar" KOTLINX_COROUTINES_JAR="${TEST_ROOT}/coroutines.jar" \
ANDROID_NDK_HOME="${TEST_ROOT}/ndk" \
    "${ROOT}/ci/build-android.sh" > "${TEST_ROOT}/build.log" 2>&1 || {
        cat "${TEST_ROOT}/build.log" >&2
        exit 1
    }
AAR="${TEST_ROOT}/out/dist/CameraProtocolFFI-${SHA8}.aar"
ANDROID_CHECKSUM="${TEST_ROOT}/out/dist/CameraProtocolFFI-android-${SHA8}.checksum"
[[ -f "${AAR}" ]]
[[ "$(cat "${ANDROID_CHECKSUM}")" == "$(shasum -a 256 "${AAR}" | awk '{print $1}')" ]]
[[ "$(cat "${APPLE_CHECKSUM}")" == 'existing Apple ZIP checksum' ]]
unzip -l "${AAR}" | grep -q jni/arm64-v8a/libcamera_protocol_ffi.so
printf 'Android packaging checksum tests passed\n'
