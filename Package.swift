// swift-tools-version: 5.9
//
// CONSUMER PINNING:
//
//   .package(url: "https://github.com/epsalmond/ptpsim", branch: "release/auto")
//
// resolves to the latest Package.swift on the `release/auto` branch, which CI
// updates automatically after each successful xcframework build with the
// matching release URL + SHA-256 checksum. Cheapest path for client application: SPM
// re-resolves and you get the latest CameraProtocolFFI without writing your
// own binaryTarget snippet.
//
// To pin a specific release: replace `branch:` with `revision:` and the
// commit SHA on `release/auto` whose update message names the desired sha-<8>
// release tag. Or — for one-shot pinning without depending on this Package.swift
// at all — use `ci/spm-snippet.sh <sha-tag>` to render a self-contained
// `.binaryTarget(...)` block (see docs/SPM_INTEGRATION.md).
//
// The Package.swift on `main` is a placeholder — its checksum below is sentinel
// zeros that point at no real release; `swift package resolve` against `main`
// will fail. ALWAYS depend on `release/auto` (or a pinned `release/auto`
// commit), never `main`. CI's bump step rewrites this file to point at each
// release; see ci/update-package-swift.sh + .woodpecker/xcframework.yml.

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
