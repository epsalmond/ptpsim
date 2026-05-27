# Implementation Plan: `camera-config` — standalone camera manifest/config system with modes as a first-class axis

Status: decisions **locked** (see below); remaining open items are build-time
choices, not blockers. Revised 2026-05-26 (integrates an external hole-poking
review — see "How the review reshaped this"). Grounded in `DESIGN.md` (Manifest
Model + Transport/Mode matrix), `docs/TRANSPORTS.md`, `docs/plans/manifest-system.md`,
`docs/plans/ios-adoption.md`, and the current `crates/camera-manifest/` source.
A delta on what exists, not greenfield.

## Context (orientation)

**What this is.** `camera-config` is the data + engine that answers per-camera
questions — "does this model/fw/mode support operation X?", "how do I encode
aperture?", "which camera is this BLE advert?", "what modes exist over USB?". It
is the single source of camera-protocol truth that every client queries instead
of hardcoding.

**Why it exists.** ptpsim's premise is *no per-manufacturer code* — adding a
camera is a data change, never a new crate. Today that data lives in a
`camera-manifest` crate *inside* ptpsim. This plan extracts it so the boundary
between engine code and camera data is a **repo boundary**, and makes ptpsim a
*consumer* like everyone else. client application (iOS, then macOS/Android/Linux) is the
other consumer — see `ios-adoption.md`, which depends on this.

