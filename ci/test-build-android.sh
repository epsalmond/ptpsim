#!/usr/bin/env bash
# Exercise Android packaging without installing an NDK or compiling bindings.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "${TEST_ROOT}"' EXIT
mkdir -p "${TEST_ROOT}/bin" "${TEST_ROOT}/out/dist" "${TEST_ROOT}/target/debug"

# The Kotlin package is declared once, in uniffi.toml. build-android.sh must
# expect the generated file under that same package, or a real build fails.
KOTLIN_PACKAGE="$(sed -n '/^\[bindings\.kotlin\]/,/^\[/s/^package_name = "\(.*\)"$/\1/p' \
    "${ROOT}/crates/camera-protocol-ffi/uniffi.toml")"
[[ -n "${KOTLIN_PACKAGE}" ]] || {
    echo "uniffi.toml declares no [bindings.kotlin] package_name" >&2
    exit 1
}
BUILD_KOTLIN_PACKAGE="$(sed -n 's/^KOTLIN_PACKAGE="\(.*\)"$/\1/p' "${ROOT}/ci/build-android.sh")"
[[ "${BUILD_KOTLIN_PACKAGE}" == "${KOTLIN_PACKAGE}" ]] || {
    echo "build-android.sh KOTLIN_PACKAGE '${BUILD_KOTLIN_PACKAGE}' does not match uniffi.toml '${KOTLIN_PACKAGE}'" >&2
    exit 1
}
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
        # Like uniffi-bindgen, write the file under the uniffi.toml package.
        mkdir -p "$2/${MOCK_KOTLIN_PACKAGE_DIR}"
        printf 'bindings\n' > "$2/${MOCK_KOTLIN_PACKAGE_DIR}/camera_protocol_ffi.kt"
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
PATH="${TEST_ROOT}/bin:${PATH}" MOCK_KOTLIN_PACKAGE_DIR="${KOTLIN_PACKAGE//.//}" \
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
unzip -p "${AAR}" AndroidManifest.xml | grep -q "package=\"${KOTLIN_PACKAGE}\""
printf 'Android packaging checksum tests passed\n'
