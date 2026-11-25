---
description: How Apple FFI artifacts are promoted from merged main commits: supported xcframework slices and the tag-driven promotion procedure.
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
workspace manifests, `.cargo/`, the rustc wrapper, or the script. The job fails
when the baseline differs from the current surface in any way. Downstream
consumers vendor these bindings, so a removed or changed item stops their code
from compiling at the next update. Without this check, such a change passes
ptpsim CI when no in-repo code uses the item.

The snapshot comes from the metadata that UniFFI embeds in a staticlib.
`ci/build-android.sh` generates the Kotlin bindings from a host build, and
`ci/build-xcframework.sh` generates the Swift bindings from the iOS device
release build. The FFI surface must be the same for every target and build
profile, and three comparisons check it:

- The `ffi-surface` job compares a host dev build (`ci/ffi-surface.sh`) and an
  `aarch64-apple-ios` release staticlib built on Linux
  (`ci/ffi-surface.sh check --target aarch64-apple-ios`). A staticlib needs no
  Apple linker or SDK.
- `ci/build-xcframework.sh` compares the exact iOS device staticlib it
  generates the Swift bindings from
  (`ci/ffi-surface.sh check --library <staticlib>`), so a promotion fails when
  the shipped surface differs from the committed one.

A difference that only the iOS comparisons report comes from target- or
profile-dependent code. Make the surface target-independent instead of
updating the baseline, because `update` reads the host build.

The host mode also runs `ffi-surface cfg-scan`, a source heuristic that names
the likely cause with a file and line. It flags build-dependent names
(`target_*`, `unix`, `windows`, `debug_assertions`, `panic`, and similar) in
`#[cfg]`, `#![cfg]`, the predicate of `cfg_attr`, `cfg!` and `cfg_select!`,
`#[path]` and `include!` that leave `src/`, and a build script. It ignores
comments and literals. It does not see predicates forwarded through
`macro_rules!` fragments, custom `--cfg` names set by cargo config or
`RUSTFLAGS`, or macros from dependencies; the iOS comparisons cover those. A
justified exception carries the comment
`// ffi-surface: allow-build-cfg <reason>` on the flagged line or the line
above it.

The snapshot also records every `crates/camera-protocol-ffi/uniffi.toml`
setting, including empty tables, and the `uniffi_bindgen` version from
`Cargo.lock`. One snapshot covers Swift and Kotlin because both bindings derive
their names from the Rust names by fixed case conversion. Records and enums
that reach an object are marked `contains_object`. The test is UniFFI's: an
object reached through fields, containers, custom types, other records and
enums, their methods and constructors, or callback interface methods. For such
types the Swift generator keeps `Equatable` only if the type exports `Eq`,
keeps `Hashable` only if it exports `Hash`, and drops `Codable` (when
enabled); the Kotlin generator adds `Disposable`.

Every difference fails the job. The report labels each difference so the
author knows whether the pull request breaks consumers:

| Change | Label |
| --- | --- |
| Item removed or renamed | Breaking for consumers |
| Function, constructor, or method signature changed (parameter names or types, return type, `async`, `throws`); a parameter that loses its default; a parameter added to a method | Breaking for consumers |
| Record field removed, reordered, its type changed, or its default removed; a field added without a default, or added before an existing field | Breaking for consumers |
| Record or enum starts or stops reaching an object (`contains_object`) and loses a Swift conformance it does not export, or Kotlin `Disposable` | Breaking for consumers |
| Record or enum starts reaching an object but exports `Eq` and `Hash` (and `Codable` generation is off) | Additive: Kotlin gains `Disposable` |
| Enum case added, removed, reordered, or its fields changed (UniFFI generates exhaustive Swift and Kotlin enums even for `#[non_exhaustive]`) | Breaking for consumers |
| Method added to a callback trait or callback interface | Breaking for consumers: foreign implementations must add it |
| Object kind or enum shape changed | Breaking for consumers |
| Binding setting or generator version added, removed, or changed | Breaking for consumers |
| Method added to a Rust-implemented object | Breaking for implementers: consumer types that conform to the generated Swift protocol or Kotlin interface must add it |
| Function or constructor default changed, a parameter gaining a default, or new trailing parameters that all have defaults; a method default changed or added on an existing parameter; a record field default changed or added | Source-compatible behaviour change: existing Swift and Kotlin call sites compile, callers that omit the value get the new default, and code that refers to a function as a value may need updating |
| New top-level item, new constructor, or a record field with a default appended after the existing fields | Additive |

Update the baseline in the same pull request as the change, so reviewers see
the surface change next to the code change:

```sh
ci/ffi-surface.sh update
git add crates/camera-protocol-ffi/ffi-surface.txt
```

Additions fail the job too. An item that is missing from the baseline is not
protected, and a later pull request could remove it without a failure.

The snapshot does not cover runtime behavior, manifest data, or binding code
that changes without a metadata change other than through a UniFFI upgrade
(the generator version line catches upgrades). A consumer still records the
ptpsim commit it vendors and reviews the baseline diff between its old and new
commit.

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
* `--module-name` CLI flag is gone in 0.31; it moved to `[bindings.swift] module_name = ...` in `crates/camera-protocol-ffi/uniffi.toml`.
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