**Scale & threat model (write this down — it decides several arguments below).**
This is an **embedded library + a small, curated data corpus** — on the order of
a handful of manufacturers, authored and reviewed by us, shipped as a **signed
bundle** to first-party clients. It is **NOT** a public multi-tenant endpoint,
**NOT** a runtime ingestion point for untrusted third-party manifests, **NOT**
serving thousands of vendors. Consequence: **optimize for auditability and
simplicity over open-ended expressivity.** Because the data is curated and signed
and drives what bytes a client puts on the wire, a closed declarative grammar
beats a scripting sandbox, and a simple data-selected version comparator beats a
regex/AST engine — precisely *because* of this scale, not in spite of it. (If this
were ever a public ingestion endpoint, the calculus would change. By design it
isn't.)

**Three constraints inherited from the wider design (don't violate):**
- **Sans-io.** The engine does no I/O. It answers from data and evaluates
  predicates over values the I/O-owning client feeds it.
- **Two repos, two licenses.** Engine = MIT (`camera-config`); data = Apache-2.0
  (`camera-config-data`). Only protocol *facts* go in the public data repo; RE
  *narrative* stays private (spec-vs-discovery).
- **Bounded declarative data, no procedural DSL.** The engine has a fixed set of
  computations; the data only selects/parameterizes them. The line is precise:
  **declarative predicates and value-policies are fine and expected; procedural
  control flow (loops, sequencing, computed branches) is the trap.** A closed
  boolean predicate over observed state is data, not a program.

## Decisions locked (baked in; the rest of the doc elaborates)

1. **Names.** Engine `camera-config`; data repo `camera-config-data`; the RE/
   characterization tool renamed `camera-probe` → **`protocol-mapper`**. Never
   ship a `cc` binary.
2. **Extract + rename + split immediately** — no real consumers yet, so this is
   step 1, not a deferred churn.
3. **Two axes.** *What is it?* = body (`mfr/model/soc/fw`). *Where is it?* = camera
   state. The engine resolves facts keyed by (What, Where).
4. **Connection and mode are ORTHOGONAL axes — not nested.** Both are "Where"
   (connection is a user-selected state too), but mode capabilities are largely
   transport-independent, so nesting modes under connections would duplicate them.
   The Where-coordinate is the **pair `(connection, mode)`**. Gating intersects the
   two; it does not tree them. *(Revised from an earlier connection-as-root model.)*
5. **Gating is a flat catalog of predicates, not a tree walk.** An operation/
   feature declares `modes: [Shooting/Stills]` (path-prefix matched, so hierarchy
   gives inheritance), optionally `connections: [App, USBTether]` (default = all),
   and optionally `requires: <predicate>` (a runtime prerequisite over observed
   props — e.g. card-inserted, not-writing-buffer). This avoids pseudo-mode
   explosion (`Stills/NoCard`, `Stills/Writing`).
6. **Modes are hierarchical paths** (`Shooting/Stills`) — for gate inheritance
   (prefix match), UI grouping, and `detect` attachment. A **mode graph** of
   action-bearing **mode-entry** edges (optionally `from`-qualified) describes
   transitions; an edge carries **wire actions OR a user-instruction** (connection
   switches usually can't be app-driven, only requested). A wire action is a closed
   `Step` vocabulary — `setProp`/`getProp`/`readEcho`/`sendOp`, each with optional
   `tolerant` (a non-OK PTP *response* is logged + swallowed; only transport failure
   aborts) and `sendOp` `params` that are **literals or a named runtime slot**
   (`{runtime: openCaptureTxId}`) the I/O-owning app binds from its session state
   (cf. value-policy `from-pairing`). **The no-DSL line:** named runtime slots are
   data; arithmetic, branches, or loops over them would be the script trap — not added.
7. **A workflow ≠ a mode.** Mode = camera state (gates). Workflow = app-side user
   task that *traverses* the mode graph (map vs route).
8. **One closed predicate grammar** serves both `requires` (gating) and `detect`
   (mode determination): leaf = property comparison (`eq`/`ne`/`lt`/`gt`, optional
   `mask`), connectives = `all`/`any`/`not`. Finite, total, side-effect-free —
   **not** an embedded script engine (that would make a signed, wire-driving
   artifact Turing-complete and unauditable; wrong trade at this scale).
9. **Schema = hard-nested sections** (`identification`/`protocol`/`capability`/
   `establishment`) + a flat `modes` table + a `connections` table — so sensitive
   `establishment` can ship as a private overlay while the rest is public.
10. **Resolution is a continuous funnel, not rigid phases.** Feed the engine any
    observed facts (VID/PID, mDNS TXT, BLE advert, `GetDeviceInfo`, observed
    props); it returns the **narrowest config + residual ambiguity**. Identification
    (signal→target) and config resolution are the *same* query. Invariant: minimal
    facts must still yield usable bring-up config (fw-agnostic facts resolve early).
11. **Firmware is a literal string** (`"2.30"` Fuji, `"1.2.3"` Canon) — identity is
    always the raw string. Ordering for ranges is a **data-selected named comparator**
    (default dotted-integer, variable-arity, component-wise within a manufacturer)
    that **fails soft**: an unparseable version falls back to exact-string match,
    never panics or drops data.
12. **Signing is packaging-layer** (CI/CD signs the bundle, verify-before-extract +
    a signed sha256 index), not per-record.
13. **The connection/mode matrix is filled empirically** by `protocol-mapper`
    against a real camera; the sketches below are placeholders until that lands.
14. **Composition is by id-keyed reference, never body→body.** Connections/modes
    are records with stable ids; a body lists the ones it has by `ref`. The engine
    loads all id-keyed records into a namespace — *where* the YAML physically lives
    (body file / manufacturer file / a shared `connections/xlv-http.yaml`) is
    irrelevant to resolution. So an interface shared across bodies (e.g. the XLV/HTTP
    surface the GFX100 II has built-in and the X-H2S gets via the FT-XH adapter) is
    one shared record both bodies reference — *not* one body importing another's
    manifest (that makes a body a hidden dependency and is the trap).
15. **Connection availability is conditional.** A body's connection `ref` may carry
    `availableWhen: { firmware: { lt|gt|... } }` (evaluated via the §4 version
    comparator — e.g. instax-printer, present ≤fw2.30, removed at fw2.40) and/or
    `requiresHardware: <id>` (e.g. FT-XH provides XLV on the X-H2S). Deferred as
    *data*, present as *mechanism*.
16. **Value-set source = `camera` | `manifest`.** A shared connection carries the
    *mechanism* (op/prop to set a value); the allowed *value set* is sourced either
    from the camera at runtime (`source: camera`, via DevicePropDesc — authoritative)
    or declared per-body (`source: manifest`, values:[…]). **Runtime-discovered beats
    manifest-declared:** don't hand-maintain what the camera enumerates; the manifest
    fills only what the camera doesn't report (labels, gating, non-enumerated sets).
    This is how GFX100 II's 8K vs X-H2S's 6.2K recording modes differ over the *same*
    XLV mechanism without code. Fits sans-io (client reads the desc, feeds the enum).

**Build the vocabulary now, populate GFX100 II only.** The core invariant is
"a new body is a data PR, never a code change" — so the engine must be able to
*express* shared/conditional/runtime-sourced records before a second body lands,
or we break that invariant the moment we add the X-H2S. This is not speculative:
**GFX100 II alone exercises every mechanism** — instax (fw-gated connection),
XLV/HTTP (id-keyed shared connection), recording modes (camera-vs-manifest value
source) — so all of it is buildable and testable today against the one body we own.

## How the review reshaped this (so the next reviewer sees the reasoning)
An external no-context review pushed five points. Outcome: orthogonal
connection/mode (#4, adopted), property prerequisites (#5, adopted), one predicate
grammar (#8, adopted — but rejecting the reviewer's embedded-scripting fix on
auditability+scale grounds), continuous-funnel resolution (#10, adopted, simplifies),
and fail-soft data-selected version ordering (#11, adopted — lighter than the
reviewer's regex proposal because the real manufacturers use clean schemes and the
corpus is curated). The scale/threat-model section above is the context the review
lacked.

## 1. Current state (what we're a delta on)
Engine at `crates/camera-manifest/`. `model.rs` (serde: `transports`,
`operations`, `properties`, `workflows`, `media`, `events`, `quirks`), `query.rs`
(three queries: `supports_operation(workflow, code)`, `control_for(prop, mode)`,
`value_label`), `lib.rs` (`SCHEMA_VERSION = "camera-manifest/v1"`), `generate.rs`
(probe `ptpip.fact` → proposal; reads `transport`+`mode` but flattens them).
Gaps closed here: (1) no resolution funnel / version ordering / SOC tier /
value-policy; (2) `mode` conflated with `workflow`; (3) no connection binding;
(4) crate inside ptpsim, FFI is a placeholder. Good news: serde-only deps, so
extraction is a move, not a detangle.

## 2. Engine extraction (step 1)
1. Audit deps (expected serde-only) — workspace move.
2. To the engine: `model.rs`, `query.rs`, `lib.rs`, `error.rs`, new resolution
   modules (§4), `generate.rs`. Data → `camera-config-data`. Rename
   `camera-manifest`→`camera-config` and `tools/camera-probe`→`protocol-mapper`
   in the same commit.
3. ptpsim depends on `camera-config` externally; `camera-sim`/`protocol-mapper`/
   core call the same API. Mechanical, shape-compatible, then the schema delta lands.
4. FFI (`camera-protocol-ffi`, ios-adoption G4) depends on `camera-config`,
   re-exports queries, owns data loading (bundled baseline + signed OTA).
5. New `ConfigStore`: load a tree (or in-memory for FFI), build the resolution +
   identification indexes. Schema growth = additive optional fields (TRANSPORTS.md).

## 3. Data-repo layout + schema

### Layout (`camera-config-data`, Apache-2.0)
```
fuji/
  fuji.yaml                 # manufacturer-tier defaults (initiator identity, versionOrder, fallbacks)
  soc/xprocessor5.yaml      # SOC-tier facts shared by member bodies
  gfx100ii/
    gfx100ii.yaml           # model-tier: soc membership, descriptor, modes, connections
    fw2.30.yaml             # firmware-tier overrides (string-keyed)
    fw2.40.yaml             # e.g. PIN-on-pair / XLV-HTTPS delta
golden/                     # parity vectors (shared with ios-adoption harness)
```
SOC membership is data; using the tier is engine code.

### Schema shape (orthogonal axes; flat predicate-gated catalog)
Top level = `camera` (What) + four fact-sections + a flat `modes` table + a
`connections` table. **Gating lives on the operation/feature** (it declares which
modes/connections/runtime-conditions allow it) — modes and connections are
*referenced*, never duplicated.
```yaml
schema: camera-config/v1                                               # v1 never shipped under the old name; grows additively, no v2
camera: { make: fuji, model: gfx100ii, soc: xprocessor5, fw: "2.30" }   # What

identification:                      # PUBLIC: observed signal → this target
  ble:  { mfrId: 0x04d8, services: [...] }
  usb:  [{ vid: 0x04cb, pid: 0x3105 }]
  mdns: { announces: false }

protocol:                            # ENGINE: wire facts + their gating
  operations:
    InitiateOpenCapture:
      code: 0x101c
      modes: [Shooting]              # prefix match → valid in Shooting/Stills & /Video
      requires: { prop: "0xd212", mask: 0x00ff, ne: 0x00 }   # runtime prereq (predicate)
    StepFNumber:        { code: 0x902d, modes: [Shooting/Stills] }
    GetObjectHandles:   { code: 0x1007, modes: [ImageTransfer] }
    RawConvert:         { code: 0x91xx, modes: [RawConversion], connections: [USBTether] }
  properties: { "0x5007": {...}, "0xd02a": {...} }
  events: {...}
  quirks: {...}

capability:                          # UI: static body descriptors (not gated)
  af: { points: 425, grid: [25, 17] }
  subjectDetection: [face, animal, bird]

modes:                               # defined ONCE; hierarchical paths
  Shooting:        { capabilities: [exposureControl] }       # inherited by children
  Shooting/Stills: { capabilities: [liveView, remoteControl],
                     detect: { prop: "0xdf01", eq: 0x1600 } } # detect = same predicate grammar
  Shooting/Video:  { capabilities: [movieRecord] }
  ImageTransfer:   { detect: { prop: "0xdf01", eq: 0x1400 } }
  RawConversion:   {}                                         # availability set by ops' `connections`

connections:                         # the connection axis (a user-selected state)
  App:                               # WiFi-AP path
    establishment: ble-to-wifi-v1    # ← sensitive; private-overlay-able
    bind: { command: 55740, event: 55741, liveview: 55742 }   # per shipping app (command+1 event, +2 stream)
    discovery: { mechanism: ble, announces: camera }
    entries:                         # mode-graph edges: wire action OR user-instruction
      - { to: Shooting/Stills, do: { setProp: { "0xdf01": 0x1600 } } }
      - { to: ImageTransfer,   do: { setProp: { "0xdf01": 0x1400 } } }
      - { from: Shooting/Stills, to: Shooting/Video, do: {...} }
  USBTether:        { platforms: [macos, android], discovery: { mechanism: usb } }
  WirelessTether:   { establishment: port-knock-v1, entry: { userInstruction: "Set camera to Wireless Tether" } }
  # HTTP / MemoryCard / Auto: stubs until the protocol-mapper survey
```

### The predicate grammar (closed, declarative — shared by `requires` and `detect`)
```
predicate :=
    { prop: <code>, eq|ne|lt|gt: <value>, mask?: <value> }   # leaf: property comparison
  | { all: [predicate, ...] }                                # AND
  | { any: [predicate, ...] }                                # OR
  | { not: predicate }
```
Total, side-effect-free, no loops, no property *writes*. Evaluated by the engine
over property values the client supplies (sans-io). This is the **entire**
conditional vocabulary — gating prerequisites and mode detection both use it. If a
future need can't be expressed here, add a named leaf op (e.g. `inRange`), not a
script.

Notes: gating intersects axes — an op is available iff `(connection, mode)` matches
its `modes`/`connections` sets **and** its `requires` predicate holds over current
state. Mode availability per connection is expressed by ops' `connections` (no
duplication). `Auto` = camera-side determination; declared, app detects which it
landed in.

## 3a. Composable shared records (the X-H2S / instax / recording-mode cases)
Three real cases drove decisions #14–#16; the schema must *express* all of them now
(GFX100 II exercises every one), populated only for the body we have.

**Connections/modes are id-keyed records a body references.** Mechanism (how to
talk over an interface) lives on the connection record; capability (what *this*
body can do) lives on the body; the resolved camera is their intersection.
```yaml
# gfx100ii.yaml — the body references connections by id, with conditions
connections:
  - ref: xlv-http                                    # built-in on this body
  - ref: instax-printer
    availableWhen: { firmware: { lt: "2.40" } }      # ≤2.30 only; removed at 2.40 (§4 comparator)
# x-h2s.yaml (future, DATA-ONLY — no engine change)
connections:
  - ref: xlv-http
    requiresHardware: ft-xh                          # adapter provides the same interface
  - ref: instax-printer
```
```yaml
# connections/xlv-http.yaml — shared mechanism, referenced by many bodies
id: xlv-http
establishment: ...            # how to open it (private-overlay-able)
modes: [Shooting/Video, ...]  # connection-bound modes
operations:
  setRecordingMode: { code: 0x...., property: "0xRRRR", modes: [Shooting/Video] }
```
**Value-set source** — shared mechanism, body/camera-specific values:
```yaml
properties:
  "0xRRRR":                    # recordingMode
    descriptor:
      form: enum
      source: camera           # camera returns the enum (DevicePropDesc) → authoritative, zero-maintenance
      # OR: source: manifest, values: [...]   # only when the camera doesn't enumerate it
```
Runtime-discovered (`source: camera`) wins; `manifest` fills gaps + labels + gating.
8K (GFX100 II) vs 6.2K (X-H2S) over the *same* XLV op becomes a data difference.

**Deferred as data, not mechanism:** the shared `xlv-http`/`instax` extraction and
`x-h2s.yaml` themselves; SOC-based *fallback resolution* (explicit shared records do
the sharing, so SOC likely reduces to a pure grouping/discovery tag with no
resolution behavior — leave that hook unbuilt). `fuji.yaml` is real and kept
(manufacturer-tier `versionOrder` + initiator identity + fallbacks; never a model).

## 4. Resolution engine (the finite vocabulary — no procedural DSL)
New modules in `camera-config`:
- **`resolve.rs` — the funnel.** `resolve(observed_facts) -> Resolution` where
  `observed_facts` is any subset of {VID/PID, mDNS, BLE advert, fw, observed prop
  values}, and `Resolution { config, candidates, ambiguity }`. Specificity fallback
  merges `root→fuji→soc→model→fw`, most-specific-wins; **the same fallback runs on
  the mode-path axis** (`Shooting/*`→`Shooting/Stills`) for gate inheritance. Feed
  more facts → narrower result. **Invariant:** with minimal facts (no fw), the
  funnel still returns usable bring-up config (init/`GetDeviceInfo`/discovery live
  at the manufacturer/model tier). Identification is just the funnel with only
  signal facts.
- **version comparator (fail-soft, data-selected).** Identity = raw string always.
  For range queries, a manufacturer-tier `versionOrder` names a finite engine
  comparator (default `dotted-int`: parse to a component vector, compare
  component-wise within a manufacturer, pad trailing with 0; `"2.30"`→`[2,30]`,
  `"1.2.3"`→`[1,2,3]`, `[2,9] < [2,30]`). **If a version can't be parsed for
  ordering, fall back to exact-string match** — never panic, never drop. Ranges are
  rare (instax/PIN deltas); exact-match is the common path and needs no parsing.
- **SOC tier** — inject `soc/<tier>` between manufacturer and model per `soc:`.
- **value-policy** — `fixed` / `generated(scheme,persist)` / `from-pairing(source)`
  (ios-adoption init identity/tail).
- **predicate evaluator** — evaluates §3's grammar over supplied prop values.

### Firmware narrowing within the funnel
fw can arrive from *any* fact, not just `GetDeviceInfo`: a newer camera's mDNS TXT
or BLE advert may carry it, and **bring-up behavior can narrow it** (on fw 2.40
"pair" demands PIN, on 2.30 it doesn't — modeled as a reactive-workflow transition
on the observed "PIN requested" event, which is also a fw-narrowing signal:
`PIN-requested ⟹ fw ≥ 2.40`). The funnel consumes whichever arrives first; no
client-side phase choreography.

## 5. Query API — funnel + orthogonal coordinate
```rust
// the funnel: feed any observed facts, get narrowest config + what's still ambiguous
fn resolve(&self, facts: &ObservedFacts) -> Resolution;   // subsumes identify()

// Where-coordinate queries (orthogonal connection × mode)
fn connections_for(&self, t, platform) -> Vec<ConnectionInfo>;   // USBTether hidden on iOS
fn modes_for(&self, t, connection) -> Vec<ModeInfo>;             // hierarchical paths
fn operation_available(&self, t, connection, mode_path, op: u16, observed: &PropView) -> Support;
//   intersects modes-set + connections-set + evaluates `requires` over `observed`
fn capabilities(&self, t, connection, mode_path) -> Vec<Capability>;
fn control_for(&self, t, connection, mode_path, prop: u16) -> Option<&Control>;
fn mode_graph(&self, t, connection) -> ModeGraph;                // nodes + entry edges
fn mode_entry(&self, t, connection, from: Option<&str>, to) -> Option<ModeEntry>;
fn detect_mode(&self, t, connection, observed: &PropView) -> Option<ModePath>;  // evaluates detect predicates
fn value(&self, t, key: &str) -> Option<ResolvedValue>;          // value-policy
fn value_label(&self, t, prop: u16, value: i64) -> Option<&str>;
fn discovery_for(&self, t, connection) -> Option<DiscoveryDescriptor>;
```
Keep `supports_operation(workflow, code)` as a deprecation shim so `camera-sim`/
ptpsim don't break mid-extraction.

### Identification / discovery — the "who announces" axis (now part of the funnel)
Each connection's `discovery` descriptor records the mechanism + who announces;
only some auto-discover:

| Connection | Mechanism | Announces | Data |
|---|---|---|---|
| App / BLE | passive scan | camera | mfr ID `0x04d8`, service UUIDs |
| mDNS/Bonjour | browse | sometimes app initiates | service type + direction (+ maybe fw in TXT) |
| USBTether | enumerate | neither | VID:PID + interface class |
| WirelessTether / knock | app initiates | camera silent | "not auto-discoverable; needs address" |
| HTTP/XLV | probe / SSDP | app probes | probe recipe |

The app scans/enumerates/probes (platform I/O) and feeds observed records into
`resolve` as facts. Identification signatures are PUBLIC (any app needs them);
distinct from the sensitive establishment handshake (separate section).

## 5b. Resolution trace + the dev iteration loop
The legibility primitive that makes fast config iteration possible: every decision
query (`operation_available`, `detect_mode`, `control_for`, `resolve`) can also emit a
**`ResolutionTrace`** — a structured, serializable, side-effect-free explanation of
*why* it answered as it did:
- which tiers merged (`root→mfr→soc→model→fw`) and which one supplied the winning fact;
- the `(connection, mode)` evaluated, and the path-prefix match that applied;
- which predicate decided it (`requires`/`detect`/`availableWhen`) and its truth over the
  supplied observed values;
- the value-policy applied; and residual `candidates`/`ambiguity` (the funnel's output).

It is pure — computed from the same data, no I/O. **It is what "see what the manifest
delivered" means.** Two loops consume it, both legible because of it:
- **App (human loop):** the FFI returns the trace alongside the answer (a `*_explained`
  variant or a trace field, opt-in for dev/telemetry builds); the app's telemetry
  captures it verbatim. Flow: error fires → telemetry shows the error *and* the manifest's
  answer that produced it → edit the manifest → **hot-reload the bundle** (no rebuild) →
  retry. The app always resolves **locally** (embedded FFI, sans-io) — never remotely —
  so the loop tunes exactly the path that ships (no dev/prod skew).
- **Automation (protocol-mapper):** a thin **gRPC resolver wrapping the sans-io engine**
  (the wrapper owns I/O; the engine stays pure) hosts resolution for an automated
  mutate→observe→converge loop against the **simulator** (or a camera) — try a manifest,
  read the trace + the responder's reaction, adjust, repeat. The app feeds *real-usage*
  telemetry into this; it does **not** host the resolver. → dev-tool follow-up, not engine
  scope. Keeps the gRPC/I/O in the tool layer where it belongs.

**Landed 2026-05-26** as an `explain()` sibling (not trace-on-every-call):
`operation_available_explained(connection, mode, op, observed) -> (Availability,
ResolutionTrace)` in `camera-config` (`trace.rs`), with `Predicate::explain` recording
every leaf (prop / observed / masked-effective / comparator / passed — no short-circuit).
FFI exposes it as `operation_available_explained -> GateExplanation`. Remaining (grows
with the engine): `detect`/`resolve` explained variants + the multi-tier funnel trace
when the funnel lands.

## 6. Manifest-DATA vs RUNTIME line (the crux)
- **DATA (declared):** which connections/modes exist; gating (`modes`/`connections`/
  `requires` predicates); how to enter (`entries`); `detect` predicates; discovery
  signatures. All declarative — predicates over observed state, never writes.
- **RUNTIME (engine/app does):** the app issues reads/scans (I/O) and supplies
  observed values; the engine *evaluates* predicates and resolves; the app decides
  on ambiguity (asks the user), and performs mode/connection switches.
- Rule: **manifest declares; engine evaluates; app does I/O.** The engine is pure —
  `operation_available(...observed)` / `detect_mode(...observed)` take values the
  client already read. `detect_mode → None` → app shows a picker from `modes_for`.

## 7. Migration & sequencing (the urgency)
Driver: every connection client application adds otherwise hardcodes gating in Swift. The
modes query must land **before** client application builds USB/WirelessTether gating, and G6
Swift codegen tables must be `(connection, mode)`-keyed from day one.
1. Extract `camera-config` + `camera-config-data`, rename `protocol-mapper`.
2. Add the resolution funnel + predicate evaluator + version comparator + value-policy.
3. Add the orthogonal modes/connections axis (schema + query). Model GFX100 II `App`
   modes from `fw2.30`; stub the rest from TRANSPORTS.md. **Gate: before any
   USB/WirelessTether gating Swift.**
4. Codegen (G6) emits `(connection, mode)`-keyed Swift tables; ios-adoption Phase A
   consumes them.
5. Fill the corpus incrementally (fallback covers gaps) as surveys land.
Keep the `supports_operation` shim through steps 1–3.

### 7a. Empirical survey (fills the catalog + capability metadata)
`protocol-mapper` brute-forces, against a real camera, which ops/props respond in
each `(connection, mode)` + under which runtime prerequisites, the `Auto`
resolution behavior, and the eeprom save/restore field map. Output feeds the
`protocol`/`modes`/`connections`/`capability`/`identification` data. Stays in the
**safe → settings-write** risk tiers — no RAM/firmware tier.

## 8. Verification
Engine unit tests:
- **resolution funnel:** `resolve({usb:0x04cb/0x3105})` → fuji/gfx100ii, fw
  ambiguous; add `{fw:"2.30"}` → exact; minimal-facts call still returns bring-up
  config (invariant).
- **orthogonal gating:** `operation_available(App, Shooting/Stills, StepFNumber)` =
  available; same op under `ImageTransfer` = `WrongMode`; a `USBTether`-only op
  under `App` = `WrongConnection`; gate inheritance: a `Shooting`-level op resolves
  available under `Shooting/Stills`.
- **predicate prerequisites:** an op with `requires:{prop,ne:0}` is `Blocked` when
  the supplied `observed` fails the predicate, available when it passes; `mask`/
  `all`/`any`/`not` cases.
- **version:** `[2,9] < [2,30]`; an unparseable version degrades to exact-match
  (no panic); a fw-range override applies only in-range.
- **detect:** `detect_mode({0xdf01:0x1600}) == Shooting/Stills`; unmapped → `None`.
- Schema back-compat (legacy loads); parity-harness reuse (mode-gating vectors in
  the shared golden corpus; Swift `GoldenParityTests` match the engine); anti-vcam
  CI lint greps `Sources/` for mode hex literals.

## 9. Open decisions (build-time; not blockers)
- **#A — Data loading / OTA path for the FFI** (bundled baseline + signed bundle;
  file tree vs embedded on iOS).
- **#B — Predicate leaf set** — `eq/ne/lt/gt/mask` cover known cases; add `inRange`/
  others only when a real camera needs them (closed grammar, never a script).
- **#C — Platform filtering placement** (`platforms:` on connection vs client set).
- **#D — Top-level key name** `connections:` (chosen) vs DESIGN's `transports:` —
  cosmetic; reconcile DESIGN.
- **#E — `entries` graph placement** — per-connection (as sketched) vs a top-level
  graph referencing connections; confirm once the survey shows how transitions
  actually cluster.

## Critical files
- `crates/camera-manifest/src/model.rs` — flat predicate-gated catalog: `modes`
  table, `connections` table, `requires`/`detect` predicate types, sections; → moves
  to `camera-config`.
- `crates/camera-manifest/src/query.rs` — funnel + orthogonal-coordinate API +
  predicate evaluator.
- `crates/camera-manifest/src/lib.rs` — `ConfigStore`/`resolve()` + identification
  index.
- `packages/protocol-spec/fuji/gfx100ii/fw2.30.yaml` — first to grow the orthogonal
  schema; → moves to `camera-config-data`.
- `crates/camera-protocol-ffi/src/lib.rs` — FFI re-export + data load.
- `DESIGN.md` — reconcile `transports:`→`connections:`, orthogonal mode axis,
  predicate gating.
