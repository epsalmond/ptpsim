---
description: How Apple FFI artifacts are promoted from merged main commits — supported xcframework slices and the tag-driven promotion procedure.
status: reference
read-when: Cutting or consuming an Apple FFI release, or changing the xcframework release pipeline.
---

# Apple FFI releases

Apple artifacts are promoted releases, not a side effect of every merge to
`main`. A release identifies a commit already merged to `main` and validated by
the normal workspace CI. Consumers pin the resulting `sha-<commit>` artifact;
they never follow a moving branch.

## Supported slices

`CameraProtocolFFI.xcframework` contains the supported iOS architectures:

| Slice | Purpose |
| --- | --- |
| `ios-arm64` | Physical iPhone and iPad devices |
| `ios-arm64_x86_64-simulator` | Apple Silicon and Intel iOS simulators |

macOS is not part of the current binary distribution. Add a macOS slice only
when the project intentionally supports and tests that platform.

## Promotion

Promote the current `origin/main` commit with:

```bash
git fetch origin main
SHA=$(git rev-parse origin/main)
git tag "apple-ffi-${SHA:0:8}" "$SHA"
git push origin "apple-ffi-${SHA:0:8}"
```

The workflow rejects a tag whose suffix does not match its commit or whose
commit is not an ancestor of `origin/main`. It builds each architecture in a
separate step on a GitHub-hosted Apple Silicon macOS runner, generates Swift bindings with the host-only binding tool, and
publishes the XCFramework archives and checksum to the commit's `sha-<commit>`
release.

Open the tag's `Apple FFI` Actions run to inspect its build and publication
steps. The workflow uses its `GITHUB_TOKEN` to upload assets; no external
runner or release-token secret is required. A maintainer or authorized external
client must push the promotion tag: tag pushes made with a workflow
`GITHUB_TOKEN` do not trigger another workflow. The resulting artifact still
requires the consumer validation described below.

## Consumer workflow

1. Select a successful `sha-<commit>` release whose commit is merged to
   `main`.
2. Download `CameraProtocolFFI-<commit8>.xcframework.zip` and its checksum.
3. Integrate that exact artifact into the consumer project.
4. Record the ptpsim commit ID and checksum with the dependency update.
5. Run the consumer's native build and integration validation before release.

`docs/SPM_INTEGRATION.md` describes the optional pinned Swift Package Manager
form. Floating dependencies such as the retired `release/auto` branch are
unsupported.

## FFI surface baseline

`crates/camera-protocol-ffi/ffi-surface.txt` records the public surface that
the generated Swift and Kotlin bindings expose. The `ffi-surface` job in
`.github/workflows/linux.yml` runs `ci/ffi-surface.sh` on Linux for every pull
request and `main` push that changes `crates/`, `tools/uniffi-bindgen/`, the
workspace manifests, or the script. It fails when the current surface removes
or changes a baseline item. Downstream consumers vendor these bindings, so
such a change stops their code from compiling at the next update. Without this
check, a removed or renamed item passes ptpsim CI when no in-repo code uses it.

The snapshot comes from the metadata that UniFFI embeds in the library, the
same input the binding generator reads for both languages. It also records
`crates/camera-protocol-ffi/uniffi.toml` binding settings and the
`uniffi_bindgen` version from `Cargo.lock`. One snapshot covers Swift and
Kotlin because both bindings derive their names from the Rust names by fixed
case conversion.

| Change | Result |
| --- | --- |
| Item removed or renamed | Breaking |
| Function, constructor, or method signature changed (parameters, parameter names, defaults, return type, `async`, `throws`) | Breaking |
| Record field added, removed, changed, or reordered | Breaking |
| Enum case added, removed, reordered, or its fields changed | Breaking, unless the enum is `non_exhaustive` (additions only) |
| Method added to a callback trait or callback interface | Breaking: foreign implementations must add it |
| Object kind, enum shape, binding setting, or generator version changed | Breaking |
| New top-level item, or a method or constructor added to a Rust-implemented object | Addition |

An intended break updates the baseline in the same pull request, so reviewers
see the surface change next to the code change:

```sh
ci/ffi-surface.sh update
git add crates/camera-protocol-ffi/ffi-surface.txt
```

