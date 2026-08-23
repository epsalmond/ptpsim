//! Manufacturer-index FFI surface (plan §3.2 + §3.3 + §11).
//!
//! This is the **pull-model** query surface the iOS BLE-MVP consumes:
//!
//! 1. App boots and calls [`crate::ConfigStore::from_manufacturer_index`].
//! 2. BLE scan delivers an advert → app calls [`crate::ConfigStore::recognize`]
//!    with a [`ScanObservation::BleAdvert`] → receives a [`Recognition`]
//!    carrying the matched signature's facts in `runtime_scope`.
//! 3. On `Candidate`, app calls [`crate::ConfigStore::establishment`]
//!    (model, connection, initial_scope) → receives an [`EstablishmentPlan`]
//!    whose `steps` the dispatcher walks.
//! 4. Optional: [`crate::ConfigStore::refine_establishment`] when firmware is
//!    discovered mid-walk. The result distinguishes "no change" from a replacement
//!    tail and from invalid plan errors.
//!
//! Types here intentionally match plan §3.3 verbatim — they form the iOS
//! contract. The conversion adapters from [`camera_config::index`] live in
//! the bottom half of this file.

use camera_config as cc;
use camera_config::index as ix;
use std::collections::BTreeMap;

use crate::executor::ExecutorStepFailureKind;
use crate::{KeyValue, SocketRole};

// ---------------------------------------------------------------------------
// ScanObservation → Recognition (§3.2)
// ---------------------------------------------------------------------------

/// The pull-model input: what the app observed, the FFI decides what it means.
/// Plan §3.2 — only BLE is in the MVP. Later transports extend the enum
/// without changing callers.
///
/// Populate every field your platform exposes and leave the rest
/// `None`/empty — predicates over an absent field evaluate false, never
/// error (§11.14). CoreBluetooth cannot supply `ad_records` (no raw AD
/// access) and exposes TX power only when the advert carries it.
#[derive(Debug, uniffi::Enum)]
pub enum ScanObservation {
    /// A BLE advertisement seen during scan. Apple delivers service UUIDs as a
    /// list; some bodies advertise multiple — the matcher iterates the whole
    /// list.
    BleAdvert {
        service_uuids: Vec<String>,
        /// The manufacturer-specific AD record, split into company id +
        /// post-id payload — signature payload offsets are relative to the
        /// payload (§11.14). Consumers split iOS
        /// `CBAdvertisementDataManufacturerDataKey` into
        /// `(company_id_LE, payload)`; Android's
        /// `getManufacturerSpecificData(companyId)` is already the payload.
        manufacturer_data: Option<BleManufacturerData>,
        /// Service-data AD records, one entry per advertised UUID.
        service_data: Vec<BleServiceData>,
        local_name: Option<String>,
        /// Advertised TX power level (dBm), when the advert carries one.
        tx_power: Option<i8>,
        /// Raw AD records exactly as seen on air, for platforms that expose
        /// them (Android `ScanRecord.getBytes()`); empty on iOS.
        ad_records: Vec<BleAdRecord>,
    },
    /// A validated PCSS callback. The executor parses the wire message before
    /// recognition; callers that use the codec surface may construct this
    /// observation from `PcssNotifyInfo`.
    PcssNotify {
        camera_ipv4: String,
        camera_name: String,
        command_port: u16,
        service: String,
    },
    /// A USB device attachment surfaced by the host. The engine matches raw
    /// descriptor facts and platform against connection discovery data.
    UsbAttachment {
        platform: crate::Platform,
        vendor_id: u16,
        product_id: u16,
    },
}

/// The manufacturer-specific AD record split per §11.14: `payload` excludes
/// the 2-byte LE company id.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BleManufacturerData {
    pub company_id: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct BleServiceData {
    pub uuid: String,
    pub payload: Vec<u8>,
}

/// One raw AD record as seen on air — for `ad_type` 0xFF the payload
/// INCLUDES the 2-byte LE company id.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BleAdRecord {
    pub ad_type: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, uniffi::Enum)]
pub enum Recognition {
    /// No signature matched. App keeps scanning.
    NoMatch,
    /// Exactly one model + connection identified. `runtime_scope` carries
    /// every fact the signature derived (style, key bytes, etc.). App feeds
    /// it verbatim into `establishment(...)` as `initial_scope`.
    /// `runtime_scope_encodings` names the encoding each advert *capture*
    /// decoded with (`key` = capture name, `value` = encoding token) — the
    /// executor threads it back through `run_establishment` so a later
    /// `{ captured: … }` write-back re-encodes by the real encoding instead
    /// of a scope-string guess (#43). The same capture name can carry a
    /// different encoding per signature (legacy `pairingKeyBytes` is
    /// `bytes-le`, RED's is `ascii`), so this rides with the match rather
    /// than being derivable from the model.
    Candidate {
        model: String,
        connection: String,
        confidence: Confidence,
        runtime_scope: Vec<KeyValue>,
        runtime_scope_encodings: Vec<KeyValue>,
    },
    /// Multiple models matched the same signature (e.g. an advert that
    /// fits several Fuji bodies). The FFI does NOT auto-pick — the app
    /// prompts the user. `runtime_scope` here holds facts true for ALL
    /// candidates (e.g. `style: "legacy"`) and is passed to
    /// `establishment()` once the user narrows to a model.
    Disambiguate {
        family: String,
        candidates: Vec<ModelMatch>,
        runtime_scope: Vec<KeyValue>,
        hint: Option<String>,
    },
}

/// Manifest-authored policy for one saved-camera reconnect scan.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ReconnectPolicy {
    pub scan_timeout_ms: u32,
}

/// Classification of one advertisement for a known saved camera. The
/// manifest owns both identity matching and the plan selected for the state.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ReconnectDecision {
    NoMatch,
    Wake {
        plan: EstablishmentPlan,
        runtime_scope: Vec<KeyValue>,
    },
    Ready {
        plan: EstablishmentPlan,
        runtime_scope: Vec<KeyValue>,
    },
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ModelMatch {
    pub model: String,
    pub display_name: String,
    pub connection_hint: Option<String>,
}

// ---------------------------------------------------------------------------
// EstablishmentPlan + Step grammar (§3.3 + §11)
// ---------------------------------------------------------------------------

/// A walkable establishment sequence. `plan_handle` is the stable
/// `model:selector` token the dispatcher echoes to the executor/refiner:
/// connection selector for ordinary establishment, mechanism selector for a
/// reconnect decision.
#[derive(Debug, Clone, uniffi::Record)]
pub struct EstablishmentPlan {
    pub plan_handle: String,
    pub mechanism: String,
    /// Mechanism that must complete before this plan (e.g.
    /// `ble-establish-wifi-ap` carries `Some("ble-pair")`). Advisory — the
    /// consumer sequences on it; the reference walker does not enforce it.
    pub prerequisite: Option<String>,
    /// User-initiated from an established BLE link, NOT auto-chained after
    /// `prerequisite` (#91): the consumer rests at BLE-connected and runs this
    /// on a user action.
    pub on_demand: bool,
    /// Runtime parameter names the consumer binds before walking (e.g.
    /// `["launchMode"]`). Empty for plans that take no runtime input.
    pub params: Vec<String>,
    /// Slot names the host should persist after this plan to replay on a later
    /// `ble-reconnect` (#91). Empty for plans with nothing to cache.
    pub persist: Vec<String>,
    /// Executor spans followed by the selected connection's host-owned activities.
    pub activities: Vec<ConnectionActivityDescriptor>,
    /// Manifest-authored gate to walk after an orderly feature exit and before
    /// replaying `steps`. Empty means no post-exit readiness gate is declared.
    pub post_exit_readiness: Vec<Step>,
    pub steps: Vec<Step>,
}

