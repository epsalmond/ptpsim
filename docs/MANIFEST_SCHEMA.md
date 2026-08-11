---
description: The manifest schema contract — template grammar, step/observation vocabulary, predicates, control flow, and wire conventions. When code or any other doc disagrees, this document wins.
status: reference
read-when: Authoring or reviewing camera-config manifests, schema changes, FFI variants, or anything that parses/emits manifest data.
---

# Manifest schema — precise contracts and conventions (§11)

Extracted verbatim from the iOS rewrite P0/P1 plan, whose §11 served as the
schema authority ("contract tiebreaker") while the plan shipped; section
numbering (11.x) is preserved so existing references remain stable. §11.11
(uniffi packaging + iOS bundling) is release engineering, not schema, and
lives in [`APPLE_FFI_RELEASES.md`](APPLE_FFI_RELEASES.md).

This section nails down the conventions earlier sections lean on but don't
formalize. **A clean-context implementer reads this once; everything is a
contract.** Where an earlier section conflicts with §11, §11 wins.

### 11.1 Template grammar

Three reference forms appear inside step values. They are syntactically
distinct so the parser can route them at deserialization time.

| Form | Where it appears | Resolved at | Resolution source |
|---|---|---|---|
| `"{family.path.dotted}"` — bare braces, dotted path | any string field in YAML | **FFI load** | the model's resolved family-merged tree (e.g. `{ble.advert.manufacturerCompanyId}` → `1240`; nested-map paths like `{ble.advert.serviceUuids.fileTransfer}` dot-walk). Unresolved → load-time error. |
| `{ captured: <name> }` — structured StepValue | `bleWrite.value`, `udpSend.payload`, similar value-typed fields | **dispatcher walk** | `scope[<name>]` (populated by recognize-seed, `bleRead.capture_as`, `acquire`, `tcpExpect ?<name>`). Unresolved → tolerant-aware error (see §11.6). |
| `{ runtime: <slot>, encoding: <name> }` — structured StepValue | same places as `captured` | **dispatcher walk** | `runtime_params[<slot>]` (a separate map the app populates at walk start: terminal name, host IP, etc.). Unresolved → tolerant-aware error. |
| `{ template: <string> }` — inline byte/string template with bare `{name}` interpolations | `udpSend.payload`, anywhere a Vec<u8> is expected from a string | **dispatcher walk** | each `{name}` in the string resolves against scope ∪ runtime_params (scope wins on collision). Unresolved → tolerant-aware error. |

**Phases:**
- *FFI load*: family inheritance is merged into per-model resolved views. Every static path ref (`{ble.advert.manufacturerCompanyId}`) becomes a literal at this point. The plan returned by `establishment()` has only structured `Captured` / `Runtime` / `Template` / `Literal` step values, never raw path refs.
- *`establishment()` call*: the FFI takes `initial_scope` from `Recognition::Candidate` and stamps a `plan_handle`. No template resolution happens here; the plan is returned with structured StepValues intact.
- *Walk*: per-step, the dispatcher resolves each `Captured` / `Runtime` / `Template` against its growing scope + the runtime_params map. Resolved values become bytes via §11.2's encoding rules. `Literal` values are bytes-already.

**Bare `{name}` is not a top-level StepValue form.** It appears ONLY inside a `Template` string. So `bleWrite: { value: "{ssid}" }` is a YAML-author error; use `bleWrite: { value: { captured: ssid } }` or wrap explicitly in `{ template: "{ssid}" }`.

### 11.2 Encoding allowlist

Every place an `encoding:` field appears, it's drawn from this fixed enum.
Anything else is a schema-validation error at FFI load.

| Encoding | Wire bytes ↔ scope string |
|---|---|
| `utf8` | bytes ↔ UTF-8 text. Round-trip fails on invalid UTF-8 (tolerant-aware). |
| `ascii` | bytes ↔ ASCII text. Round-trip fails on non-ASCII bytes (tolerant-aware). |
| `bytes-raw` | bytes ↔ lowercase hex string (no separators). No byte-order semantic. |
| `bytes-le` | bytes ↔ lowercase hex of the bytes in wire (little-endian) order. Semantic hint only; representation matches `bytes-raw`. |
| `bytes-be` | bytes ↔ lowercase hex of the bytes in wire (big-endian) order. Semantic hint only. |
| `u8` | 1 byte ↔ decimal integer string. |
| `u16-le` / `u16-be` | 2 bytes ↔ decimal integer string. |
| `u32-le` / `u32-be` | 4 bytes ↔ decimal integer string. |

The scope (`Vec<KeyValue { key: String, value: String }>`) **always carries strings**. Encoding is the dispatcher's contract for turning bytes-from-wire into scope strings (on capture) and scope strings into bytes-to-wire (on consumption). Integer encodings render as decimal strings ("12345"); byte encodings as hex strings ("44732a80"); text encodings as the literal string.

### 11.3 GATT-name resolution

Steps in the YAML reference GATT characteristics by symbolic name (`gatt: pairingKey`). **The FFI resolves these to UUID strings at index-build time.** The Step variant returned over the uniffi boundary carries the resolved UUID, NOT the symbolic name. The dispatcher passes the UUID straight to CoreBluetooth.

If a step references a `gatt:` name not declared in the resolved family `gatt:` map, that's a load-time error.

### 11.4 BLE peripheral binding (`bleConnect` contract)

The FFI never sees a `CBPeripheral`. The app holds it.

**Sequence the app implements:**
1. BLE scan delivers an advert → app calls `recognize()` → gets `Candidate`.
2. App captures the matched `CBPeripheral` into its dispatcher-side BLE primitive (`bleIO.boundPeripheral = peripheral`). This is an app-side assignment; no FFI call.
3. App calls `store.establishment(model, "ble", initial_scope: candidate.runtime_scope)` → gets `Plan`.
4. App calls `dispatcher.walk(plan)`. The `BleConnect` step in the plan carries no parameters; the dispatcher's BLE primitive calls `connect()` on its bound peripheral.

`BleConnect` therefore has **no fields** in the Step enum (just `StepOptions`). Earlier draft showed `service_uuid: StepValue` — drop that.

### 11.4a `bleDelay` / `bleRequestMtu` / `bleDiscoverServices`

Sony pairing and Wi-Fi flows request MTU 158 before GATT traffic; Sony,
Canon, and Nikon all make service discovery an explicit state transition.
Three setup verbs make those expressible:

- `bleDelay: { durationMs: <u32> }` — wait for one nonzero, manifest-authored
  interval using the transport clock. legacy manufacturer app uses it between connect and
  MTU request; it is not folded into `bleConnect` or retry policy.
- `bleRequestMtu: { requestedMtu: <u16>, minimumMtu: <u16>? }` —
  `requestedMtu` is the request target the reference app asks the stack for
  (the Android `requestMtu` argument); a platform without a request API
  (CoreBluetooth negotiates automatically) makes no call. `minimumMtu` is a
  separately evidenced floor below which the flow actually fails; declare it
  only with wire-capture or hardware evidence, never from one data point on
  one camera/phone/OS combination. After any request, the step compares the
  negotiated MTU against a declared `minimumMtu` on every platform and fails
  (tolerant-aware per §11.6) below it; with no floor declared it succeeds at
  any negotiated MTU. `minimumMtu` greater than `requestedMtu` is a
  load-time error.
- `bleDiscoverServices: {}` — on auto-discovering stacks this is a
  completion checkpoint, not a re-trigger. Discovery timeout is dispatcher
  policy.

`bleConnect` stays connection-only (§11.4); setup is separate finite steps,
never options folded into `bleConnect`.

Saved-camera recovery uses a separate manifest-driven decision surface. The
host scans for fresh advertisements, passes each observation plus the saved
pairing scope to `reconnectDecision`, and walks either the returned wake plan or
ready reconnect plan. Wake completion is the camera's expected peer disconnect,
expressed by `bleAwaitDisconnect`; the host then scans again until an awake
signature matches. `reconnectPolicy.scanTimeoutMs` bounds each scan phase.
Reconnect-only signatures are excluded from normal discovery with
`discoverable: false`, and declared identity fields must match persisted scope.
For the legacy GFX100 II, the startup-information service selects the wake
route. The awake route requires the file-transfer service, a model-shaped local
name, and no Fuji manufacturer data; it captures the local-name short serial as
the saved-camera identity. The manufacturer-data form is pairing-mode discovery,
not awake readiness. See issue #264 for the wire observation behind this split.

### 11.4b `blePeripheralName`: platform peripheral-name capture

CoreBluetooth filters the GAP (0x1800) and GATT (0x1801) services from
discovery. A GATT read of the Device Name characteristic (0x2A00) therefore
cannot succeed on iOS, regardless of camera behavior; the same value is only
available as `CBPeripheral.name`. A step that needs the peripheral's name
must not be authored as a 0x2A00 `bleRead`.

- `blePeripheralName: { captureAs: <slot> }` captures the connected
  peripheral's platform name into scope as a UTF-8 string with any NUL
  terminator removed. Hosts without GAP access use the platform
  peripheral-name property; hosts with GAP access may satisfy the step with
  the 0x2A00 GATT read. The step implies no GATT traffic on checkpoint
  platforms and takes the usual `StepOptions`. An unavailable name
  (`CBPeripheral.name` is optional) is a transport error, never an empty
  string: the step fails, tolerant-aware as usual.

### 11.5 `refine_establishment` semantics

The FFI validates the plan handle and returns either "no change" or ONLY the
replacement unwalked tail; the dispatcher splices only for `ReplaceTail`.

```rust
pub enum EstablishmentRefinement {
    NoChange,
    ReplaceTail { steps: Vec<Step> },
}

pub fn refine_establishment(
    plan_handle: String,
    firmware: String,
    scope: Vec<KeyValue>,
    next_step_index: u32,    // index of the FIRST step that has not yet executed
) -> Result<EstablishmentRefinement, EstablishmentError>;
```

Dispatcher loop:
```
case .acquireFirmware(let from, _):
    let fw = try await runAcquire(from)
    scope["firmware"] = .string(fw)
    switch try store.refineEstablishment(planHandle, fw, scopeAsKVs(), UInt32(i + 1)) {
    case .noChange:
        break
    case .replaceTail(let steps):
        plan.steps.replaceSubrange((i + 1)..., with: steps)
    }
    // continue: i += 1 falls through to next iteration
```

The refined tail includes any `if:` evaluations that depend on `firmware` already pre-evaluated against the new value (the FFI evaluates conditions whose predicate fields are all present in `scope`; conditions referencing later acquires stay as `If` steps for the dispatcher).

If `refine_establishment` returns `NoChange`, the dispatcher leaves the existing tail in place. Invalid plan handles, unknown plans, or impossible `next_step_index` values are errors, not no-ops.

### 11.6 Tolerant + confirmation + error model

Every Step variant carries
`StepOptions { tolerant, retries, retry_delay_ms, confirms }`. `confirms` is
absent by default; its only value in this schema version is `registration`:

```yaml
- bleRead:
    gatt: transferState
    encoding: bytes
    captureAs: transferState
    tolerant: true
    confirms: registration
```

The marker is legal only on signal-bearing `bleRead`, `bleNotify`, and
`bleAwaitUntil` steps. It may appear inside an `if.then` or `if.else` branch,
because step paths and branch selection remain visible to the executor. It is
not legal in `postExitReadiness`, BLE actions, acquire delegates, retry bodies,
`bleAwaitUntil.onEach`, or failure-evidence steps: those contexts may execute
zero or multiple times and do not define the one-shot establishment verdict.
Exactly one step in an establishment plan's structural `steps` tree may carry
the marker; duplicates are a load-time error.

A completed establishment walk returns this summary in addition to its final
scope and `steps_run`:

```rust
pub struct EstablishmentWalkSummary {
    pub confirm_outcome: EstablishmentConfirmOutcome,
    pub tolerated_step_count: u32,
    pub tolerated_step_paths: Vec<String>,
}

pub enum EstablishmentConfirmOutcome {
    Satisfied,
    Unsatisfied,
    NotDeclared,
}
```

`tolerated_step_paths` uses the same exact nested paths as step reporting, in
execution order, and `tolerated_step_count` is its length. Every terminal
`Tolerated` step report contributes one entry, including nested steps.
`confirm_outcome` is `Satisfied` only when the marked step itself succeeds;
it is `Unsatisfied` when that step fails, whether tolerated or fatal, or when
it is skipped because its `if` branch is not taken. It is `NotDeclared` only
when the plan's structural `steps` tree contains no marker. A tolerated failure
of the marked step still lets the walk continue and complete; tolerance never
converts the confirmation verdict to `Satisfied`. Fatal walks continue to
return the existing executor error rather than a completed-walk summary, while
the marked step's terminal report remains `Failed`.

