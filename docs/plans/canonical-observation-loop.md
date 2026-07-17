# Canonical fail-closed observation loop

Issue: #305

This plan records the implementation contract for the one supported evidence
loop:

```text
shipping initiator or simulator responder
  -> camera-observation/v1 JSONL
  -> validate
  -> deterministic proposal
  -> complete human review
  -> atomic manifest apply
  -> shipping initiator or simulator verification
```

The predecessor `camera-config-evidence/v1` format and the standalone mapper
are migration provenance only. `camera-observation/v1` is an exact schema
discriminator, not a compatibility range. The final validator has no legacy
parser.

## Contract changes

- Add a generated JSON Schema and a Rust observation model mirrored completely
  through FFI. A bundle begins with exactly one header and then contains typed
  lifecycle, BLE GATT, PTP transaction, PTP event, HTTP exchange,
  descriptor/capability reduction, and action-invocation records.
- Require stable run and record identities, unique ordinals, exact sanitized
  body and execution context, capture clocks and loss accounting, immutable
  artifact hashes/ranges, redactions, tool versions, and epistemic metadata.
  Large bodies carry a complete hash plus contiguous per-range hashes instead
  of an inline buffer.
- Keep transaction outcome, evidence basis, observed effect, and tagged
  readback independent. A successful response is never interpreted as proof of
  a write effect.
- Replace the legacy generator with `validate`, `propose`, `apply`, and
  `schema`. Every nonblank record receives one accepted or rejected
  disposition. Every accepted record is then linked to its candidate ids or
  marked evidence-only. Any rejection blocks proposal generation; apply
  recomputes proposal integrity, requires a digest-bound review disposition for
  every candidate, and writes atomically.
- Preserve observed `(connection, mode, state)` tuples as tuples. Never infer a
  Cartesian product from independent connection, mode, or state sets.
- Make proposals, diagnostics, accounting reports, and manifest output stable
  under input file and record reordering. Stable hashes bind candidates,
  proposals, and reviews; volatile timestamps and host paths are excluded.
- Split control provenance from effect as `ControlEvidenceBasis` and
  `ControlObservedEffect`; migrate only existing assertions.
- Give every manifest action one id with explicit initiator and optional
  responder role bindings. Shared parameters, triggers, and evidence stay
  declarative. Runtime resolution validates the catalog revision, connection,
  mode, role, and exact parameter set before any side effect.
- Rename the PTP executor entry point to state that it runs the initiator role.
  FFI conversions return errors instead of dropping unmappable steps or
  triggers.
- Expose the same catalog through FFI, simulator `GET /actions`, simulator
  `POST /actions/{id}`, and the TUI proxy. TUI-only phase changes and quit live
  under the reserved `operator:*` namespace.
- Record both initiator traffic and responder behavior into one canonical
  observation store. `GET /observations?after=<cursor>` is the durable export;
  `/trace` remains a bounded operator projection with dropped and truncated
  counts.

## Migration

Use a temporary, tested converter to rewrite every committed
`camera-config-evidence/v1` line as canonical descriptor/capability reductions.
Commit a deterministic accounting report proving input/output totals and
rejection count. Generate a proposal from the complete canonical corpus, bind a
complete review file to its digest, and regenerate the consolidated manifest
through `apply`. Remove the converter and legacy parser before review.

Property type/access, descriptor, label, and value-profile facts are separate
review candidates even when they share a property code. The committed migration
normalizes 40 enum observations for eight properties whose legal values differ
by connection or mode into scoped value profiles; it never unions them into a
global descriptor. Its review contains ten rejections: nine legacy type claims
superseded by existing payload-backed width overrides, plus the incompatible
`0xd246` descriptor. Independent global descriptors, labels, and value profiles
remain accepted, while the rejected `0xd246` descriptor defers to its curated
two-value selector contract.

The manifest migration preserves facts only: current PCSS rows already proven
by writes become `writeProbe` plus `confirmed`; descriptor-only rows become
`descriptorOnly` plus `unknown`. Camera-state conclusions remain owned by #193,
hardware capture work by #176, and general simulator state injection by #164.

## Verification and delivery

- Commit positive fixtures for a retrying multi-state PTP/IP lifecycle, a
  correlated `0x500d` write/readback, a migrated USB descriptor, and one action
  invoked in both roles. Commit negative fixtures for malformed and unknown
  input, correlations, hashes, loss/truncation, conflicts, and incoherent
  orthogonal result fields.
- Test accounting, schema drift, deterministic reordering, review binding,
  tuple preservation, bounded payload recording, zero-side-effect catalog
  failures, FFI exhaustiveness, observation cursor restart behavior, and TUI
  hotkey/HTTP parity from the fetched catalog.
- Run formatting, workspace clippy and tests, deterministic regeneration, and
  generated Swift/Kotlin typechecks. Review the exact candidate for fail-closed
  accounting, role separation, determinism, FFI completeness, public hygiene,
  and a single protocol authority. Address findings or file follow-ups before
  making the pull request ready.
