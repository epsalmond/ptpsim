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
separate step, generates Swift bindings with the host-only binding tool, and
publishes the XCFramework archives and checksum to the commit's `sha-<commit>`
release.

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
