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

## Pull requests

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

## Scope

ptpsim aims to be a generic, manifest-driven PTP/IP responder. It does not
ship per-manufacturer code paths. New cameras are added by authoring a manifest
(see [`packages/camera-config-data/`](packages/camera-config-data/)), not by
patching the engine.

Probing real cameras is folding into the shipping engine itself — a headless
initiator built on the same crates and manifests (#252) — so probe results can
never drift from what the engine actually does. The earlier standalone probe
toolkit ([`epsalmond/camera-protocol-mapper`](https://github.com/epsalmond/camera-protocol-mapper))
produced the JSONL observation bundles behind existing manifests but is no
longer where new probe work should land.

## Security

If you find a security issue, please open a private advisory on GitHub rather
than a public issue.