/// The result of refining a plan after firmware is discovered mid-walk.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum EstablishmentRefinement {
    /// The existing unwalked tail remains valid.
    NoChange,
    /// Replace `plan.steps[next_step_index..]` with these steps and relative
    /// executor spans.
    ReplaceTail {
        steps: Vec<Step>,
        activities: Vec<ConnectionActivityDescriptor>,
    },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ConnectionActivityDescriptor {
    pub id: String,
    pub version: u32,
    pub display_role: ConnectionActivityDisplayRole,
    pub title: Option<String>,
    pub default_expected_duration_ms: u32,
    pub interaction_required: bool,
    pub optional: bool,
    pub binding: ConnectionActivityBinding,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ConnectionActivityBinding {
    ExecutorSpan {
        sequence: ConnectionActivitySequence,
        start_step: u32,
        end_step_exclusive: u32,
    },
    HostCheckpoint {
        name: String,
    },
    HostEstablishment {
        action: HostEstablishment,
    },
}

/// A host-owned, executable connection-establishment action. Consumers must
/// preserve the declared ordering and must not substitute route inference or a
/// disposable network probe for these actions.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum HostEstablishment {
    /// Require the observed network identity to exactly equal this runtime
    /// scope key. Missing or undisclosed identity never passes.
    NetworkIdentityExact { expected_scope: String },
    /// Open and retain the real protocol session on this socket role.
    RetainedSessionOpen { socket_role: SocketRole },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectionActivitySequence {
    Steps,
    PostExitReadiness,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectionActivityDisplayRole {
    Connecting,
    WaitingForCamera,
    ConfirmingPairing,
    PreparingConnection,
    StartingNetwork,
    JoiningNetwork,
    OpeningSession,
    Unknown { raw: String },
}

impl From<&camera_config::ConnectionActivityDescriptor> for ConnectionActivityDescriptor {
    fn from(value: &camera_config::ConnectionActivityDescriptor) -> Self {
        Self {
            id: value.id.clone(),
            version: value.version,
            display_role: (&value.display_role).into(),
            title: value.title.clone(),
            default_expected_duration_ms: value.default_expected_duration_ms,
            interaction_required: value.interaction_required,
            optional: value.optional,
            binding: (&value.binding).into(),
        }
    }
}

impl From<&camera_config::ConnectionActivityDisplayRole> for ConnectionActivityDisplayRole {
    fn from(value: &camera_config::ConnectionActivityDisplayRole) -> Self {
        use camera_config::ConnectionActivityDisplayRole as Source;
        match value {
            Source::Connecting => Self::Connecting,
            Source::WaitingForCamera => Self::WaitingForCamera,
            Source::ConfirmingPairing => Self::ConfirmingPairing,
            Source::PreparingConnection => Self::PreparingConnection,
            Source::StartingNetwork => Self::StartingNetwork,
            Source::JoiningNetwork => Self::JoiningNetwork,
            Source::OpeningSession => Self::OpeningSession,
            Source::Unknown(raw) => Self::Unknown { raw: raw.clone() },
        }
    }
}

impl From<&camera_config::ConnectionActivityBinding> for ConnectionActivityBinding {
    fn from(value: &camera_config::ConnectionActivityBinding) -> Self {
        match value {
            camera_config::ConnectionActivityBinding::ExecutorSpan(binding) => Self::ExecutorSpan {
                sequence: match binding.executor_span.sequence {
                    camera_config::ConnectionActivitySequence::Steps => {
                        ConnectionActivitySequence::Steps
                    }
                    camera_config::ConnectionActivitySequence::PostExitReadiness => {
                        ConnectionActivitySequence::PostExitReadiness
                    }
                },
                start_step: binding.executor_span.start_step,
                end_step_exclusive: binding.executor_span.end_step_exclusive,
            },
            camera_config::ConnectionActivityBinding::HostCheckpoint(binding) => {
                Self::HostCheckpoint {
                    name: binding.host_checkpoint.name.clone(),
                }
            }
            camera_config::ConnectionActivityBinding::HostEstablishment(binding) => {
                Self::HostEstablishment {
                    action: match &binding.host_establishment {
                        camera_config::ConnectionActivityHostEstablishment::NetworkIdentityExact {
                            network_identity_exact,
                        } => HostEstablishment::NetworkIdentityExact {
                            expected_scope: network_identity_exact.expected_scope.clone(),
                        },
                        camera_config::ConnectionActivityHostEstablishment::RetainedSessionOpen {
                            retained_session_open,
                        } => HostEstablishment::RetainedSessionOpen {
                            socket_role: retained_session_open.socket_role.into(),
                        },
                    },
                }
            }
        }
    }
}

/// Common per-step options (§11.6). The dispatcher's retry loop wraps every
/// verb body uniformly — adding a verb in P2 doesn't change option handling.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct StepOptions {
    pub tolerant: bool,
    pub retries: u32,
    pub retry_delay_ms: u32,
    pub confirms: Option<StepConfirmation>,
}

/// Closed confirmation marker vocabulary mirrored from camera-config §11.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StepConfirmation {
    Registration,
}

