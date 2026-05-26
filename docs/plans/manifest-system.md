# Manifest system architecture (cross-cutting)

Status: direction set 2026-05-26, structural details pending. Drives both the
ptpsim core and the iOS-adoption refactor (`ios-adoption.md`).

## Thesis
The per-manufacturer/model/firmware boundary becomes a **repo boundary**, not a
directory convention. That makes ptpsim's "no per-manufacturer code" guarantee
*structural* — brand data physically cannot pollute the engine — and turns
**ptpsim into a co-consumer of the manifest system, not its owner.** ptpsim stays
a pristine PTP engine; BLE / PTP-over-HTTP / port-knocking / any proprietary
establishment lives in the data repo and never touches it.

## Three-layer dependency DAG
- **Manifest data repo** (Apache-2.0) — facts only, per `manufacturer/model/fw`,
  in three sections:
  - *protocol* — opcodes/propcodes/framing/`workflows` (the how).
  - *capability* — what the camera has, for UIs to render (AF point count/grid,
    subject-detection modes, film simulations, …).
  - *establishment* — connection-bringup values + a direction-neutral workflow
    (BLE GATT UUIDs/handshake, PTP-over-HTTP, port-knock). Optionally split into
    a private overlay for access-gate-sensitive material.
- **Manifest engine** (MIT, standalone — extract `camera-manifest` so it does NOT
  depend on ptpsim). The shared dependency: ptpsim depends on it; client application +
  android/mac/linux depend on it (via FFI). Does the resolution code.
- **ptpsim** (MIT) — PTP engine, a consumer.

## Pure data + finite-vocabulary engine (the "light computation" line)
The manifest is declarative data; the engine has a **fixed** set of computations
and the manifest only selects/parameterizes among them. If the manifest ever
needs conditionals/loops, that's the inner-platform signal — add a named
primitive instead (same rule as `protocol-primitives`). The vocabulary:
- **Specificity fallback** — `fuji/gfx100ii/2.30` → `fuji/gfx100ii/*` → `fuji/*`
  → root; most-specific-wins.
- **Semver resolution** — declared version ranges (`1.30` bug, `[1.31,2.0)`
  fixed, `2.0` intentional change); engine matches concrete fw.
- **SOC tier** — facts at `xprocessor5` apply to all its bodies. SOC membership
  is **data** (`gfx100ii: {soc: xprocessor5}`); using the SOC tier in fallback
  order is engine code.
- **Value-policy** — `fixed(value)` / `generated(scheme, persist)` /
  `from-pairing(source)`. Unifies init tail (fixed, model tier), initiator
  GUID/name (fixed, manufacturer tier — NOT app code), per-pairing IDs
  (from-pairing). `generated:uuidv4` = data telling the engine to run its
  generator.

Client API shape: `resolve(fuji/gfx100ii/2.30, af.tracking_points)` → answer.
Apps never branch on model/fw; they ask. Fix a camera bug once in data → every
client benefits. Add a model → edit data, no code.

## Capability vs mechanism
Two sections, two consumers: the app reads **capability** to draw controls; the
engine reads **mechanism** (op/encoding) to act; `control_for` bridges them.

## Operational
- **OTA needs signing.** Manifests drive what bytes a client emits — a malicious
  manifest = arbitrary-opcode injection. Signed manifests + bundled baseline.
- **You don't need 100% data before cutover.** Specificity fallback resolves
  under-specified models to manufacturer defaults; enable the mechanism now,
  ship enough for GFX100 II, fill the corpus incrementally.
- **Licensing:** MIT for both code layers (engine + ptpsim); Apache-2.0 for the
  data (patent grant is a plus for interop). Discovery narrative stays private
  (spec-vs-discovery boundary); only facts go in the public data repo. (Not legal
  advice — the structure follows the standard interop posture.)

## Open structural questions
- Engine crate name/extraction path (`camera-manifest` today lives in ptpsim).
- Data-repo layout + how the FFI loads/updates (bundled + OTA) data.
- Establishment driver home (a generic sans-io bringup interpreter reusing the
  engine + workflows) vs leaving establishment entirely to clients for now.
- Public vs private establishment overlay policy per manufacturer.
