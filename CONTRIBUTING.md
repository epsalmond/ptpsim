# Contributing to ptpsim

Thanks for taking the time to contribute.

## License of contributions

ptpsim is dual-licensed under MIT OR Apache-2.0 (see [`LICENSE-MIT`](LICENSE-MIT)
and [`LICENSE-APACHE`](LICENSE-APACHE)).

**By submitting a pull request, you agree that your contribution is licensed
under the same terms as the project: MIT OR Apache-2.0, at the recipient's
option.** This is the standard "inbound = outbound" rule used across the Rust
ecosystem. If for any reason you cannot grant a patent license under the
Apache-2.0 terms for code you are submitting, do not submit it.

If you are submitting on behalf of an employer, you are also asserting that you
have the authority to license the contribution under these terms.

## Filing issues

Bug reports and feature ideas are welcome. Please include enough to reproduce
(versions, OS, the input that triggered it, the observed vs. expected behavior).
For wire-protocol bugs, a minimal capture or transcript is the gold standard.

If implementation is blocked on camera or protocol behavior that is not yet
established, maintainers use the `needs:protocol-evidence` label. An
evidence-gated issue must state:

- the literal protocol question and the user-visible capability it serves;
- camera model, firmware, connection persona, mode, and relevant state;
- existing public anchors and the exact missing observation;
- safety constraints, including whether hardware work is authorized; and
- the completion condition that resolves the evidence gate.

Maintainers post the canonical request comment from
[`docs/consults/README.md`](docs/consults/README.md) before applying the label.
The delimited literal question is the durable request identity; changing it
supersedes the earlier request rather than silently retargeting existing work.

`ready` and `needs:protocol-evidence` are mutually exclusive on one
implementation issue. An external evidence provider may investigate a labeled
issue, but it does not edit any tracked ptpsim artifact directly. The public
issue remains its only handoff into the normal contributor workflow: it
receives the scoped answer, uncertainty boundary, and any public reduced
fixture needed for review. A result that enables implementation transitions to
`ready`; a sufficient negative result resolves or re-scopes the issue without
`ready`; an inconclusive result keeps the evidence label and records the
smallest missing observation.
Private commands, host paths, raw captures, internal role names, and private
fact identifiers do not belong here. A neutral supporting-analysis issue link
is acceptable only when no reader needs access to it to understand or implement
the result.

## Pull requests

Cargo uses `sccache` automatically when it is available on `PATH`. Builds call
`rustc` directly when `sccache` is unavailable.

- If CI is red, diagnose it with [`docs/CI.md`](docs/CI.md): an `error`
  status means the pipeline never ran your code.
- Run `cargo fmt --all` and `cargo clippy --workspace --all-targets` before
  pushing. Workspace warnings are denied through `.cargo/config.toml`.
- Run `cargo test --workspace` and make sure it passes.
- Keep commits focused. A bug fix doesn't need surrounding cleanup; a one-shot
  doesn't need a helper. Three similar lines is better than a premature
  abstraction.
- Match the surrounding style. New code goes into a script, not a one-off
  inline blob.
- Comments say *why*, names say *what*. A comment that narrates the next line
  should be deleted or replaced with an expressive name. Reserve comments for
  facts the code cannot show: wire evidence, spec constraints, invariants owed
  to another layer.
- Merging: once the review workflow in [`AGENTS.md`](AGENTS.md) is complete and
  checks pass, the PR author merges. External contributions are merged by a
  maintainer after the same review.

## Scope

ptpsim aims to be a generic, manifest-driven PTP/IP responder. It does not
ship per-manufacturer code paths. New cameras are added by authoring a manifest
(see [`packages/camera-config-data/`](packages/camera-config-data/)), not by
patching the engine.

Real-camera probing lives in the shipping engine: a headless initiator built on
the same crates and manifests (#252). New probe work belongs there so its
results cannot drift from engine behavior.

New evidence uses `camera-observation/v1`. Run `camera-config-generate validate`
before proposing a manifest change, inspect the deterministic output from
`propose`, and record `accept`, `reject`, or `defer` for every candidate in a
digest-bound review file. `apply` is the only supported generated-data write
path. It applies accepted candidates, validates the result, and replaces the
destination atomically. Unsupported, malformed, lossy, or unaccounted input is
an error; it must never be filtered out or converted into a partial manifest.

## Security

If you find a security issue, please open a private advisory on GitHub rather
than a public issue.