/// The establishment step verbs (plan §3.3 + §11, USB per §11.29).
/// Externally inlined so each variant is a flat record at the uniffi layer.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum Step {
    BleConnect {
        opts: StepOptions,
    },
    /// Wait for the connected peripheral to drop the link. Remote-boot
    /// plans use the peer disconnect as the transition into a fresh scan.
    BleAwaitDisconnect {
        timeout_ms: u32,
        opts: StepOptions,
    },
    /// Request an ATT MTU before GATT traffic. `requested_mtu` is the
    /// reference app's request target; a platform without a request API
    /// (CoreBluetooth) makes no call. The step compares the negotiated MTU
    /// against a declared `minimum_mtu` on every platform and fails below
    /// it; with no floor, any negotiated MTU succeeds.
    BleRequestMtu {
        requested_mtu: u16,
        minimum_mtu: Option<u16>,
        opts: StepOptions,
    },
    /// Explicit GATT service-discovery checkpoint. On auto-discovering
    /// stacks, complete when discovery has completed — don't re-trigger.
    BleDiscoverServices {
        opts: StepOptions,
    },
    BleWrite {
        gatt: String,
        value: StepValue,
        /// Subscribed characteristic whose buffered prefix is atomically
        /// fenced immediately before the write, when declared.
        notification_fence: Option<String>,
        opts: StepOptions,
    },
    BleRead {
        gatt: String,
        encoding: String,
        capture_as: String,
        /// Applied to the wire bytes BEFORE `encoding` decode (§11.13).
        transform: Vec<Transform>,
        opts: StepOptions,
    },
    /// The connected peripheral's platform name (§11.4b): `CBPeripheral.name`
    /// on CoreBluetooth, where the GAP service is filtered from discovery and
    /// a GATT 0x2A00 read cannot succeed; a GATT read on stacks that expose
    /// it. Binds a UTF-8 string with any NUL terminator removed; an
    /// unavailable name fails the step rather than binding an empty string.
    BlePeripheralName {
        capture_as: String,
        opts: StepOptions,
    },
    /// CCCD-enable only. Success on descriptor-write ack — no notification
    /// payload is waited for. Use for pair-finalization rounds where the
    /// camera advances on the CCCD write itself.
    BleSubscribe {
        gatt: String,
        timeout_ms: u32,
        /// Which CCCD value to write (§11.8) — notify or indicate.
        mode: CccdMode,
        opts: StepOptions,
    },
    /// CCCD-enable AND wait for a matching notification payload. Use when
    /// the plan needs to capture or gate on a specific notification value.
    BleNotify {
        gatt: String,
        until: BleNotifyUntil,
        /// Whole matching payload → scope (kept alongside field captures).
        capture_as: Option<String>,
        /// Field captures: window → transform chain → encoding → scope.
        /// A failing capture is skipped, it does not fail the step.
        capture: Vec<NotifyCapture>,
        /// CCCD value the subscribe phase writes — notify or indicate.
        mode: CccdMode,
        timeout_ms: u32,
        opts: StepOptions,
    },
    /// Observe a characteristic until `until` holds, optionally acting each
    /// unsatisfied iteration (§11.15). The dispatcher loops: observe (poll a
    /// `read` source or await the `notify` stream) → apply captures into
    /// scope → if `until` holds, done; else run `on_each` and observe again,
    /// up to `timeout_ms`. Reference semantics:
    /// `camera_sim::ble::run_await_until`.
    BleAwaitUntil {
        source: AwaitSource,
        /// Field captures applied to each observed value before `until`
        /// (window → transform → encoding → scope). Fail-soft.
        capture: Vec<NotifyCapture>,
        /// Whole observed value → scope (hex) each iteration.
        capture_as: Option<String>,
        /// Satisfied when this predicate holds over scope.
        until: Predicate,
        /// Optional terminal rejection evaluated after `until`. Invalid with
        /// a seeded notify source; use an explicit pre-command read instead.
        fail_when: Option<Predicate>,
        /// Additional evidence required before `fail_when` becomes terminal.
        failure_evidence: Option<BleAwaitFailureEvidence>,
        /// Steps run each iteration `until` is not yet met, before the next
        /// observe. `Vec<Step>` (may be empty for a pure poll).
        on_each: Vec<Step>,
        timeout_ms: u32,
        /// Poll cadence for a `read` source (ms); ignored for `notify`.
        interval_ms: u32,
        opts: StepOptions,
    },
    /// Frame + write ONE window of a host blob, selected by a captured chunk
    /// index (#112). Run from a `bleAwaitUntil` `on_each` driven by the camera's
    /// `fileTransactionState` notifications: each announces the next index, this
    /// frames `source[window]` with the declared header and writes it to `gatt`.
    /// The dispatcher owns the slice math + frame assembly. Reference semantics:
    /// `camera_sim::ble::run_write_chunk`.
    BleWriteChunk {
        /// Runtime slot holding the whole blob as a bytes-raw hex string.
        source: String,
        /// Scope key holding the current chunk index (a bleAwaitUntil capture).
        index: String,
        /// Window size in bytes (the final window is the remainder).
        size: u32,
        gatt: String,
        /// Declared frame header, emitted before the window payload.
        frame: Vec<ChunkFrameField>,
        /// Index the final (remainder) window carries (Fuji `0xffff`).
        sentinel_index: u32,
        opts: StepOptions,
    },
    Acquire {
        name: String,
        /// `Vec<Step>` of length 1 holding the inner step. uniffi 0.31 does
        /// not implement `Lift<UniFfiTag>` for `Box<T>` where `T` is a
        /// recursive uniffi::Enum (only `Arc<T>` is supported, and `Arc`
        /// would be semantically wrong here — Acquire owns its child step,
        /// it doesn't share it). The Vec wrapper is a uniffi-level
        /// workaround; the dispatcher's length-1 invariant is documented
        /// in the iOS implementation notes.
        from: Vec<Step>,
        opts: StepOptions,
    },
    AcquireFirmware {
        from: AcquireSource,
        opts: StepOptions,
    },
    If {
        condition: Predicate,
        then_branch: Vec<Step>,
        else_branch: Vec<Step>,
        /// §11.6: when true, an unbound predicate field evaluates as false
        /// (else-branch runs / step is skipped) rather than erroring.
        tolerant: bool,
    },
    /// Run a body repeatedly only when a selected typed failure is followed
    /// by a matching scope predicate.
    Retry {
        steps: Vec<Step>,
        when_failure: ExecutorStepFailureKind,
        on_failure: Vec<Step>,
        retry_when: Predicate,
        max_attempts: u32,
        retry_delay_ms: u32,
        failure_context: Vec<String>,
    },
    /// Manifest-authored inter-operation delay, in milliseconds. New exported
    /// variants stay appended so existing generated bindings keep their enum
    /// discriminants.
    BleDelay {
        duration_ms: u32,
        opts: StepOptions,
    },
    /// Finite Nikon LSS authentication primitive. Cipher state remains inside
    /// the Rust executor and is never surfaced through UniFFI or scope.
    NikonLssAuthenticate {
        gatt: String,
        client_device_id: StepValue,
        nonce: StepValue,
        timeout_ms: u32,
        opts: StepOptions,
    },
    /// Read/decrypt the fixed LSS Wi-Fi connection-configuration fields.
    NikonLssReadConnectionConfiguration {
        gatt: String,
        flags_capture_as: String,
        ssid_capture_as: String,
        password_capture_as: String,
        security_mode_capture_as: String,
        spp_max_length_capture_as: Option<String>,
        opts: StepOptions,
    },
    /// `usbClaim` (§11.29) with the symbolic interface name already resolved
    /// to its class/subclass/protocol triple against the family
    /// `usb.interfaces` map: the transport claims the interface matching all
    /// three bytes.
    UsbClaim {
        class: u8,
        subclass: u8,
        protocol: u8,
        opts: StepOptions,
    },
    /// `usbBulkOut` (§11.29): resolve `data` per §11.1 and write the bytes to
    /// the bulk OUT endpoint.
    UsbBulkOut {
        data: StepValue,
        opts: StepOptions,
    },
    /// `usbBulkIn` (§11.29): read up to `max_length` bytes from the bulk IN
    /// endpoint, run the §11.13 capture pipeline, and bind the result under
    /// `capture_as`.
    UsbBulkIn {
        max_length: u32,
        encoding: String,
        capture_as: String,
        /// Applied to the wire bytes BEFORE `encoding` decode (§11.13).
        transform: Vec<Transform>,
        opts: StepOptions,
    },
    /// `usbAwaitInterrupt` (§11.29): await one interrupt IN event frame and
    /// capture it with the `usbBulkIn` pipeline.
    UsbAwaitInterrupt {
        encoding: String,
        capture_as: String,
        /// Applied to the frame bytes BEFORE `encoding` decode (§11.13).
        transform: Vec<Transform>,
        /// The wait's wall-clock budget; `None` applies the executor's
        /// single-call backstop.
        timeout_ms: Option<u32>,
        opts: StepOptions,
    },
}

/// One field of a declared `bleWriteChunk` frame header (#112): a computed
/// quantity (`field`) emitted at the wire width/order `encoding` (token).
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChunkFrameField {
    pub field: ChunkField,
    pub encoding: String,
}

/// The computed quantities a [`ChunkFrameField`] can emit. Mirrors `ix::ChunkField`.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ChunkField {
    Index,
    Length,
}

impl From<ix::ChunkField> for ChunkField {
    fn from(f: ix::ChunkField) -> Self {
        match f {
            ix::ChunkField::Index => ChunkField::Index,
            ix::ChunkField::Length => ChunkField::Length,
        }
    }
}

/// Step value forms (§11.1). At the FFI boundary:
/// * `Literal { bytes }` — the loader decoded the YAML hex string to bytes.
/// * `Template { value, transform }` — interpolate `{name}` against scope at
///   walk time, then apply the [`Transform`] chain in order.
/// * `Runtime { slot, encoding?, transform }` — app supplies before walk.
/// * `Captured { name, transform }` — earlier step / recognize-seed named
///   this slot; the RED `F557D96B` echo write uses
///   `Captured { name: "idNumber", transform: [BitOr(0x20000000)] }`.
///
/// An empty `transform` vec means no transform.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum StepValue {
    Literal {
        bytes: Vec<u8>,
    },
    Template {
        value: String,
        transform: Vec<Transform>,
    },
    Runtime {
        slot: String,
        encoding: Option<String>,
        transform: Vec<Transform>,
    },
    Captured {
        name: String,
        transform: Vec<Transform>,
    },
}

