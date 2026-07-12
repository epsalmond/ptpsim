# ptpsim — Camera Protocol Simulator

Status: baseline design for ptpsim, a standalone camera-protocol simulator.
client application is its first consumer and replaces its current C `vcam` with it; ptpsim
itself is intended to be published as an open-source project, generic across
manufacturers. Design conflicts are resolved in favor of whichever option gives
ptpsim a more viable open-source path or broader model support.

## Purpose

ptpsim is a scriptable camera-protocol platform. It must run a believable
PTP/IP camera for App Review, support real tethered-shooting workflows, let us iterate quickly in development, and let us


The central loop is:

1. Probe a real camera from an app or tool. (camera-protocol-mapper)
2. Upload the observation bundle.
3. Generate or update a camera manifest.
4. Run a simulator from that manifest.
5. Use the same manifest to drive app feature gating, tests, and diagnostics.

Fuji reference app/GFX100 II is the first target. Nikon (and RED, now Nikon-owned) is the
next, then Canon; other manufacturers should fit without changing the core
architecture.

ptpsim is not only a fake camera for App Review — it is a protocol-exploration
platform for unlocking behavior vendors never exposed. The origin goal was
extracting IMU/gyro data for gyroflow stabilization; that is a probe-and-
understand problem, and it is the reason `camera-protocol-mapper` (the probe) is the heart
of the project rather than an accessory. Whatever a probe can learn, the
simulator can then reproduce and any client can build on.

## Design Principles

- Keep packet codecs shared, but keep roles separate. The app (client application, camera control app) is an initiator;
  the simulator is a responder. They share type-safe protocol primitives and
  manifests, not the same session state machine.
- Make observed behavior data. Camera model differences, firmware differences,
  port layouts, property descriptors, quirks, and workflow gates live in
  manifests and scenario files.
- Treat discovery and connection setup as first-class, per-model data. Cameras
  routinely use port-knocking, SSDP-like discovery, and connections orchestrated
  across multiple ports — and the mechanics vary by model, firmware, and mode.
  This is why a transport is defined by how a connection comes into being, not
  just by a port number (see Transport And Mode Matrix).
- Treat scripts as product surface. Every useful simulator action must be
  available through a CLI and a structured control API, not only through Rust
  internals.
- Prefer Rust for protocol and simulator runtime code. Use Ruby where it is
  already the deployment/control-plane language or where integration tests are
  clearer as orchestration.
- Build from captures and probes. Golden packets, wire captures, and black-box
  smoke tests are the authority, not hand-written assumptions or vendor helper
  layers with their own issues.
