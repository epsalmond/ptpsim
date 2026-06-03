// swift-tools-version: 5.9
//
// ⚠️  DO NOT depend on this file at `main` / a tagged commit / any other ref.
//     The URL below is a PLACEHOLDER (`sha-PLACEHOLDER`) and the checksum is
//     sentinel zeros; `swift package resolve` against this file fails with
//     `badResponseStatusCode(404)` from a non-existent release asset.
//
// CONSUMERS — pin the `release/auto` branch:
//
//   .package(url: "https://github.com/epsalmond/ptpsim", branch: "release/auto")
//
// `release/auto` is CI-managed: every successful xcframework build pushes
// a fresh Package.swift to that branch with the matching release URL +
// SHA-256 checksum. SPM re-resolves on each `swift package resolve` and you
// get the latest CameraProtocolFFI without writing your own binaryTarget.
//
// To pin a specific release on `release/auto`, use `revision:` with the
// commit SHA whose message names the desired `sha-<8>` tag.
//
// To pin a specific release WITHOUT depending on `release/auto`, render a
// self-contained `.binaryTarget(...)` snippet with
// `ci/spm-snippet.sh <sha-tag>` and paste it into your own Package.swift.
// See `docs/SPM_INTEGRATION.md`.
//
// The placeholder-rewrite lives in `ci/update-package-swift.sh` (driven by
// `.woodpecker/xcframework.yml`'s `bump-package-swift` step).

import PackageDescription

let package = Package(
    name: "CameraProtocolFFI",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: [
        .library(name: "CameraProtocolFFI", targets: ["CameraProtocolFFI"]),
    ],
    targets: [
        .binaryTarget(
            name: "CameraProtocolFFI",
            url: "https://github.com/epsalmond/ptpsim/releases/download/sha-PLACEHOLDER/CameraProtocolFFI-PLACEHOLDER.xcframework.zip",
            checksum: "0000000000000000000000000000000000000000000000000000000000000000"
        ),
    ]
)