/// Closed byte→byte transform vocabulary (§11.13). The dispatcher applies
/// the chain in order between resolving bytes and using them (write value)
/// or before `encoding`-decoding them (read/notify captures). Semantics are
/// specified by `camera_config::index::eval::apply_transforms` — implement
/// the dispatcher side to match its unit tests. Chain failure (out-of-range
/// slice, integer op on > 8 bytes, wrong width for `uuidFromBytes`) counts
/// as step failure under §11.6.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum Transform {
    /// Input ≤ 8 bytes LE; re-emit at input width.
    BitOr {
        operand: u64,
    },
    /// Input ≤ 8 bytes LE; re-emit at input width.
    BitAnd {
        operand: u64,
    },
    /// Window `[at, at+length)`; `length` absent = to end.
    Slice {
        at: u64,
        length: Option<u64>,
    },
    /// Same as `Slice { at: count, length: None }`.
    DropPrefix {
        count: u64,
    },
    ReverseBytes,
    /// Exactly 16 bytes → 36 ASCII bytes of the canonical uppercase UUID.
    UuidFromBytes,
    /// Input ≤ 8 bytes LE: `(value & mask) >> shift`, re-emit at input width.
    Bits {
        mask: u64,
        shift: u32,
    },
    /// Append one zero byte. Kept at the end to preserve existing UniFFI enum
    /// discriminants for generated clients.
    AppendNul,
    /// Extend to exactly `length` bytes with `byte`; longer input fails.
    PadRight {
        length: u64,
        byte: u8,
    },
}

/// Where an `acquire` / `acquireFirmware` step pulls its value from.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum AcquireSource {
    BleAdvert {
        offset: u32,
        length: u32,
        encoding: String,
    },
    BleRead {
        gatt: String,
        encoding: String,
    },
    UserPrompt {
        text: String,
    },
}

/// Where `bleAwaitUntil` observes (§11.15): poll a readable characteristic,
/// or consume a characteristic's notification stream, optionally preceded by
/// one seed read after the notification accept path is armed. Seeded notify
/// sources cannot declare rejection predicates because callback transports may
/// deliver read responses and notifications through the same callback.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum AwaitSource {
    Read {
        gatt: String,
    },
    Notify {
        gatt: String,
        mode: CccdMode,
        seed_read: bool,
    },
}

/// Probe and confirmation predicate for a potential `bleAwaitUntil` failure.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BleAwaitFailureEvidence {
    pub steps: Vec<Step>,
    pub when: Predicate,
}

impl TryFrom<&ix::BleAwaitFailureEvidence> for BleAwaitFailureEvidence {
    type Error = crate::ConfigError;

    fn try_from(value: &ix::BleAwaitFailureEvidence) -> Result<Self, Self::Error> {
        Ok(Self {
            steps: value
                .steps
                .iter()
                .map(Step::try_from)
                .collect::<Result<_, _>>()?,
            when: (&value.when).into(),
        })
    }
}

/// CCCD subscription mode (§11.8): `ENABLE_NOTIFICATION_VALUE` vs
/// `ENABLE_INDICATION_VALUE` in Android terms; on iOS both map to
/// `setNotifyValue(true)` (CoreBluetooth picks per the characteristic's
/// properties) — the mode is still carried so non-CoreBluetooth
/// dispatchers write the right descriptor value.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum CccdMode {
    Notify,
    Indicate,
}

/// One field capture from a notification payload (§11.13 capture pipeline:
/// window `[at, at+length)` → transform chain → `encoding` decode → bind to
/// scope under `name`). `length` absent = to end.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NotifyCapture {
    pub at: u64,
    pub length: Option<u64>,
    pub transform: Vec<Transform>,
    pub encoding: String,
    pub name: String,
}

/// `bleNotify` acceptance condition (§11.8). The Equals variant carries the
/// decoded payload bytes — the loader applied `encoding:` if it was present
/// in YAML.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum BleNotifyUntil {
    Any,
    Equals { value: Vec<u8> },
    Matches { pattern: String },
}

/// `if:` predicate (§3.3). `value` is always stringified — runtime_scope
/// carries strings (§11.2 encoding rules govern how bytes/ints round-trip).
#[derive(Debug, Clone, uniffi::Record)]
pub struct Predicate {
    pub field: String,
    pub op: PredicateOp,
    pub value: String,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum PredicateOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
}

// ---------------------------------------------------------------------------
// Camera-config index types → FFI types (conversion adapters)
// ---------------------------------------------------------------------------

impl From<&ix::StepOptions> for StepOptions {
    fn from(o: &ix::StepOptions) -> Self {
        StepOptions {
            tolerant: o.tolerant,
            retries: o.retries,
            retry_delay_ms: o.retry_delay_ms,
            confirms: o.confirms.map(Into::into),
        }
    }
}

impl From<ix::StepConfirmation> for StepConfirmation {
    fn from(value: ix::StepConfirmation) -> Self {
        match value {
            ix::StepConfirmation::Registration => Self::Registration,
        }
    }
}

impl From<ix::PredicateOp> for PredicateOp {
    fn from(op: ix::PredicateOp) -> Self {
        match op {
            ix::PredicateOp::Eq => PredicateOp::Eq,
            ix::PredicateOp::Ne => PredicateOp::Ne,
            ix::PredicateOp::Gt => PredicateOp::Gt,
            ix::PredicateOp::Gte => PredicateOp::Gte,
            ix::PredicateOp::Lt => PredicateOp::Lt,
            ix::PredicateOp::Lte => PredicateOp::Lte,
            ix::PredicateOp::In => PredicateOp::In,
        }
    }
}

impl From<&ix::Predicate> for Predicate {
    fn from(p: &ix::Predicate) -> Self {
        Predicate {
            field: p.field.clone(),
            op: p.op.into(),
            value: p.value.clone(),
        }
    }
}

impl From<&ix::Transform> for Transform {
    fn from(t: &ix::Transform) -> Self {
        match t {
            ix::Transform::BitOr(operand) => Transform::BitOr { operand: *operand },
            ix::Transform::BitAnd(operand) => Transform::BitAnd { operand: *operand },
            ix::Transform::Slice { at, length } => Transform::Slice {
                at: *at as u64,
                length: length.map(|l| l as u64),
            },
            ix::Transform::DropPrefix(count) => Transform::DropPrefix {
                count: *count as u64,
            },
            ix::Transform::ReverseBytes => Transform::ReverseBytes,
            ix::Transform::AppendNul => Transform::AppendNul,
            ix::Transform::UuidFromBytes => Transform::UuidFromBytes,
            ix::Transform::Bits { mask, shift } => Transform::Bits {
                mask: *mask,
                shift: *shift,
            },
            ix::Transform::PadRight { length, byte } => Transform::PadRight {
                length: *length as u64,
                byte: *byte,
            },
        }
    }
}

fn transforms(chain: &[ix::Transform]) -> Vec<Transform> {
    chain.iter().map(Into::into).collect()
}

impl From<ix::CccdMode> for CccdMode {
    fn from(m: ix::CccdMode) -> Self {
        match m {
            ix::CccdMode::Notify => CccdMode::Notify,
            ix::CccdMode::Indicate => CccdMode::Indicate,
        }
    }
}

impl From<&ix::NotifyCapture> for NotifyCapture {
    fn from(c: &ix::NotifyCapture) -> Self {
        NotifyCapture {
            at: c.at as u64,
            length: c.length.map(|l| l as u64),
            transform: transforms(&c.transform),
            encoding: c.encoding.as_token().to_string(),
            name: c.name.clone(),
        }
    }
}

impl TryFrom<&ix::StepValue> for StepValue {
    type Error = crate::ConfigError;

