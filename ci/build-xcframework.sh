#!/usr/bin/env bash
# Build and package the iOS CameraProtocolFFI XCFramework.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-${ROOT}/out}"
PROFILE="${PROFILE:-release}"
TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT}/target}"
FULL_SHA="$(cd "${ROOT}" && git rev-parse HEAD 2>/dev/null || true)"
SHA8="${FULL_SHA:0:8}"
SHA8="${SHA8:-dev}"

IOS_DEVICE_TARGET="aarch64-apple-ios"
IOS_SIM_ARM_TARGET="aarch64-apple-ios-sim"
IOS_SIM_X86_TARGET="x86_64-apple-ios"
CRATE="camera-protocol-ffi"
STATICLIB="libcamera_protocol_ffi.a"
XCF_NAME="CameraProtocolFFI"

usage() {
    cat <<EOF
Usage: $0 build <target>|bindings|package|all

Targets: ${IOS_DEVICE_TARGET}, ${IOS_SIM_ARM_TARGET}, ${IOS_SIM_X86_TARGET}
EOF
}

require() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FATAL: missing required tool: $1" >&2
        exit 1
    }
}

require_macos() {
    [[ "$(uname)" == "Darwin" ]] || {
        echo "FATAL: this command requires macOS and Xcode." >&2
        exit 1
    }
}

profile_dir() {
    if [[ "${PROFILE}" == "dev" ]]; then
        echo debug
    else
        echo "${PROFILE}"
    fi
}

build_target() {
    local target="$1"
    case "${target}" in
        "${IOS_DEVICE_TARGET}"|"${IOS_SIM_ARM_TARGET}"|"${IOS_SIM_X86_TARGET}") ;;
        *) echo "FATAL: unsupported Apple target: ${target}" >&2; exit 2 ;;
    esac
    require_macos
    require cargo
    require swiftc
    require rustup
    rustup target list --installed | grep -qx "${target}" || {
        echo "FATAL: rustup target '${target}' is not installed." >&2
        exit 1
    }
    echo "==> Building ${CRATE} for ${target} (${PROFILE})"
    cargo build -p "${CRATE}" --profile "${PROFILE}" --target "${target}"
}

generate_bindings() {
    require_macos
    require cargo
    local swift_dir="${OUT_DIR}/swift"
    local library="${TARGET_DIR}/${IOS_DEVICE_TARGET}/$(profile_dir)/${STATICLIB}"
    [[ -f "${library}" ]] || {
        echo "FATAL: build ${IOS_DEVICE_TARGET} before generating bindings." >&2
        exit 1
    }
    rm -rf "${swift_dir}"
    mkdir -p "${swift_dir}"

    # The generator is a host-only tool. Keep its CLI dependency graph out of
    # all three cross-compiled iOS libraries and do not optimize tooling that
    # is never shipped.
    cargo build -p ptpsim-uniffi-bindgen --bin uniffi-bindgen
    "${TARGET_DIR}/debug/uniffi-bindgen" generate \
        -l swift \
        -o "${swift_dir}" \
        "${library}"
    test -f "${swift_dir}/${XCF_NAME}.swift"
    swiftc -typecheck \
        -I "${swift_dir}" \
        -Xcc "-fmodule-map-file=${swift_dir}/${XCF_NAME}FFI.modulemap" \
        "${swift_dir}/${XCF_NAME}.swift" \
        "${ROOT}/ci/swift/ExecutorBindingsStub.swift"
}

package_xcframework() {
    require_macos
    require xcodebuild
    require lipo
    require tar
    require zip
    require shasum

    local profile_dir
    profile_dir="$(profile_dir)"
    local swift_dir="${OUT_DIR}/swift"
    local sim_dir="${OUT_DIR}/lipo/ios-simulator"
    local xcf_dir="${OUT_DIR}/xcf"
    local dist_dir="${OUT_DIR}/dist"
    local device_lib="${TARGET_DIR}/${IOS_DEVICE_TARGET}/${profile_dir}/${STATICLIB}"
    local sim_arm_lib="${TARGET_DIR}/${IOS_SIM_ARM_TARGET}/${profile_dir}/${STATICLIB}"
    local sim_x86_lib="${TARGET_DIR}/${IOS_SIM_X86_TARGET}/${profile_dir}/${STATICLIB}"
    local sim_lib="${sim_dir}/${STATICLIB}"

    for path in "${device_lib}" "${sim_arm_lib}" "${sim_x86_lib}" \
        "${swift_dir}/${XCF_NAME}.swift"; do
        [[ -f "${path}" ]] || { echo "FATAL: missing build input: ${path}" >&2; exit 1; }
    done

    rm -rf "${sim_dir}" "${xcf_dir}"
    mkdir -p "${sim_dir}" "${xcf_dir}" "${dist_dir}"
    lipo -create -output "${sim_lib}" "${sim_arm_lib}" "${sim_x86_lib}"
    xcodebuild -create-xcframework \
        -library "${device_lib}" -headers "${swift_dir}" \
        -library "${sim_lib}" -headers "${swift_dir}" \
        -output "${xcf_dir}/${XCF_NAME}.xcframework"

    local tarball="${dist_dir}/${XCF_NAME}-${SHA8}.tar.gz"
    local zipfile="${dist_dir}/${XCF_NAME}-${SHA8}.xcframework.zip"
    local checksum="${dist_dir}/${XCF_NAME}-${SHA8}.checksum"
    rm -f "${tarball}" "${zipfile}" "${checksum}"
    tar -czf "${tarball}" -C "${xcf_dir}" "${XCF_NAME}.xcframework"
    (cd "${xcf_dir}" && zip -qry "${zipfile}" "${XCF_NAME}.xcframework")
    shasum -a 256 "${zipfile}" | awk '{print $1}' > "${checksum}"
    du -sh "${xcf_dir}/${XCF_NAME}.xcframework" "${tarball}" "${zipfile}"
}

cd "${ROOT}"
case "${1:-}" in
    build)
        [[ $# -eq 2 ]] || { usage >&2; exit 2; }
        build_target "$2"
        ;;
    bindings)
        [[ $# -eq 1 ]] || { usage >&2; exit 2; }
        generate_bindings
        ;;
    package)
        [[ $# -eq 1 ]] || { usage >&2; exit 2; }
        package_xcframework
        ;;
    all)
        [[ $# -eq 1 ]] || { usage >&2; exit 2; }
        build_target "${IOS_DEVICE_TARGET}"
        build_target "${IOS_SIM_ARM_TARGET}"
        build_target "${IOS_SIM_X86_TARGET}"
        generate_bindings
        package_xcframework
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
