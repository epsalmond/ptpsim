# Android integration — consuming `CameraProtocolFFI` from a Kotlin app

How an Android consumer wires up the Rust + uniffi Kotlin bindings
from a published release. Parallel to `docs/SPM_INTEGRATION.md` (iOS),
same FFI surface (`ConfigStore`, `Action`, `Step`, etc.) — just different
packaging.

## What's in the release

Every push to `main` builds three Android cdylib ABIs, compiles the uniffi
Kotlin bindings, and publishes a real **Android Archive (`.aar`)**:

| asset | role |
|---|---|
| `CameraProtocolFFI-<sha8>.aar` | the artifact: `classes.jar` (compiled bindings) + `AndroidManifest.xml` + `jni/<abi>/*.so` |
| `CameraProtocolFFI-<sha8>.checksum` | SHA-256 of the `.aar` |

Three ABIs are bundled — covers everything except 32-bit x86:

- `arm64-v8a` (aarch64-linux-android) — modern devices + emulators
- `armeabi-v7a` (armv7-linux-androideabi) — older 32-bit ARM devices
- `x86_64` (x86_64-linux-android) — emulators

The `.aar` is consumed as a normal Gradle **file dependency** — no hand-copying
of loose `.so` / `.kt` files. (It is attached to a GitHub release rather than
published to Maven; a Maven/JitPack coordinate is a possible future follow-up,
but a file dependency needs no repository wiring.)

## Consumer integration

### 1. Download the `.aar`

```bash
TAG=sha-<8>    # e.g. sha-cded3f95
gh release download "$TAG" \
    --repo epsalmond/ptpsim \
    --pattern "CameraProtocolFFI-*.aar" \
    --dir path/to/your/app/libs
```

### 2. `build.gradle` (or `build.gradle.kts`) — add runtime dependencies

```kotlin
android {
    // The bundled bindings require minSdk >= 21 (the .aar's AndroidManifest).
    defaultConfig { minSdk = 21 }
}

dependencies {
    implementation(files("libs/CameraProtocolFFI-<sha8>.aar"))
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.6.4")
    // ... your other deps
}
```

> The `.aar` carries `classes.jar` + the per-ABI `.so`s; the Android Gradle
> plugin unpacks `jni/<abi>/*.so` into the APK automatically (filtered by your
> `abiFilters`). You do **not** add `jniLibs` or copy the Kotlin source.

> **JNA is required.** uniffi's Kotlin runtime calls the native library through
> JNA. The `@aar` suffix is mandatory — JNA ships separate JVM (`.jar`) and
> Android (`.aar`) artifacts; without the suffix Gradle picks the JVM one and
> the JNI library is missing at runtime. `5.14.0` matches the JNA the bindings
> were compiled against in CI.

> **Kotlin coroutines are required.** Async Rust functions and foreign async
> traits generate Kotlin `suspend` glue backed by `kotlinx.coroutines`. The
> release is a file dependency, so it has no Maven metadata through which to
> declare JNA or coroutines transitively. `1.6.4` is the compatibility baseline
> compiled by CI; a consumer may use a newer version compatible with its Kotlin
> toolchain.

### 3. Use the FFI

Same `ConfigStore` API as iOS — the FFI surface is platform-neutral. See
`docs/INTEGRATION.md` for the full call list. The Kotlin runtime mirrors
the Swift one:

```kotlin
import uniffi.camera_protocol_ffi.ConfigStore

val store = ConfigStore.fromManufacturerIndex(
    indexYaml = readAsset("fuji/index.yaml"),
    modelBodies = listOf(
        KeyValue("gfx100ii", readAsset("fuji/gfx100ii/gfx100ii.yaml")),
        KeyValue("xa7", readAsset("fuji/xa7/xa7.yaml")),
        KeyValue("fuji-generic", readAsset("fuji/fuji-generic/fuji-generic.yaml")),
    ),
)
```

The manifest YAMLs are shipped as a separate release artifact
(`camera-config-data-<sha8>.tar.gz` — see `docs/INTEGRATION.md §5`).
Extract it into `assets/` for `readAsset(...)` to find at runtime.

## Bumping to a new release

Re-run the `gh release download` for the new tag into `libs/` and bump the
`implementation(files("libs/CameraProtocolFFI-<sha8>.aar"))` coordinate. (A
remote Maven coordinate, which would allow version-pinning without vendoring the
file, is a possible future follow-up.)

## See also

- `docs/INTEGRATION.md` — full FFI surface + integration pattern
- `docs/SPM_INTEGRATION.md` — the iOS equivalent
- `ci/build-android.sh` — the build recipe (runnable locally if you
  have Rust + Android NDK + the 3 targets + `kotlinc` + a JNA jar)
- `.woodpecker/linux.yml` — the CI step that publishes the `.aar`
- `ci/images/ci-android/Dockerfile` — the CI image carrying `kotlinc` + JNA