    fn try_from(v: &ix::StepValue) -> Result<Self, Self::Error> {
        Ok(match v {
            ix::StepValue::Literal { literal } => StepValue::Literal {
                bytes: ix::eval::yaml_literal_to_bytes(literal, None).ok_or_else(|| {
                    crate::ConfigError::Contract(format!(
                        "step literal {literal:?} does not encode to wire bytes"
                    ))
                })?,
            },
            ix::StepValue::Template {
                template,
                transform,
            } => StepValue::Template {
                value: template.clone(),
                transform: transforms(transform),
            },
            ix::StepValue::Runtime {
                runtime,
                encoding,
                transform,
            } => StepValue::Runtime {
                slot: runtime.clone(),
                encoding: encoding.map(|e| e.as_token().to_string()),
                transform: transforms(transform),
            },
            ix::StepValue::Captured {
                captured,
                transform,
            } => StepValue::Captured {
                name: captured.clone(),
                transform: transforms(transform),
            },
        })
    }
}

impl From<&ix::AcquireSource> for AcquireSource {
    fn from(s: &ix::AcquireSource) -> Self {
        match s {
            ix::AcquireSource::BleAdvert {
                offset,
                length,
                encoding,
            } => AcquireSource::BleAdvert {
                offset: *offset,
                length: *length,
                encoding: encoding.as_token().to_string(),
            },
            ix::AcquireSource::BleRead { gatt, encoding } => AcquireSource::BleRead {
                gatt: gatt.clone(),
                encoding: encoding.as_token().to_string(),
            },
            ix::AcquireSource::UserPrompt { text } => {
                AcquireSource::UserPrompt { text: text.clone() }
            }
        }
    }
}

impl TryFrom<&ix::BleNotifyUntil> for BleNotifyUntil {
    type Error = crate::ConfigError;

    fn try_from(u: &ix::BleNotifyUntil) -> Result<Self, Self::Error> {
        Ok(match u {
            ix::BleNotifyUntil::Any => BleNotifyUntil::Any,
            ix::BleNotifyUntil::Equals { value, encoding } => BleNotifyUntil::Equals {
                value: ix::eval::yaml_literal_to_bytes(value, *encoding).ok_or_else(|| {
                    crate::ConfigError::Contract(format!(
                        "BLE notify equals value {value:?} with encoding {encoding:?} does not encode to wire bytes"
                    ))
                })?,
            },
            ix::BleNotifyUntil::Matches { pattern } => BleNotifyUntil::Matches {
                pattern: pattern.clone(),
            },
        })
    }
}

impl From<&ix::AwaitSource> for AwaitSource {
    fn from(s: &ix::AwaitSource) -> Self {
        match s {
            ix::AwaitSource::Read { gatt } => AwaitSource::Read { gatt: gatt.clone() },
            ix::AwaitSource::Notify {
                gatt,
                mode,
                seed_read,
            } => AwaitSource::Notify {
                gatt: gatt.clone(),
                mode: (*mode).into(),
                seed_read: *seed_read,
            },
        }
    }
}

impl From<ix::RetryFailureKind> for ExecutorStepFailureKind {
    fn from(kind: ix::RetryFailureKind) -> Self {
        match kind {
            ix::RetryFailureKind::DeadlineExceeded => Self::DeadlineExceeded,
            ix::RetryFailureKind::ConditionRejected => Self::ConditionRejected,
            ix::RetryFailureKind::Other => Self::Other,
        }
    }
}

impl TryFrom<&ix::Step> for Step {
    type Error = crate::ConfigError;

    fn try_from(s: &ix::Step) -> Result<Self, Self::Error> {
        step_from_ix(s, None)
    }
}

/// USB verbs carry family context: one surfacing without the family
/// `usb.interfaces` map escaped the loader's plan scoping (§11.29).
fn usb_outside_family_block() -> crate::ConfigError {
    crate::ConfigError::Schema(
        "USB establishment verbs are only valid inside family usb establishments".into(),
    )
}

/// Mirror one index step into its FFI record. `usb_interfaces` is the family
/// `usb.interfaces` map (§11.29) when converting a raw USB establishment
/// plan: `usbClaim` names resolve to their triple against it, mirroring how
/// the index build resolves §11.3 GATT names before the FFI sees them. BLE
/// plans, BLE actions, and refined tails convert with `None`; USB verbs
/// cannot load there.
fn step_from_ix(
    s: &ix::Step,
    usb_interfaces: Option<&BTreeMap<String, ix::UsbInterfaceTriple>>,
) -> Result<Step, crate::ConfigError> {
    Ok(match s {
        ix::Step::BleConnect(inner) => Step::BleConnect {
            opts: (&inner.opts).into(),
        },
        ix::Step::BleDelay(inner) => Step::BleDelay {
            duration_ms: inner.duration_ms,
            opts: (&inner.opts).into(),
        },
        ix::Step::BleAwaitDisconnect(inner) => Step::BleAwaitDisconnect {
            timeout_ms: inner.timeout_ms,
            opts: (&inner.opts).into(),
        },
        ix::Step::BleRequestMtu(inner) => Step::BleRequestMtu {
            requested_mtu: inner.requested_mtu,
            minimum_mtu: inner.minimum_mtu,
            opts: (&inner.opts).into(),
        },
        ix::Step::BleDiscoverServices(inner) => Step::BleDiscoverServices {
            opts: (&inner.opts).into(),
        },
        ix::Step::BleRead(inner) => Step::BleRead {
            gatt: inner.gatt.clone(),
            encoding: inner.encoding.as_token().to_string(),
            capture_as: inner.capture_as.clone(),
            transform: transforms(&inner.transform),
            opts: (&inner.opts).into(),
        },
        ix::Step::BlePeripheralName(inner) => Step::BlePeripheralName {
            capture_as: inner.capture_as.clone(),
            opts: (&inner.opts).into(),
        },
        ix::Step::BleWrite(inner) => Step::BleWrite {
            gatt: inner.gatt.clone(),
            value: StepValue::try_from(&inner.value)?,
            notification_fence: inner.notification_fence.clone(),
            opts: (&inner.opts).into(),
        },
        ix::Step::BleSubscribe(inner) => Step::BleSubscribe {
            gatt: inner.gatt.clone(),
            timeout_ms: inner.timeout_ms,
            mode: inner.mode.into(),
            opts: (&inner.opts).into(),
        },
        ix::Step::BleNotify(inner) => Step::BleNotify {
            gatt: inner.gatt.clone(),
            until: BleNotifyUntil::try_from(&inner.until)?,
            capture_as: inner.capture_as.clone(),
            capture: inner.capture.iter().map(Into::into).collect(),
            mode: inner.mode.into(),
            timeout_ms: inner.timeout_ms,
            opts: (&inner.opts).into(),
        },
        ix::Step::BleAwaitUntil(inner) => Step::BleAwaitUntil {
            source: (&inner.source).into(),
            capture: inner.capture.iter().map(Into::into).collect(),
            capture_as: inner.capture_as.clone(),
            until: (&inner.until).into(),
            fail_when: inner.fail_when.as_ref().map(Into::into),
            failure_evidence: inner
                .failure_evidence
                .as_ref()
                .map(BleAwaitFailureEvidence::try_from)
                .transpose()?,
            on_each: inner
                .on_each
                .iter()
                .map(|step| step_from_ix(step, usb_interfaces))
                .collect::<Result<_, _>>()?,
            timeout_ms: inner.timeout_ms,
            interval_ms: inner.interval_ms,
            opts: (&inner.opts).into(),
        },
        ix::Step::BleWriteChunk(inner) => Step::BleWriteChunk {
            source: inner.source.clone(),
            index: inner.index.clone(),
            size: inner.size,
            gatt: inner.gatt.clone(),
            frame: inner
                .frame
                .iter()
                .map(|f| ChunkFrameField {
                    field: f.field.into(),
                    encoding: f.encoding.as_token().to_string(),
                })
                .collect(),
            sentinel_index: inner.sentinel_index,
            opts: (&inner.opts).into(),
        },
        ix::Step::Acquire(inner) => Step::Acquire {
            name: inner.name.clone(),
            from: vec![step_from_ix(&inner.from, usb_interfaces)?],
            opts: (&inner.opts).into(),
        },
        ix::Step::AcquireFirmware(inner) => Step::AcquireFirmware {
            from: (&inner.from).into(),
            opts: (&inner.opts).into(),
        },
        ix::Step::If(inner) => Step::If {
            condition: (&inner.condition).into(),
            then_branch: inner
                .then
                .iter()
                .map(|step| step_from_ix(step, usb_interfaces))
                .collect::<Result<_, _>>()?,
            else_branch: inner
                .else_branch
                .iter()
                .map(|step| step_from_ix(step, usb_interfaces))
                .collect::<Result<_, _>>()?,
            tolerant: inner.tolerant,
        },
        ix::Step::Retry(inner) => Step::Retry {
            steps: inner
                .steps
                .iter()
                .map(|step| step_from_ix(step, usb_interfaces))
                .collect::<Result<_, _>>()?,
            when_failure: inner.when_failure.into(),
            on_failure: inner
                .on_failure
                .iter()
                .map(|step| step_from_ix(step, usb_interfaces))
                .collect::<Result<_, _>>()?,
            retry_when: (&inner.retry_when).into(),
            max_attempts: inner.max_attempts,
            retry_delay_ms: inner.retry_delay_ms,
            failure_context: inner.failure_context.clone(),
        },
        ix::Step::NikonLssAuthenticate(inner) => Step::NikonLssAuthenticate {
            gatt: inner.gatt.clone(),
            client_device_id: StepValue::try_from(&inner.client_device_id)?,
            nonce: StepValue::try_from(&inner.nonce)?,
            timeout_ms: inner.timeout_ms,
            opts: (&inner.opts).into(),
        },
        ix::Step::NikonLssReadConnectionConfiguration(inner) => {
            Step::NikonLssReadConnectionConfiguration {
                gatt: inner.gatt.clone(),
                flags_capture_as: inner.flags_capture_as.clone(),
                ssid_capture_as: inner.ssid_capture_as.clone(),
                password_capture_as: inner.password_capture_as.clone(),
                security_mode_capture_as: inner.security_mode_capture_as.clone(),
                spp_max_length_capture_as: inner.spp_max_length_capture_as.clone(),
                opts: (&inner.opts).into(),
            }
        }
        ix::Step::UsbClaim(inner) => {
            let interfaces = usb_interfaces.ok_or_else(usb_outside_family_block)?;
            let triple = interfaces.get(&inner.interface).ok_or_else(|| {
                crate::ConfigError::Contract(format!(
                    "usbClaim interface '{}' is not declared in the family usb.interfaces map",
                    inner.interface
                ))
            })?;
            Step::UsbClaim {
                class: triple.class,
                subclass: triple.subclass,
                protocol: triple.protocol,
                opts: (&inner.opts).into(),
            }
        }
        ix::Step::UsbBulkOut(inner) => {
            let _ = usb_interfaces.ok_or_else(usb_outside_family_block)?;
            Step::UsbBulkOut {
                data: StepValue::try_from(&inner.data)?,
                opts: (&inner.opts).into(),
            }
        }
        ix::Step::UsbBulkIn(inner) => {
            let _ = usb_interfaces.ok_or_else(usb_outside_family_block)?;
            Step::UsbBulkIn {
                max_length: inner.max_length,
                encoding: inner.encoding.as_token().to_string(),
                capture_as: inner.capture_as.clone(),
                transform: transforms(&inner.transform),
                opts: (&inner.opts).into(),
            }
        }
        ix::Step::UsbAwaitInterrupt(inner) => {
            let _ = usb_interfaces.ok_or_else(usb_outside_family_block)?;
            Step::UsbAwaitInterrupt {
                encoding: inner.encoding.as_token().to_string(),
                capture_as: inner.capture_as.clone(),
                transform: transforms(&inner.transform),
                timeout_ms: inner.timeout_ms,
                opts: (&inner.opts).into(),
            }
        }
    })
}