- Open source the whole simulator, including captured device behavior. The
  engine (codecs, manifest schema, media store, sim runtime, probe) and the
  captured camera manifests, captures, and fixtures are all public. The point is
  a community loop: someone with a camera we do not own runs the probe,
  contributes a manifest, and both this stack and arbitrary third-party apps get
  better camera support. The only private parts are a consumer's own app,
  backend, and management sidecar (e.g. client application's).
- Redistributability is a contributor rule, not a private/public split. Captures
  and manifests get a redaction pass (serials, device GUIDs, network
  identifiers); media and firmware fixtures must be synthetic or licensed for
  redistribution — never copyrighted vendor payloads.
- Evidence is provenance, never a load-time dependency. A manifest must validate
  and run with `evidence:` ids unresolved. Prefer citing redistributable sources
  (wire captures, public specs) so a public manifest is self-justifying; only
  app-internal provenance (a consumer's private docs) is allowed to dangle.
- ptpsim owns no lifecycle policy. It exposes health, shutdown, and control
  endpoints and honors `SIGTERM`. Leasing, pooling, and orchestration live in a

- Keep client application's current PTP camera emulator as a short-term compatibility runner. Do not
  keep expanding its C/Fuji implementation into the long-term architecture.

## Chosen Direction

Default implementation choices:

- Build ptpsim in Rust.
- ptpsim lives in its own standalone repo, published open source. client application
  consumes the published crates (git or crates.io) and deletes its placeholder
  `camera-protocol-{core,fuji,ffi}` crates in favor of them.
- Keep the simulator service as a Rust process. Ruby stays in client application's
  backend/admin/control-plane layer (the management sidecar) and can orchestrate
  integration tests. The sidecar is a client application concern, not part of ptpsim.
- Use YAML manifests as the reviewed source format and generate JSON Schema plus
  any Swift/Rust lookup tables from them.
- Share packet codecs, manifest types, media metadata, and compatibility queries
  between the app, probes, and simulator.
- Do not share initiator and responder workflow state machines.

This preserves the important reuse while avoiding a simulator that merely
replays app assumptions back to the app.

## High-Level Architecture

```text
                 probe tool / app
                       |
             observation bundle JSONL
                       |
              manifest generator
                       |
       camera manifest + scenario manifests
                       |
    +------------------+------------------+
    |                                     |
 app protocol client                simulator service
 initiator role                     responder role
    |                                     |
 shared Rust crates: ptp-core, media model, protocol primitives, manifest types
```

The shared code must not hide the role boundary. For example, `ptp-core` can
parse and serialize an `OperationRequest`, but only the app client decides when
to send it and only the simulator decides how to respond.

## Runtime Component Model

The simulator runtime is an actor tree. Each actor owns a narrow state surface
and communicates with typed messages. There are no process-global sockets,
transaction counters, or event destinations.

```text
camera-sim-service
  |
  +-- InstanceSupervisor
  |     owns profile id, media root, bind addresses, graceful shutdown

  |
  +-- TransportSupervisor
  |     owns TCP/UDP listeners and accepted socket routing
  |
  +-- CameraInstance
        owns manifest, media store, virtual clock, trace sink
        |
        +-- CommandSessionActor
        |     owns PTP session id, transaction validation, active workflow
        |
        +-- EventSocketActor
        |     owns async event delivery and disconnect behavior
        |
        +-- LiveViewStreamActor
        |     owns frame source, pacing, and frame packetization
        |
        +-- ScriptActor
              owns control API commands, faults, resets, and scenario assertions
```

One leased camera instance can accept at most one active app command session by
default. Multi-client behavior should be explicit in the manifest or scenario:
reject, queue, replace existing, or allow read-only observers. For App Review,
the backend should still lease one instance per install id so reviewers do not
share camera state.

Packet flow:

```text
TCP socket
  -> transport frame reader
  -> ptp-core packet parser
  -> command session actor
  -> active workflow handler
  -> media/property/event services
  -> ptp-core packet serializer
  -> transport writer
```

The event and live-view sockets are not side channels hidden inside workflow
code. They are registered capabilities on the `CameraInstance`, so scripts and
tests can see whether a client opened the expected sockets.

## Public Contract Sketch

These are illustrative Rust boundaries, not final names.

```rust
// ptp-core
pub enum PtpIpPacket {
    InitCommandRequest(InitCommandRequest),
    InitCommandAck(InitCommandAck),
    OperationRequest(OperationRequest),
    StartDataPacket(StartDataPacket),
    DataPacket(DataPacket),
    OperationResponse(OperationResponse),
    Event(EventPacket),
}

pub struct OperationRequest {
    pub code: OperationCode,
    pub transaction_id: u32,
    pub params: Vec<u32>,
}

pub trait PtpCodec: Sized {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError>;
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeError>;
}
```

```rust
// camera-config
pub struct CameraManifest {
    pub camera: CameraIdentity,
    pub transports: BTreeMap<TransportId, TransportSpec>,
    pub workflows: BTreeMap<WorkflowId, WorkflowSpec>,
    pub operations: OperationCatalog,
    pub properties: PropertyCatalog,
    pub media: MediaPolicy,
    pub quirks: Vec<Quirk>,
}

impl CameraManifest {
    pub fn supports_operation(&self, workflow: WorkflowId, code: u16) -> Support;
    pub fn property_desc(&self, code: u16, state: &CameraState) -> Option<PropertyDescSpec>;
    pub fn workflow(&self, id: WorkflowId) -> Option<&WorkflowSpec>;
}
```

```rust
// camera-media-store
pub trait MediaStore {
    fn scan(&mut self) -> Result<ScanSummary, MediaError>;
    fn handles(&self, query: ObjectQuery) -> Vec<ObjectHandle>;
    fn object_info(&self, handle: ObjectHandle) -> Result<ObjectInfo, MediaError>;
    fn thumbnail(&self, handle: ObjectHandle) -> Result<ByteSource, MediaError>;
    fn read_range(&self, handle: ObjectHandle, offset: u64, len: u32)
        -> Result<ByteSource, MediaError>;
}

pub enum ByteSource {
    Memory(Bytes),
    FileRange { path: PathBuf, offset: u64, len: u64 },
    Generated { len: u64, seed: u64 },
}
```

```rust
// camera-sim
pub trait WorkflowHandler {
    fn on_operation(
        &mut self,
        ctx: &mut SimContext,
        req: OperationRequest,
        data: Option<Bytes>,
    ) -> Result<WorkflowReply, SimError>;
}

pub enum WorkflowReply {
    Response(OperationResponse),
    DataAndResponse { data: DataPacket, response: OperationResponse },
    CloseConnection(CloseReason),
}
```

The app-side client may use `ptp-core`, `camera-config`, and selected Fuji
value-formatting helpers. It must not depend on `camera-sim` or responder
workflow handlers.

## Repository Shape

ptpsim is its own standalone repo, published open source. client application consumes the
crates as dependencies; its placeholder `camera-protocol-{core,fuji,ffi}` crates
are deleted in favor of these. A monorepo home was rejected because a generic,
community-contributable simulator should not carry client application's App Review backend
or app sources. Captured manifests, captures, and fixtures are public — they are
the community camera-behavior database, not private client application data.

```text
crates/
  ptp-core/                 packet codecs, containers, object/property encoders
  camera-config/          schemas, validation, compatibility queries
  camera-media-store/       filesystem/object handle model, thumbnails, RAF/MOV/JPG
  camera-sim/               generic responder engine + scripting runtime
  protocol-primitives/      concern-organized framing/quirk/establishment primitives
  camera-protocol-ffi/      optional Swift/Ruby FFI boundary

services/
  camera-sim-service/       Rust HTTP/control service plus PTP listeners

packages/
  camera-config-data/       captured camera manifests + probe/label evidence
  camera-protocol-python/   Python bindings for the FFI
  fixtures/                 small redistributable media fixtures

tools/
  camera-simctl/            CLI wrapper for local and remote simulator control
  golden/                   golden-packet capture fixtures for the codec tests
```

`camera-protocol-mapper` — the Python probe/exploration tool (seeded by
fuji-remote) — lives in its own repository,
[`epsalmond/camera-protocol-mapper`](https://github.com/epsalmond/camera-protocol-mapper),
not in this tree: it must run with nothing but a stock interpreter in field
conditions. Its only coupling to the Rust side is the JSONL observation bundle,
vendored here as evidence under `packages/camera-config-data/`.

The internal crate names above are the workspace paths. Published crate names
take a `ptpsim-` prefix (`ptpsim-core`, `ptpsim-manifest`, …) to avoid
crates.io collisions on generic names; reconcile and reserve names before the
first publish.

Public vs private boundary:

- Public (ptpsim repo): everything that is the simulator — engine crates, the
  generic `camera-sim` + `protocol-primitives`, manifest schema, probe tooling,
  service and CLI, and the captured camera manifests, captures, and
  redistributable fixtures. Model-specific behavior lives in manifest data, not
  code, so a new camera is a manifest contribution, not a fork.
- Private (stays in client application): only client application's own app, App Review backend, and
  management sidecar. Manifests may reference client application-internal evidence docs by
  id; those references dangle harmlessly in the public repo because evidence is
  optional at load time.

Contributor hygiene before anything lands public: captures and manifests pass a
redaction step (serials, device GUIDs, network identifiers), and media/firmware
fixtures are synthetic or redistribution-licensed. Captured manifests and
contributions are data; pick a license (e.g. CC-BY or the project license for
data) and a contribution agreement before the first external probe lands.

Operating model — one engine, three consumer seams:

ptpsim is the single home for camera behavior. Everything downstream is a
consumer of a published artifact — never a fork, never a sibling
implementation.

- **Apps** consume the FFI: vendored bindings + manifests per platform
  (Swift via the Apple FFI releases, Kotlin via the Android/Linux targets).
- **Hosted simulators** consume the container image — the publishable
  `camera-sim-service` artifact #247 defines, with `/healthz`, the control
  endpoint, and mountable startup-state YAML. A deployment — e.g. a hosted
  review/demo camera pool — is configuration around that image, pinned by
  digest, never a patched build.
- **Protocol discovery** consumes the crates themselves: probing a real camera
  runs through the headless initiator built on the shipping engine and
  manifests (#252, on the #250 executor), and observations flow back through
  the in-repo generator intake. Standalone probe tools are explicitly
  rejected: the predecessor mapper toolkit proved the concept but drifted from
  the engine, and its results stopped transferring.

Two standing rules fall out of this:

1. **Upstream-first.** When a consumer needs camera behavior the engine lacks,
   the fix is a consumer-neutral engine/manifest feature here — not a
   workaround in the consumer. Consumers pick behavior from manifest traits;
   a camera-specific branch in a consumer is a bug in this repo.
2. **Probe work lands here first.** Any "drive a real camera to learn what it
   does" effort starts in this repo, links the shipping crates, and ends as
   manifest data plus evidence. If a result cannot round-trip into a manifest,
   the schema gap is the work item.

## Core Crates

### `ptp-core`

Responsibilities:

- PTP/IP packet parsing and serialization.
- Command, data, response, event, and init container types.
- Transaction/session id handling primitives.
- ObjectInfo, StorageInfo, DeviceInfo, DevicePropDesc, and PTP array/string
  encoding.
- Standard PTP operation/property/object-format registries.
- Golden packet fixtures and round-trip tests.

Important rule: `ptp-core` is protocol syntax only. It does not know that Fuji
live view requires a `0xdf01 = 22` prelude, that a camera has a DCIM folder, or that a
session is in image-import mode.

### `camera-config`

Responsibilities:

- Versioned manifest schema and validation.
- Manufacturer/model/firmware identity.
- Transport definitions.
- Operation support matrix.
- Property descriptors and dynamic descriptor rules.
- Workflow definitions and state-machine references.
- Quirk flags with evidence links.
- Compatibility queries used by the app and simulator.
- **Canonical manifest generation** from observation bundles. This is the one
  authoritative generator: it consumes the JSONL bundle and emits a reviewable
  manifest proposal. It lives here, next to the schema and validation it must
  agree with, and it serves every bundle source (`camera-protocol-mapper`, Wireshark/JSONL
  capture import, app-integrated probe), not just the Python prober. A stored
  bundle deterministically reproduces its proposal through this generator.

Manifests are append-friendly. A new probe should be able to add a property
descriptor, operation variant, timing observation, or failure response without
requiring a code change unless the underlying packet type is new.

Generation deliberately does not live in `camera-protocol-mapper`: keeping it behind the
bundle seam stops "what a valid manifest is" from being implemented twice and
drifting. If the generator needs richer context (which plan ran, engaged mode,
op risk), that goes into the bundle, not into a second generator. `camera-protocol-mapper`
may emit a best-effort **draft** proposal for field sanity-checks, explicitly
marked non-canonical; only the `camera-config` proposal feeds review.

### `camera-media-store`

Responsibilities:

- Filesystem-backed camera card model.
- Symlink-safe traversal with explicit root confinement.
- Stable object handle assignment.
- Folder/date grouping views used by Fuji image import.
- ObjectInfo generation for JPEG, RAF, MOV, HEIF, and unknown files.
- Thumbnail selection and extraction policy.
- RAF preview extraction using a spec-backed parser.
- Large object metadata, including the 32-bit wireless-transfer ceiling.
- File-backed partial reads without loading whole media files.

The store must support synthetic metadata for test cases, including a logical
`>4 GB` MOV backed by a sparse file or generated stream.

### `camera-sim` (the generic responder engine)

There is **no per-manufacturer crate** (`fuji-app`, `nikon-*`, `canon-*`). A
per-brand emulator crate would just move the vcam problem into `crates/`. Instead
one generic engine runs every camera from manifest data, and the small
irreducible non-data code lives in `protocol-primitives`, organized by concern.

Responsibilities:

- Runtime engine for responder sessions (reference app session startup/init-ack,
  three-socket model, image-import, live-view, camera-controls, firmware) — all
  driven by the manifest, none brand-hardwired.
- A **manifest state-machine interpreter** that runs the `workflows` transition
  tables. Workflow states/gates are data; the interpreter is generic.
- Generic operation handlers (GetDeviceInfo / handles / partial-object /
  propdesc / set-prop) bound to the manifest + media-store.
- Camera-initiated pull queues keyed by manifest transfer id. These queues use
  fixed external head indices and never alias public card enumeration handles.
- A property engine including generic vendor-step ("advance within an ordered
  value set") and manifest-defined record-stream readback (e.g. `0xd212`).
- Generic event emission (focus, capture, object-added, postview, teardown) from
  manifest `events` triggers.
- Script execution; deterministic virtual clock; scenario load + reset;
  per-client isolation; event scheduler + fault injection; structured trace of
  every packet, state transition, media read, and script action.

Constants, supported ops, property forms, and workflow gates come from manifests.

### `protocol-primitives` (concern-organized, shared)

The only code that is genuinely not data, kept finite and shared — never a brand
silo. Each primitive has an id that manifests reference (`framing:`, `quirk:`,
`handler:`):

- Wire framing transforms beyond `ptp-core`'s vanilla codec (e.g. Fuji's
  compressed framing, later Canon EOS events) — peers in one codec registry.
- Live-view frame packetization (wrapping a JPEG in a vendor frame header).
- Connection-establishment strategies (BLE-opened / UDP-knock / direct-bind).
- Computed quirks where a value is assembled/derived, not looked up (e.g. the
  `0xd212` status bundle, checksums, one-shot-per-boot knock).

Adding a camera/manufacturer is a manifest + captures; a new entry here is needed
only for a genuinely new wire format or computed quirk, and it lands as a peer
available to all, not in a per-brand crate. Do not push procedural quirks into a
declarative DSL — data selects and parameterizes; these primitives implement.

### `camera-protocol-mapper`

`camera-protocol-mapper` is the initiator-side probe and exploration tool — the
"run this against your camera, it probably won't break it" half of the loop that
lets ptpsim, client application, and any other client add cameras nobody on the team owns.
It is seeded by the existing `fuji-remote` work (fuji wireless tethering port-knocking,
property sweeps, `firmware_settings.dat` save/receive).

It is a **Python** tool in its own repository
(`epsalmond/camera-protocol-mapper`), not part of this workspace.
Python is deliberate: the probe often has to run in hostile field conditions —
(a typical use case might be walking a photographer through the process over the phone) into a box next to the camera, or an unfamiliar Windows machine over remote
desktop — with no build toolchain. A stock Python interpreter is the lowest
common denominator, and port-knocking, fuzzing, and one-off protocol weirdness
are quick to read and write.

Responsibilities:

- Initiator-side probe plans, keyed by manufacturer + transport + mode (not
  hardwired): handshake/knock sequences, BLE pairing, DeviceInfo + property-descriptor sweeps,
  object enumeration, and opt-in fuzzers (e.g. settings-blob save/receive).
- Real-camera observation bundle writer (the JSONL bundle is the only contract
  with the Rust side — see Probe And Manifest Pipeline).
- Capture import from Wireshark/JSONL traces where available.
- Optional best-effort **draft** manifest proposal for field sanity-checks,
  explicitly non-canonical. Canonical generation belongs to `camera-config`
  (see there); `camera-protocol-mapper`'s job is to produce a faithful bundle.
- Optional app-integrated probe mode.

The observation bundle is the seam. `camera-protocol-mapper` stays Python and portable;
the Rust side consumes bundles. Probe plans that prove stable can be ported into
Rust over time without changing the contract, but there is no requirement to.

Probe output is evidence, not automatically trusted truth. Manifest updates must
record which probe or capture produced each assertion.

**Risk classes.** Every probe plan declares a risk class, and the tool enforces
it. Each tier up requires a more explicit, louder opt-in; everything above
`safe` ships under a plain **use-at-your-own-risk, no-warranty** notice. Tiers,
escalating:

1. `safe` (default) — read-mostly: DeviceInfo, descriptor sweeps, object
   enumeration, handshake/transport capture. No state change.
2. `settings-write` — writes documented properties and settings blobs (the
   vendor-catalog fuzzing; checksums in the format catch gross errors, so this is
   generally safe and reversible). Persists user settings.
3. `ram` — RAM read/write using manufacturer test modes. Volatile; 
   "probably fine after a reboot," recoverable by power cycle.
4. `firmware-supported` — firmware update through the manufacturer's own path
   with its safety checks intact.
5. `firmware-unlocked` — firmware writes that bypass the manufacturer's safety
   checks to write arbitrary data. Can brick. Hope you have equipment insurance.

Known destructive opcode to call out explicitly: **factory reset**. A single
operation resets the camera to defaults. It is not a brick, but it silently
wipes user configuration, so it is `settings-write` at minimum and the probe
must hold an explicit denylist so a blind opcode sweep can never fire it by
accident. The probe knows this opcode precisely in order to avoid it.

The aggressive tiers are valuable and stay (vendor catalogs enumerate exactly which
properties to fuzz, and that is how new cameras get mapped); they are just gated
behind escalating consent so the default experience matches the "probably won't
break it" promise.

## Manifest Model

The manifest is the long-term extension mechanism. It should be YAML or JSON for
reviewability, with a generated JSON Schema.

Minimal shape:

```yaml
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
  identities:
    ptpDeviceName: GFX100 II
    serialPattern: "xxxxxxxxxxxxxxxxxxxxxxxx"

transports:
  appAp:
    kind: ptpip-app
    bind:
      command: 55740
      event: 55741      # command+1 (per the shipping app)
      liveview: 55742   # through-picture stream, command+2 (per the shipping app)
    init:
      ackDeviceGuid: "0870b061-0a8b-4593-b2e7-9357dd36e050"
      friendlyNameLength: 26
      tailHex: "cc004f000000000000000000000057004d0042000000000000000000"
  pcssTether:
    kind: ptpip-pcss
    status: planned
    callbackPort: 51560
    knockPort: udp/51562
    announcedCommandPort: 15740

workflows:
  imageImport:
    startup:
      - setProp: { code: 0xdf00, type: u16, value: 6 }
      - setProp: { code: 0xdf01, type: u16, value: 20 }
      - getProp: { code: 0xdf28 }
      - setPropFromCameraMax: { code: 0xdf28, max: 3 }
    gates:
      beforeEnumeration:
        require:
          - op: 0x9054
          - op: 0x9055
          - op: 0x9050
          - op: 0x9053

properties:
  "0xd02a":
    name: stillIso
    type: u32
    access: readWrite
    setMode: absolute
    values:
      - 100
      - 200
      - 400
      - 800
      - 1600
      - 3200
      - 6400

quirks:
  - id: fuji-objectinfo-size-sentinel
    appliesTo: [imageImport]
    behavior: objects may report ObjectCompressedSize 0xffffffff and expose true transfer size separately

```

Manifest codegen can produce Rust constants and Swift-facing compatibility
tables, but the source of truth remains the manifest. Some properties (such as f.stop) may read their allowed values at runtime.

### Manifest Sections

Required sections:

- `schema`: manifest schema version.
- `camera`: manufacturer, model, firmware, advertised identity, serial policy.
- `evidence`: named sources used by later manifest entries.
- `transports`: socket layout, discovery/rendezvous rules, init packet shape,
  accepted host-name formats, retry/close behavior.
- `operations`: supported standard and vendor operation codes, valid params,
  response behavior, data phase direction, workflow availability.
- `properties`: property descriptors, dynamic descriptor rules, value labels,
  set method, readback source, and unavailable behavior.
- `workflows`: state machines and required command preludes.
- `media`: filesystem rules, object formats, thumbnail policy, folder/date
  grouping, transfer limits.
- `events`: event codes, emission triggers, params, and destination socket.
- `quirks`: named deviations with evidence links.

Evidence is provenance, not a load-time dependency. A manifest must validate and
run with `evidence:` ids referencing sources that are absent (e.g. a consumer's
private app-flow docs not shipped in this repo). The engine treats unresolved
evidence as a lint/warning at most, never a load failure. Prefer redistributable
evidence (wire captures, public specs) so public manifests are self-justifying;
let only app-internal provenance dangle.

All nontrivial entries should reference an evidence id:

```yaml
evidence:
  appLiveViewCapture:
    kind: wire-capture
    path: client application/apps/apple/docs/APP_LIVEVIEW_CODE_MAP.md
    date: "2026-05-23"
  tetherStateMachine:
    path: fuji-remote/TETHER_STATE_MACHINE.md
    date: "2026-05-23"
```

Operation entry shape:

```yaml
operations:
  "0x101b":
    name: GetPartialObject
    owner: standard-ptp
    dataPhase: in
    params:
      - { name: handle, type: objectHandle }
      - { name: offset, type: u32 }
      - { name: length, type: u32 }
    workflows: [imageImport]
    handler: media.partialObject
    evidence: [appImageImportCapture]
  "0x902d":
    name: StepFNumber
    owner: fuji-vendor
    dataPhase: none
    params:
      - { name: direction, type: enum, values: { wider: 1, narrower: 0 } }
    workflows: [liveView, cameraControls]
    handler: property.step
    property: "0x5007"
    evidence: [appLiveViewCapture]
```

Property entry shape:

```yaml
properties:
  "0x5007":
    name: aperture
    ptpName: FNumber
    type: u16
    access: readWrite
    descriptor:
      form: enum
      values: [280, 350, 400, 530, 560, 710, 800, 1000, 1100, 1600, 2200, 65535]
    controls:
      liveView:
        setMethod: vendorStep
        operation: "0x902d"
        readback: "0xd212"
      tether:
        setMethod: absolute
        operation: "0x1016"
    labels:
      280: "f/2.8"
      400: "f/4"
      65535: "body"
    evidence: [appLiveViewCapture, tetherStateMachine]
```

Workflow entry shape:

```yaml
workflows:
  liveView:
    transport: appAp
    states:
      - disconnected
      - initAcked
      - sessionOpen
      - functionModeSet
      - remoteExNegotiated
      - captureOpen
      - streaming
      - stopping
      - closed
    transitions:
      - from: sessionOpen
        to: functionModeSet
        on:
          - setProp: { code: "0xdf00", value: 6 }
          - setProp: { code: "0xdf01", value: 22 }
      - from: remoteExNegotiated
        to: captureOpen
        on:
          - operation: "0x101c"
    sockets:
      command: appAp.command
      event: appAp.event
      stream: appAp.liveview
    evidence: [appLiveViewCapture]
```

Media policy shape:

```yaml
media:
  rootLayout:
    dcimFolder: DCIM
    cameraFolderPattern: "[0-9]{3}_FUJI"
  handlePolicy:
    assignment: stable-sorted-path
    persistFile: .camera-sim-handles.json
  symlinks:
    allowInsideRoot: true
    rejectEscapes: true
  formats:
    jpeg: { objectFormat: "0x3801", thumbnail: embedded-or-generated }
    raf: { objectFormat: "0xb103", thumbnail: raf-preview-directory }
    mov: { objectFormat: "0x300d", thumbnail: generated-poster }
  transfer:
    maxWirelessObjectSize: 4294967295
    oversizeBehavior: report-ceiling-and-reject-download
```

## Probe And Manifest Pipeline

The probe pipeline has three artifacts:

- Observation bundle: raw events from a real camera run. The only thing
  `camera-protocol-mapper` is required to produce; the contract for everything downstream.
- Manifest proposal: normalized behavior inferred from a bundle by the canonical
  generator in `camera-config`. (`camera-protocol-mapper` may emit a non-canonical draft
  in the field, but only the canonical proposal feeds review.)
- Accepted manifest: reviewed, versioned source of truth.

The upload API is intentionally boring so field devices, app builds, and lab
scripts can all use it:

```http
POST /protocol-observations
Content-Type: application/jsonl

...observation bundle...
```

The server stores the raw bundle immutably, runs schema validation, attaches
build/device/camera metadata, and creates a manifest proposal:

```http
POST /manifest-proposals
Content-Type: application/json

{"observation_bundle_id":"uuid","base_manifest":"fuji/gfx100ii/fw0230"}
```

Accepted proposals write a normal manifest file under `packages/camera-config-data`.
The output must be reviewable as text and reproducible from the stored
observation bundle.

Observation bundle format:

```json
{"ts":"...","kind":"ptpip.fact","transport":"app","mode":"import","state":"probing","subject":{"kind":"prop","code":"0xd02a","op":"GetDevicePropValue"},"params":[],"result":{"response":"0x2001","value_hex":"..."}}
{"ts":"...","kind":"ptpip.fact","transport":"app","mode":"liveview","state":"streaming","subject":{"kind":"op","code":"0x101c"},"params":[],"result":{"response":"0x2001"}}
{"ts":"...","kind":"state","workflow":"LiveView","from":"Opening","to":"Streaming","evidence":"0x101c ok"}
{"ts":"...","kind":"media","handle":"0x00000005","name":"DSCF1494.MOV","size":4289912320}
```

The generator can infer:

- Operation support and response codes.
- Property descriptor type/access/forms.
- Property value readbacks after writes.
- Required prelude commands for workflows.
- Transport port layout and init packet shape.
- Object enumeration and transfer behavior.
- Timing windows and retry-worthy failures.

The generator must not infer semantic labels without evidence. Unknown values
stay named `raw_0x...` until validated.

## Simulator Service

The simulator service is a Rust binary using `tokio`.

Responsibilities:

- Run one simulator instance from a profile and bind addresses given at startup.
- Listen on IPv6 (required — the Apple App Review network is NAT64/IPv6-only, so
  review instances are reached over raw IPv6), optionally IPv4 for lab/dev.
- Expose PTP/IP camera ports directly.
- Expose a local/admin HTTP API for health and control.
- Load manifest and scenario files.
- Serve fixed or mounted media.
- Emit structured logs.
- Shut down gracefully on `POST /shutdown` or `SIGTERM`, draining in-flight
  transfers within a bounded window.

The instance is lease-agnostic. It does not know about reviewer leases, pools,

that (see "Control Plane Boundary").

Default ports:

- PTP command: `55740`
- Fuji event socket: `55741` (command+1)
- Fuji live-view (through-picture) stream: `55742` (command+2)
- Health/control HTTP: configurable, local by default

Health endpoint:

```http
GET /healthz
```

Response (the exact shape `camera_sim_service::control::Health` emits today):

```json
{
  "ok": true,
  "instance_id": "<lease-uuid-or-'local'>",
  "profile": "fuji/gfx100ii",
  "bind": "[2602:...]:55740",
  "sessions": 1,
  "media_root": "/var/lib/ptpsim/media-root"
}
```

Control endpoints should be enabled only on a private interface or protected by

(graceful drain) are implemented; the rest of the table below is planned and
distinguished so consumers don't depend on what isn't there yet.

Implemented:

- `GET /healthz`
- `POST /shutdown`

Planned (DO NOT depend on yet; request upstream when needed):

- `POST /reset`
- `POST /scenario/load`
- `POST /script`
- `POST /faults`
- `GET /trace`
- `GET /metrics`

Configuration:

```yaml
instance:
  id: "${INSTANCE_ID}"
  profile: fuji/gfx100ii/fw0230
bind:
  host: "::"
  ptpCommandPort: 55740
  ptpEventPort: 55741
  ptpLiveViewPort: 55742
  controlHost: "127.0.0.1"
  controlPort: 8080
media:
  root: /opt/camera/DCIM/100_FUJI
  liveViewFrames: /opt/camera/DCIM/mjpeg/640x480
logging:
  format: json
  level: info
  tracePackets: true
  redactPayloadsLargerThan: 4096
```

Structured log event shape:

```json
{
  "ts": "2026-05-24T12:00:00.000Z",
  "level": "info",
  "instance_id": "uuid",
  "session_id": "cmd-1",
  "event": "ptp.operation",
  "workflow": "imageImport",
  "transaction": 14,
  "op": "0x101b",
  "params": ["0x00000005", "0x00000000", "0x00a00000"],
  "response": "0x2001",
  "bytes_out": 10485760
}
```

Deployment image:

- Multi-stage Rust build.
- Runtime contains only simulator binaries, manifests, schemas, and fixed media.
- Media can be baked into the image for App Review or mounted for lab runs.
- Same image runs on `linux/amd64` and `linux/arm64`.

  process with explicit per-instance socket binding if operationally simpler.

## Control Plane Boundary

ptpsim's only orchestration surface is HTTP plus `SIGTERM`. Everything about
when instances start, how many run, who they belong to, and how long they live

JetStream, leasing, and pooling out of the published binary.

```text

  |  spawn process, pass profile + bind + media
  |  poll GET /healthz, aggregate into pool inventory
  |  POST /shutdown or SIGTERM to retire
  v
ptpsim instance  (HTTP control + PTP/IP listeners, no lease awareness)
```


vcam: a host-side agent consumes NATS commands (`vcam.cmd.{up,down,restart}`)
and publishes a `vcam_pool` KV inventory snapshot that the backend mirrors into
`review_camera_instances`. Migrating to ptpsim means that agent (a) spawns the
ptpsim binary instead of `vcam`, and (b) builds the inventory snapshot by
polling each instance's `/healthz` instead of vcam's mechanism. The
NATS/KV/lease contract is unchanged and stays in client application. The `/healthz`
response shape below is the contract the sidecar depends on.

## Scriptability

Scriptability is required for tests, review support, demos, and protocol
exploration.

CLI examples (today's binaries are `camera-sim-service` and `camera-simctl`;
the `camera-protocol-mapper` / generator examples are aspirational shape sketches —
the current generator lives at `cargo run -p camera-config --bin camera-config-generate`):

```sh
# IMPLEMENTED today (matches services/camera-sim-service/src/main.rs):
camera-sim-service \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml \
  --media-root <path/to/DCIM-root> \
  --profile fuji/gfx100ii \
  --connection app \
  --startup-state scenarios/fuji/gfx100ii/iso-2000.yaml \
  --command-bind  '[::]:55740' \
  --event-bind    '[::]:55741' \
  --liveview-bind '[::]:55742' \
  --liveview-dir  packages/fixtures/liveview/640x480 \
  --control-bind  '127.0.0.1:8080'

# PCSS / infrastructure-mode direct responder shape. This binds only the
# command socket; live view is served by command-channel 0x9018 polling.
camera-sim-service \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml \
  --media-root <path/to/DCIM-root> \
  --profile fuji/gfx100ii \
  --connection wireless-tether \
  --command-bind '[::]:15740' \
  --control-bind '127.0.0.1:8080'

# Optional LAN-fidelity PCSS establishment: also listen for UDP DISCOVERY and
# call back to the host's manifest-declared callback port with NOTIFY/DSCPORT.
# By default the PCSS object queue is seeded from the media root at startup.
# Add --pcss-shutter-enqueue-count N to start empty and enqueue N objects after
# each manifest-described shutter sequence.
camera-sim-service \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml \
  --media-root <path/to/DCIM-root> \
  --profile fuji/gfx100ii \
  --connection wireless-tether \
  --command-bind '[::]:15740' \
  --knock-bind  '[::]:51562' \
  --pcss-init-fails 1 \
  --pcss-shutter-enqueue-count 2 \
  --control-bind '127.0.0.1:8080'

# IMPLEMENTED today (tools/camera-simctl):
camera-simctl health  --control 127.0.0.1:8080
camera-simctl shutdown --control 127.0.0.1:8080
camera-simctl smoke   --host    127.0.0.1:55740    # design gate #5

# PLANNED (not implemented yet — paper shape for the script/fault surface):
camera-simctl script run scenarios/fuji/image-import-happy.yaml
camera-simctl fault set --op 0x101b --after-bytes 10485760 --action disconnect
camera-protocol-mapper probe --plan fuji/app/image-import --out observations/run.jsonl
```

Scenario script shape:

```yaml
name: image-import-happy
initialState:
  workflow: ImageImport
  mediaRoot: fixtures/gfx100ii/DCIM/100_FUJI
expect:
  - receive: { op: 0x1002 }
    respond: ok
  - receive: { setProp: 0xdf01, value: 20 }
    transition: ImageImportPrelude
  - receive: { op: 0xd621 }
    respondFrom: mediaStore.handles
```

Scripts can be strict for tests or permissive for App Review. Strict mode fails
on unexpected operations. Permissive mode logs and returns realistic
`OperationNotSupported`, `InvalidParameter`, or `AccessDenied` responses.

Startup-state overlays are the boot-time counterpart to scenario scripts: they
describe the physical/EEPROM-backed state this simulator instance starts in,
without changing the manifest's model of the camera. The service applies the
overlay after manifest/media/engine construction and before any PTP/control
listener serves traffic. Unknown properties, bad datatypes, unsupported schema
names, and profile/connection mismatches are fatal startup errors.
`camera_initiated_transfer_active` carries an externally observed trigger match;
it defaults false so reserved-queue routing cannot intercept ordinary PTP object
lookups before the consumer or test fixture supplies that transition.

```yaml
schema: ptpsim-startup-state/v1
profile: fuji/gfx100ii
connection: app
camera_initiated_transfer_active: true
props:
  "0xd02a": 2000
```

The same overlay shape is accepted by the local control API as JSON:

```sh
curl -X PATCH http://127.0.0.1:8080/state \
  -H 'Content-Type: application/json' \
  -d '{"props":{"0xd02a":2000}}'
```

State observation stays push-first. `--state-callback <http-url>` POSTs an
initial state snapshot when the callback task starts, then POSTs debounced
snapshots after state-changing operations or control mutations. `GET /state`
exists for operator inspection and debug tooling; tests should not infer state
transitions by polling it.

State snapshots include `transfer_queues.standard` and
`transfer_queues.camera_initiated` when those queues are configured. Each
reports `queued`, `completed`, and `total`; completion means the queue's own
acknowledgement boundary (`DeleteObject` for the standard queue, delivered EOF
plus final OK for the camera-initiated queue).

Fault script examples:

```yaml
faults:
  - name: disconnect-during-mov-download
    when:
      op: "0x101b"
      handleName: "*.MOV"
      afterBytes: 10485760
    action: closeCommandSocket

  - name: slow-liveview
    when:
      stream: liveview
    action:
      delayFramesMs: 120

  - name: busy-first-init
    when:
      packet: InitCommandRequest
      count: 1
    action:
      initFail: "0x2019"
```

Faults are part of the public simulator contract. They are how the app tests
pause/resume, reconnect, route-loss, timeout, and protocol-error paths without
hand-editing simulator code.

## App Integration

The app should consume the same manifest types through generated Swift data or
FFI-backed Rust queries.

### Intent → mechanism resolution

This is why the app consumes manifests at all. The app expresses **intent on a
named control** — `aperture.stepNarrower()`, `iso.set(800)`, tap-to-focus — never
a raw opcode. The manifest resolves that intent to the concrete mechanism for the
connected camera's `(model, firmware, transport, mode, state)`: which operation
code, set method (absolute vs vendor step), value encoding, readback property,
and whether the action is even legal right now.

The aperture entry is the canonical example: the same `aperture.stepNarrower()`
becomes vendor-step `0x902d` with a `0xd212` readback in live-view mode, and an
absolute `0x1016` set over tether. The app code path does not change; the
resolution does. This is the payoff of making behavior data — one app stays
correct across models, firmwares, transports, and modes.

Two delivery channels for that resolution:

- **Generated tables (build time)** for what is static: value labels and
  formatting (`280 → "f/2.8"`, `65535 → "body"`), code constants, and feature-
  gating tables.
- **Runtime manifest queries (FFI or table-indexed-by-profile)** for what depends
  on the connected camera and engaged mode: "how do I set aperture *now*", "is
  this allowed in `usb/webcam`", and quirk effects like the >4 GB → memory-card-
  only behavior.

What stays the app's job: it is the initiator, so it owns *when* and *sequencing*
(opening live view, running preludes, reacting to the user). The manifest gives
it vocabulary, gates, and quirks — not a high-level API that hides the session.

The app uses manifests for:

- Intent → mechanism resolution for operations and properties (above).
- Feature gating by model/firmware.
- Known transport options.
- Property labels and value formatting.
- Probe-plan selection.
- Diagnostic explanations.
- Simulator compatibility tests.

The app must not share responder state machines. It may share parser/serializer
tests and manifest compatibility queries.

Agent-paste handoff for the Apple-side track:
[`docs/handoff-ios-agent.md`](docs/handoff-ios-agent.md) (production app
adopting the manifest/FFI).

## Relationship To The Fixture TUI

client application's fixture TUI (`tools/tui`) and ptpsim are orthogonal and compose at
the BLE↔Wi-Fi seam; neither subsumes the other.

- The TUI is an app-state injector and the BLE/Wi-Fi boundary. It is how a test
  asserts states like "already Bluetooth-paired" without real wireless, and a
  shortcut for setting up app states for screenshots and user-flow automation.
  It is moving out of the camera/PTP interface entirely. Dev/test only.
- ptpsim is the PTP/IP wire — a believable camera responder. Dev, test, and
  production verification (it is the binary client application leases for App Review).

In review and production the app's `connectToDevCameraHost()` skips BLE/Wi-Fi
and opens PTP/IP directly, so ptpsim stands alone with no TUI involved. In local
development the TUI injects the "paired" state and the app then talks to ptpsim
over PTP/IP. The seam is BLE: the TUI owns everything up to and including
pairing; ptpsim owns everything on the PTP/IP transport.

BLE itself is a plausible future ptpsim component — per-model BLE variance is
device behavior the app should not need to know, which is the same
manifest-as-data argument that justifies ptpsim for PTP. It is explicitly out of
scope now; client application's script-driven BLE plus the TUI's "already-paired" shortcut
cover the need. The transport model leaves room for a `ble` transport kind so
this can be added later without restructuring.

## App Review Runtime

The existing backend lease model stays valid:

- One reviewer install gets one simulator instance.
- Runtime API returns raw IPv6 host plus device identity.
- The app connects directly to PTP/IP ports.
- If the pool is empty, the app reports review camera unavailable and leaves the
  normal app path intact.

Profile-key reconciliation: client application's backend keys profiles as `fuji_gfx100ii`
(`Runtime::Service` `PROFILES` → `{display_name, model}`), while ptpsim profile
ids look like `fuji/gfx100ii/fw0230`. Since ptpsim naming wins, client application
refactors its `PROFILES` keys (and the `vcam_pool` snapshot `profile` field) to
the ptpsim id. The `model` string must stay the real `GFX100 II` so the app's
model-based feature gating still fires; only `display_name` is the test marker.


by polling each instance's `/healthz`. The aggregate snapshot it writes:

```json
{

  "capacity": 8,
  "count": 3,
  "instances": [
    {
      "id": "uuid",
      "ipv6": "2602:...",
      "profile": "fuji/gfx100ii/fw0230",
      "up": true,
      "ptp_ports": [55740, 55741, 55742],
      "health": "ok"
    }
  ],
  "ts": "..."
}
```

## Transport Roadmap

Phase 1: reference app AP PTP/IP.

- Command socket on `55740`.
- Through-picture MJPEG live stream on `55741`.
- Event socket on `55742`.
- Fuji image import and live view workflows.
- IPv6 direct bind for cloud review instances. (Apple review environment is ipv4-free)

Phase 2: PCSS tether.

- Host callback listener on `51560`.
- UDP knock on `51562`.
- Camera `NOTIFY` with `DSCPORT`.
- PTP/IP command session on announced port, typically `15740`.
- Wired/wireless tether properties and `0x9018` live-view path.

Phase 2b: PTP/USB and its modes.

- USB is one link with several mutually distinct modes the camera gates
  differently: RAW conversion, backup/restore, USB webcam, and plain PTP image
  access. See "Transport And Mode Matrix" — each mode is manifest data.
- `http` (the camera's embedded webserver, previously called XLV): a top
  exploration target for GFX100 II — it may be where the no-op-seeming live-view
  size properties are actually real. Opportunistic, not blocking, but high-value.

Phase 3: Other manufacturers.

- Nikon next (and RED, which Nikon now owns) — their app/tooling situation is the
  kind of gap ptpsim exists to close. Then Canon PTP/IP/EOS.
- Each manufacturer is a manifest + captures, run by the same generic engine. New
  code only for a genuinely new wire format (→ `ptp-core`) or computed quirk (→
  `protocol-primitives`), added as a shared peer — never a per-manufacturer crate.
- Shared media store and manifest pipeline reused unchanged. (Manufacturers have
  proprietary RAW formats — handled as media-store format entries, still data.)

Paper designs for the next transports (PTP/USB + modes, XLV HTTP/HTTPS, wireless
tether) live in [`docs/TRANSPORTS.md`](docs/TRANSPORTS.md) — each lands as an
adapter + manifest data, with the core crates unchanged.

Out of scope for now (candidate later transport): a `ble` transport kind for
discovery/pairing emulation. `camera-protocol-mapper`'s script-driven BLE plus the TUI's
"already-paired" shortcut cover the current need. If emulation becomes
necessary, BLE belongs in ptpsim as a data-driven transport for the same reason
as PTP — per-model variance the app should not have to know.

## Transport And Mode Matrix

What operations and properties a camera allows is not a function of
`(model, firmware)` alone. It is a function of
`(model, firmware, transport, mode, state)`, and the gating is frequently
arbitrary — one mode forbids an operation a sibling mode allows, with no
underlying rule. This is the central reason behavior must be data: there is
nothing to encode but a lookup table.

Observed transports and modes for Fuji:

- `app` — Wi-Fi app protocol (three sockets; live view + image import).
- `ptpip` — PTP/IP command session (PCSS tether path).
- `usb` — PTP over USB, one link with several mutually exclusive modes:
  - `usb/raw-conversion` (note: still allows some camera ops that make no sense
    in this mode — cross-mode bleed is real and must be captured, not assumed)
  - `usb/backup-restore`
  - `usb/webcam`
  - `usb/image` (plain PTP object access)
- `http` (was "XLV") — the camera's embedded HTTP webserver. Reportedly serves
  up to 4 connected cameras, and is a candidate for activating the live-view
  size properties that appear named for that purpose but no-op over other
  transports. Top exploration target for GFX100 II.

`mode` is a first-class manifest axis alongside `transport`, and the dependency
is stronger than a support matrix. Two things resolve against the full
`(transport, mode, state)` tuple, not just availability:

- **Connection establishment.** A connection is defined by *how it comes into
  being*, not by being TCP. Some ports are opened by BLE, some by UDP knock,
  some by direct bind. A "TCP connection" reached through a BLE-opened port is
  not interchangeable with one reached through a knock. The transport spec owns
  the establishment mechanism per mode — the lower comms layer being OS
  pass-through does not make it absent from the model; it reaches through.
- **Operation semantics.** The same opcode can *mean different things* in
  different engaged modes, not merely be allowed or denied. Workflow handlers
  are selected by engaged mode; a handler in one mode is a genuinely different
  handler in another. This is why probe observations are per-mode and cannot be
  inferred across modes.

Explicitly not modeled: Wi-Fi AP vs infrastructure. The OS handles it and it is
pass-through *and the protocol behaves identically*, so it does not exist here.
(Contrast the modes above, where the lower layer is also pass-through but the
camera's behavior is not — those must be modeled.)

This matrix is also a probe target: `camera-protocol-mapper` plans are keyed by
`(manufacturer, transport, mode)`, so "what does this camera allow, and mean, in
USB webcam mode" is a probe you run, not an assumption.

## Transport Contracts

### reference app AP PTP/IP

Listener setup:

1. Select a manifest connection with `--connection` (`app` by default).
2. Bind only the selected connection's declared socket roles.
3. For `app`, the default roles are command `55740`, event `55741`, and live-view stream `55742`.
4. For `wireless-tether`, the default role is command `15740`; live view is command-channel polling.
5. Accept command socket first. Event/live-view sockets may connect before or
   after workflow startup, but the workflow decides when bytes are sent.

Command session:

```text
client -> command: InitCommandRequest
camera -> command: InitCommandAck
client -> command: OpenSession
camera -> command: OK
client -> command: workflow-specific operations
camera -> command: data/response packets
```

Event session:

```text
client -> event: TCP connect
camera -> event: event packets when workflow triggers them
```

Live-view stream:

```text
client -> liveview: TCP connect
camera -> liveview: repeated length-prefixed Fuji frame packets
```

The live-view frame source is a trait:

```rust
pub trait FrameSource {
    fn next_frame(&mut self, clock: &dyn Clock) -> Result<Frame, FrameError>;
}
```

Implementations:

- Directory-backed MJPEG frames.
- Static JPEG loop.
- Generated test pattern.
- Later: host-side transcode source for real tether workflows.

### PCSS Tether

PCSS is a separate transport profile, not a special case of reference app AP mode.

Modeled real-camera flow:

```text
host listens TCP :51560
host sends UDP camera:51562 DISCOVERY ... HOST:<hostIP> ... SERVICE:PCSS/1.0
camera dials host:51560 and sends NOTIFY ... DSCPORT:<port>
host replies HTTP/1.1 200 OK
host connects TCP camera:<DSCPORT>, usually 15740
PTP/IP InitCommandRequest -> InitCommandAck -> OpenSession
```

Simulator modes:

- Responder mode for app review: simulator exposes the selected manifest
  connection, defaulting to the app command/event/live-view sockets.
- Full PCSS lab mode: simulator also implements UDP knock and callback behavior
  so desktop tether clients can be tested. It is opt-in via `--knock-bind`;
  hosted direct-connect instances do not bind the UDP listener by default.
- PCSS object transfer is queue-shaped: `0x1007` lists queued handles,
  `0x1008`/`0x100A`/`0x1009` inspect or pull them, and `0x100B` drains them.
  The default queue is seeded at startup; `--pcss-shutter-enqueue-count N`
  starts empty and adds media handles after each manifest shutter sequence.
- Camera-initiated media transfer is a separate pull queue. BLE only signals
  availability and handoff state; the app opens the declared PTP/IP endpoint and
  requests the fixed queue head. No image bytes are emitted unsolicited.
- A streamed head advances only after the service writes all object bytes and
  the final successful PTP response. Partial reads and transport failures remain
  retryable and do not change the queue.

PCSS must live behind a manifest transport entry because ports, callback
behavior, and one-shot-per-boot quirks can vary by model and firmware.

## Fuji Workflows

### LiveView

State machine:

```text
Disconnected
  -> InitAcked
  -> SessionOpen
  -> FunctionModeSet(df00=6, df01=22)
  -> RemoteExNegotiated(df2a)
  -> CaptureOpen(0x101c)
  -> Streaming(55741)
  -> Stopping
  -> Closed
```

Supported behavior:

- Push MJPEG frames from directory or generated frame source.
- Emit Fuji length-prefixed frame packets.
- Keep command and event sockets independent.
- Support `0xd212` status reads.
- Support ISO absolute writes to `0xd02a`.
- Support shutter/aperture/EV step ops `0x902c`, `0x902d`, `0x902e`.
- Support tap-to-focus `0x9026`/`0x9027` and AF status readback.
- Support live-view shutter `0x100e` with realistic events.

### ImageImport

State machine:

```text
Disconnected
  -> InitAcked
  -> SessionOpen
  -> FunctionModeSet(df00=6, df01=20)
  -> ImportVersionNegotiated(df28)
  -> BrowsePrelude
  -> EnumeratingFoldersDates
  -> HandlesReady
  -> Thumbnailing
  -> Downloading
  -> Closed
```

Supported behavior:

- `0x9054` current object metadata.
- `0x9055` current object thumbnail.
- `0x9050` folder list.
- `0x9053` date list.
- `0xd620` object count.
- `0xd621` object handles.
- Standard `GetObjectInfo`, `GetThumb`, `GetPartialObject`.
- File-backed chunked downloads.
- Oversize MOV import via reported-size sentinel, extension true-size lookup,
  and high/low partial-read offsets.

### CameraControls

State machine:

```text
SessionOpen
  -> DescriptorSweep
  -> StatusRead
  -> ControlIdle
  -> ApplyingWrite
  -> ReadbackConfirm
```

Supported behavior:

- Descriptor-backed list-pick controls.
- Ring-pick controls through vendor step ops.
- Dynamic `0xd212` status bundle.
- Manifest-selected read-only and unavailable responses.

### Firmware

Firmware transfer remains modeled but inert until deliberately implemented.

State machine:

```text
SessionOpen
  -> FirmwarePrelude
  -> FileInfoAccepted
  -> ReceivingChunks
  -> TransferComplete
  -> CameraInstallPrompt
```

The simulator can exercise app gates and progress UI without shipping a real
firmware payload.

## Testing Strategy

Test layers:

- Unit tests in `ptp-core` for packet/container/property/object encoding.
- Manifest schema tests and compatibility-query tests.
- Media-store tests with symlink escapes, sparse files, RAF fixtures, MOV
  metadata, and thumbnail extraction.
- Fuji workflow state-machine tests.
- Golden packet tests from real captures and known-good probe runs.
- Script runner tests for strict and permissive scenarios.
- Black-box smoke client similar to current `scripts/smoke-gfx100ii.py`.
- App integration tests that run the simulator and drive the real app client.
- Soak tests for multiple concurrent leased instances on one 1 GB VM.

Every manifest assertion should be traceable to one of:

- real-camera probe observation,
- wire capture,
- official/public protocol spec,
- deliberate synthetic test fixture.

Concrete gates before replacing C vcam in App Review:

1. `ptp-core` golden packet tests pass for InitCommandRequest/Ack,
   OpenSession, GetObjectInfo, GetThumb, GetPartialObject, SetDevicePropValue,
   and Fuji event packets.
2. Media-store tests prove symlink escapes are rejected, in-root symlinks work,
   RAF thumbnail extraction reads bounded ranges, and a logical `>4 GB` MOV is
   represented without allocating the full size.
3. Fuji image-import session-flow test completes from init through handles,
   thumbnails, partial download, and close.
4. Fuji live-view session-flow test opens all three sockets, streams frames, and
   handles ISO/step/focus/shutter commands while frames continue.
5. Black-box smoke client downloads at least one JPEG, exercises a RAF
   thumbnail when available, and reports the missing-real-media messages when
   fixtures are absent.
6. App Review mode leases one IPv6 instance, the app connects to it, starts live
   view, browses files, and downloads a file.
7. Soak test runs five leased instances on the target 1 GB VM profile without
   unbounded memory growth.

## Performance And Resource Targets

Initial targets for cloud review instances:

- Idle RSS under 50 MB per simulator instance.
- Live-view streaming without buffering more than two frames per session.
- File downloads use bounded chunk buffers.
- Stream completion is acknowledged back to the engine only after the transport
  writer successfully emits both the data phase and final response.
- Five simultaneous reviewers on a 1 GB instance.
- Structured log volume capped or sampled for long live-view runs.

The tethered-shooting product target is higher:

- Keep 60 fps 640x480 MJPEG streaming smooth.
- Do not block stream reads on command-socket maintenance.
- Use async tasks and backpressure rather than per-frame thread creation.

## Migration Plan

1. Freeze client application's current C vcam scope to App Review and existing smoke
   coverage.
2. Stand up the standalone ptpsim repo and add `ptp-core` with packet and PTP
   object/property encoders.
3. Port the existing `smoke-gfx100ii.py` expectations (in the legacy vcam repo)
   into golden tests.
4. Build `camera-media-store` and reproduce current file browsing behavior.
5. Implement Fuji image-import workflow from the manifest.
6. Implement reference app live-view sockets and directory-backed MJPEG streaming.
7. Add control API (`/healthz`, `/shutdown`, scenario/fault/trace) and scenario
   runner.
8. client application refactor: delete placeholder `camera-protocol-{core,fuji,ffi}`
   crates and depend on the published ptpsim crates; land the redacted GFX100 II
   manifest in ptpsim's public `packages/camera-config-data`; reconcile client application's
   `PROFILES` keys to ptpsim profile ids.
9. Point the client application management sidecar at the ptpsim binary and have it build
   the `vcam_pool` inventory snapshot by polling `/healthz`.
10. Run the app against ptpsim in review mode.
11. Move cloud pool from the C vcam image to the ptpsim image.
12. Keep C vcam only as historical reference.

## Requirement Traceability

| Requirement | Design evidence |
|---|---|
| `ptp-core` packet parsing/serialization and containers | `ptp-core` responsibilities and public `PtpIpPacket`/`PtpCodec` sketch |
| ObjectInfo and property descriptors | `ptp-core`, `camera-config`, property schema, media policy |
| Manifest-driven workflow state machines (generic `camera-sim` interpreter) | Fuji workflow sections for LiveView, ImageImport, CameraControls, Firmware |
| Filesystem-backed media store | `camera-media-store` contract and media policy schema |
| Symlink-safe traversal | media-store responsibilities and concrete test gate |
| RAF/MOV/JPG behavior | media-store responsibilities, media policy, test gates |
| `>4 GB` metadata | media-store synthetic metadata and transfer ceiling policy |
| reference app transport `55740/55741/55742` | manifest transport, service defaults, reference app transport contract |
| Later PCSS `51560/51562/15740` | manifest transport and PCSS transport contract |
| Scenario/profile data-driven behavior | manifest model, scenario script shape, GFX100 II profile as data |
| One simulator per reviewer lease | runtime component model and App Review runtime |
| IPv6 bind | simulator service responsibilities and config |
| Health endpoint | `/healthz` contract |
| Structured logs | structured log event shape |
| Golden packet tests | testing strategy and replacement gates |
| Session-flow tests | Fuji workflow tests and replacement gates |
| Black-box smoke client | testing strategy and replacement gates |
| Rust preference | chosen direction and Rust service/crate layout |
| Ruby allowed for service/testing | chosen direction and open implementation boundary |
| Probe/upload/write manifest loop | purpose section and probe/manifest pipeline |
| Avoid app-shaped fake | design principles, role boundary, app integration section |

## Resolved Implementation Decisions

- Repo: ptpsim is a standalone open-source repo. client application consumes the published
  crates and deletes its placeholder `camera-protocol-*` crates. Monorepo
  rejected because a generic, community-contributable simulator should not carry
  client application's app/backend.
- Crate names: ptpsim naming (`ptp-core`, `camera-sim`, `camera-config`, …)
  wins over client application's `camera-protocol-*`; client application refactors to match.
  Published crates carry a `ptpsim-` prefix to avoid crates.io collisions.
- No per-manufacturer crates. One generic `camera-sim` engine + a concern-organized
  `protocol-primitives` registry; manufacturer differences are manifest data.
  Adding a camera is a data PR, not a new crate (the anti-vcam test).
- Control plane: ptpsim is lease-agnostic. It exposes `/healthz`, `/shutdown`,
  and control endpoints, and honors `SIGTERM`. Leasing/pooling/NATS stay in
  client application's management sidecar, which builds pool inventory by polling
  `/healthz`. A management sidecar talking to NATS is acceptable; it is not part
  of ptpsim.
- Open-source boundary: the whole simulator is public — engine, schema, probe,
  service, CLI, manufacturer crates, and captured manifests/captures/fixtures.
  Only a consumer's app, backend, and management sidecar stay private. Evidence
  is provenance only — manifests must load and run with it unresolved; captures
  and fixtures pass redaction/redistribution hygiene before landing.
- Manifest format: YAML source with generated JSON Schema.
- Swift boundary: generated Swift tables first; Rust FFI only for logic that
  would otherwise be duplicated incorrectly.
- Ruby: keep leasing/control-plane code in Ruby (client application sidecar); run protocol
  runtime in Rust (ptpsim).
- TUI: orthogonal to ptpsim. TUI owns BLE/app-state injection (dev/test only);
  ptpsim owns the PTP/IP wire (dev/test/production-verification).
- Probe tool: `camera-protocol-mapper`, a Python tool in its own repository
  (not part of this workspace), seeded by fuji-remote. Python for field portability
  (ssh/RDP, no toolchain). The JSONL observation bundle is the only seam; stable
  plans may be ported to Rust later but need not be. Probe plans declare an
  escalating risk class: `safe` (default, read-mostly) → `settings-write`
  (fuzzing) → `ram` → `firmware-supported` → `firmware-unlocked`, each a louder
  opt-in, all above `safe` under use-at-your-own-risk/no-warranty. The factory-
  reset opcode is denylisted so a blind sweep can never fire it.
- Transport + mode: not just availability — *connection establishment* (BLE- vs
  knock- vs direct-opened) and *operation semantics* resolve against
  `(model, firmware, transport, mode, state)`. Same opcode can mean different
  things per mode; ops bleed across modes. `mode` is a first-class manifest axis
  (USB raw-conversion / backup-restore / webcam / image; plus `http` = the
  camera webserver, ex-"XLV"). All data, not code. Wi-Fi AP vs infra is not
  modeled (OS pass-through *and* identical protocol behavior).
- Canonical manifest generator: lives in `camera-config` (Rust), consuming the
  observation bundle — not in `camera-protocol-mapper`. One generator next to the schema
  it validates against, serving all bundle sources, reproducible from a stored
  bundle. `camera-protocol-mapper` may emit a non-canonical field draft; needed context
  goes into the bundle, not a second generator.
- Manufacturer order: Fuji, then Nikon (and RED, Nikon-owned), then Canon.
- PCSS: start as a sibling transport module under Fuji, not inside reference app AP
  workflow code.
- BLE emulation: out of scope now; candidate future ptpsim transport.

## Remaining Design Risks

- Probe-to-manifest inference can overfit one capture. Mitigation: store raw
  observations, require evidence references, keep unknown semantic labels raw,
  and make generated proposals reviewable.
- Official vendor tooling may use alternate workflows for the same apparent feature.
  Mitigation: transport/workflow is part of the manifest key, not only model
  name and firmware.
- A simulator can become too forgiving and hide app bugs. Mitigation: strict
  scenario mode for tests, permissive mode only for review/demo operation.
- Large media and live-view streams can become memory hazards. Mitigation:
  media-store `ByteSource`, bounded chunk reads, frame-source backpressure, and
  soak tests.
- Firmware-mode probing can damage the test camera itself, not just risk users.
  Firmware experiments (supported and unlocked paths) have already left a GFX100
  II in a state where the upgrade cycle will not complete, which blocks reaching
  fw 2.40 — the firmware most of the GFX fleet now runs and the version we most
  need a clean manifest for. Mitigation: keep firmware tiers strictly opt-in and
  off the default sweep; preserve at least one known-good camera on the target
  firmware for capture; treat captures as the durable asset so a bricked or
  stuck unit does not erase the protocol knowledge it produced.