**Dispatcher's per-step retry loop:**
```
for attempt in 0...retries {
    do { try perform(step); break }
    catch {
        if attempt == retries {
            if step.opts.tolerant {
                log("tolerant step failed, continuing: \(error)")
                break
            }
            throw error
        }
        await sleep(retry_delay_ms.ms)
    }
}
```

**Errors that count as "step failed":**
- Wire-level failure (BLE GATT error, write timeout, read returned error code).
- Capture decode failure (e.g. `encoding: ascii` on a non-ASCII byte).
- Transform-chain failure (out-of-range `slice`, integer op on > 8 bytes — §11.13).
- Template ref unresolved (`{captured: name}` where name isn't in scope).
- `bleRead.capture_as` of a GATT char absent from the camera's exposed catalog.

Fatal executor failures expose a stable failure kind. `DeadlineExceeded` covers
executor-owned verb backstops, step-level budgets, and transport-reported
timeouts; `ConditionRejected` covers a manifest-declared terminal condition;
`Other` covers every remaining failure. The failure also carries a curated
`context` list. Only keys explicitly named by a control-flow step may enter
that list; the executor never exports its complete runtime scope.

The `retry` control-flow step provides predicate-gated recovery when ordinary
per-step retries are too broad:

```yaml
- retry:
    steps: [ <step>, ... ]
    whenFailure: conditionRejected
    onFailure: [ <diagnostic step>, ... ]
    retryWhen: { stateErrorDetails: { eq: 2 } }
    maxAttempts: 2
    retryDelayMs: 200
    failureContext: [apState, stateErrorDetails]
```

`steps` and `onFailure` share the surrounding runtime scope. An unsuccessful
body is handled only when its stable kind equals `whenFailure`; unrelated
failures escape unchanged. On a selected failure, `onFailure` runs before
`retryWhen` is evaluated, so it may capture the diagnostic value used by the
predicate. A missing predicate field is an error. A false predicate or an
exhausted `maxAttempts` rethrows the selected body failure with only the named
`failureContext` values attached. A true predicate with attempts remaining
sleeps for `retryDelayMs`, then reruns `steps`. `maxAttempts` includes the
first attempt and must be at least one. The outer step report's `attempts`
counts retries consumed (zero on the first attempt); nested steps retain their
own reports. `onFailure` is
also run for the final selected failure, while an `onFailure` failure escapes
as its own error.

The retry step does not reset transport state. Within one executor walk,
subscribing again to the same GATT characteristic and mode is idempotent: an
already-successful subscription is reused rather than rewriting its CCCD.

**`If` step's `tolerant: bool`:**
- `false` (default): predicate references an unbound scope key → hard dispatcher error.
- `true`: predicate references an unbound scope key → predicate evaluates as `false` → else branch (or skip if no else).

This is the second-class case of tolerance — useful for "if the body exposed this characteristic, do the extra dance; otherwise skip."

### 11.7 Signature match precedence

When multiple signatures match a single observation, **file declaration order wins** (top-of-file first). Authors order signatures by precedence.

When multiple MODELS match the same signature (e.g. a Fuji advert with no model-distinguishing bytes), the result is `Recognition::Disambiguate { candidates: ... }` in model declaration order. The runtime_scope at the Disambiguate level carries facts true for all candidates (e.g. `style: "legacy"`).

The FFI does NOT auto-pick a model from a Disambiguate. The app prompts the user or applies its own heuristic, then passes the chosen model id to `establishment(...)` with the Disambiguate's runtime_scope.

### 11.8 `bleNotify` acceptance condition

One field — `until` — a tagged union of acceptance conditions. No `expect` field (drop it from earlier sections).

```rust
pub enum BleNotifyUntil {
    Any,                              // first notification, any payload
    Equals { value: Vec<u8> },        // payload byte-equals (encoding-decoded if YAML used encoding+string)
    Matches { pattern: String },      // regex match on UTF-8 decoding of payload
}

pub enum Step {
    BleNotify {
        gatt: String,
        until: BleNotifyUntil,
        capture_as: Option<String>,   // bind WHOLE matching payload to scope if set
        capture: Vec<NotifyCapture>,  // field captures: window → transform chain → encoding → scope
        mode: CccdMode,               // notify (default) | indicate — which CCCD value to write
        timeout_ms: u32,
        opts: StepOptions,
    },
    ...
}
```

YAML form:
```yaml
- bleNotify:
    gatt: apState
    until: { equals: 0x8001, encoding: u16-le }
    captureAs: apStatus              # optional — whole payload, for debugging/unknown records
    capture:                         # optional — transformed field captures (§11.13 pipeline)
      - { at: 2, length: 1, encoding: u8, name: wifiStatus }
    mode: notify                     # default; `indicate` for indication CCCDs (Canon/Nikon)
    timeoutMs: 5000
    options: { tolerant: false, retries: 0 }
```

If `until.equals` value is supplied as a string with an `encoding:` field, the FFI decodes to bytes at load time. The Step over the wire carries the decoded `Vec<u8>`.

A failing field capture (window out of range, chain failure, decode mismatch)
is **skipped**, it does not fail the step — `until` alone gates step success.

For CCCD-only flows where the camera advances on the descriptor-write
callback itself (no notification payload is emitted), use `bleSubscribe`
instead — same `gatt` + `timeoutMs` + `mode`, no `until` / `captureAs` /
`capture`. The Fuji `mFirstRegisterNotify` / `mSecondRegisterNotify` rounds in
`fuji/index.yaml` are the canonical example.

`mode` selects the CCCD value written by either verb: Android-style
dispatchers write `ENABLE_NOTIFICATION_VALUE` vs `ENABLE_INDICATION_VALUE`;
CoreBluetooth maps both to `setNotifyValue(true)` (the OS picks per the
characteristic's properties), so iOS dispatchers may ignore it.

### 11.9 Inheritance + override rules

When a model's resolved view merges with a family it inherits from, per-field type:

| Field type | Merge rule |
|---|---|
| Scalar (string, int, bool) | Model wins if present; else inherits. |
| Map (e.g. `gatt:`) | Per-key: model's keys add to / override the family's. Family-only keys survive. |
| Array (`establishment.steps`, `signatures.*.capture`, etc.) | Model REPLACES the entire array if present. No partial-array merge in this version. |

For the MVP, no model overrides any establishment array; gatt entries merge per-key (rare); signatures are entirely model-level. Schema-validation passes catch broken inheritance refs at load.

### 11.10 Loader contract (`from_manufacturer_index`)

```rust
pub fn from_manufacturer_index(
    index_yaml: String,
    model_bodies: Vec<KeyValue>,   // (model_id, yaml)
) -> Result<Arc<Self>, ConfigError>;
```

Behavior:
- For every `models[*].id` in the index, `model_bodies` MUST contain an entry with matching key. Missing → `ConfigError::MissingModelBody { id }`.
- Extra entries in `model_bodies` (id not referenced in index) → warned via `tracing::warn!`, ignored.
- YAML parse failure on `index_yaml` → `ConfigError::IndexParse(err)`. Failure on any model body → `ConfigError::BodyParse { id, err }`.
- Inheritance reference to a non-existent family → `ConfigError::UnknownFamily { model_id, family_id }`.
- Signature schema validation (encoding allowlist, gatt-name refs, predicate field names) → `ConfigError::Validation { path, message }`.

The loader is fail-fast: any error aborts the entire load. There is no partial-success path.

### 11.11 uniffi packaging + iOS bundling

Moved to [`APPLE_FFI_RELEASES.md`](APPLE_FFI_RELEASES.md) — packaging is
release engineering, not manifest schema.

### 11.12 What's still out of scope after §11

Things explicitly NOT defined here, to be picked up in P2 or as separate decisions:

- mDNS observation shape.
- USB connections and establishment plans: now defined (§11.29).
- WiFi-join primitive's success/failure semantics (platform-dependent).
- Live re-loading of the manifest set after FFI init (not needed for MVP).
- Authentication / signing of the manifest bundle (not needed for MVP).
- Telemetry / trace export for failed walks (the dispatcher logs; how those logs egress is the app's call).

### 11.13 Transform vocabulary (multivendor pass, 2026-06)

The old two-entry `ValueTransform` (`bitOr`/`bitAnd`) is absorbed into a closed,
chainable `Transform` vocabulary. Every `transform:` site accepts a single
single-entry mapping (a 1-element chain) or a list applied in order:

```yaml
transform: { bitOr: 0x20000000 }          # 1-element chain (unchanged authoring)
transform:
  - slice: { at: 3, length: 1 }
  - bits: { mask: 0x0C, shift: 2 }
```

**Allowlist (closed — no arbitrary expressions):**

| primitive | operand | semantics |
|---|---|---|
| `bitOr` / `bitAnd` | u64 | input ≤ 8 bytes read LE, op applied, re-emitted at input width LE |
| `slice` | `{ at, length? }` | window `[at, at+length)`; `length` omitted = to end; out-of-range **fails** (never clamps) |
| `dropPrefix` | usize | sugar for `slice: { at: n }` |
| `reverseBytes` | `{}` | reverse byte order |
| `appendNul` | `{}` | append exactly one zero byte |
| `padRight` | `{ length, byte }` | append `byte` until input is exactly `length`; fail when input is longer or `length > 65535` |
| `uuidFromBytes` | `{}` | exactly 16 bytes → the 36 ASCII bytes of the canonical uppercase 8-4-4-4-12 UUID string (bind with `encoding: ascii`) |
| `bits` | `{ mask, shift? }` | input ≤ 8 bytes read LE: `(value & mask) >> shift`, re-emitted at input width LE |

Transforms are **bytes → bytes** and total: a chain either produces bytes or
fails the step/capture (§11.6). Integer *decode* deliberately lives in the
`encoding` allowlist (§11.2) applied after the chain, not in the transform
vocabulary — this deviates from the risk-pass doc's literal list ("endian
integer decode as transform") to keep the vocabulary closed under
composition.

**Pipelines:**
- capture (read/notify): `wire bytes → transform chain → encoding decode → scope string`
- write value: `resolve (captured/runtime/template) → encoding decode → transform chain → wire bytes`

Statically-invalid operands (`slice.length: 0`, `bits.mask: 0`,
`bits.shift ≥ 64`, `padRight.length: 0`, `padRight.length > 65535`, operands on `reverseBytes`/`appendNul`/`uuidFromBytes`) are load
errors. Reference evaluation: `camera_config::index::eval::apply_transforms`
— its unit tests are the executable spec a dispatcher must match.

`StepValue::Literal` takes no transform: anything a chain could do to a
load-time literal belongs in the authored bytes.

### 11.14 BLE advert predicate model (multivendor pass, 2026-06)

Added by the multivendor schema risk pass — Nikon recognizes by service UUID
+ local name (no manufacturer data), Canon by service UUID + manufacturer
data, Sony by manufacturer-data bitfields. The old
`require: { manufacturerCompanyId, advertContainsService }` +
`manufacturerData:` pair is replaced by a predicate over the observation
plus a `capture:` list.

**Signature shape:**

```yaml
signatures:
  <name>:
    kind: bleAdvert
    require: <predicate>          # single predicate or all/any/not combinator
    capture: [ <capture>, ... ]   # optional field captures (§11.13 pipeline)
    scope: { <literal facts> }    # optional
    suggests: { connection, confidence }
```

**Predicate grammar (closed):** a single-entry mapping, one of

| key | body | semantics |
|---|---|---|
| `all` / `any` | list of predicates (non-empty) | conjunction / disjunction |
| `not` | one predicate | negation — see absent-field caveat below |
| `manufacturerData` | `{ companyId?, length?/minLength?, assertByte, assertBits }` | over the manufacturer AD record; payload constraints apply to the **post-company-id payload**. `companyId` optional, but data SHOULD pin it whenever the vendor advertises one (false-positive window otherwise — #23) |
| `serviceUuids` | `{ contains: <uuid> }` | advert's service-UUID list contains the UUID (case-insensitive) |
| `serviceData` | `{ uuid, length?/minLength?, assertByte, assertBits }` | over the service-data payload advertised for `uuid` |
| `localName` | exactly one of `{ equals \| prefix \| contains }` | plain string ops — regex is deliberately NOT in the engine vocabulary |
| `txPower` | `{ min?, max? }` (at least one) | advertised TX power within bounds |
| `rawAdRecord` | `{ adType, length?/minLength?, assertByte, assertBits }` | over a raw AD record **as seen on air** — for AD type 0xFF that INCLUDES the 2-byte LE company id |

`assertByte: { index, equals }`; `assertBits: { offset?, mask, equals }`
reads the minimum LE width covering `mask` at byte `offset` and checks
`(value & mask) == equals` (payload too short → false).

**Absent-field rule:** a predicate over a field the advert did not carry (no
manufacturer data, no local name, no TX power, empty AD-record list)
evaluates **false**, never an error. Consequently `not:` over an absent
field evaluates **true** — authors beware. iOS CoreBluetooth never supplies
raw AD records, so `rawAdRecord` predicates never match observations from
that platform; prefer `manufacturerData`/`serviceData`/`serviceUuids` forms
unless the data is Android-only.

**Captures:** `{ source, at?, length?, transform?, encoding, name }` —
source bytes → window `[at, at+length)` (length omitted = to end) →
transform chain (§11.13) → `encoding` decode → runtime_scope under `name`.
Sources: `manufacturerData` (post-company-id payload), `localName` (UTF-8
bytes), `{ rawAdRecord: <adType> }` (as-on-air), `{ serviceData: "<uuid>" }`.
A capture that fails anywhere is skipped — matching is decided by `require`
alone.

**Evaluation** lives in `camera_config::index::eval`
(`BleAdvertFacts`, `advert_matches`, `advert_scope`) — signatures are never
exposed over uniffi; the FFI converts `ScanObservation::BleAdvert` into
`BleAdvertFacts` and delegates. §11.7 precedence and Disambiguate semantics
are unchanged; the Disambiguate-level runtime_scope still intersects

after the user picks).

**Family constants** renamed in the same pass: `ble.advert.fujiCompanyId` →
`ble.advert.manufacturerCompanyId` (now optional),
`ble.advert.legacyServiceUuid` → `ble.advert.serviceUuids.fileTransfer`
(named map). Hard migration — no aliases; pre-production per AGENTS.md.

### 11.15 `bleAwaitUntil` — await / poll-until control flow (2026-06)

The control-flow primitive four camera flows converge on (ptpsim #32-V2 Sony
Wi-Fi handoff, #29 postview-await, #42 AF poll, #46 transfer loop): observe a
characteristic repeatedly until a condition holds, optionally acting each
unsatisfied iteration. Lives in the BLE establishment grammar first (it has
the scope/capture/`Predicate`/timeout machinery and the `ble.rs` reference
walker); **the PTP-IP action/mode-entry grammar mirrors this contract** —
now landed in §11.16 (#29/#42 land there), with its own scope (a `PropView`),
a reference executor (`camera_sim::ptpip::walk_ptpip`), and a PTP-native
condition vocabulary.

```yaml
- bleAwaitUntil:
    source: { notify: { gatt: launchState, mode: notify } }  # or: { read: <gatt> }
    capture: [ { at: 3, length: 1, encoding: u8, name: wifiStatus } ]  # §11.13 pipeline, per iteration
    captureAs: wholePayload         # optional whole value → scope (hex), per iteration
    until: { wifiStatus: { eq: 1 } } # a Predicate over scope — the `if:` vocabulary
    failWhen: { wifiStatus: { eq: 0 } } # optional terminal rejection
    failureEvidence:                  # optional compound rejection proof
      steps:
        - bleRead: { gatt: failureDetail, encoding: u16-le, captureAs: failureDetail, tolerant: true }
      when: { failureDetail: { ne: 0 } }
    onEach:                          # steps run each iteration `until` is NOT yet met
      - bleWrite: { gatt: launchRequest, value: { literal: "01" } }
    timeoutMs: 5000
    options: { tolerant: false, retries: 0 }
```

**Semantics.** Each iteration: (1) observe one value — a fresh `read` of the
characteristic, or the next `notify` payload (CCCD enabled once up front with
`mode`); (2) apply `capture`/`captureAs` into runtime_scope (§11.13 pipeline,
fail-soft — a window/transform/decode miss is skipped); (3) evaluate `until`
(a [`Predicate`] over scope, §3.3 vocabulary) — if it holds, the step
succeeds; (4) otherwise run `onEach` and observe again. The condition is
uniform across `read` and `notify` sources because captures bridge the
observed bytes into scope first.

After captures are applied, `until` is evaluated first. If it is false and the
optional `failWhen` predicate is true, the observation is eligible to fail with
`ConditionRejected`; `onEach` does not run for a confirmed failure. If both
predicates match the same observation, `until` wins. An unbound `failWhen`
field does not match. Without `failureEvidence`, a matching `failWhen` is
immediately terminal.

When `failureEvidence` is present, a matching `failWhen` runs its non-empty
`steps` inside the same await budget. The executor clears the `when` field
before those steps, preventing a tolerated probe failure from reusing stale
evidence. The failure becomes `ConditionRejected` only if `when` matches after
the probe; otherwise observation continues. This models compound rejection
evidence without teaching the engine any manufacturer-specific state values.
For a notify source, those evidence steps cannot read the source characteristic,
including through nested control flow. Callback transports cannot distinguish
that read response from a racing notification, so compound evidence must come
from a separate characteristic.

A notify source cannot combine `seedRead: true` with `failWhen`. Callback APIs
such as CoreBluetooth deliver read responses and notifications through the
same callback, so a notification racing the read response cannot carry reliable
provenance. When rejection must be causal to a command, read the baseline
before the command, then subscribe, issue the command, and use a
notification-only await with `failWhen`. The loader rejects the unsafe
combination.

When a notification-only await must accept only observations caused by a
particular command attempt, that command's `bleWrite` declares
`notificationFence: <subscribed-gatt>`. The transport atomically discards the
named stream's already-buffered prefix immediately before issuing the write.
This operation is one transport call, not a queue drain followed by a write;
the latter has a race window. A retry repeats the fenced write, so notifications
left over from the prior attempt cannot satisfy or reject the current attempt.

`notify.seedRead` defaults to `false`. When `true`, the dispatcher enables the
CCCD and waits for its acknowledgement, arms the notification accept path,
then issues exactly one fresh read of the same characteristic. The read
response and subsequent notifications use the same captures and `until`
predicate; the first satisfying payload wins. An unsatisfying seed is one
normal observation (`onEach` runs), after which the source is notification-only
and issues no further reads. Subscription/read errors use the normal
step-level retry/tolerant model. This ordering closes the already-in-state gap
without a read-poll loop and prevents a notification arriving between subscribe
and seed-read from being dropped. On callback APIs such as CoreBluetooth, the
read response and notifications must feed one acceptance path rather than
competing per-characteristic waiters.

**Timeout.** `timeoutMs` is the dispatcher's wall-clock budget; the step fails
(tolerant-aware per §11.6) if `until` isn't met before it elapses. `intervalMs`
is the read-poll cadence (the dispatcher sleeps between reads); it's ignored
for `notify`, including seeded notify (cadence is the camera's). The **reference walker** models the
timeout deterministically as **source-exhaustion** (notify queue drained / read
sequence consumed without satisfaction) plus a 256-iteration cap — the analogue
of "retries are first-try-or-never" for the deterministic oracle.

**`onEach`** is a `Vec<Step>` (may be empty for a pure poll). It runs only when
`until` is not yet satisfied, *after* an unsatisfied observation and *before*
the next — so a launch-request write lands once per not-yet-launched status,
never after success. A pre-loop action goes in a `bleWrite` before the
`bleAwaitUntil`, not in `onEach`.

**Not this verb:** bounded-loop / for-each over a known collection (ptpsim #46,
iterate object handles + chunk each) is a *distinct* construct — iteration over
a set, not a condition-wait — and is a separate future addition. `bleAwaitUntil`
waits on an evolving condition; it does not enumerate.

Reference semantics: `camera_sim::ble::run_await_until` — its tests are the
executable spec a platform dispatcher must match.

### 11.16 PTP-IP `awaitUntil` — poll-until for the action/mode-entry grammar (2026-06)

The PTP-IP step grammar (`camera_config::model::Step`) gains the await/poll-until
verb, mirroring §11.15's **contract** — not its types. The two grammars share the
control-flow semantics (observe until a condition holds, act each unsatisfied
iteration, deterministic timeout); they do **not** share a scope type or a step
enum (decision: two grammars, one contract). PTP-IP is PTP-native where BLE is
GATT/byte-window-native:

| Aspect | BLE `bleAwaitUntil` (§11.15) | PTP-IP `awaitUntil` (§11.16) |
| --- | --- | --- |
| Source | `read` **or** `notify` over byte windows | `poll` a property (`GetDevicePropValue`, loop) **or** `event` push (single-shot, #54) |
| Capture | `capture`/`captureAs` + transform/encoding pipeline | each property read populates the `PropView`; enclosing `captures` may bind the final successful poll without another read |
| Scope | `BTreeMap<String,String>` runtime_scope + encodings | `camera_config::predicate::PropView` (`BTreeMap<u16,i64>`) |
| `until` | `index::Predicate` over string scope | `camera_config::Predicate` over `PropView` (mask/eq/ne/lt/gt + all/any/not) — `mask` handles `0xd212` composite sub-fields |
| Reference | `camera_sim::ble::run_await_until` | `camera_sim::ptpip::walk_ptpip` |

```yaml
# An action/entry step (e.g. #42 tap-to-AF then wait for the box to lock):
- sendOp: "0x9026"            # LockS1Lock (tap-to-AF), packed AF-area param
  params: [0x09060403]
- awaitUntil:
    source: { poll: "0xd209" } # poll S1_LOCK_COLOR (GetDevicePropValue)
    until: { prop: "0xd209", eq: 1 }   # the PTP Predicate over observed values
    onEach: []                # steps run each unsatisfied poll (often empty)
    timeoutMs: 5000
    intervalMs: 250           # poll cadence; 0 = dispatcher default
```

**Semantics (poll source).** Each iteration: (1) `GetDevicePropValue(prop)` → the
decoded value lands in the observed `PropView` keyed by its prop code;
(2) evaluate `until` (a `Predicate` over that view); if it holds, the step
succeeds and applies the enclosing step's captures to that final property
reply; (3) otherwise run `onEach` and poll again. `propValue` is the only capture
source valid on `awaitUntil`. It is valid for `poll` and for `event` with
`thenPoll`, invalid for an event without `thenPoll`, and consumes the reply
already obtained for the terminal predicate evaluation rather than issuing an
extra read. The condition is uniform because every poll bridges the wire value
into the typed view first. `awaitUntil` is a condition-wait, distinct from
`repeat` (a bounded loop) and from a future for-each over a collection (#46 —
iteration, not a wait).

**Timeout.** `timeoutMs` is the dispatcher's wall-clock budget; the step fails
(tolerant-aware) if `until` isn't met before it elapses. The reference executor
models this deterministically as a 256-iteration cap (`MAX_AWAIT_ITERS`) — the
analogue of §11.15's source-exhaustion model.

**Camera-side settle (sim).** An action's responder role may install a generic
`PropertyTransition`: a target property, an optional initial scalar, a terminal
scalar supplied either as a fixed value or by a named responder parameter, and
`settleAfterPolls`. The terminal value becomes visible after that many polls of
the target property; zero means immediate. The transition belongs to the
resolved responder invocation, not to an unconditional shared operation effect.
Simulator tests of an action's initiator/responder contract seed the evolving
property through the responder role and never add a shared-operation mutation
merely to make the initiator walk terminate. This is the PTP analogue of the BLE
walker's `serve_read_sequence`.

**FFI.** `awaitUntil` mirrors to `EntryStep::AwaitUntil { source, until,
on_each, captures, timeout_ms, interval_ms, tolerant }`, where `source` is a
`FfiAwaitSource` enum (`Poll { prop }` | `Event { code, then_poll }`) mirroring
`cc::AwaitSource`. `until` is a full recursive `FfiPredicate` (a partial mirror
would silently drop `all`/`any`/`not`); the dispatcher owns the loop and
evaluates via the `await_until_satisfied(until, observed)` helper so Swift never
re-implements the predicate logic.

Reference semantics: `camera_sim::ptpip::walk_ptpip` — its tests
(`af_lock_round_trips_via_op_effect_and_poll_until`,
`af_capture_round_trips_via_event_source_then_single_read`) are the executable
spec a platform dispatcher must match.

#### 11.16.1 Source: poll vs. event — the hybrid push-then-read (#54)

The camera's Wi-Fi-AP `app` (reference app) path has a **wire-proven event push** on the
PTP/IP event socket (55741): capture lifecycle (`0xC004`→`0xC001`→`0x400D`) and
`0xC005` AFCAPTUER arrive unsolicited on fw 02.30. **The critical split:** events
are **completion/lifecycle** signals, NOT value-change signals — the camera emits
**no** `DevicePropChanged (0x4006)` for the `0xD2xx` range on fw 02.30. So the AF
*result* (`0xD209` green/red) is poll-only even though `0xC005` says AF
*finished*. The optimal pattern is therefore a **hybrid**: await the completion
event (push), then do a **single** value read for the result — not a blind poll
loop.

```yaml
- sendOp: "0x9026"                       # tap-to-AF
  params: [0x09060403]
- awaitUntil:
    source: { event: { code: "0xc005", thenPoll: "0xd209" } }
    until: { prop: "0xd209", eq: 1 }     # over the ONE post-event read
    timeoutMs: 5000
```

**Semantics (event source, re-poll — #185).** (1) await an event packet with
`code` on the event socket (single-shot: the push either arrives within the
budget or never will); (2) if `thenPoll` is set, `GetDevicePropValue(thenPoll)`
into the `PropView` and evaluate `until` — **re-polling at `intervalMs` cadence
until it holds or `timeoutMs` elapses**; (3) when it holds, apply any enclosing
`propValue` captures to that final `thenPoll` reply without another read. The
event acknowledges the operation;
it does NOT guarantee the polled value has settled: fw02.30 fires `0xC005`
~100ms after `0x9026` while `0xD209` still reads pre-settle (client application#157 wire
capture — a fast consumer's single instantaneous read deterministically lost
the race). `thenPoll: null` = event arrival alone satisfies `until` over the
existing scope (a prior `getProp`) — nothing to re-read, one evaluation. The
reference executor's `await_iterations` records the observation count on
satisfaction (`1` = settled on the first read, `≥2` = re-polls absorbed settle
latency) and `0` when a *tolerant* step bails on a missing event. A platform
dispatcher must match this: await `code` on the connection's event socket
(55740+1), then pace `thenPoll` reads by `intervalMs` (0 = dispatcher default)
within `timeoutMs`.

**Event delivery fallback (§11.29).** The single-shot event wait above
assumes a `reliable` event channel: the push either arrives within the
budget or never will. On a `bestEffort` connection any single event may be
lost, so an event wait that exhausts its budget is not proof the event
never fired. Such a step falls back to its `thenPoll` loop instead of
failing: polling starts at the `intervalMs` cadence and continues until
`until` holds or `timeoutMs` elapses. Every event-source `awaitUntil` on a
`bestEffort` connection MUST declare `thenPoll`; the loader rejects one
without it. A connection with `events.delivery: none` forbids event-source
awaits outright. `reliable` keeps the current semantics.

**Emission and settle (authoring note).** An op declares the events it pushes
via `Operation.emits: [<code>]` (parallel to `effects`; the engine queues them
on an OK response, and the `app` event socket forwards them — see below). The
former "event-coupled effects MUST settle ≤1" invariant is **retired** (#185):
it assumed the single post-event read, and the client application#157 wire capture showed
real fw settles the value *after* the event. Author `settleAfterPolls` to the
measured latency (gfx100ii `0x9026` → `0xD209` uses `settle: 2`) — the
re-polling event source absorbs it, and simulating the latency is what keeps
consumers honest about pacing. Composite container reads (`0xD212`) tick member
settles exactly like direct member reads, so container-observing consumers see
the same behavior.

**Live event socket.** `camera-sim-service` pushes these for real: the command
loop drains the engine's queued `emits` after each operation and broadcasts the
codes; each connected event-socket client writes them with the connection's
declared `eventFraming` (`standard` PTP/IP or USB/PIMA) and a zero transaction
ID. Fire-and-forget to currently-connected clients — the real app opens the
event socket during session setup, before triggering a capture. Reference test:
`service_pushes_completion_event_on_event_socket`.

**Per-connection applicability.** The `app` (BLE→Wi-Fi-AP reference app, 55740/55741)
connection is the hybrid push-then-read **happy path** — its event channel is
wire-proven. `wireless-tether` (PCSS, 15740) has **no event channel** (wire-audit:
zero event packets across active shooting) → poll-only. USB connections declare
their event behavior per connection with the §11.29 `events.delivery` trait:
`none` is poll-only, `bestEffort` takes the fallback above, and `reliable`
keeps the happy path. The source is per-step manifest data, so poll-only
connections simply keep authoring `{ poll: }` and the engine needs no branch.

**Out of scope (deferred follow-ups).** The real gfx100ii data authoring —
`0x9026 emits: ["0xc005"]` and switching the `app` AF/capture entry steps to
`{ event: }` — is curated protocol-mapper/data-owner work; this change ships only
a synthetic round-trip fixture. Event **param payloads** (the AF event's
`(counter, …)`) are a later `emits` refinement; the consumer keys on the event
*code*. Confirm `0xC005`/apState values against a rig capture (#52).

### 11.17 Nikon LSS named primitives

Nikon SnapBridge LSS authentication and encrypted connection configuration are
finite engine primitives, never open manifest script. The implementation-independent
wire, compatibility-constant, cipher, failure, provenance, and secret-handling
contract is [`docs/NIKON_LSS.md`](NIKON_LSS.md). The protocol implementation
lives in `protocol-primitives::nikon_lss`; schema, native FFI, UniFFI, executor,
and reference-walker variants are appended without renumbering existing enums.

**Authentication step.**

```yaml
- nikonLssAuthenticate:
    gatt: "{ble.gatt.lssAuthentication}"
    clientDeviceId: { runtime: nikonClientDeviceId, encoding: bytes-raw }
    nonce: { runtime: nikonLssNonce, encoding: bytes-raw }
    timeoutMs: 10000
```

Both inputs must resolve to exactly eight bytes. `clientDeviceId` is persistent;
`nonce` is fresh runtime entropy and therefore cannot be manifest-baked. The
loader requires `retries: 0` because the generic retry wrapper would reuse that
resolved nonce. At the start of an attempt the executor clears any prior LSS
session, enables indications, and uses a notification-fenced write for both
stage 1 and stage 3 so stale buffered indications cannot satisfy either await.
It retains the resulting opaque `NikonLssSession` only in executor state. Nothing
is bound into manifest scope by this step. Missing entropy, timeouts, malformed
lengths/stages, and proof mismatches fail closed.

**Connection-configuration step.**

```yaml
- nikonLssReadConnectionConfiguration:
    gatt: "{ble.gatt.lssConnectionConfiguration}"
    flagsCaptureAs: nikonConnectionFlags
    ssidCaptureAs: wifiSsid
    passwordCaptureAs: wifiPassword
    securityModeCaptureAs: wifiSecurity
    sppMaxLengthCaptureAs: sppMaxLength # optional
```

This step requires the executor-retained authenticated session, performs one
GATT read, and decodes the layout in `docs/NIKON_LSS.md`. It binds only named
configuration fields. Raw keys, derived seeds, expanded schedules, and cipher
contexts are not schema fields, scope values, logs, traces, or FFI values.

### 11.18 Operation/property classification, value rows, profiles, and masks

Operation and property catalog classification separates inventory knowledge
from executable or user-facing behavior.

`Operation.kind` is a closed classification with two values:

- `executable` (the default when `kind` is omitted): an authored operation whose
  existing connection, mode, and predicate gates may resolve to available.
- `advertisedOnly`: a code positively observed in a camera inventory, without a
  claim that invoking it is safe. It remains enumerable, but availability
  resolution MUST return unavailable and consumers MUST NOT execute it solely
  from this row.

Generator-created inventory operations use `advertisedOnly`. Existing authored
operations omit the field and remain executable for compatibility.

`Property.kind` is a closed classification with three values:

- `setting` (the default when `kind` is omitted): an ordinary property that a
  consumer may consider for user-facing settings, subject to its other access
  and capability metadata.
- `scaffold`: protocol machinery such as keepalives or virtual-shutter state
  that may look writable on the wire but MUST NOT appear as a user-facing
  setting.
- `catalogOnly`: a positively observed property inventory row without a
  user-facing setting or implicit write claim. It remains enumerable.

Generator-created unresolved/raw properties use `catalogOnly`; existing
authored `setting` and `scaffold` behavior is unchanged. The default
`executable` and `setting` values are omitted when serializing manifests to keep
the authored form compact. Unknown classification strings are schema errors.
FFI consumers receive the resolved classifications as `OperationKind` and
`PropertyKind`, never as optional or open-ended strings.

`Operation.handler` is a closed set selecting the simulator's dispatch behavior
for a cataloged operation:

- `property.step`: advance the `property` enum one slot; the request's first
  parameter is the direction. The operation MUST declare `property`.
- `object.size`: answer an extended object-size query from the `objectSize`
  block. The operation MUST declare `objectSize`.

An unknown handler string is a schema error at load, not a silent dispatch
fallback. An `objectSize` block without the `object.size` handler is also a
schema error. A cataloged operation with no handler is an executable no-op:
the camera acknowledges it without a simulated camera-side effect.

`Property.access` is a closed set with two values, `readOnly` and
`readWrite`. The simulator answers `SetDevicePropValue` with `AccessDenied`
unless the property declares `readWrite`, and the served `DevicePropDesc`
GetSet flag matches the same claim. A property with no `access` makes no
write claim and is served get-only. Observation intake normalizes an access
value the closed model cannot represent (a legacy reduction carrying a
nonstandard descriptor GetSet byte, e.g. `gs2`) to no claim; the manifest
itself never loads an unknown value.

`Property.computed` is a closed set naming a simulator-computed value source,
served instead of stored property state:

- `objectCount`: the count of currently enumerable objects, served as `u32`.
- `objectHandles`: the enumerable object handles, served as a count-prefixed
  `u32` array (the `GetObjectHandles` encoding).

The manifest names the quantity; the engine holds no per-code special cases.

`Mode.phase` is a closed set (`sessionOpen`, `imageImport`, `liveView`,
`streaming`) mapping a detected mode to a simulator workflow phase. When a
mode's `detect` predicate selects it on a property write, the engine enters
the declared phase. The phase applies on mode transitions; writes that keep
the same mode leave in-session phase state (such as streaming) untouched. An
unknown phase value is a manifest load error. Leaving a mode whose declared
phase is `imageImport` resets the simulator's bootstrap gate progress,
including transitions into modes that declare no phase. Transport states
(`disconnected`, `queuedReceive`, `closed`) are not declarable workflow
phases.

The simulator enforces catalog availability before it dispatches an
operation: connection, mode, `kind`, and `requires` all resolve first, and a
refused operation answers `OperationNotSupported` (a failed `requires`
prerequisite answers `GeneralError`). Three bootstrap operations are exempt:
`OpenSession`, `CloseSession`, and `GetDeviceInfo`. The mode axis engages
only once a mode is detected; a session with no detected mode gates on
connection, kind, and `requires` alone, because some transports never flip a
mode selector (PCSS enters transfer implicitly). The three property-access
operations (`GetDevicePropDesc`, `GetDevicePropValue`,
`SetDevicePropValue`) skip the mode axis: the property surface is modeled
per property (`access`, `requiresGate`), and catalog mode rows for those
standard operations are transport observations, not camera refusals.
`OpenSession` rejects the session id 0 with `InvalidParameter` (PTP forbids
only zero; non-1 ids are accepted absent wire evidence of refusal) and
answers `SessionAlreadyOpen` on a second open instead of resetting the
session. A camera binds its session to the transport: when the owning command
connection ends without `CloseSession`, the session state (open flag, phase,
gates) is cleared so a reconnecting client can open again.

`initialValue` is the property's seed value before any camera read or
write. A numeric-typed property takes an integer; a `type: str` property takes
a quoted YAML string (`initialValue: "4000x2664"`), and an integer there is a
schema error, as is a string on a numeric type.

Property value metadata has three distinct layers. Consumers must keep them
separate:

- `valueRows`: presentation/codec rows for known raw values. Exact rows win for
  label lookup and preserve non-literal encoded forms for round-trip.
- `valueEncoding.sentinel` and `valueEncoding.masks`: generic bit-mask forms for
  values that share a flag/sentinel plus a base value. `sentinel` is the legacy
  single-mask slot; `masks` adds zero or more peer mask descriptors. The schema
  is shape-based (`mask`, optional `equals`, optional `meaning`, `labelPrefix`)
  and carries no manufacturer code.
- `valueEncoding.decoder`: an optional numeric fallback after exact labels.
  `integer` formats a bounded integer directly. `shutterSpeed` formats a flagged
  scaled denominator as `1/N` and an unflagged scaled value as seconds followed
  by `"`. Exact `valueRows` and `labels` remain authoritative.
- `structuredText`: an optional grammar for a PTP `STR` property with a
  delimiter and ordered named fields. The first scalar kind is
  `signedInteger`; the FFI encoder validates field count and shape but does not
  infer numeric bounds that the evidence does not provide.
- `valueProfiles`: connection/mode-scoped legal-value metadata. Profiles may
  come from `DevicePropDesc`, a body-specific capture/write walk, or another
  reviewed evidence source. When a matching profile exists, clients use its
  rows to filter write/UI choices for that property/connection/mode. A row's
  `raw` is the canonical write value; `aliases` are alternate readback/input
  forms that map to that canonical row; `legal: false` marks observed/candidate
  values that must not be offered or sent. Absence of a profile for one property
  never permits reusing another property's profile.

Profile matching is exact on `connection`; `mode` matches the declared path and
child paths (`shooting/stills` covers `shooting/stills/manual`). If multiple
profiles match, the most specific connection plus longest-mode profile wins.

Reviewed semantic metadata enters through `camera-observation/v1`, never by
editing a consolidated manifest. Operation and property capability subjects may
carry `canonicalName`; property subjects may additionally carry
`sourceNativeName` and typed `valueRows`. Every name and row contains its own
`evidenceReference` plus epistemic class, confidence, alternatives, and
falsifier. Integer rows use their declared signed/unsigned width; 64- and
128-bit values use canonical decimal text so JSON and foreign-language bridges
cannot round them. `str` rows remain strings. Intake rejects noncanonical or
out-of-range decimal text and rejects any row whose type differs from the
property's declared type.

Proposal review treats each name, each global value row, and each scoped value
profile as an independent digest-bound candidate. Identical assertions merge
their provenance and observed execution contexts in deterministic order, so an
accepted profile still registers the modes in which it was observed. Apply may
replace a generated `raw_0x....` canonical name or the generated standard PTP
name for that code; a different curated name is a hard conflict.
Assertion provenance is retained under the manifest's `semanticAssertions`
ledger and exposed through the catalog FFI. The ledger is query metadata only:
the simulator does not consult it for availability, state, gates, responses,
current/default values, descriptor form, or write behavior.

Evidence roles remain orthogonal. Captures determine wire codes, types, access,
and observed values. Human-authored vendor material may support semantic names
and labels. Firmware strings and secondary implementations remain candidate
evidence at their reviewed confidence; a name or match alone never creates
runtime behavior.

### 11.19 PTP-IP sequence gates

Some responders require an ordered wire ritual before a later property or
operation is answered. This is not a property predicate: it is history over
successful requests in the current session. The schema models that with
simulator-side sequence gates:

```yaml
sequenceGates:
  imageImportBootstrap:
    evidence: [docImageTransfer]

steps:
  - { getProp: "0xd212", tolerant: true, startsGate: imageImportBootstrap }
  - { setProp: "0xdf01", value: 0x14 }
  - { sendOp: "0x9050", tolerant: true }
  - { getProp: "0xd22b", tolerant: true }
  - { sendOp: "0x9053", params: [0, 0x7530], tolerant: true }
  - { getProp: "0xd212", tolerant: true, completesGate: imageImportBootstrap }

properties:
  "0xd620":
    name: imageImportObjectCount
    requiresGate: { name: imageImportBootstrap, failure: noResponse }
```

**Semantics.** A `startsGate` marker begins matching at that successful step.
The gate is satisfied only when each matchable step through the corresponding
`completesGate` succeeds in order. `tolerant: true` only lets a dispatcher
continue after a non-OK PTP response; a tolerated non-OK response does not
advance or satisfy the gate. `CloseSession`, a new `OpenSession`, and
engine-observed mode-exit transitions reset gate state. `failure: noResponse`
means the simulator writes no response and keeps the command socket open, so
clients observe a read timeout rather than a PTP error or a dropped connection.

**Scope.** Gate declarations are body-level today. Do not declare `mode` or
`connections` until the responder consumes that context; `Engine::on_operation`
currently receives only a PTP request, not the active connection id.

**FFI.** Sequence gates are not exported to app consumers by default. Consumers
already receive the executable `EntryStep` sequence, including the extra
`getProp` / `sendOp` steps that satisfy the gate. The gate metadata exists so
the simulator can be a negative oracle for skipped setup. If a future consumer
needs to reason about gates directly, mirror `sequenceGates` / `requiresGate`
through UniFFI with seam tests; do not partially mirror only one side.

### 11.19a Mode-entry execution and outer re-establishment

A mode entry carries exactly one execution variant:

- `ptp` is an ordered list from the PTP `Step` grammar and runs on the current
  session.
- `userInstruction` is a manual camera/host action and contains no wire steps.
- `reestablishConnection` exits the current PTP session, replays the existing
  connection establishment plan with fixed runtime parameters, opens a fresh
  PTP session, and then runs the target mode's cold (`from: null`) entry.

`reestablishConnection` contains `exitSteps` plus `params`. It implicitly targets
the connection containing the entry; the FFI includes that connection id in the
returned variant. The consumer executes the exit steps, closes remaining socket
roles, releases the OS network association, walks the establishment plan's
`postExitReadiness` and `steps`, associates to the returned network, opens a new
PTP session with reset transaction state, and queries the cold entry for `to`.
The cold entry MUST be `ptp`, never another re-establishment, which prevents
recursive plans and keeps the cold vendor bootstrap authored once.

These are load-time invariants, not lints. The containing connection MUST name a
resolvable establishment mechanism, its fixed `params` keys MUST exactly match
that plan's declared runtime parameters when the manufacturer index is present,
and every PTP/exit step MUST have a complete hand-written FFI mirror. Loading
fails rather than returning a partial plan that could destroy the working
session before discovering the missing continuation.

This is composition across existing grammars, not a new BLE or OS verb in the
PTP `Step` vocabulary. `closeSession.transportClose: true` means consume the PTP
CloseSession response, close auxiliary roles normally, send and flush the
connection's declared transport-close frame on the command socket, then perform
a clean shutdown/close. It does not promise that the endpoint remains
immediately redialable.

### 11.20 Camera-initiated pull transfers

Some cameras signal queued media over BLE, bring up a network connection, then
wait for the app to pull a private queue. This is not unsolicited object data and
is not the public card enumeration used by ordinary image import or tethering.

This declaration is not a BLE peripheral implementation. It tells consumers
which GATT state transition signals the transfer and gives the simulator a
separate PTP pull queue. The current simulator service seeds that queue at
startup; it does not advertise a Bluetooth peripheral. A consumer or test
fixture injects the resolved trigger state through the generic state overlay's
`camera_initiated_transfer_active` field. Reserved routing remains disabled
until that field is true. An optional BLE transport adapter remains tracked by
issue #164.

```yaml
cameraInitiatedTransfer:
  trigger:
    match: all
    states:
      - gatt: apState
        triggerValues: ["0380"]
        baselineValues: ["0080"]
      - gatt: transferState
        triggerValues: ["0180"]
        baselineValues: ["0080"]
  handoff:
    connection: app
    socketRole: command
    cachedCredentialsAllowed: true
    functionLaunch: { gatt: functionLaunchRequest, value: "0300", required: false }
  monitorRecovery: savedCameraReconnect
  receive:
    mode: reserved-photo-receive
    # Pre-mode wire values: 8 queued in the comparison capture, 1 in the
    # successful one-object capture.
    count: { property: "0xd212", member: "0xdf41" }
    headIndex: 1
    metadata: { operation: "0x1008", phases: [afterCountBeforeModeEntry, afterModeEntry] }
    data: { operation: "0x101b", chunkLimitProperty: "0xd235" }
    completion: readToEof
  evidence: [wireAutoImageTransfer, wireAutoImageTransferQueued, staticAutoImageTransfer]
```

`match: all` evaluates the latest value of every declared BLE state. Trigger
and baseline values are exact wire-order bytes. Symbolic GATT names resolve
through the model's merged manufacturer-index catalog at load time, exactly as
BLE steps do; an undefined name fails the index load.

After the consumer observes the declared trigger match, it sets
`camera_initiated_transfer_active` true, opens the declared connection/socket
role, and reads the count source. It performs metadata immediately after that
count read when `phases` contains `afterCountBeforeModeEntry`, walks
`mode_entry`, and performs metadata afterward when phases contains
`afterModeEntry`. It then reads the chunk limit and pulls the fixed head index
through the declared data operation. GFX100 II
declares both phases: reference app probes `GetObjectInfo(1)`, enters reserved mode and
negotiates the function version, then probes `GetObjectInfo(1)` again. An
intervening operation cancels the pre-mode reserved probe so ordinary public
object lookup cannot alias the fixed reserved index.

The camera owns one transfer queue. `readToEof` completes a head only after
successful delivered byte ranges cover the entire object and the final PTP OK
response is written. Partial reads, failed writes, disconnects, session close,
and transport sentinels do not dequeue. Queue advancement beyond one object is
not claimed for GFX100 II. The checked wire evidence covers a one-object drain
as of 2026-07-07.

The FFI mirrors the complete declaration through
`camera_initiated_transfer(model)`. It returns resolved UUIDs and bytes, the
resolved endpoint, monitor-recovery route, receive fields, and evidence. A
`savedCameraReconnect` monitor recovery means run the existing fresh-advert
`reconnectDecision` loop under `reconnectPolicy`; a disconnected monitor must
not redial a cached peripheral. The query requires a
manufacturer-index store; a single-body store has no shared GATT catalog and
therefore returns no resolved camera-initiated transfers.

### 11.20a Heterogeneous record-stream payloads

A `recordStream` payload declares its count width, record-code width, default
fixed numeric width, and ordered member allowlist. A scalar member entry is
shorthand for the default fixed unsigned encoding. A detailed entry overrides
that encoding for its containing payload only:

```yaml
payload:
  form: recordStream
  countWidth: 2
  record: { codeWidth: 2, valueWidth: 4 }
  members:
    - "0xdf41"
    - code: "0x5010"
      encoding: { kind: signed, width: 4 }
    - code: "0xd22f"
      encoding: { kind: ptpString }
      simulatorValue: ""
```

Detailed encodings are the closed forms `{ kind: fixed, width: 1|2|4 }`,
`{ kind: signed, width: 1|2|4 }`, and `{ kind: ptpString }`.
`simulatorValue` is optional for numeric members and must fit the declared
encoding when present. A nonnumeric member without matching mutable property
state requires a simulator value so the generic responder does not invent one.

`fixed` is a raw unsigned little-endian field. A signed runtime value is valid
only when it is nonnegative and its full magnitude fits the declared width.
Negative values are rejected for every signed source width; the codec never
infers two's-complement, sign-extension, or zero-extension from the source
value type. A producer that owns a signed-to-raw conversion supplies the
canonical unsigned value. `signed` decodes to the matching signed PTP value and
encodes negative values as two's-complement at its declared width. A narrower
signed source value is sign-extended to that width.

The decoder reads records in order. Declared codes consume their payload-local
encoding, including declared fixed, signed, and PTP-string members. An
undeclared code triggers a bounded re-frame search over the candidate fixed
widths (1, 2, and 4 bytes). A candidate walk is accepted only when it consumes
exactly the declared record count and the complete payload, and the complete
walk with the fewest undeclared members wins. The decoder omits each
undeclared record and adds a `skippedUndeclaredMember` diagnostic containing
its code and raw value at the winning walk's width.

Two distinct minimal walks that both re-frame cleanly are a hard
undeclared-member error rather than a coin flip, as is a payload no candidate
walk completes, and a search that exhausts its state budget fails closed the
same way. Skip diagnostics are best-effort: a misaligned stream that
re-frames cleanly at one unique width can still attribute the wrong width and
raw value to a skipped member. The decoder never guesses an undeclared PTP
string, never pads a short fixed value, and never coerces a PTP string into a
number. Payloads without an undeclared member retain the existing
count-authoritative handling of bytes after the declared record count.
Duplicate members, unsupported widths, incompatible simulator values, and
direct-codec value/encoding mismatches fail loud. The simulator ignores
encoding-incompatible mutable state in favor of a compatible declared fallback
or the encoding's zero value. A compatible positive value that exceeds the
declared width still fails loud rather than falling back or truncating.

The public codec returns a `RecordStreamResult` containing ordered
`RecordStreamRecord` values and collected decode diagnostics. Each record
carries its code and lossless `PtpValue`; the current encodings produce `U32`
or `Str`. A member lookup returns `Option<PtpValue>`, so malformed input is a
codec error, an absent member is `None`, numeric zero is `Some(U32(0))`, and an
empty PTP string is `Some(Str(""))`. Callers inspect diagnostics to distinguish
a normal absent member from an undeclared fixed-width member that was skipped.
The old fixed-layout `parse_live_status` convenience is not a valid decoder for
heterogeneous D212 payloads and is retired; consumers resolve `PayloadInfo`
from the manifest and call `parse_record_stream`.

Record encodings are scoped to the containing payload. They do not assign a
global datatype to a mode/persona-overloaded property code. The PTP executor
adds only numeric record values to predicate scope; it consumes and returns
string records without pretending numeric predicates can evaluate them.

### 11.21 PTP failure-selected retry control flow

The PTP mode-entry/action grammar has a closed `retry` step for a logical wire
sequence that is safe to replay only after explicitly selected failures — named
non-OK PTP responses, and optionally whole failure classes:

```yaml
- retry:
    steps: [ <PTP step>, ... ]
    fallback: [ <alternate PTP step>, ... ]
    whenResponseCodes: ["0x2019"]
    whenFailureClasses: ["decode"]
    maxAttempts: 5
    retryDelayMs: 100
```

`maxAttempts` includes the initial attempt. A matching failure reruns the
complete nested `steps` after `retryDelayMs`; transport failures and unselected
failures escape immediately. After the budget is exhausted, the final failure
escapes unless `fallback` is present. A fallback runs once after the final
selected failure and replaces that failure when it succeeds. Unselected and
transport failures never enter it. The outer step's ordinary `tolerant` flag
may accept a final response failure, but it does not broaden which failures
select retry or fallback. `steps` must be non-empty, a present `fallback` must
be non-empty, at least one of `whenResponseCodes`/`whenFailureClasses` must be
non-empty, and `maxAttempts` must be at least one. Neither branch may contain a
`loop`, including through nested control flow. Failure-selected retry belongs
inside the per-element body. This keeps a failure on a later element from
replaying already-completed elements.

Bindings used after the retry must be produced by both the primary and fallback
branches. This permits response-selected acquisition, such as a vendor handle
list with a standard `GetObjectHandles` fallback, without making later steps
branch-aware.

`whenFailureClasses` is a closed vocabulary with a single member today:
`decode` — the step's PTP response was OK but its data payload failed to
decode (truncated record stream, short scalar, malformed array framing).
Field evidence shows cameras can serve a framing-valid but short property
payload while settling into a mode, so a manifest may declare bounded replay
for it. Decode selection never covers manifest/shape contract errors (unbound
slots, capture-kind mismatches) or transport failures, and `tolerant` remains
response-only: a step whose payload cannot be decoded after the retry budget
still fails loud, because silently continuing would skip gate-completing
captures and hide real parser defects.

This is failure policy, not a timer or readiness guess. Manifests may use a
bounded policy that is more reliable than a reference client when field evidence
shows a nominally terminal failure is transient. Such a deviation stays exact,
finite, and data-authored; consumers never string-match an error description.
The FFI mirrors the shape as `EntryStep::Retry`, and the reference simulator
executes the same selection and attempt rules.

### 11.22 Captured PTP collections and `forEach`

Collection acquisition is an ordinary, retryable PTP step; iteration performs
no implicit wire I/O. A `getProp` captures a count-prefixed PTP `u32[]` into a
named collection slot, and a later `forEach.in` names that slot:

```yaml
- retry:
    whenResponseCodes: ["0x2002", "0x2013", "0x2019"]
    maxAttempts: 3
    retryDelayMs: 1000
    steps:
      - getProp: "0xd621"
        captures: [{ bind: objectHandles, as: ptpU32Array }]
- loop:
    forEach:
      in: objectHandles
      bind: handle
      body: [ <PTP step>, ... ]
```

`ptpU32Array` decodes the standard PTP array framing (a little-endian `u32`
element count followed by that many little-endian `u32` elements). It is valid
only on a non-tolerant `getProp` and binds collection scope, not the scalar
`PropView`. The collection must be definitely bound before the loop; an unbound
slot or malformed payload fails loud. An empty collection succeeds without
running the body, and the existing deterministic iteration cap still applies.

This separation is the retry boundary: response-selected retry may replay the
collection-producing read, but once it succeeds the loop body runs at most once
per captured element. A body failure escapes normally and never replays an
earlier element. The FFI mirrors the capture as
`CaptureSourceInfo::PtpU32Array` and `FfiLoopKind::ForEach.collection` as the collection
slot name.

An unrepeated `sendOp` may instead capture `transactionId`. The executor binds
the transaction allocated to that successful request, allowing a later step or
old-session exit to reference it through an ordinary runtime parameter. This
capture is invalid on another verb or unless `sendOp.repeat` is exactly one
(including its default), because any other authored count makes the intended
transaction ambiguous. The FFI mirrors it as
`CaptureSourceInfo::TransactionId`.

A connection may select retries for PTP/IP initialization with
`initRetries: { max, backoffMs, whenReasons }`. `whenReasons` is a non-empty
list of hexadecimal InitFail reason codes. Consumers retry only a listed
reason, at most `max` times with `backoffMs` between attempts. An unlisted
reason and every transport failure escape this same-socket InitFail retry
immediately. On PCSS auto-discovery only, unavailability during the first Init
transport attempt may independently select the single outer rendezvous recovery
defined in §11.22a. The FFI exposes the retry list as typed
`InitRetryPolicyInfo` rather than asking consumers to interpret manifest extras.

#### 11.22a PCSS discovery targets

A PCSS rendezvous declares how its discovery datagram may be addressed:

```yaml
knock:
  callbackPort: 51560
  knockPort: 51562
  protocol: "PCSS/1.0"
  cameraName: "GFX100 II"
  discoveryTargets:
    default: subnetBroadcast
    supported: [subnetBroadcast, explicitUnicast]
    retryDiscoveredUnicast: true
```

`supported` is non-empty and duplicate-free, and `default` must be one of its
members. `retryDiscoveredUnicast: true` requires both `subnetBroadcast` and
`explicitUnicast` support. `subnetBroadcast` means IPv4 subnet-directed
broadcast, not multicast or the limited broadcast address: the consumer selects
an interface, derives that interface's directed broadcast from its address and
netmask, enables UDP broadcast, binds the callback listener, and sends to the
manifest's `knockPort`. It does not require or accept a camera-address
parameter. The first protocol-valid callback identifies the discovered camera.
Its `DSC` IPv4 field must parse and equal its TCP peer address, and when
`cameraName` is declared the typed `CAMERANAME` field must equal it before the
consumer acknowledges the callback. The callback's advertised `DSCPORT`
selects the command port.

`explicitUnicast` sends the byte-identical discovery datagram directly to a
caller-supplied camera IPv4 address on `knockPort`. In this mode the callback's
`DSC` and TCP peer must both match that address. When
`retryDiscoveredUnicast` is true and a subnet-broadcast callback's command
endpoint or first Init transport attempt is unavailable, the consumer performs
one new rendezvous round: it sends the byte-identical discovery datagram by
unicast to the learned `DSC`, waits for a fresh validated callback, and uses
that callback's new `DSCPORT`. It does not merely retry the old command
endpoint; a failed recovery round escapes normally. Protocol rejection,
malformed input, and local/clock failures do not authorize this recovery. In
both modes the discovery payload's `HOST` field is the route-selected local
callback IPv4 address, never the UDP destination. The FFI exposes the default,
supported target modes, recovery selection, and parsed camera IPv4 through
`PcssRendezvousInfo` / `PcssNotifyInfo`, so a consumer can make the camera
address optional only when the selected manifest-authored mode permits it.

#### 11.22b Polled live-view lifecycle

`startLiveView`, `pollLiveView`, and `stopLiveView` are exact members of the
closed action-verb vocabulary. Each resolves to a connection-specific,
manifest-authored PTP plan. `pollLiveView` requests exactly one payload: it does
not contain an implicit polling loop, open a new session, or establish the
camera-side live-view state. Callers repeat that action to implement a frame
loop.

The PCSS plan selects vendor-polled delivery with a `D1BC=2` property write,
then starts live view with `0x101c(0, 0)` and stops it with `0x1018(1)`. The
literal stop parameter is connection-specific; it is not the transaction ID
allocated to the start request. A poll may apply a finite response-selected
retry for the captured transient `0x2002` response; transport failures and
every other response still escape immediately under §11.21.

A caller validating live view followed by transfer invokes `startLiveView`,
`pollLiveView`, `enumerateObjects`, and `stopLiveView` in that order on one open
session. The PCSS `enumerateObjects` action is therefore mode-neutral, and its
`0x1007` operation gate covers both `shooting/stills` and `image-transfer`; the
per-object transfer actions remain image-transfer scoped. No `DF01` mode flip
occurs on this connection. The FFI mirrors all three live-view action verbs;
spelling and casing other than exact lower-camel-case names fail action parsing.

### 11.23 Semantic connection activities

Connection setup exposes a small, stable activity vocabulary alongside the raw
step stream. Activities let every consumer present coherent progress and seed a
reasonable initial elapsed-time expectation without interpreting GATT verbs,
step paths, or vendor-specific plan structure. They are descriptive metadata:
`defaultExpectedDurationMs` never changes executor deadlines, retries, or wire
behavior. Authored duration defaults are curated p75-like initial seeds: they
are deliberately conservative display estimates, not measured guarantees or a
contract to copy an unreliable reference application exactly.

Each activity descriptor contains a dot-delimited stable `id`, a positive
`version` (initially `1`), a `displayRole`, a positive
`defaultExpectedDurationMs`, `interactionRequired`, an `optional` marker, and
exactly one binding:

```yaml
activities:
  - id: camera.link.connect
    version: 1
    displayRole: connecting
    defaultExpectedDurationMs: 4000
    interactionRequired: false
    optional: false
    executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 }
```

`optional` is presentation metadata only. It lets a consumer distinguish an
activity that is not applicable on a particular walk from one that is still
upcoming; it never alters executor branching or decides whether an activity
runs.

Manufacturer-index establishment plans use `executorSpan`. `sequence` is
`steps` or `postExitReadiness`; bounds are top-level step indices. Spans for
each non-empty sequence MUST be ordered, non-overlapping, in bounds, and cover
the complete sequence. A nested branch, retry body, await callback, or acquire
delegate inherits the activity of its containing top-level step. An empty
sequence needs no span. Authored activity-list order is execution order, so all
`postExitReadiness` spans in a declared establishment list MUST precede all
`steps` spans. Establishment activities cannot use host checkpoints.

Body-manifest connections may use a descriptive `hostCheckpoint`, but use
`hostEstablishment` whenever a consumer must execute a specific host-side gate:

```yaml
activities:
  - id: camera.network.associate
    version: 2
    displayRole: joiningNetwork
    defaultExpectedDurationMs: 12000
    interactionRequired: false
    hostEstablishment:
      networkIdentityExact: { expectedScope: ssid }
  - id: camera.session.open.ap
    version: 2
    displayRole: openingSession
    defaultExpectedDurationMs: 10000
    interactionRequired: false
    hostEstablishment:
      retainedSessionOpen: { socketRole: command }
```

`networkIdentityExact.expectedScope` names a runtime-scope key. The host reads
that value and passes only when its observed network identity equals it exactly;
missing, undisclosed, or mismatched identity is a failed gate. It does not
permit route equality as a substitute. On an indexed load, the selected
establishment must declare that key in `persist` or produce it from its step
tree. `retainedSessionOpen.socketRole` opens
the real protocol session and retains it as the endpoint-reachability proof; it
is not a TCP connect-and-close or other disposable preflight probe. The host
must execute typed host-establishment activities in declared order.

Host checkpoints remain presentation-only descriptions of work outside the
Rust BLE executor. Their names are unique within a connection, and the executor
never emits their events. Typed host-establishment activities are also
host-owned, but their action and parameters are part of the manifest contract.
Within a connection, exact-network gates cannot repeat the same expected scope
and retained-session gates cannot repeat the same socket role.
Activity ids are unique within a plan or connection. Repeated `(id, version)`
pairs across a loaded store MUST have identical role, expected duration,
interaction, optional metadata, and semantic binding. Executor span coordinates
are plan-local, so only the `executorSpan` binding kind participates in that
identity; host-checkpoint names and typed host-establishment actions and
parameters participate in full. Changing an activity boundary or meaning
requires a version bump. Tuning only the display-duration seed does not;
consumers may replace that seed with their own learned value for the same
activity identity and version.

The initial display roles are `connecting`, `waitingForCamera`,
`confirmingPairing`, `preparingConnection`, `startingNetwork`, `joiningNetwork`,
and `openingSession`. The FFI preserves an unrecognized future token as
`Unknown { raw }`; consumers must not fail merely because a newer manifest adds
a role.

`establishment(...)` returns one merged list in execution order: the selected
plan's establishment spans in authored order, followed by the selected
connection's host activities in authored order. A mechanism-selected reconnect
plan has no connection host activities. `connectionEstablishment(...)` returns
the connection's host activities directly. Their order is the declared contract
because the host owns their execution.

The executor accepts a `ConnectionActivityObserver` alongside `StepObserver`.
It emits `Started`, `Retrying { retry }`, and exactly one terminal `Succeeded`,
`Failed { failure }`, or `Cancelled` event for each executor span it enters.
Every terminal carries a `ConnectionActivityTerminalSummary`. A
`ConnectionActivityRetry` contains the one-based total-attempt `ordinal`, the
local retry `limit`, and a typed `ConnectionActivityFailure`. The failure's
`context` is an ordered list of decoded values named by the retry step's
`failureContext`; retry mechanisms without a manifest context list expose an
empty list, never a raw diagnostic string.

`ordinal` resets for each retry primitive, while the terminal summary's
`retryCount` counts every replay across the complete activity. Its optional
`lastRetry` preserves the most recent retry snapshot, including through a later
successful attempt whose final scope has advanced. A failed terminal carries
the final failure separately from `lastRetry`, so an exhausted attempt cannot
overwrite the failure that authorized the preceding replay. Activity start
precedes the first raw step report, retry capture happens after `onFailure` and
predicate evaluation but before its event, delay, and next attempt, and a
terminal activity event follows the final raw terminal report. Tolerated step
failures do not end the activity. Dropping the executor future emits exactly one
`Cancelled` with the retries already consumed. `StepReport` carries the optional
activity id and version so raw diagnostics remain correlatable without
consumers reconstructing spans.

Firmware refinement replaces both the unwalked step tail and its relative
activity spans. The internal resolver keeps native index steps and descriptors;
the public `ReplaceTail` mirrors FFI steps plus relative replacement spans. The
walker splices both together, so a refined tail remains completely covered and
does not require an FFI-to-schema conversion. When the first replacement span
has the same id, version, and metadata as the activity containing
`acquireFirmware`, that activity continues across the splice with one lifecycle;
a different identity or version ends the current activity before the tail.

### 11.23a Socket binding availability

Socket bindings use a port scalar when the listener is immediately available.
An event or live-view binding MAY use a descriptor to declare that the camera
does not listen until a successful operation completes:

```yaml
bindings:
  command: 55740
  event:
    port: 55741
    availableAfter: { operation: "0x101c" }
  liveView:
    port: 55742
    availableAfter: { operation: "0x101c" }
```

`availableAfter.operation` names an operation in the manifest catalog. It is
valid only on event and live-view bindings. The simulator refuses TCP
connections on that port until the operation succeeds in the current session.
The declared condition takes precedence over the causal prefix inferred from
the connection's `openChannel` steps. A binding without `availableAfter` keeps
the inferred behavior.

`commandListenerVolatile: true` declares that closing the active command
transport may remove its listener. A caller cannot use an immediate redial as
generic recovery. A manifest-authored outer connection re-establishment may
create a new listener. The field defaults to `false`.

### 11.24 Rust-owned PTP entry execution

The `EntryStep` grammar is executed in Rust behind a foreign async
`PtpExecutorTransport`. This is the PTP counterpart to §9.3's BLE executor:
consumers supply raw framed command I/O, event-code-selective raw event I/O,
manifest-triggered auxiliary-channel open, transaction-id reservation,
command-session close/reopen, and a host
`sleep(ms)` clock. Event selection preserves unrelated queued frames for their
normal consumers; Rust still parses and verifies the selected frame. The engine resolves
the connection framing and owns step sequencing, response parsing, retry and
tolerance policy, scalar/collection capture, predicates, loops, and deadlines.
The host continues to own sockets, cached PTP/IP identity, OS network state,
connection establishment replay, and transition selection. `openChannel` keeps
listener availability causal: Rust calls the host only after preceding plan
steps succeed, so consumers do not infer readiness from a role, port, model, or
transport name.
Manifest load accepts `openChannel` only for a bound event or live-view role,
only as a top-level mode-entry or action step, and only after a strict
simulator-matchable wire step; consecutive channel opens share that boundary.

Mode entries and actions MAY carry `activities` using §11.23 descriptors with
`executorSpan.sequence: steps`. A non-empty executable sequence with activities
MUST be covered completely by ordered, non-overlapping, in-bounds top-level
spans. Re-establishment activities cover `exitSteps`; the remaining outer
lifecycle uses the connection/establishment activities already defined in
§11.23. `userInstruction` entries cannot declare executor spans. Current plans
without PTP activity metadata remain valid and emit no semantic activity
events; executors never infer display roles or duration seeds from verbs.

Every PTP transport call has a Rust-owned deadline raced against the host
clock. Ordinary steps also have a 60-second aggregate budget, preventing a
multi-phase or repeated verb from restarting the backstop indefinitely.
`awaitUntil.timeoutMs` is its aggregate budget and covers event receipt,
post-event polling, `onEach`, and interval sleeps. The default 10-second
per-call backstop applies where the manifest has no explicit budget. Losing or
cancelling the race drops the foreign future, and an active semantic activity
terminates exactly once as failed or cancelled.

Standard PTP/IP data-out uses `StartData` followed by `EndData`; data-in must
finish with `EndData` and match the declared total length. Scalar property
reads always populate predicate scope even without a named capture. A
manifest-declared record-stream property instead populates its allowed member
observations, keeping composite polling generic and data-driven.

Only non-OK PTP responses participate in `tolerant` semantics. Retry replays
its nested sequence only for an exact declared response or a declared failure
class (§11.21); an unselected failure escapes immediately, and exhausted retry
may be tolerated only by the enclosing retry step — and only when the final
failure is a non-OK response. A transport/framing/malformed-capture failure is
always fatal, except that a manifest may select bounded replay of payload
*decode* failures via `whenFailureClasses: ["decode"]`. Completed collection elements are
never replayed after a later element fails because schema validation rejects a
loop beneath retry; per-element retries are nested inside the loop body.

The typed result preserves unsigned 64-bit and string scalar bindings, captured
collections, ordered data outputs, response parameters, transaction ids, and
the completed-step count. The shared `StepReport` carries optional PTP
operation/property/response/transaction fields and the declared tolerance in
addition to BLE's characteristic field. Composite reports identify the final
or failure-determining transaction, allowing consumers to retain raw
diagnostics without interpreting error strings.

### 11.25 PCSS discovery and establishment

`FamilyBlock.pcss` owns the manufacturer-family auto-discovery endpoint and
bounded search policy needed before a body manifest is selected. A
`pcssNotify` signature matches the parsed `CAMERANAME` and `SERVICE` fields and
suggests the model's PCSS connection. Recognition consumes typed fields, never
raw HTTP-like text.

PCSS known-address establishment is independently valid. It binds the callback
listener, sends `DISCOVERY` by unicast to the supplied camera address, validates
and acknowledges `NOTIFY`, and connects to the returned `DSC:DSCPORT`.
Auto-discovery performs one family-level subnet-broadcast search, recognizes the
callback, and uses its advertised endpoint directly. When the selected body
permits `retryDiscoveredUnicast`, an unavailable command endpoint or first Init
transport attempt authorizes one fresh unicast rendezvous to the learned `DSC`;
protocol rejection and malformed input do not. A saved or manually supplied
address never depends on broadcast support.

The executor validates the initiator GUID, friendly name, and complete Init
encoding before binding the callback listener or sending discovery. A successful
command Init response is the exact 68-byte PCSS `InitCommandAck`; a header-only
or standard-layout type-2 packet is malformed and terminal.

The Rust-owned executor performs sequencing, deadlines, rendezvous replay, and
InitFail classification behind `PcssExecutorTransport`; the foreign host owns
only UDP/TCP primitives and the wall clock. `next_callback` returns a typed
`PcssCallback` carrying the TCP peer IPv4 address and payload so Rust can
require peer = `DSC`; explicit unicast additionally requires both to equal the
requested address. When the selected body's knock contract declares a camera
name, both auto-discovery and explicit unicast reject a callback whose typed
`CAMERANAME` differs before acknowledging it or opening the command endpoint.
Any u32 InitFail reason may be retried only when selected by manifest policy
(`0x2019` is the current GFX100 II selection). Each retry reuses the active
command socket and the byte-identical Init request. Other reasons and exhausted
retries fail with a typed error. `DSC` and `DSCPORT` are authoritative runtime
values; a manifest command binding is a simulator/default bind, not an initiator
fallback.

`initRetries` is a closed policy: a non-zero `max` requires a non-zero
`backoffMs` and at least one unique hexadecimal `whenReasons` value fitting
u32. A zero `max` requires both companions to be empty or zero.

### 11.26 Canonical observation bundle

`camera-observation/v1` is the only accepted evidence-bundle discriminator. It
is an exact validation token, not a compatibility promise. A JSONL bundle begins
with exactly one `bundleHeader` and contains only the closed record kinds
`lifecycle`, `bleGatt`, `ptpTransaction`, `ptpEvent`, `httpExchange`,
`capability`, and `actionInvocation`. Unknown schemas, fields, and kinds fail the
bundle.

The header carries the stable run id; exact sanitized manufacturer, body
pseudonym, model and firmware; client artifact and platform; capture interfaces;
clock definitions and mappings; loss counters; redactions; tool versions; and
artifact length/SHA-256 metadata. Every record carries a stable record id,
unique ordinal, `(connection, mode, state)` context, physical context, artifact
ranges, and epistemic metadata. Correlation keys include connection instance,
session or endpoint set, and transaction id where applicable. PTP events are
separate records and are never folded into a response. They link to a transaction
only when the recorder knows that exact causal record; unmatched asynchronous
events remain explicitly unlinked.

Payloads use inline bytes only below the recorder's bound. Larger bodies carry
streamed length, a whole-payload SHA-256, and contiguous per-range SHA-256
metadata starting at offset zero. Optional artifact ranges are ordered, in
bounds, and consistent with their declared artifact. Hashes are lowercase
64-digit SHA-256 values. Clocks are explicitly named; cross-clock time claims
require a declared mapping.

Transaction `outcome` (`ok`, `nonOk`, `timeout`, `transportAbort`, or
`incomplete`), `evidenceBasis` (`descriptorOnly` or `writeProbe`),
`observedEffect` (`confirmed`, `ackNoEffect`, `protocolRefused`,
`destructiveClamp`, or `unknown`), and `readback` are independent. Readback is a
tagged `observed` value containing baseline, request, settling/deadline, observed
value/time/source, or `notObserved` with a reason. Validator rules reject
combinations that claim knowledge unsupported by the transaction and readback.

Validation accounts for every nonblank line with a stable diagnostic code.
Malformed JSON, missing or duplicate identities/ordinals, dangling links,
invalid clocks/hashes/ranges, incomplete PTP claims, loss/truncation that makes
an assertion unusable, conflicting facts, and incoherent result/effect/readback
block the complete bundle. No accepted subset proceeds to proposal generation.

Proposals preserve each observed `(connection, mode, state)` tuple atomically
and give every candidate a stable content hash. Every accepted input record is
linked to all candidates it supports or marked `evidenceOnly`. Input path,
input order, record order, wall-clock generation time, and host paths do not
affect proposal bytes. Apply recomputes candidate and proposal digests, then
requires a review bound to that digest with exactly one `accept`, `reject`, or
`defer` disposition per candidate. Only accepted assertions are applied; the
complete manifest is validated before atomic replacement.

Every capability record also carries `inventoryCompleteness`, independently
for the operation or property inventory named by its subject. Omission means
`partial`; existing bundles therefore remain partial. `complete` is an explicit
attestation that the inventory covers the exact header camera model, firmware,
and the record's `(connection, mode, state)` observation context. A positive
row in either form means only "advertised here," not "confirmed working here."
Absence from a partial inventory has no negative meaning. A `supported: false`
assertion is valid and reviewable only when its exact record declares
`inventoryCompleteness: complete`; a negative from a partial inventory is a
validation error. Completeness in one context never transfers to another.

### 11.27 Shared action identity and roles

Each connection action has one stable id, shared parameters, triggers, evidence,
and explicit role bindings. `initiator` contains runtime parameters, PTP steps,
and semantic activities. Optional `responder` contains runtime parameters and
one closed simulator mutation or replay primitive. Triggers are declarations;
neither role executes them implicitly.

Initiator parameter declarations normalize to `{ name, kind, required }`.
Existing bare names remain valid shorthand for `{ name: <bare-name>, kind:
u64, required: true }`; the expanded form supports `u64` or `string` and may set
`required: false`. The deterministic catalog exposes the normalized form, with
`String` alongside its numeric parameter kinds. Optional means the caller may
omit the argument: absence is valid and does not synthesize a value.

Each supplied invocation value is an `ActionArgument`. Its serde/HTTP form is
untagged — a JSON unsigned number or JSON string, never a discriminator object —
and its UniFFI mirror is a closed enum with `U64` and `String` variants.
Resolution checks each value against the declared kind. Wrong types, like all
other invocation-shape failures, are rejected before transport I/O or simulator
mutation.

`setProp.value` is either an integer literal or a runtime-slot reference. A
runtime reference carries `ifMissing: error | skip`; `error` is the default.
`skip` is legal only when the slot names an optional initiator parameter. A
missing skipped value completes the step successfully with no transport I/O;
using `skip` for a required or undeclared parameter is a schema error. Present
values are validated against the target property's manifest type before I/O:
integer property types require a numeric argument, while `str` requires a
string. When that property declares `structuredText`, string validation also
enforces its delimiter, exact field count, and declared field kinds. Thus an
optional value can suppress a write, but can never weaken type or structured-
text validation when present.

An `awaitUntil` step may carry the enclosing `captures` field described in
§11.16. Only `propValue` is valid there, and it binds the final property reply
that satisfied a `poll` or an event source's `thenPoll`; it never causes an
additional `GetDevicePropValue`.

A deterministic catalog exposes catalog revision, action id, connection, mode,
supported roles, exact parameter declarations, triggers, and availability. An
invocation carries the revision, id, role, mode, and an ordered parameter map.
Unknown action or connection, wrong mode or role, stale revision, duplicate,
missing, or extra parameters fail before transport I/O or simulator mutation.
Rust's PTP entry point is explicitly initiator execution. FFI conversion of an
action step, activity, or trigger is fallible and all-or-error; it never uses
`filter_map` to discard an unrepresentable member.

The initial responder proof binds wireless-tether `shutter` to enqueue the
explicitly requested `objectCount`. The parameter defaults to one and is bounded
by that action's declared `objectsAvailable` range. This is a role binding, not
general simulator state injection.

For the PCSS action covered by #340, `0xD395` is optional independently on each
attempt. When absent, its `setProp` uses `ifMissing: skip` and performs no I/O;
when present, the property's `structuredText` contract requires exactly three
signed decimal fields. `0xD209` remains the polled terminal result, and the
action captures the final satisfying value directly from `awaitUntil` for the
caller. The corresponding responder proof uses `PropertyTransition` for
`0xD209`, so the terminal result is invocation-scoped rather than an
unconditional effect of the shared operation.

### 11.28 Observation and operator HTTP surfaces

The initiator transport and simulator command path write the same canonical
observation records. The simulator exposes `GET /actions`,
`POST /actions/{id}`, and cursor-based
`GET /observations?after=<cursor>`. Observation cursors survive ordinary service
restart when the configured log is reused. `/trace` remains a bounded human
operator projection and explicitly reports dropped events, truncated fields,
and its current cursor.

The TUI fetches and proxies the simulator catalog. Local phase patches and quit
are named under reserved `operator:*` ids and use `/operator/actions/{id}`;
they never appear as manifest actions.

### 11.29 USB connections and establishment plans

USB adds two connection `kind` values and a USB step vocabulary for their
establishment plans (#342). Both kinds carry PTP over USB. They differ in
who owns the session, the framing, and the transaction ids.

- `usb` (raw). The initiator owns the device handle, the interface claim,
  the bulk OUT/IN and interrupt IN endpoints, the PTP session, and
  transaction ids. PTP containers ride the bulk endpoints with USB/PIMA
  framing; events ride the interrupt IN endpoint.
- `usb-passthrough`. A platform daemon owns the device, framing, session,
  and transaction ids. The host speaks typed PTP transactions and never
  handles raw containers. Establishment attaches to the session the daemon
  already opened: no interface claim, no `OpenSession`.

Both kinds remain ordinary `connections.<id>` entries. Platform
availability stays data-driven through `connections(platform)`; modes,
actions, and mode entries are declared per connection exactly as on the
existing kinds. A `platforms:` list gates a connection per host OS against
the closed token set `ios|macos|android|linux`; the loader rejects an
unknown token with an error naming the token and the connection.

USB attachment recognition uses typed connection discovery data (#462):

```yaml
discovery:
  mechanism: usb
  announces: attachment
  platforms: [ios, macos]
  vid: 0x04cb
  # pid: 0x1234  # optional, only when descriptor evidence establishes it
```

`discovery.mechanism` is a lowercase kebab-case token. Whitespace is invalid.
`discovery.platforms` controls automatic recognition and must be a subset of
the connection's `platforms` list. USB discovery requires at least one entry.
The list may be narrower than connection availability. macOS can therefore
expose raw `usb` for an explicit adapter while its ImageCapture attachment
selects `usb-passthrough`.

`vid` is required and nonzero for `mechanism: usb`. `pid` is optional and
nonzero when declared. An absent PID is a vendor-level candidate, not durable
camera identity. The consumer runs the manifest `readDeviceInfo` action over
the selected transport, parses the result with `parse_device_info`, and calls
`confirm_device_info(model, deviceInfo)`. The engine normalizes and compares
the parsed manufacturer and model to the selected body's manifest identity.
A mismatch fails confirmation.

**Connection trait fields.** Two per-connection trait fields in the #81
pattern (declarative data the consumer selects behavior from, never an
`id` branch):

```yaml
connections:
  usbTether:
    kind: usb
    establishment: usb-claim-session
    session: { ownership: initiatorOwned }
    events: { delivery: bestEffort }
```

- `session.ownership` is `initiatorOwned` (the executor opens and owns the
  PTP session) or `daemonAttached` (a platform daemon owns the session; the
  executor attaches and sends no session-management operations). A `usb`
  connection is `initiatorOwned`; a `usb-passthrough` connection is
  `daemonAttached`. The trait field, not the `kind` string, selects
  executor behavior.
- `events.delivery` is `reliable` (every pushed event is delivered; the
  current event-socket semantics of §11.16.1), `bestEffort` (pushed events
  exist but any single event may be lost), or `none` (the connection has no
  event channel). The `thenPoll` requirement scopes to the `EntryStep`
  `awaitUntil` grammar: on a `bestEffort` connection every event-source
  `awaitUntil` MUST declare `thenPoll` so a missed event reconciles by
  polling (§11.16.1); the loader rejects an event-source await without
  `thenPoll` on such a connection. `none` forbids event-source awaits on
  the connection outright, and the loader also rejects a `none` connection
  whose USB establishment plan awaits interrupt frames.

**Family `usb` block.** `families.<fam>.usb` owns the family-level USB
facts, parallel to `ble` (§11.4) and `pcss` (§11.25):

```yaml
families:
  fuji:
    usb:
      interfaces:
        stillImage: { class: 6, subclass: 1, protocol: 1 }
        vendor: { class: 255, subclass: 255, protocol: 0 }
      establishments:
        usb-claim-session:
          mechanism: usb-claim-session
          steps:
            - usbClaim: { interface: stillImage }
            - usbBulkOut: { data: { captured: openSessionContainer } }
            - usbBulkIn: { maxLength: 512, encoding: bytes-raw, captureAs: openSessionResponse }
```

- `interfaces` maps a symbolic interface name to its USB
  class/subclass/protocol triple (each a u8). `usbClaim` references the
  symbolic name; the FFI resolves it to the triple at index-build time,
  exactly like §11.3 GATT-name resolution. The Step variant returned over
  the uniffi boundary carries the resolved triple, not the name. A step
  naming an undeclared interface is a load-time error.
- `establishments` are named plans keyed by mechanism. A body connection's
  `establishment:` field selects one. The plans reuse the §11
  `EstablishmentBlock` shape (`params`, `persist`, `activities`,
  `postExitReadiness`, `steps`).

**USB verbs.** One-entry YAML mappings carrying the usual `StepOptions`,
mirroring the BLE verb design (§11.4, §11.4a):

- `usbClaim: { interface: <symbolic name> }`: claim the resolved interface
  on the bound device. Valid only in a raw `usb` establishment; a
  `daemonAttached` connection has no interface to claim.
- `usbBulkOut: { data: <StepValue> }`: resolve `data` per §11.1 and write
  the bytes to the bulk OUT endpoint. The §11.13 write pipeline applies
  (resolve → encoding decode → transform chain → wire bytes).
- `usbBulkIn: { maxLength: <u32>, encoding: <Encoding>, captureAs: <slot>, transform?: [...] }`:
  read up to `maxLength` bytes from the bulk IN endpoint, then run the
  §11.13 capture pipeline (transform chain → encoding decode → scope
  string) and bind the result under `captureAs`.
- `usbAwaitInterrupt: { encoding: <Encoding>, captureAs: <slot>, transform?: [...], timeoutMs?: <u32> }`:
  await one interrupt IN event frame and capture it with the same pipeline
  as `usbBulkIn`. A strict wait verb: a miss fails the step after its
  budget. The budget is `timeoutMs`; absent, the executor's 10-second
  single-call backstop applies, so plans authored before the field existed
  are unaffected. The `events.delivery` `thenPoll` rule does not govern it
  (that rule scopes to the `EntryStep` `awaitUntil` grammar), but a
  connection declaring `events.delivery: none` has no event channel, so the
  loader rejects its establishment plan when the plan awaits an interrupt
  frame.

USB verbs are valid only inside `families.<fam>.usb.establishments` plans;
the loader rejects them anywhere else. BLE verbs keep their existing
scoping. A `usb-passthrough` connection runs no USB verbs: its mode entries
and actions execute the existing `EntryStep` transaction grammar over
`PtpTransactionTransport` instead.

**Raw executor transport.** `UsbExecutorTransport` is the foreign
(`with_foreign`) async trait a host implements for raw `usb`
establishments, the USB counterpart to §9.3's BLE executor. It is raw I/O
only. Rust owns step sequencing, capture/transform/encoding evaluation,
retry and tolerance policy, and deadlines.

| method | contract |
|---|---|
| `claim_interface(class: u8, subclass: u8, protocol: u8)` | claim the interface matching the resolved triple |
| `bulk_out(data: Vec<u8>)` | write one bulk OUT transfer |
| `bulk_in(max_length: u32) -> Vec<u8>` | read one bulk IN transfer of at most `max_length` bytes |
| `next_interrupt_event() -> Vec<u8>` | await one interrupt IN event frame; may pend indefinitely, the executor owns every deadline |
| `release_and_close()` | release the claimed interface and close the device handle |
| `sleep(ms: u32)` | the host wall clock |

Every method is fallible with `UsbTransportError`:

| variant | raised when |
|---|---|
| `NotConnected` | no matching device is attached |
| `DeviceGone` | the device detached mid-operation |
| `Stall` | an endpoint answered STALL |
| `Timeout` | a transfer exceeded its deadline |
| `NotAuthorized` | the platform denied USB access |
| `ClaimFailed { owner }` | another driver holds the interface; `owner` names it when the platform reports one |
| `OpenFailed` | the device could not be opened |
| `Failed` | any remaining failure |

Deadlines are executor-owned: the executor races each pending transport
call against `sleep`, the same contract as the BLE trait. A lost or
cancelled race drops the foreign future, so every method must be
cancellation-safe.

**Transaction transport.** `PtpTransactionTransport` is the foreign async
trait a host implements for `daemonAttached` connections. The daemon owns
framing and session state, so the seam is typed transactions, not byte
frames.

| method | contract |
|---|---|
| `execute(opcode: u16, params: Vec<u32>, data_out: Option<Vec<u8>>, timeout_ms: u32) -> PtpTransactionResult` | run one typed PTP transaction; `PtpTransactionResult` carries `response_code`, response `params`, and optional `data_in`; the daemon enforces the per-call `timeout_ms` |
| `read_partial_object(handle: u32, offset: u64, length: u32, timeout_ms: u32) -> Vec<u8>` | read one object range |
| `next_event(event_code: u16) -> PtpTransactionEvent` | return the next event matching `event_code` as `{ event_code, params }`; code-selective, the host retains unrelated events for their normal consumers, the same contract as `PtpExecutorTransport::next_event_frame` (§11.24) |
| `shutdown()` | detach from the daemon session (named `shutdown`: a `close` method clashes with `AutoCloseable.close()` in the Kotlin bindings) |
| `sleep(ms: u32)` | the host wall clock |

Every method is fallible with `PtpTransactionError` (`NotConnected`,
`DeviceGone`, `Stall`, `Timeout`, `NotAuthorized`, `Failed`), the
`UsbTransportError` vocabulary minus the claim/open variants the daemon
owns. The executor supplies `timeout_ms` from the step's manifest budget
(§11.24 defaults apply). Aggregate budgets (`awaitUntil.timeoutMs`, the
60-second step aggregate) stay executor-owned and race against `sleep`.

**Executor entry points.**

| call | walks |
|---|---|
| `run_usb_establishment` | a raw `usb` establishment plan over `UsbExecutorTransport` |
| `run_mode_entry_txn` | a mode entry's `EntryStep` grammar over `PtpTransactionTransport` |
| `run_initiator_action_txn` | one action's initiator binding over `PtpTransactionTransport` |
| `run_initiator_action_txn_to_sink` | the same initiator walk over `PtpTransactionTransport`, streaming each completed data output to `PtpDataOutputSink` |

The transaction entry points run the same grammar, retry, tolerance,
capture, predicate, loop, and deadline semantics as their frame-based
counterparts (§11.24); only the transport seam differs. The existing
frame-based entry points are unchanged.

The GFX100 II pass-through row exposes only `readDeviceInfo` in this contract.
It does not claim ImageCapture catalog behavior, transfer, or USB live view.