// ---------------------------------------------------------------------------
// recognize() — observation → decision
// ---------------------------------------------------------------------------

/// Match a [`ScanObservation::BleAdvert`] (converted to
/// [`ix::eval::BleAdvertFacts`]) against every (model, signature) pair in
/// the resolved index, in file-declaration order (§11.7).
///
/// For the MVP the family fact "all Fuji adverts" never disambiguates by
/// model (the GFX100 II is the only declared model). When P2 adds more
/// models with overlapping signatures, this surfaces `Disambiguate` with
/// scope facts common to all matches.
pub fn recognize_ble(
    index: &ix::ResolvedManufacturerIndex,
    facts: &ix::eval::BleAdvertFacts,
) -> Recognition {
    // Walk models in declaration order; per model walk signatures in file
    // order. The MVP returns the FIRST matching signature; multi-model
    // disambiguation gets added when a second body matches the same family
    // signature.
    let mut matches: Vec<(String, String, &ix::BleAdvertSignature, bool)> = Vec::new();
    for model in &index.models {
        for (_sig_name, sig) in &model.signatures {
            let ix::Signature::BleAdvert(ble_sig) = sig else {
                continue;
            };
            if !ix::eval::advert_matches(ble_sig, facts) {
                continue;
            }
            if ble_sig.discoverable {
                matches.push((
                    model.id.clone(),
                    model.display_name.clone(),
                    ble_sig,
                    model.fallback,
                ));
            }
            // §11.7: first matching signature for THIS model wins; do not
            // fall through a reconnect-only state into a broader discovery
            // signature for the same model.
            break;
        }
    }

    // Closest-match ranking (#311). fuji-generic is the FAMILY BASELINE: every
    // Fuji body advertises and connects the same way, so matching any Fuji
    // advert already means we know how to connect. A specific model is a
    // refinement of that baseline and wins by being more specific — the same
    // "most specific ... wins" principle (plan §11.18) already
    // applies to value-profile selection, extended here to model matching.
    //
    // Mechanically: drop every baseline (`fallback`) match whenever a
    // more-specific match is present, BEFORE the count decision — otherwise a
    // co-matching baseline would demote a specific model's lone Candidate into
    // a Disambiguate and break the consumer's discovery path (the app acts only
    // on Candidate). A baseline-only match set is left untouched, so it behaves
    // exactly as before: one baseline → Candidate, several → Disambiguate.
    if matches.iter().any(|(_, _, _, fallback)| !fallback) {
        matches.retain(|(_, _, _, fallback)| !fallback);
    }

    match matches.len() {
        0 => Recognition::NoMatch,
        1 => {
            let (model_id, _display, sig, _fallback) = &matches[0];
            let runtime_scope = ix::eval::advert_scope(sig, facts)
                .into_iter()
                .map(|(key, value)| KeyValue { key, value })
                .collect();
            let runtime_scope_encodings = ix::eval::advert_capture_encodings(sig)
                .into_iter()
                .map(|(key, encoding)| KeyValue {
                    key,
                    value: encoding.as_token().to_string(),
                })
                .collect();
            Recognition::Candidate {
                model: model_id.clone(),
                connection: sig.suggests.connection.clone(),
                confidence: confidence_from(sig.suggests.confidence),
                runtime_scope,
                runtime_scope_encodings,
            }
        }
        _ => {
            // Multi-model match: surface scope facts true across all
            // candidates (intersection of literal scopes). Mfg-data
            // captures vary per model, so they're left out of the

            let intersection = intersect_scope(matches.iter().map(|(_, _, s, _)| *s));
            let runtime_scope = intersection
                .into_iter()
                .map(|(k, v)| KeyValue { key: k, value: v })
                .collect();
            // Family inference: for MVP the family id is hard-coded as
            // the manufacturer index's manufacturer name lowercased. P2
            // will surface this from the signature/model graph properly.
            let family = index.manufacturer.to_lowercase();
            let candidates = matches
                .iter()
                .map(|(id, display, _, _)| ModelMatch {
                    model: id.clone(),
                    display_name: display.clone(),
                    connection_hint: None,
                })
                .collect();
            Recognition::Disambiguate {
                family,
                candidates,
                runtime_scope,
                hint: None,
            }
        }
    }
}

