# Android integration — consuming `CameraProtocolFFI` from a Kotlin app

How an Android consumer wires up the Rust + uniffi Kotlin bindings
from a published release. Parallel to `docs/SPM_INTEGRATION.md` (iOS),
same FFI surface (`ConfigStore`, `Action`, `Step`, etc.) — just different
packaging.

## What's in the release

Every push to `main` builds three Android cdylib ABIs + the uniffi Kotlin
bindings and publishes a source-distribution tarball:

| asset | role |
|---|---|
| `CameraProtocolFFI-<sha8>-android.tar.gz` | the artifact (contains `android/jniLibs/<abi>/*.so` + `android/kotlin/uniffi/camera_protocol_ffi/*.kt`) |
| `CameraProtocolFFI-<sha8>-android.checksum` | SHA-256 of the tarball |

Three ABIs are built — covers everything except 32-bit x86:

- `arm64-v8a` (aarch64-linux-android) — modern devices + emulators
- `armeabi-v7a` (armv7-linux-androideabi) — older 32-bit ARM devices
- `x86_64` (x86_64-linux-android) — emulators

> **Not yet a real `.aar`.** This is a source-distribution tarball you
> integrate by hand into your Android module's `src/main/` tree. A
> turn-key `.aar` (compiled `classes.jar` in an Android-Archive zip) is
> task #43 — needs `kotlinc` + `android.jar` in CI. Until that lands,
> the bytes are here and the integration is two `cp -r` commands.

## Consumer integration

### 1. Download + extract

```bash
TAG=sha-<8>    # e.g. sha-cded3f95
gh release download "$TAG" \
    --repo epsalmond/ptpsim \
    --pattern "CameraProtocolFFI-*-android.tar.gz" \
    --dir /tmp
tar -xzf /tmp/CameraProtocolFFI-*-android.tar.gz -C /tmp
ls /tmp/android
#   jniLibs/
#     arm64-v8a/libcamera_protocol_ffi.so
#     armeabi-v7a/libcamera_protocol_ffi.so
#     x86_64/libcamera_protocol_ffi.so
#   kotlin/
#     uniffi/camera_protocol_ffi/camera_protocol_ffi.kt
```

### 2. Drop into your Android module

```bash
cp -r /tmp/android/jniLibs   path/to/your/app/src/main/
cp -r /tmp/android/kotlin/*  path/to/your/app/src/main/java/
```

The `src/main/jniLibs/<abi>/*.so` is the Android Gradle plugin's expected
location — ABIs are automatically packed into the APK based on the
target SDK's `abiFilters`. The Kotlin file goes under `src/main/java/`
with its package (`uniffi.camera_protocol_ffi`) preserved as a directory
path.

### 3. `build.gradle` (or `build.gradle.kts`) — add the JNA dependency

uniffi's Kotlin runtime depends on JNA for the FFI calls. Add to your
module's `dependencies`:

```kotlin
dependencies {
    implementation("net.java.dev.jna:jna:5.13.0@aar")
    // ... your other deps
}
```

> The `@aar` suffix is required — JNA ships separate JVM (`.jar`) and
> Android (`.aar`) artifacts; without the suffix Gradle picks the wrong
> one and the JNI library is missing at runtime.

### 4. Use the FFI

Same `ConfigStore` API as iOS — the FFI surface is platform-neutral. See
`docs/INTEGRATION.md` for the full call list. The Kotlin runtime mirrors
the Swift one:

```kotlin
import uniffi.camera_protocol_ffi.ConfigStore

val store = ConfigStore.fromManufacturerIndex(
    indexYaml = readAsset("fuji/index.yaml"),
    modelBodies = listOf(
        KeyValue("gfx100ii", readAsset("fuji/gfx100ii/gfx100ii.yaml")),
    ),
)
```

The manifest YAMLs are shipped as a separate release artifact
(`camera-config-data-<sha8>.tar.gz` — see `docs/INTEGRATION.md §5`).
Extract it into `assets/` for `readAsset(...)` to find at runtime.

## Bumping to a new release

Re-run the `gh release download` + `cp -r` flow with the new tag, and
sync your tree. There's no SPM-style branch-pinning equivalent for
Gradle today — task #43 (the real `.aar`) would unlock that via
Maven/JitPack publishing.

## See also

- `docs/INTEGRATION.md` — full FFI surface + integration pattern
- `docs/SPM_INTEGRATION.md` — the iOS equivalent
- `ci/build-android.sh` — the build recipe (runnable locally if you
  have Rust + Android NDK + the 3 targets installed)
- `.woodpecker/linux.yml` — the CI step that publishes the tarball
