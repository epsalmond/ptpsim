---
description: GitHub Actions checks, release artifacts, and private image publication.
status: reference
read-when: Diagnosing a CI failure or changing a CI workflow.
---

# CI workflows

GitHub Actions runs public build and test work on hosted runners. Workflow
files live in [`.github/workflows/`](../.github/workflows/):

- `linux.yml` runs smoke, public-content, commit-message, documentation,
  workspace, FFI surface, and Kotlin binding checks on pull requests and `main`
  pushes. Android libraries, the Python wheel, and manifest data publish from
  `main`. The FFI surface job and its baseline are described in
  [APPLE_FFI_RELEASES.md](APPLE_FFI_RELEASES.md#ffi-surface-baseline).
- `xcframework.yml` promotes `apple-ffi-<sha8>` tags on hosted Apple Silicon
  macOS. The tag must identify a commit already merged to `main`.
- `oci-image.yml` builds the service image without pushing it on pull requests
  and `main` pushes. A default-branch build populates the shared BuildKit cache.

Branch protection on `main` lists the required status checks. A job that its
path filter skips reports `skipped`, which satisfies a required check, so
path-filtered jobs can be required.

Fork pull requests use read-only repository access. Build and test jobs need
no repository secrets. Release jobs use the workflow's `GITHUB_TOKEN` with
job-scoped `contents: write` permission and run only from trusted pushes.

Rust jobs cache Cargo dependencies with `Swatinem/rust-cache`. Cache keys
include `Cargo.lock`; a lockfile change can require a cold build. OCI dry runs
use BuildKit's GitHub Actions cache. Changed-file selection limits expensive
jobs while preserving stable check names. A commit message containing `[ALL]`
requests all applicable checks. The OCI workflow also supports manual dispatch.

The documentation job rejects an em dash (U+2014) on any line of a tracked
Markdown file. Code spans and fenced code blocks are exempt, and so is
`.agents/plans/`, which holds dated agent plan records that are kept as
written; the exclusion list is `em_dash_excluded` in `scripts/lint-docs.sh`.
The script still accepts `--base <revision>` so the workflow's existing call
works, but the option has no effect. `scripts/lint-docs.sh --self-test`
exercises the check in a scratch repository.

The check is not a full Markdown parser. It flags em dashes in indented code
blocks, in fences inside blockquotes, and in fences indented four or more
spaces inside list items. Move such a block to a fenced block at the left
margin, or reword the line.

Android setup uses the hosted runner's SDK at `ANDROID_HOME`. It invokes
`cmdline-tools/latest/bin/sdkmanager` within that SDK because the command-line
tools directory is not on the runner's `PATH`. The setup installs the pinned
NDK and Android platform before exporting `ANDROID_NDK_HOME` for the build.

## Release assets

Release jobs check out the exact triggering commit. Main-branch artifact jobs
and Apple promotions share the `sha-<sha8>` GitHub release for that commit.
`ci/ensure-github-release TAG TARGET_COMMIT` creates that release or observes a
concurrent creator. It verifies that the release tag resolves to the full
triggering commit. Parallel jobs upload only their own named assets.

| Asset | Meaning |
| --- | --- |
| `CameraProtocolFFI-<sha8>.aar` | Android native libraries and Kotlin bindings |
| `CameraProtocolFFI-android-<sha8>.checksum` | SHA-256 of the Android AAR |
| `CameraProtocolFFI-<sha8>.xcframework.zip` | Apple binary distribution |
| `CameraProtocolFFI-<sha8>.checksum` | SHA-256 of the Apple ZIP for Swift Package Manager |

The Android qualifier prevents its checksum from overwriting the Apple
checksum when both platforms publish the same commit. Apple promotion and
consumer validation are described in [APPLE_FFI_RELEASES.md](APPLE_FFI_RELEASES.md).

## Private image publication

[`.woodpecker/oci-image.yml`](../.woodpecker/oci-image.yml) retains the private
registry publish, verify, and smoke sequence for `main` pushes. The registry
credentials and deployment settings remain the deployment owner's
responsibility. GitHub Actions does not publish or mirror that image.

The portable toolchain image recipes in `ci/images/` remain available to
external build infrastructure. Hosted workflows install their own tools and do
not depend on those images. The legacy macOS agent provisioning files under
`ci/ansible/` are retained for existing operators; hosted Apple releases do not
use them.

## Failure diagnosis

For Actions, open the failing job and read its first failing step. Validate
workflow edits locally with `actionlint`. A canceled superseded run does not
establish whether the current head passes.

Woodpecker `failure` means a step ran and exited nonzero. `error` can mean the
configuration could not be fetched or validated, or the runner refused the
job. Inspect the pipeline before changing source. A pipeline with no steps
has no application failure log. Rerunning a configuration-fetch error reuses
the stored configuration; a maintainer must create a fresh run instead.
Validate the retained workflow with
`woodpecker-cli lint .woodpecker/oci-image.yml` before pushing an edit.
Instance diagnostics belong to the operator's infrastructure documentation.

When an explicit wait is required and the shared waiter is installed, run
`wait-for-status gh-checks <PR-number>` once for PR checks or
`wait-for-status woodpecker-deploy <pipeline-URL>` once for an existing private
publication pipeline. The waiter retains failure logs and does not create or
move release tags. Actions run pages remain available to contributors without
the shared helper.

## Migration acceptance

Issue [#527](https://github.com/epsalmond/ptpsim/issues/527) remains open until
hosted PR runs demonstrate warm-cache timings and an actual `apple-ffi-*` tag
produces the expected Apple release assets. Local workflow validation does
not establish either result. Confirm that Woodpecker runs only private image
publication after cutover.