/// Match a parsed PCSS callback against model signatures. Dynamic endpoint
/// values ride in runtime scope so auto-discovery can immediately converge on
/// the same known-address establishment entry point.
pub fn recognize_pcss(
    index: &ix::ResolvedManufacturerIndex,
    camera_ipv4: &str,
    camera_name: &str,
    command_port: u16,
    service: &str,
) -> Recognition {
    let mut matches = Vec::new();
    for model in &index.models {
        for (_name, signature) in &model.signatures {
            let ix::Signature::PcssNotify(signature) = signature else {
                continue;
            };
            if signature.require.camera_name == camera_name && signature.require.service == service
            {
                matches.push((model, signature));
                break;
            }
        }
    }
    let scope = || {
        vec![
            KeyValue {
                key: "cameraIpv4".into(),
                value: camera_ipv4.into(),
            },
            KeyValue {
                key: "cameraName".into(),
                value: camera_name.into(),
            },
            KeyValue {
                key: "commandPort".into(),
                value: command_port.to_string(),
            },
            KeyValue {
                key: "service".into(),
                value: service.into(),
            },
        ]
    };
    match matches.as_slice() {
        [] => Recognition::NoMatch,
        [(model, signature)] => Recognition::Candidate {
            model: model.id.clone(),
            connection: signature.suggests.connection.clone(),
            confidence: confidence_from(signature.suggests.confidence),
            runtime_scope: scope(),
            runtime_scope_encodings: Vec::new(),
        },
        many => Recognition::Disambiguate {
            family: index.manufacturer.to_lowercase(),
            candidates: many
                .iter()
                .map(|(model, signature)| ModelMatch {
                    model: model.id.clone(),
                    display_name: model.display_name.clone(),
                    connection_hint: Some(signature.suggests.connection.clone()),
                })
                .collect(),
            runtime_scope: scope(),
            hint: Some("Multiple models match this PCSS callback".into()),
        },
    }
}

/// Match a host USB attachment against per-connection manifest discovery data.
/// Connection availability and automatic-recognition platforms are separate:
/// a raw connection may remain queryable on a host whose attachment callback
/// is owned by a platform daemon and therefore selects pass-through.
pub fn recognize_usb_attachment(
    store: &cc::ConfigStore,
    platform: &str,
    vendor_id: u16,
    product_id: u16,
) -> Recognition {
    let Some(index) = store.index.as_ref() else {
        return Recognition::NoMatch;
    };
    let mut matches = Vec::new();
    for model in &index.models {
        let Some(model_store) = store.model_store(&model.id) else {
            continue;
        };
        let available: std::collections::BTreeSet<&str> =
            model_store.connections_available().into_iter().collect();
        for (connection_id, connection) in &model_store.manifest.connections {
            if !available.contains(connection_id.as_str())
                || !connection_platform_matches(connection, platform)
            {
                continue;
            }
            let Some(discovery) = connection.discovery.as_ref() else {
                continue;
            };
            if discovery.mechanism != "usb"
                || !discovery.auto_discoverable
                || (!discovery.platforms.is_empty()
                    && !discovery.platforms.iter().any(|token| token == platform))
                || discovery.vid != Some(vendor_id)
                || discovery.pid.is_some_and(|pid| pid != product_id)
            {
                continue;
            }
            matches.push((model, connection_id.clone(), discovery.pid.is_some()));
        }
    }

    let scope = || {
        vec![
            KeyValue {
                key: "usbVendorId".into(),
                value: vendor_id.to_string(),
            },
            KeyValue {
                key: "usbProductId".into(),
                value: product_id.to_string(),
            },
        ]
    };
    match matches.as_slice() {
        [] => Recognition::NoMatch,
        [(model, connection, product_specific)] => Recognition::Candidate {
            model: model.id.clone(),
            connection: connection.clone(),
            confidence: if *product_specific {
                Confidence::High
            } else {
                Confidence::Medium
            },
            runtime_scope: scope(),
            runtime_scope_encodings: Vec::new(),
        },
        many => Recognition::Disambiguate {
            family: index.manufacturer.to_lowercase(),
            candidates: many
                .iter()
                .map(|(model, connection, _)| ModelMatch {
                    model: model.id.clone(),
                    display_name: model.display_name.clone(),
                    connection_hint: Some(connection.clone()),
                })
                .collect(),
            runtime_scope: scope(),
            hint: Some("Multiple models match this USB attachment".into()),
        },
    }
}

fn connection_platform_matches(connection: &cc::Connection, platform: &str) -> bool {
    match connection.extra.get("platforms") {
        Some(serde_yaml::Value::Sequence(platforms)) => platforms
            .iter()
            .any(|candidate| candidate.as_str() == Some(platform)),
        _ => true,
    }
}

pub fn reconnect_policy(
    index: &ix::ResolvedManufacturerIndex,
    model: &str,
) -> Option<ReconnectPolicy> {
    let model = index
        .models
        .iter()
        .find(|candidate| candidate.id == model)?;
    let policy = model.ble.as_ref()?.reconnect.as_ref()?;
    Some(ReconnectPolicy {
        scan_timeout_ms: policy.scan_timeout_ms,
    })
}

pub fn reconnect_decision(
    index: &ix::ResolvedManufacturerIndex,
    model: &str,
    facts: &ix::eval::BleAdvertFacts,
    persisted_scope: &[KeyValue],
) -> ReconnectDecision {
    let Some(model_view) = index.models.iter().find(|candidate| candidate.id == model) else {
        return ReconnectDecision::NoMatch;
    };
    let persisted: std::collections::BTreeMap<&str, &str> = persisted_scope
        .iter()
        .map(|kv| (kv.key.as_str(), kv.value.as_str()))
        .collect();
    for (_name, signature) in &model_view.signatures {
        let ix::Signature::BleAdvert(signature) = signature else {
            continue;
        };
        let Some(route) = &signature.reconnect else {
            continue;
        };
        if !ix::eval::advert_matches(signature, facts) {
            continue;
        }
        let scope = ix::eval::advert_scope(signature, facts);
        let observed: std::collections::BTreeMap<&str, &str> = scope
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        if route.identity.is_empty()
            || !route.identity.iter().all(|key| {
                observed.get(key.as_str()).copied() == persisted.get(key.as_str()).copied()
            })
        {
            continue;
        }
        let Some(plan) = build_establishment_mechanism(
            index,
            model,
            &route.mechanism,
            &route.mechanism,
            None,
            persisted_scope,
        ) else {
            return ReconnectDecision::NoMatch;
        };
        let runtime_scope = scope
            .into_iter()
            .map(|(key, value)| KeyValue { key, value })
            .collect();
        return match route.disposition {
            ix::ReconnectDisposition::Wake => ReconnectDecision::Wake {
                plan,
                runtime_scope,
            },
            ix::ReconnectDisposition::Ready => ReconnectDecision::Ready {
                plan,
                runtime_scope,
            },
        };
    }
    ReconnectDecision::NoMatch
}

