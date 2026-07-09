# Apple FFI releases

Apple artifacts are promoted releases, not a side effect of every merge to
`main`. A release must identify a commit already merged to `main` and already
validated by the normal workspace CI. Consumers pin the resulting
`sha-<commit>` artifact; they never follow a moving branch.

## Supported slices

`CameraProtocolFFI.xcframework` contains only the platforms ptpsim's current
consumer needs:

| Slice | Consumer |
| --- | --- |
| `ios-arm64` | iPhone and iPad builds, including TestFlight |
| `ios-arm64_x86_64-simulator` | Apple Silicon development and the Intel macOS screenshot runner |

There is no ptpsim macOS application or current macOS FFI consumer, so macOS
static libraries are not release artifacts. Add a macOS slice only when a real
consumer owns and tests that contract.

client application vendors a reviewed XCFramework from a specific ptpsim release. Its
TestFlight and screenshot pipelines consume that checked-in artifact and must
not build ptpsim or resolve an unmerged ptpsim revision.

## Promotion and timing policy

The Apple workflow is triggered explicitly for a merged commit. It builds each
Rust target and packages the framework in separate measured steps. Every step
has a five-minute hard limit. Lower thresholds are build-time ratchets: crossing
one fails the Apple workflow. A dependent NAS-side workflow publishes that
failure through the global `notify.alert.*` NATS path, where it reaches the
alert Discord channel. NATS remains private to the management network rather
than being exposed to the macOS runner.

Promote the current `origin/main` commit with:

```bash
git fetch origin main
SHA=$(git rev-parse origin/main)
git tag "apple-ffi-${SHA:0:8}" "$SHA"
git push origin "apple-ffi-${SHA:0:8}"
```

The workflow rejects a tag whose suffix does not match its commit or whose
commit is not an ancestor of `origin/main`.

The Rust 1.97 cold-run baselines measured on the four-core Intel runner are
2m28s for device arm64, 1m55s for simulator arm64, 1m37s for simulator x86_64,
3m18s for the host binding generator, and 12s for packaging. Warning ratchets
leave cold-run headroom below the five-minute hard limit; warm-cache runs are
expected to be substantially faster.

Host-only binding-generator dependencies must not be present in an Apple target
build. If a cold target build cannot stay below five minutes after keeping that
graph narrow, dependency compilation moves to a separately versioned cache
bundle keyed by the Rust toolchain, Cargo lockfile, target, and build profile.
The release workflow restores that bundle and compiles only ptpsim sources.

## Consumer workflow

1. Select a successful `sha-<commit>` release whose commit is merged to
   `main`.
2. Download `CameraProtocolFFI-<commit8>.xcframework.zip` and its checksum.
3. Vendor the archive and generated Swift source in the consumer repository.
4. Commit the ptpsim commit ID and checksum with the vendored files.
5. Run the consumer's native build and screenshot validation before merge.

`docs/SPM_INTEGRATION.md` describes the optional pinned Swift Package Manager
form. Floating `release/auto` dependencies are unsupported.
