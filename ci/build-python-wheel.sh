#!/usr/bin/env bash
#
# Build a Python wheel of camera-protocol-ffi for the host platform.
#
# Outputs (under packages/camera-protocol-python/):
#
#     dist/camera_protocol_ffi-<ver>-cp3-<platform>.whl
#
# Runs on Linux or macOS. The wheel is platform-specific (it bundles the
# native libcamera_protocol_ffi.{so,dylib}). For cross-platform
# distribution, run this on each target platform and ship the resulting
# wheels separately.
#
# Primary consumer: client application's test-injection TUI. See
# packages/camera-protocol-python/README.md.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE="camera-protocol-ffi"
PKG_DIR="${ROOT}/packages/camera-protocol-python"

cd "${ROOT}"

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
require python3
python3 -c "import build" 2>/dev/null || {
    echo "FATAL: python module 'build' missing. Install with:" >&2
    echo "       pip install build" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# Resolve host platform native lib name
# ---------------------------------------------------------------------------

case "$(uname -s)" in
    Linux)  NATIVE_LIB="libcamera_protocol_ffi.so" ;;
    Darwin) NATIVE_LIB="libcamera_protocol_ffi.dylib" ;;
    *)      echo "FATAL: unsupported platform $(uname -s)" >&2; exit 1 ;;
esac

# ---------------------------------------------------------------------------
# Build the cdylib + staticlib (release profile)
# ---------------------------------------------------------------------------

echo "==> Building ${CRATE} (release)"
cargo build -p "${CRATE}" --release

TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT}/target}"
NATIVE_LIB_PATH="${TARGET_DIR}/release/${NATIVE_LIB}"
test -f "${NATIVE_LIB_PATH}" || {
    echo "FATAL: ${NATIVE_LIB_PATH} not produced" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# Generate the Python module from the staticlib's metadata
# ---------------------------------------------------------------------------
#
# IMPORTANT: feed bindgen the .a (staticlib), NOT the .so. The workspace
# Cargo.toml sets `strip = true` in [profile.release], which removes the
# uniffi metadata section from the cdylib (.so) but leaves it intact in
# the staticlib (.a). The wheel ships the .so at runtime; the .a is only
# consulted to drive bindgen.

STAGE="${PKG_DIR}/src/camera_protocol_ffi"
rm -rf "${STAGE}" "${PKG_DIR}/dist" "${PKG_DIR}/build" "${PKG_DIR}"/*.egg-info
mkdir -p "${STAGE}"

echo "==> Generating Python bindings via uniffi-bindgen (-l python)"
cargo run --bin uniffi-bindgen -- generate \
    -l python \
    -o "${STAGE}" \
    "${TARGET_DIR}/release/libcamera_protocol_ffi.a"

# uniffi emits camera_protocol_ffi.py at the top of the output dir; rename
# to __init__.py so it forms the package body.
if [[ -f "${STAGE}/camera_protocol_ffi.py" ]]; then
    mv "${STAGE}/camera_protocol_ffi.py" "${STAGE}/__init__.py"
else
    echo "FATAL: uniffi did not produce camera_protocol_ffi.py in ${STAGE}" >&2
    ls -la "${STAGE}" >&2
    exit 1
fi

# Bundle the runtime native lib.
cp "${NATIVE_LIB_PATH}" "${STAGE}/"

# ---------------------------------------------------------------------------
# Build the wheel
# ---------------------------------------------------------------------------

echo "==> python -m build --wheel"
cd "${PKG_DIR}"
python3 -m build --wheel --no-isolation

WHEEL="$(find dist -maxdepth 1 -name '*.whl' -print -quit)"
echo
echo "Built: ${WHEEL}"
echo "Size:  $(du -h "${WHEEL}" | cut -f1)"
