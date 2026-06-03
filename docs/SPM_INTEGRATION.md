# SPM integration — consuming `CameraProtocolFFI` via Swift Package Manager

How client application (or any Swift consumer) wires up `CameraProtocolFFI.xcframework`
as a Swift Package Manager dependency from a published release.

## Why we don't ship a `Package.swift` at the repo root

SPM `binaryTarget(url:, checksum:)` requires the checksum to match the
**specific** artifact URL — and a `Package.swift` committed at SHA `X`
cannot know the checksum of the build that comes from SHA `X` (chicken
and egg). Projects that pin a `Package.swift` at HEAD either (a) ship
a release-please-style flow that updates `Package.swift` in a follow-up
commit after each release, or (b) accept that `Package.swift` is "one
release behind."

We've deferred that automation (tracked as task #38 follow-up). For now,
**consumers write their own `Package.swift` snippet** pointing at a
specific release. The release ships every byte you need:

| asset | role |
|---|---|
| `CameraProtocolFFI-<sha8>.xcframework.zip` | the artifact SPM downloads |
| `CameraProtocolFFI-<sha8>.checksum` | SHA-256 hex of the zip — paste into your `Package.swift` |
| `CameraProtocolFFI-<sha8>.tar.gz` | legacy / non-SPM consumers (e.g. `curl + tar -xzf` into Xcode project) |

## Consumer integration

### One-time setup — pick a release

Every push to `main` triggers a build that publishes a release at
`sha-<8-char-commit>`. Pick the release you want to consume (typically
the latest green build) and grab its checksum:

```bash
TAG=sha-<8>    # e.g. sha-cded3f95
gh release download "$TAG" \
    --repo epsalmond/ptpsim \
    --pattern '*.checksum' \
    --dir /tmp
CHECKSUM=$(cat /tmp/CameraProtocolFFI-*.checksum)
echo "$CHECKSUM"
```

### Your `Package.swift` snippet

```swift
// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "YourApp",
    platforms: [.iOS(.v15), .macOS(.v12)],
    dependencies: [],
    targets: [
        // ... your app targets ...
        .binaryTarget(
            name: "CameraProtocolFFI",
            url: "https://github.com/epsalmond/ptpsim/releases/download/sha-<8>/CameraProtocolFFI-<8>.xcframework.zip",
            checksum: "<PASTE THE CHECKSUM HERE>"
        ),
    ]
)
```

Where `<8>` is the 8-char SHA from the release tag you picked, and the
`<PASTE THE CHECKSUM HERE>` is the hex from the `.checksum` asset.

### One-liner: render the snippet from a release tag

`ci/spm-snippet.sh` fetches the checksum and emits a ready-to-paste
`binaryTarget(...)` block:

```bash
./ci/spm-snippet.sh sha-cded3f95
```

```
.binaryTarget(
    name: "CameraProtocolFFI",
    url: "https://github.com/epsalmond/ptpsim/releases/download/sha-cded3f95/CameraProtocolFFI-cded3f95.xcframework.zip",
    checksum: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
)
```

Paste that into your `Package.swift`'s `targets:` array. (`gh` CLI must be
authenticated against `epsalmond/ptpsim`.)

### Bumping to a new release

Re-run `ci/spm-snippet.sh <new-tag>` and replace the `.binaryTarget(...)`
in your `Package.swift`. SPM will re-resolve on the next `swift build` /
`swift package resolve`.

## What about the manifest YAMLs?

The xcframework gives you the FFI surface (`ConfigStore`, `Action`,
`Step`, etc.). The actual camera knowledge — `gfx100ii.yaml`,
`gfx100ii.consolidated.yaml`, `fuji.yaml` — is data the FFI consumes,
**not** part of the binary. Today consumers vendor those files from
`packages/camera-config-data/` in the source tree separately; co-shipping
them with the release is task #39 (decision: roll them into the
xcframework workflow's release vs. wait for the Apache data repo split
per `project-manifest-system`).

## See also

- `docs/INTEGRATION.md` — the full integration story (FFI surface,
  binding generation, `Action` / `Step` grammar, etc.)
- `docs/plans/action-verbs.md` — the `actions:` block contract
- `ci/build-xcframework.sh` — the build recipe (run locally to reproduce
  a release-shaped artifact)
- `.woodpecker/xcframework.yml` — the publishing workflow
