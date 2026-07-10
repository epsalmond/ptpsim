# Pinned SPM integration

`CameraProtocolFFI.xcframework` is published only for explicitly promoted,
merged ptpsim commits. Do not depend on `main`, `release/auto`, or another
moving branch. Consumers should select and review a specific archive as
described in `docs/APPLE_FFI_RELEASES.md`.

Each promoted `sha-<8>` release contains:

| Asset | Role |
| --- | --- |
| `CameraProtocolFFI-<8>.xcframework.zip` | iOS device and universal simulator binary |
| `CameraProtocolFFI-<8>.checksum` | Swift Package Manager checksum |
| `CameraProtocolFFI-<8>.tar.gz` | Direct-vendoring archive |

For a consumer that intentionally uses Swift Package Manager, render a pinned
binary target from the selected release:

```bash
./ci/spm-snippet.sh sha-cded3f95
```

The output belongs in the consumer's `Package.swift` `targets` array:

```swift
.binaryTarget(
    name: "CameraProtocolFFI",
    url: "https://github.com/epsalmond/ptpsim/releases/download/sha-cded3f95/CameraProtocolFFI-cded3f95.xcframework.zip",
    checksum: "<checksum from the same release>"
)
```

The XCFramework contains native code and generated Swift bindings. Camera
manifest YAML remains a separately versioned release asset and must be pinned
to the compatible ptpsim commit as part of the consumer's vendor update.

See `docs/INTEGRATION.md` for the FFI surface and local binding generation.