Additions do not fail the job. They produce a warning annotation and a job
summary entry. Update the baseline for additions in the same pull request as
well: an addition left out of the baseline is not protected, and removing it
later passes the check. Running `update` for any later change also picks up
earlier unrecorded additions, and the baseline diff shows them.

The snapshot does not cover runtime behavior, manifest data, or binding code
that changes without a metadata change other than through a UniFFI upgrade
(the generator version line catches upgrades). The job reports differences; a
consumer still records the ptpsim commit it vendors and reviews the baseline
diff between its old and new commit.

## Build recipe (formerly manifest-schema §11.11)
The repo's existing `crates/camera-protocol-ffi` is already uniffi-annotated. P1 ships:

**Build artifact:** `CameraProtocolFFI.xcframework`, with `ios-arm64` and a
universal `ios-arm64_x86_64-simulator` slice. These cover physical iOS devices
and both common simulator architectures. macOS is not part of the current
binary distribution. The artifact is promoted explicitly from a commit already
merged to `main`; the current distribution authority is
`docs/APPLE_FFI_RELEASES.md`.

**Verified recipe:** the original macOS spike ran on 2026-06-01 against
uniffi 0.31, Xcode 26.3, and rustc 1.95.0. The recipe lives at
`ci/build-xcframework.sh`; `.github/workflows/xcframework.yml` invokes it.

```sh
# 1. compile each Apple target's staticlib
cargo build -p camera-protocol-ffi --release --target aarch64-apple-ios
cargo build -p camera-protocol-ffi --release --target aarch64-apple-ios-sim
cargo build -p camera-protocol-ffi --release --target x86_64-apple-ios

# 2. fat-combine the simulator architectures into one platform entry
mkdir -p out/lipo/ios-simulator
lipo -create \
  -output out/lipo/ios-simulator/libcamera_protocol_ffi.a \
  target/aarch64-apple-ios-sim/release/libcamera_protocol_ffi.a \
  target/x86_64-apple-ios/release/libcamera_protocol_ffi.a

# 3. generate Swift bindings.
#    NOTE: the host-only package keeps the large CLI dependency graph out of
#    all cross-compiled libraries.
#    `--library` is auto-detected from the .a path; module name comes
#    from crates/camera-protocol-ffi/uniffi.toml (CameraProtocolFFI).
mkdir -p out/swift
cargo build -p ptpsim-uniffi-bindgen --bin uniffi-bindgen
target/debug/uniffi-bindgen generate \
  -l swift -o out/swift \
  target/aarch64-apple-ios/release/libcamera_protocol_ffi.a

# 4. assemble the xcframework
mkdir -p out/xcf
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libcamera_protocol_ffi.a \
  -headers out/swift \
  -library out/lipo/ios-simulator/libcamera_protocol_ffi.a \
  -headers out/swift \
  -output out/xcf/CameraProtocolFFI.xcframework
```

**Drift vs the original draft (resolved):**
* `uniffi-bindgen-swift` (separate 0.28-era binary) → single `uniffi-bindgen` shipped by our crate; `-l swift` selects Swift, `-l kotlin` / `-l python` reach Android + Linux from the same binary (P2).
* `--module-name` CLI flag is gone in 0.31 — moved to `[bindings.swift] module_name = ...` in `crates/camera-protocol-ffi/uniffi.toml`.
* `--library` flag deprecated; auto-detected from the .a path.

**Distribution:** an `apple-ffi-<sha8>` promotion tag identifies a validated,
merged commit. GitHub Actions publishes the archive and checksum to that commit's
`sha-<sha8>` GitHub release. Consumers vendor and review that exact artifact;
they do not follow a moving release branch or build ptpsim inside their native
application pipeline.

**Manifest data bundling on the iOS side:** the iOS team copies `packages/camera-config-data/fuji/index.yaml` and `packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml` into the iOS app bundle as Resources at build time. `ConfigStore.fromManufacturerIndex(...)` reads them via standard Bundle APIs.

For a local reproduction, run `ci/build-xcframework.sh all` on a Mac with Xcode
and the three targets installed (`rustup target add aarch64-apple-ios
aarch64-apple-ios-sim x86_64-apple-ios`).