fn intersect_scope<'a>(
    sigs: impl Iterator<Item = &'a ix::BleAdvertSignature>,
) -> Vec<(String, String)> {
    let mut maps: Vec<&std::collections::BTreeMap<String, String>> =
        sigs.map(|s| &s.scope).collect();
    if maps.is_empty() {
        return Vec::new();
    }
    let first = maps.remove(0);
    let mut out = Vec::new();
    for (k, v) in first {
        if maps.iter().all(|m| m.get(k) == Some(v)) {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

fn confidence_from(c: ix::Confidence) -> Confidence {
    match c {
        ix::Confidence::High => Confidence::High,
        ix::Confidence::Medium => Confidence::Medium,
        ix::Confidence::Low => Confidence::Low,
    }
}

// ---------------------------------------------------------------------------
// establishment() — model + connection + initial_scope → plan
// ---------------------------------------------------------------------------

pub(crate) fn validate_ble_plan_mappings(
    index: &ix::ResolvedManufacturerIndex,
) -> Result<(), crate::ConfigError> {
    for model in &index.models {
        let Some(ble) = &model.ble else {
            continue;
        };
        for (mechanism, establishment) in &ble.establishments {
            for (sequence, steps) in [
                ("steps", establishment.steps.as_slice()),
                (
                    "postExitReadiness",
                    establishment.post_exit_readiness.as_slice(),
                ),
            ] {
                for (step_index, step) in steps.iter().enumerate() {
                    Step::try_from(step).map_err(|error| match error {
                        crate::ConfigError::Contract(message) => crate::ConfigError::Contract(
                            format!(
                                "model `{}` mechanism `{mechanism}` {sequence}[{step_index}]: {message}",
                                model.id
                            ),
                        ),
                        other => other,
                    })?;
                }
            }
        }
        for (action, definition) in &ble.actions {
            for (step_index, step) in definition.steps.iter().enumerate() {
                Step::try_from(step).map_err(|error| match error {
                    crate::ConfigError::Contract(message) => crate::ConfigError::Contract(format!(
                        "model `{}` action `{action}` steps[{step_index}]: {message}",
                        model.id
                    )),
                    other => other,
                })?;
            }
        }
    }
    Ok(())
}

/// USB mirror of [`validate_ble_plan_mappings`] (§11.29): every family USB
/// establishment plan must convert to its FFI mirror, which also resolves
/// every `usbClaim` interface name against the family `usb.interfaces` map.
pub(crate) fn validate_usb_plan_mappings(
    index: &ix::ResolvedManufacturerIndex,
) -> Result<(), crate::ConfigError> {
    for model in &index.models {
        let Some(usb) = &model.usb else {
            continue;
        };
        for (mechanism, establishment) in &usb.establishments {
            for (sequence, steps) in [
                ("steps", establishment.steps.as_slice()),
                (
                    "postExitReadiness",
                    establishment.post_exit_readiness.as_slice(),
                ),
            ] {
                for (step_index, step) in steps.iter().enumerate() {
                    step_from_ix(step, Some(&usb.interfaces)).map_err(|error| match error {
                        crate::ConfigError::Contract(message) => crate::ConfigError::Contract(
                            format!(
                                "model `{}` mechanism `{mechanism}` {sequence}[{step_index}]: {message}",
                                model.id
                            ),
                        ),
                        other => other,
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// Build the establishment plan registered under `mechanism` for `model`.
/// The caller resolves `mechanism` from the body manifest's
/// `connections[connection].establishment` and supplies that connection's
/// `kind`; the kind selects the family registry the plan comes from — the
/// family USB registry for a raw `usb` connection (§11.29), the BLE registry
/// otherwise. Returns `None` if the model lacks that family block or no plan
/// is registered under `mechanism`.
///
/// `initial_scope` is currently informational — the plan's steps don't
/// inline-resolve scope at this layer (the dispatcher does that mid-walk).
/// Per §11.1 *establishment-call phase*, the plan is returned with
/// structured `Captured` / `Runtime` / `Template` step values intact.
pub fn build_establishment(
    index: &ix::ResolvedManufacturerIndex,
    model: &str,
    connection: &str,
    mechanism: &str,
    connection_kind: Option<&str>,
    _initial_scope: &[KeyValue],
) -> Option<EstablishmentPlan> {
    build_establishment_mechanism(
        index,
        model,
        connection,
        mechanism,
        connection_kind,
        _initial_scope,
    )
}

fn build_establishment_mechanism(
    index: &ix::ResolvedManufacturerIndex,
    model: &str,
    handle_selector: &str,
    mechanism: &str,
    connection_kind: Option<&str>,
    _initial_scope: &[KeyValue],
) -> Option<EstablishmentPlan> {
    let model_view = index.models.iter().find(|m| m.id == model)?;
    // The connection kind selects the family registry (§11.29). Only a raw
    // `usb` connection's establishment lives in the USB block; everything
    // else resolves against BLE exactly as before.
    let (block, usb_interfaces) = match connection_kind {
        Some("usb") => {
            let usb = model_view.usb.as_ref()?;
            (usb.establishment(mechanism)?, Some(&usb.interfaces))
        }
        _ => (model_view.ble.as_ref()?.establishment(mechanism)?, None),
    };
    let steps = block
        .steps
        .iter()
        .map(|step| step_from_ix(step, usb_interfaces))
        .collect::<Result<_, _>>()
        .expect("plans validated at store load");
    Some(EstablishmentPlan {
        plan_handle: format!("{model}:{handle_selector}"),
        mechanism: block.mechanism.clone(),
        prerequisite: block.prerequisite.clone(),
        on_demand: block.on_demand,
        params: block.params.clone(),
        persist: block.persist.clone(),
        activities: block.activities.iter().map(Into::into).collect(),
        post_exit_readiness: block
            .post_exit_readiness
            .iter()
            .map(|step| step_from_ix(step, usb_interfaces))
            .collect::<Result<_, _>>()
            .expect("plans validated at store load"),
        steps,
    })
}

/// The output of [`crate::ConfigStore::ble_action`]: a walkable BLE-native
/// control action over an established link (#91) — `remote-shutter`,
/// `write-time`, `write-gps`. The `Step` values keep their structured forms; the
/// host binds `params` and walks the steps from the resting BLE link.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BleActionPlan {
    pub action: String,
    pub params: Vec<String>,
    pub steps: Vec<Step>,
    pub evidence: Vec<String>,
}

/// Build the BLE action plan registered under `action` for `model`. Looks it up
/// in the index family BLE `actions` registry. Returns `None` if the model has
/// no BLE block or no action is registered under `action`.
pub fn build_ble_action(
    index: &ix::ResolvedManufacturerIndex,
    model: &str,
    action: &str,
) -> Option<BleActionPlan> {
    let model_view = index.models.iter().find(|m| m.id == model)?;
    let ble = model_view.ble.as_ref()?;
    let block = ble.action(action)?;
    let steps = block
        .steps
        .iter()
        .map(Step::try_from)
        .collect::<Result<_, _>>()
        .expect("BLE plans validated at store load");
    Some(BleActionPlan {
        action: action.to_string(),
        params: block.params.clone(),
        steps,
        evidence: block.evidence.clone(),
    })
}

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ConfigError conversion (camera-config side → FFI side)
// ---------------------------------------------------------------------------

impl From<cc::ConfigError> for crate::ConfigError {
    fn from(e: cc::ConfigError) -> Self {
        crate::ConfigError::Parse(e.to_string())
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;

    #[test]
    fn unknown_activity_display_role_survives_the_ffi_mirror() {
        let source: camera_config::ConnectionActivityDescriptor = serde_yaml::from_str(
            r#"
id: camera.test.future
version: 1
displayRole: futureRole
defaultExpectedDurationMs: 1
interactionRequired: false
hostCheckpoint: { name: future }
"#,
        )
        .expect("unknown roles remain parseable");
        let ffi = ConnectionActivityDescriptor::from(&source);
        assert_eq!(
            ffi.display_role,
            ConnectionActivityDisplayRole::Unknown {
                raw: "futureRole".into()
            }
        );
    }

    #[test]
    fn activity_title_survives_the_ffi_mirror() {
        let source: camera_config::ConnectionActivityDescriptor = serde_yaml::from_str(
            r#"
id: camera.test.titled
version: 1
displayRole: connecting
title: Opening camera session
defaultExpectedDurationMs: 1
interactionRequired: false
hostCheckpoint: { name: titled }
"#,
        )
        .expect("titled activity remains parseable");
        let ffi = ConnectionActivityDescriptor::from(&source);
        assert_eq!(ffi.title.as_deref(), Some("Opening camera session"));
    }
}
