//! In-memory BLE GATT responder + reference establishment walker (issue #25
//! Phase 1). The simulator side of the BLE pair flow: the responder plays the
//! camera (advert constants, GATT catalog, per-characteristic read/notify
//! policy from manifest data + test scripting), the walker plays the app
//! dispatcher, executing a manifest establishment plan against it. No real
//! radio — Phase 2 (BlueZ) would implement the same surface over a stack.
//!
//! Like the PTP/IP [`crate::Engine`], this is generic: vendor behavior comes
//! from manifest data and per-test policy, never from code branches. The
//! walker doubles as the executable reference for what a platform dispatcher
//! must do per verb (resolution semantics delegate to
//! `camera_config::index::eval`, the engine-owned spec).

use std::collections::{BTreeMap, BTreeSet};

use camera_config::index::eval;
use camera_config::index::{
    AcquireSource, AwaitSource, BleAwaitUntilStep, BleNotifyUntil, BleWriteChunkStep, CccdMode,
    ChunkField, Encoding, NotifyCapture, PredicateOp, RetryFailureKind, Step, StepValue,
};
use protocol_primitives::{NikonConnectionConfiguration, NikonLssClient, NikonLssSession};

/// Reference-walker bound on a `bleAwaitUntil` loop: the deterministic
/// analogue of the dispatcher's wall-clock `timeout_ms`. A sticky-unsatisfied
/// source (a `serve_read` value that never meets `until`) hits this and fails
/// like a real timeout rather than spinning forever.
const MAX_AWAIT_ITERS: usize = 256;

/// Reference-walker guard on a `bleWriteChunk` upload (#112): the deterministic
/// analogue of a transfer budget. A blob needing more windows than this fails
/// rather than letting a corrupt size spin. Mirrors [`MAX_AWAIT_ITERS`].
const MAX_CHUNK_WINDOWS: usize = 4096;

/// One interaction the responder observed, in arrival order. Tests assert on
/// this log to prove a plan drove the camera in the expected reference app order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BleEvent {
    Connect,
    PeerDisconnect,
    RequestMtu { requested: u16, negotiated: u16 },
    DiscoverServices,
    Read { uuid: String },
    PeripheralName,
    NotificationFence { uuid: String },
    Write { uuid: String, value: Vec<u8> },
    Subscribe { uuid: String, mode: CccdMode },
}

#[derive(Debug, Clone)]
struct ScriptedNotification {
    payload: Vec<u8>,
    /// `None` means already buffered before the next fenced write. A tagged
    /// payload becomes eligible after the named write reaches that ordinal.
    after_fenced_write: Option<(String, u32)>,
}

/// One transport-neutral action in an exact GATT exchange script.
#[derive(Debug, Clone)]
enum ScriptedGattAction {
    ExactWrite { uuid: String, value: Vec<u8> },
    Indication { uuid: String, payload: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BleError {
    NotConnected,
    ServicesNotDiscovered,
    PeerDisconnectNotObserved,
    /// The characteristic isn't in this body's exposed catalog (or, for a
    /// read, has no value policy) — the in-memory analogue of a GATT
    /// attribute-not-found error.
    NotExposed(String),
    /// No peripheral name was scripted; the in-memory analogue of a host
    /// stack that cannot supply `CBPeripheral.name` for the bound peripheral.
    NoPeripheralName,
    /// The ATT MTU request itself failed (a GATT error or timeout), as
    /// distinct from negotiating below a manifest floor.
    MtuRequestFailed,
    /// An exact-write script was active and the next write did not match it.
    UnexpectedWrite {
        expected_uuid: String,
        expected_value: Vec<u8>,
        actual_uuid: String,
        actual_value: Vec<u8>,
    },
    ScriptOutOfOrder {
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for BleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BleError::NotConnected => write!(f, "peripheral not connected"),
            BleError::ServicesNotDiscovered => write!(f, "GATT services not discovered"),
            BleError::PeerDisconnectNotObserved => write!(f, "peer disconnect not observed"),
            BleError::NotExposed(uuid) => write!(f, "characteristic {uuid} not exposed"),
            BleError::NoPeripheralName => write!(f, "peripheral name not served"),
            BleError::MtuRequestFailed => write!(f, "MTU request failed"),
            BleError::UnexpectedWrite {
                expected_uuid,
                expected_value,
                actual_uuid,
                actual_value,
            } => write!(
                f,
                "unexpected write to {actual_uuid}: {actual_value:02x?}; expected {expected_uuid}: {expected_value:02x?}"
            ),
            BleError::ScriptOutOfOrder { expected, actual } => {
                write!(f, "scripted GATT action out of order: expected {expected}, got {actual}")
            }
        }
    }
}

/// Deterministic in-memory GATT peripheral. Construct from the manifest's
/// symbolic-name → UUID catalog, then script per-characteristic behavior:
/// [`serve_read`](Self::serve_read) values and
/// [`queue_notification`](Self::queue_notification) payloads.
///
/// A characteristic in the catalog accepts writes and CCCD subscriptions; a
/// read additionally needs a served value (a catalogued-but-unserved
/// characteristic read fails like a real body that doesn't expose it — the
/// LEGACY `deviceIdentificationNumber` case).
pub struct BleResponder {
    catalog: BTreeSet<String>,
    read_values: BTreeMap<String, Vec<u8>>,
    /// Per-read evolving values (for `bleAwaitUntil` read-poll loops): each
    /// read pops the next, the last value is sticky. Checked before
    /// `read_values`.
    read_sequences: BTreeMap<String, Vec<Vec<u8>>>,
    notify_queues: BTreeMap<String, Vec<ScriptedNotification>>,
    /// Global so scripts can assert ordering across characteristics. Empty
    /// preserves the responder's historical permissive-write behavior.
    gatt_script: Vec<ScriptedGattAction>,
    fenced_write_counts: BTreeMap<String, u32>,
    mtu_cap: u16,
    /// Script the MTU request itself failing (a GATT error or timeout), as
    /// distinct from negotiating below a manifest floor.
    mtu_request_fails: bool,
    /// The platform peripheral name a `blePeripheralName` step serves (§11.4b).
    /// Unset fails the step, like a catalogued-but-unserved read.
    peripheral_name: Option<String>,
    connected: bool,
    services_discovered: bool,
    peer_disconnect_pending: bool,
    log: Vec<BleEvent>,
    /// Cross-transport arming link (#102): a write to `arm_uuid`
    /// (`IMAGE_TRANSFER_SETTING`) notes the prep; a write to `launch_uuid`
    /// (function-launch) latches the linked engine armed to that prep flag.
    arming: Option<crate::link::SharedLink>,
    arm_uuid: Option<String>,
    launch_uuid: Option<String>,
    /// A write to `name_uuid` (`deviceNameString`) registers the host's device
    /// name on the linked engine (#109) — the init handshake gates on it.
    name_uuid: Option<String>,
}

impl BleResponder {
    /// `catalog` is the manifest's `gatt:` map values (UUIDs). Names are not
    /// kept — steps arrive with UUIDs already resolved by the loader (§11.3).
    pub fn new<I: IntoIterator<Item = String>>(catalog: I) -> Self {
        BleResponder {
            catalog: catalog.into_iter().collect(),
            read_values: BTreeMap::new(),
            read_sequences: BTreeMap::new(),
            notify_queues: BTreeMap::new(),
            gatt_script: Vec::new(),
            fenced_write_counts: BTreeMap::new(),
            mtu_cap: 247,
            mtu_request_fails: false,
            peripheral_name: None,
            connected: false,
            services_discovered: false,
            peer_disconnect_pending: false,
            log: Vec::new(),
            arming: None,
            arm_uuid: None,
            launch_uuid: None,
            name_uuid: None,
        }
    }

    /// Link this responder to an engine's arming state (#102): a write to
    /// `arm_uuid` (`IMAGE_TRANSFER_SETTING`) arms the AP handoff, and a write to
    /// `launch_uuid` (function-launch) latches the engine armed to it — so a plan
    /// that skips the prep write leaves the engine to drop `InitCommandRequest`.
    pub fn link_arming(
        mut self,
        link: crate::link::SharedLink,
        arm_uuid: &str,
        launch_uuid: &str,
    ) -> Self {
        self.catalog.insert(arm_uuid.to_string());
        self.catalog.insert(launch_uuid.to_string());
        self.arming = Some(link);
        self.arm_uuid = Some(arm_uuid.to_string());
        self.launch_uuid = Some(launch_uuid.to_string());
        self
    }

    /// Link this responder so a write to `name_uuid` (`deviceNameString`) registers
    /// the host's device name on the engine (#109): the init handshake then drops
    /// an `InitCommandRequest` whose friendly name disagrees. Shares the same
    /// `SharedLink` as [`Self::link_arming`] (cloned `Arc`), so both may be chained.
    pub fn link_device_name(mut self, link: crate::link::SharedLink, name_uuid: &str) -> Self {
        self.catalog.insert(name_uuid.to_string());
        self.arming = Some(link);
        self.name_uuid = Some(name_uuid.to_string());
        self
    }

    /// Serve `bytes` on every read of `uuid` (also adds it to the catalog).
    pub fn serve_read(mut self, uuid: &str, bytes: &[u8]) -> Self {
        self.catalog.insert(uuid.to_string());
        self.read_values.insert(uuid.to_string(), bytes.to_vec());
        self
    }

    /// Serve an evolving sequence of read values on `uuid`: each `read` pops
    /// the next, the last value is sticky (returned on every read once the
    /// sequence is down to one). Drives `bleAwaitUntil` read-poll loops —
    /// e.g. `serve_read_sequence(color, [0, 0, 1])` models AF focusing then
    /// locking. Takes precedence over `serve_read` for the same uuid.
    pub fn serve_read_sequence(mut self, uuid: &str, values: Vec<Vec<u8>>) -> Self {
        self.catalog.insert(uuid.to_string());
        self.read_sequences.insert(uuid.to_string(), values);
        self
    }

    /// Buffer a notification payload immediately. A later fenced write for
    /// this characteristic discards it as pre-command state.
    pub fn queue_notification(mut self, uuid: &str, payload: &[u8]) -> Self {
        self.catalog.insert(uuid.to_string());
        self.notify_queues
            .entry(uuid.to_string())
            .or_default()
            .push(ScriptedNotification {
                payload: payload.to_vec(),
                after_fenced_write: None,
            });
        self
    }

    /// Script a notification caused by the `ordinal`th fenced write to
    /// `write_uuid`. It is not buffered until that write occurs, which lets the
    /// deterministic oracle distinguish stale queue entries from causal ones.
    pub fn queue_notification_after_fenced_write(
        mut self,
        uuid: &str,
        write_uuid: &str,
        ordinal: u32,
        payload: &[u8],
    ) -> Self {
        self.catalog.insert(uuid.to_string());
        self.catalog.insert(write_uuid.to_string());
        self.notify_queues
            .entry(uuid.to_string())
            .or_default()
            .push(ScriptedNotification {
                payload: payload.to_vec(),
                after_fenced_write: Some((write_uuid.to_string(), ordinal)),
            });
        self
    }

    /// Require the next scripted action to be this exact write. The global
    /// action order makes this useful for any finite multi-characteristic
    /// exchange without teaching the responder a vendor protocol.
    pub fn expect_exact_write(mut self, uuid: &str, value: &[u8]) -> Self {
        self.catalog.insert(uuid.to_string());
        self.gatt_script.push(ScriptedGattAction::ExactWrite {
            uuid: uuid.to_string(),
            value: value.to_vec(),
        });
        self
    }

    /// Queue an indication as the next scripted action. It becomes available
    /// only after every earlier exact write/indication has been consumed.
    pub fn queue_ordered_indication(mut self, uuid: &str, payload: &[u8]) -> Self {
        self.catalog.insert(uuid.to_string());
        self.gatt_script.push(ScriptedGattAction::Indication {
            uuid: uuid.to_string(),
            payload: payload.to_vec(),
        });
        self
    }

    /// Cap the negotiable ATT MTU (default 247, typical BLE 5 stack).
    pub fn with_mtu_cap(mut self, cap: u16) -> Self {
        self.mtu_cap = cap;
        self
    }

    /// Script the MTU request itself failing, like a GATT error or timeout on
    /// a request-API platform (#449).
    pub fn with_failing_mtu_request(mut self) -> Self {
        self.mtu_request_fails = true;
        self
    }

    /// Serve a platform peripheral name to `blePeripheralName` steps (§11.4b).
    pub fn with_peripheral_name(mut self, name: &str) -> Self {
        self.peripheral_name = Some(name.to_string());
        self
    }

    /// The platform peripheral-name lookup: host-side on stacks that hide the
    /// GAP service, never a GATT read. Unserved fails like an unserved read.
    pub fn peripheral_name(&mut self) -> Result<String, BleError> {
        if !self.connected {
            return Err(BleError::NotConnected);
        }
        self.log.push(BleEvent::PeripheralName);
        self.peripheral_name
            .clone()
            .ok_or(BleError::NoPeripheralName)
    }

    pub fn connect(&mut self) {
        self.connected = true;
        self.services_discovered = false;
        self.log.push(BleEvent::Connect);
    }

    /// Script the camera dropping the active link (remote-boot lifecycle).
    pub fn queue_peer_disconnect(mut self) -> Self {
        self.peer_disconnect_pending = true;
        self
    }

    pub fn await_disconnect(&mut self) -> Result<(), BleError> {
        if !self.connected {
            return Err(BleError::NotConnected);
        }
        if !self.peer_disconnect_pending {
            return Err(BleError::PeerDisconnectNotObserved);
        }
        self.peer_disconnect_pending = false;
        self.connected = false;
        self.services_discovered = false;
        self.log.push(BleEvent::PeerDisconnect);
        Ok(())
    }

    pub fn request_mtu(&mut self, requested: u16) -> Result<u16, BleError> {
        if !self.connected {
            return Err(BleError::NotConnected);
        }
        if self.mtu_request_fails {
            return Err(BleError::MtuRequestFailed);
        }
        let negotiated = requested.min(self.mtu_cap);
        self.log.push(BleEvent::RequestMtu {
            requested,
            negotiated,
        });
        Ok(negotiated)
    }

    pub fn discover_services(&mut self) -> Result<(), BleError> {
        if !self.connected {
            return Err(BleError::NotConnected);
        }
        self.services_discovered = true;
        self.log.push(BleEvent::DiscoverServices);
        Ok(())
    }

    fn require_char(&self, uuid: &str) -> Result<(), BleError> {
        if !self.connected {
            return Err(BleError::NotConnected);
        }
        if !self.services_discovered {
            return Err(BleError::ServicesNotDiscovered);
        }
        if !self.catalog.contains(uuid) {
            return Err(BleError::NotExposed(uuid.to_string()));
        }
        Ok(())
    }

    pub fn read(&mut self, uuid: &str) -> Result<Vec<u8>, BleError> {
        self.require_char(uuid)?;
        self.log.push(BleEvent::Read {
            uuid: uuid.to_string(),
        });
        // Sequenced reads (await loops) take precedence: advance while more
        // than one remains, stick on the last.
        if let Some(seq) = self.read_sequences.get_mut(uuid) {
            if seq.len() > 1 {
                return Ok(seq.remove(0));
            }
            if let Some(last) = seq.first() {
                return Ok(last.clone());
            }
        }
        self.read_values
            .get(uuid)
            .cloned()
            .ok_or_else(|| BleError::NotExposed(uuid.to_string()))
    }

    pub fn write(&mut self, uuid: &str, value: &[u8]) -> Result<(), BleError> {
        self.preflight_write(uuid, value)?;
        if matches!(
            self.gatt_script.first(),
            Some(ScriptedGattAction::ExactWrite { .. })
        ) {
            self.gatt_script.remove(0);
        }
        self.log.push(BleEvent::Write {
            uuid: uuid.to_string(),
            value: value.to_vec(),
        });
        // Cross-transport arming (#102): the prep write arms; function-launch
        // latches the linked engine armed to whether the prep write preceded it.
        // The deviceNameString write (#109) registers the host's name for the init
        // friendly-name consistency gate. The three UUIDs are distinct.
        if let Some(link) = &self.arming {
            if self.arm_uuid.as_deref() == Some(uuid) {
                link.note_prep_write();
            } else if self.launch_uuid.as_deref() == Some(uuid) {
                link.launch_ap();
            } else if self.name_uuid.as_deref() == Some(uuid) {
                if let Ok(name) = std::str::from_utf8(value) {
                    link.note_device_name(name.to_string());
                }
            }
        }
        Ok(())
    }

    fn preflight_write(&self, uuid: &str, value: &[u8]) -> Result<(), BleError> {
        self.require_char(uuid)?;
        match self.gatt_script.first() {
            Some(ScriptedGattAction::ExactWrite {
                uuid: expected_uuid,
                value: expected_value,
            }) if expected_uuid != uuid || expected_value != value => {
                Err(BleError::UnexpectedWrite {
                    expected_uuid: expected_uuid.clone(),
                    expected_value: expected_value.clone(),
                    actual_uuid: uuid.to_string(),
                    actual_value: value.to_vec(),
                })
            }
            Some(ScriptedGattAction::Indication {
                uuid: expected_uuid,
                ..
            }) => Err(BleError::ScriptOutOfOrder {
                expected: format!("indication from {expected_uuid}"),
                actual: format!("write to {uuid}"),
            }),
            _ => Ok(()),
        }
    }

    /// Atomically discard the notification characteristic's buffered prefix
    /// and issue the write. Scripted notifications caused by this write become
    /// eligible only after the write is recorded.
    pub fn write_with_notification_fence(
        &mut self,
        uuid: &str,
        value: &[u8],
        notification_uuid: &str,
    ) -> Result<(), BleError> {
        // Validate both characteristics before mutating the queue or ordinal.
        // `write` has no remaining fallible work after this preflight.
        self.preflight_write(uuid, value)?;
        self.require_char(notification_uuid)?;
        let next_ordinal = self.fenced_write_counts.get(uuid).copied().unwrap_or(0) + 1;
        let fenced_write_counts = &self.fenced_write_counts;
        if let Some(queue) = self.notify_queues.get_mut(notification_uuid) {
            queue.retain(|entry| {
                entry
                    .after_fenced_write
                    .as_ref()
                    .is_some_and(|(write_uuid, ordinal)| {
                        !fenced_write_counts
                            .get(write_uuid)
                            .is_some_and(|count| count >= ordinal)
                    })
            });
        }
        self.log.push(BleEvent::NotificationFence {
            uuid: notification_uuid.to_string(),
        });
        self.fenced_write_counts
            .insert(uuid.to_string(), next_ordinal);
        self.write(uuid, value)
    }

    /// CCCD descriptor write — success IS the ack (§11.8 `bleSubscribe`).
    pub fn subscribe(&mut self, uuid: &str, mode: CccdMode) -> Result<(), BleError> {
        self.require_char(uuid)?;
        self.log.push(BleEvent::Subscribe {
            uuid: uuid.to_string(),
            mode,
        });
        Ok(())
    }

    /// Pop the next queued notification payload for `uuid`, if any.
    pub fn take_notification(&mut self, uuid: &str) -> Option<Vec<u8>> {
        if let Some(action) = self.gatt_script.first() {
            match action {
                ScriptedGattAction::Indication {
                    uuid: scripted_uuid,
                    ..
                } if scripted_uuid == uuid => {
                    let ScriptedGattAction::Indication { payload, .. } = self.gatt_script.remove(0)
                    else {
                        unreachable!("matched indication above")
                    };
                    return Some(payload);
                }
                _ => return None,
            }
        }
        let queue = self.notify_queues.get_mut(uuid)?;
        let eligible = match &queue.first()?.after_fenced_write {
            None => true,
            Some((write_uuid, ordinal)) => self
                .fenced_write_counts
                .get(write_uuid)
                .is_some_and(|count| count >= ordinal),
        };
        eligible.then(|| queue.remove(0).payload)
    }

    /// Every interaction, in order.
    pub fn log(&self) -> &[BleEvent] {
        &self.log
    }

    /// Convenience: the payloads written to `uuid`, in order.
    pub fn written(&self, uuid: &str) -> Vec<&[u8]> {
        self.log
            .iter()
            .filter_map(|e| match e {
                BleEvent::Write { uuid: u, value } if u == uuid => Some(value.as_slice()),
                _ => None,
            })
            .collect()
    }

    /// Convenience: the CCCD-subscribed UUIDs, in order.
    pub fn subscribed(&self) -> Vec<&str> {
        self.log
            .iter()
            .filter_map(|e| match e {
                BleEvent::Subscribe { uuid, .. } => Some(uuid.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// Walk failure: which step (by verb + position) and why. Tolerant steps
/// never produce one — their failures are skipped like a real dispatcher.
#[derive(Debug)]
pub struct WalkError {
    pub step: String,
    pub kind: RetryFailureKind,
    pub message: String,
    pub context: BTreeMap<String, String>,
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.message)
    }
}

/// Whether a completed establishment walk observed its declared registration
/// confirmation signal (plan §11.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstablishmentConfirmOutcome {
    Satisfied,
    Unsatisfied,
    NotDeclared,
}

/// Per-walk confirmation verdict plus terminal tolerated-step reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EstablishmentWalkSummary {
    pub confirm_outcome: EstablishmentConfirmOutcome,
    pub tolerated_step_count: u32,
    pub tolerated_step_paths: Vec<String>,
}

/// Result of a completed walk: final scope, step count, and establishment
/// confirmation/tolerance summary.
pub struct WalkOutcome {
    pub scope: BTreeMap<String, String>,
    pub steps_run: usize,
    pub summary: EstablishmentWalkSummary,
}

struct WalkCtx<'a> {
    responder: &'a mut BleResponder,
    scope: BTreeMap<String, String>,
    /// Encoding each scope key was captured with — lets `{ captured: … }`
    /// writes re-encode integer captures (the RED `idNumber` u32) instead
    /// of guessing from the scope string.
    encodings: BTreeMap<String, Encoding>,
    runtime_params: BTreeMap<String, String>,
    subscriptions: BTreeSet<(String, bool)>,
    /// Opaque authenticated cipher state, never exposed through scope/logs.
    nikon_lss_session: Option<NikonLssSession>,
    steps_run: usize,
    confirm_outcome: EstablishmentConfirmOutcome,
    tolerated_step_paths: Vec<String>,
}

/// Execute an establishment plan against the responder — the reference
/// dispatcher. `retries`/`retryDelayMs` are honoured as semantics but not as
/// timing: the responder is deterministic, so a step either succeeds on the
/// first try or never (a retry loop would spin on the same answer). Regex
/// `until: matches` is unsupported here (the engine deliberately carries no
/// regex dependency); plans using it need a platform dispatcher.
///
/// `initial_encodings` carries the encoding each recognition-seeded capture
/// decoded with (`eval::advert_capture_encodings`), seeding `ctx.encodings`
/// just as in-walk `bleRead`/`bleNotify` captures do. Without it a later
/// `{ captured: … }` write-back of an advert capture falls back to the
/// scope-string heuristic, which silently hex-decodes an even-length all-hex
/// ASCII value instead of writing its bytes (#43).
pub fn walk_establishment(
    responder: &mut BleResponder,
    steps: &[Step],
    initial_scope: &BTreeMap<String, String>,
    initial_encodings: &BTreeMap<String, Encoding>,
    runtime_params: &BTreeMap<String, String>,
) -> Result<WalkOutcome, WalkError> {
    let confirm_outcome = if steps.iter().any(step_declares_confirmation) {
        EstablishmentConfirmOutcome::Unsatisfied
    } else {
        EstablishmentConfirmOutcome::NotDeclared
    };
    let mut ctx = WalkCtx {
        responder,
        scope: initial_scope.clone(),
        encodings: initial_encodings.clone(),
        runtime_params: runtime_params.clone(),
        subscriptions: BTreeSet::new(),
        nikon_lss_session: None,
        steps_run: 0,
        confirm_outcome,
        tolerated_step_paths: Vec::new(),
    };
    walk_steps(&mut ctx, steps, "steps")?;
    Ok(WalkOutcome {
        scope: ctx.scope,
        steps_run: ctx.steps_run,
        summary: EstablishmentWalkSummary {
            confirm_outcome: ctx.confirm_outcome,
            tolerated_step_count: ctx.tolerated_step_paths.len() as u32,
            tolerated_step_paths: ctx.tolerated_step_paths,
        },
    })
}

fn walk_steps(ctx: &mut WalkCtx<'_>, steps: &[Step], path: &str) -> Result<(), WalkError> {
    for (i, step) in steps.iter().enumerate() {
        let here = format!("{path}[{i}].{}", step.verb_name());
        let tolerant = match step {
            Step::If(s) => s.tolerant, // §11.6: If's tolerant gates predicate fields, not body errors
            other => other.options().tolerant,
        };
        match run_step(ctx, step, &here) {
            Ok(()) => {
                ctx.steps_run += 1;
                if step.options().confirms.is_some() {
                    ctx.confirm_outcome = EstablishmentConfirmOutcome::Satisfied;
                }
            }
            Err(e) if tolerant && !matches!(step, Step::If(_)) => {
                // Tolerant step failure: skip and continue (§11.6).
                let _ = e;
                ctx.steps_run += 1;
                ctx.tolerated_step_paths.push(here);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn step_declares_confirmation(step: &Step) -> bool {
    if step.options().confirms.is_some() {
        return true;
    }
    match step {
        Step::Acquire(step) => step_declares_confirmation(&step.from),
        Step::If(step) => step
            .then
            .iter()
            .chain(&step.else_branch)
            .any(step_declares_confirmation),
        Step::BleAwaitUntil(step) => step
            .failure_evidence
            .iter()
            .flat_map(|evidence| &evidence.steps)
            .chain(&step.on_each)
            .any(step_declares_confirmation),
        Step::Retry(step) => step
            .steps
            .iter()
            .chain(&step.on_failure)
            .any(step_declares_confirmation),
        _ => false,
    }
}

/// The scope slot an `acquire` delegate binds its result to — the delegate's
/// own explicit `capture_as`. `acquire` aliases THIS slot under its `name`, so
/// a delegate without one (e.g. a `bleNotify` using only field `capture`s) has
/// nothing for acquire to bind and is rejected rather than guessed at.
fn primary_capture_name(step: &Step) -> Option<&str> {
    match step {
        Step::BleRead(s) => Some(&s.capture_as),
        Step::BlePeripheralName(s) => Some(&s.capture_as),
        Step::BleNotify(s) => s.capture_as.as_deref(),
        Step::BleAwaitUntil(s) => s.capture_as.as_deref(),
        _ => None,
    }
}

fn run_step(ctx: &mut WalkCtx<'_>, step: &Step, here: &str) -> Result<(), WalkError> {
    let err = |message: String| WalkError {
        step: here.to_string(),
        kind: RetryFailureKind::Other,
        message,
        context: BTreeMap::new(),
    };
    match step {
        Step::BleConnect(_) => {
            ctx.responder.connect();
            Ok(())
        }
        // The deterministic reference walker has no clock. Successful
        // traversal is sufficient to prove the authored delay is accepted;
        // the asynchronous dispatcher above owns elapsed-time behavior.
        Step::BleDelay(_) => Ok(()),
        Step::BleAwaitDisconnect(_) => ctx
            .responder
            .await_disconnect()
            .map_err(|e| err(e.to_string())),
        Step::BleRequestMtu(s) => {
            let negotiated = ctx
                .responder
                .request_mtu(s.requested_mtu)
                .map_err(|e| err(e.to_string()))?;
            // §11.4a: the checkpoint is the evidenced floor, not the request
            // target. No floor means any negotiated MTU succeeds.
            if let Some(minimum) = s.minimum_mtu {
                if negotiated < minimum {
                    return Err(err(format!(
                        "negotiated MTU {negotiated} < required {minimum}"
                    )));
                }
            }
            Ok(())
        }
        Step::BleDiscoverServices(_) => ctx
            .responder
            .discover_services()
            .map_err(|e| err(e.to_string())),
        Step::BleRead(s) => {
            let wire = ctx
                .responder
                .read(&s.gatt)
                .map_err(|e| err(e.to_string()))?;
            // §11.13 capture pipeline: bytes → transform chain → encoding.
            let bytes = eval::apply_transforms(&wire, &s.transform)
                .ok_or_else(|| err("transform chain failed".into()))?;
            let value = eval::decode_bytes(&bytes, s.encoding)
                .ok_or_else(|| err(format!("decode as {} failed", s.encoding.as_token())))?;
            ctx.scope.insert(s.capture_as.clone(), value);
            ctx.encodings.insert(s.capture_as.clone(), s.encoding);
            Ok(())
        }
        Step::BlePeripheralName(s) => {
            let raw = ctx
                .responder
                .peripheral_name()
                .map_err(|e| err(e.to_string()))?;
            // §11.4b: UTF-8 with any NUL terminator removed; a name that is
            // empty after the trim is unavailable and fails like an unserved
            // one.
            let name = eval::decode_bytes(raw.as_bytes(), Encoding::Utf8Cstring)
                .ok_or_else(|| err("peripheral name is not valid UTF-8".into()))?;
            if name.is_empty() {
                return Err(err("peripheral name unavailable".into()));
            }
            ctx.scope.insert(s.capture_as.clone(), name);
            ctx.encodings.insert(s.capture_as.clone(), Encoding::Utf8);
            Ok(())
        }
        Step::BleWrite(s) => {
            let bytes = resolve_value(ctx, &s.value).map_err(err)?;
            match &s.notification_fence {
                Some(notification_uuid) => {
                    ctx.responder
                        .write_with_notification_fence(&s.gatt, &bytes, notification_uuid)
                }
                None => ctx.responder.write(&s.gatt, &bytes),
            }
            .map_err(|e| err(e.to_string()))
        }
        Step::BleSubscribe(s) => {
            ensure_subscribed(ctx, &s.gatt, s.mode).map_err(|e| err(e.to_string()))
        }
        Step::BleNotify(s) => {
            ensure_subscribed(ctx, &s.gatt, s.mode).map_err(|e| err(e.to_string()))?;
            let payload = ctx
                .responder
                .take_notification(&s.gatt)
                .ok_or_else(|| err("no notification arrived (queue empty)".into()))?;
            let accepted = match &s.until {
                BleNotifyUntil::Any => true,
                BleNotifyUntil::Equals { value, encoding } => {
                    let want = eval::yaml_literal_to_bytes(value, *encoding)
                        .ok_or_else(|| err("until.equals value undecodable".into()))?;
                    payload == want
                }
                BleNotifyUntil::Matches { .. } => {
                    return Err(err(
                        "until.matches (regex) is unsupported in the reference walker".into(),
                    ));
                }
            };
            if !accepted {
                return Err(err("notification payload did not satisfy until".into()));
            }
            apply_value_captures(ctx, &payload, &s.capture_as, &s.capture);
            Ok(())
        }
        Step::BleAwaitUntil(s) => run_await_until(ctx, s, here),
        Step::BleWriteChunk(s) => run_write_chunk(ctx, s, here),
        Step::Acquire(s) => {
            // Run the delegate through `walk_steps` (a one-element slice) so its
            // OWN tolerant/retry options apply at its level rather than the
            // acquire's (#44 finding 2). Then alias the slot the delegate
            // explicitly declared it captures into, by name — not whatever key
            // a scope set-diff happens to surface, which mis-picks the
            // lexicographically-smallest key on a multi-capture delegate and
            // silently aliases nothing when the delegate overwrites a
            // pre-existing (e.g. recognize-seeded) key (#44 finding 1).
            walk_steps(
                ctx,
                std::slice::from_ref(s.from.as_ref()),
                &format!("{here}.from"),
            )?;
            let target = primary_capture_name(&s.from).ok_or_else(|| {
                err(format!(
                    "acquire delegate `{}` declares no capture_as to bind",
                    s.from.verb_name()
                ))
            })?;
            // A tolerant delegate that failed bound nothing — there is then no
            // value to alias, and that is not an error (its own tolerance
            // already decided to continue).
            if let Some(v) = ctx.scope.get(target).cloned() {
                if let Some(enc) = ctx.encodings.get(target).copied() {
                    ctx.encodings.insert(s.name.clone(), enc);
                }
                ctx.scope.insert(s.name.clone(), v);
            }
            Ok(())
        }
        Step::AcquireFirmware(s) => match &s.from {
            AcquireSource::BleRead { gatt, encoding } => {
                let wire = ctx.responder.read(gatt).map_err(|e| err(e.to_string()))?;
                let value = eval::decode_bytes(&wire, *encoding)
                    .ok_or_else(|| err(format!("decode as {} failed", encoding.as_token())))?;
                ctx.scope.insert("firmware".to_string(), value);
                ctx.encodings.insert("firmware".to_string(), *encoding);
                Ok(())
            }
            other => Err(err(format!(
                "acquireFirmware source {other:?} unsupported in the reference walker"
            ))),
        },
        Step::NikonLssAuthenticate(s) => {
            // Match the async executor: a new authentication invalidates any
            // prior session even when this attempt fails.
            ctx.nikon_lss_session = None;
            let client_device_id: [u8; 8] = resolve_value(ctx, &s.client_device_id)
                .map_err(&err)?
                .try_into()
                .map_err(|bytes: Vec<u8>| {
                    err(format!(
                        "clientDeviceId must resolve to exactly 8 bytes (got {})",
                        bytes.len()
                    ))
                })?;
            let nonce: [u8; 8] = resolve_value(ctx, &s.nonce)
                .map_err(&err)?
                .try_into()
                .map_err(|bytes: Vec<u8>| {
                    err(format!(
                        "nonce must resolve to exactly 8 bytes (got {})",
                        bytes.len()
                    ))
                })?;
            ensure_subscribed(ctx, &s.gatt, CccdMode::Indicate).map_err(|e| err(e.to_string()))?;
            let mut client = NikonLssClient::new(client_device_id, nonce);
            let stage1 = client
                .stage1_record()
                .map_err(|e| err(format!("LSS stage 1 failed: {e}")))?;
            ctx.responder
                .write_with_notification_fence(&s.gatt, &stage1, &s.gatt)
                .map_err(|e| err(e.to_string()))?;
            let stage2 = ctx
                .responder
                .take_notification(&s.gatt)
                .ok_or_else(|| err("no LSS stage 2 indication arrived".into()))?;
            let stage3 = client
                .handle_stage2(&stage2)
                .map_err(|e| err(format!("LSS stage 2 failed: {e}")))?;
            ctx.responder
                .write_with_notification_fence(&s.gatt, &stage3, &s.gatt)
                .map_err(|e| err(e.to_string()))?;
            let stage4 = ctx
                .responder
                .take_notification(&s.gatt)
                .ok_or_else(|| err("no LSS stage 4 indication arrived".into()))?;
            ctx.nikon_lss_session = Some(
                client
                    .finish_stage4(&stage4)
                    .map_err(|e| err(format!("LSS stage 4 failed: {e}")))?,
            );
            Ok(())
        }
        Step::NikonLssReadConnectionConfiguration(s) => {
            let session = ctx
                .nikon_lss_session
                .as_ref()
                .ok_or_else(|| err("Nikon LSS session is not authenticated".into()))?;
            let wire = ctx
                .responder
                .read(&s.gatt)
                .map_err(|e| err(e.to_string()))?;
            let config = session
                .decode_connection_configuration(&wire)
                .map_err(|e| err(format!("LSS connection configuration failed: {e}")))?;
            bind_nikon_connection_configuration(ctx, s, config);
            Ok(())
        }
        Step::If(s) => {
            let field_value = ctx.scope.get(&s.condition.field);
            let holds = match field_value {
                None if s.tolerant => false, // §11.6: unbound field → false
                None => {
                    return Err(err(format!(
                        "predicate field '{}' unbound in scope",
                        s.condition.field
                    )));
                }
                Some(actual) => predicate_holds(actual, s.condition.op, &s.condition.value),
            };
            let branch = if holds { &s.then } else { &s.else_branch };
            let branch_path = format!("{here}.{}", if holds { "then" } else { "else" });
            walk_steps(ctx, branch, &branch_path)
        }
        Step::Retry(s) => {
            let mut attempts = 0;
            loop {
                match walk_steps(ctx, &s.steps, &format!("{here}.steps")) {
                    Ok(()) => return Ok(()),
                    Err(mut body_error) => {
                        if body_error.kind != s.when_failure {
                            return Err(body_error);
                        }
                        walk_steps(ctx, &s.on_failure, &format!("{here}.onFailure"))?;
                        let actual = ctx.scope.get(&s.retry_when.field).ok_or_else(|| {
                            err(format!(
                                "retryWhen field '{}' unbound in scope",
                                s.retry_when.field
                            ))
                        })?;
                        let should_retry =
                            predicate_holds(actual, s.retry_when.op, &s.retry_when.value);
                        attempts += 1;
                        if !should_retry || attempts >= s.max_attempts {
                            body_error.context = s
                                .failure_context
                                .iter()
                                .filter_map(|key| {
                                    ctx.scope.get(key).map(|value| (key.clone(), value.clone()))
                                })
                                .collect();
                            return Err(body_error);
                        }
                    }
                }
            }
        }
    }
}

fn ensure_subscribed(ctx: &mut WalkCtx<'_>, gatt: &str, mode: CccdMode) -> Result<(), BleError> {
    let key = (gatt.to_string(), matches!(mode, CccdMode::Indicate));
    if ctx.subscriptions.contains(&key) {
        return Ok(());
    }
    ctx.responder.subscribe(gatt, mode)?;
    ctx.subscriptions.insert(key);
    Ok(())
}

/// Bind a value (read result / notification payload) into scope: the whole
/// value under `capture_as` (hex), then each field capture through the
/// §11.13 pipeline (window → transform → encoding). Fail-soft: a capture
/// whose window/transform/decode fails is skipped, never an error. Shared by
/// `bleNotify` and each `bleAwaitUntil` iteration.
fn apply_value_captures(
    ctx: &mut WalkCtx<'_>,
    value: &[u8],
    capture_as: &Option<String>,
    captures: &[NotifyCapture],
) {
    if let Some(name) = capture_as {
        ctx.scope.insert(name.clone(), eval::hex_lower(value));
        ctx.encodings.insert(name.clone(), Encoding::Bytes);
    }
    for cap in captures {
        let end = match cap.length {
            Some(l) => cap.at.saturating_add(l),
            None => value.len(),
        };
        if cap.at > value.len() || end > value.len() {
            continue;
        }
        let Some(bytes) = eval::apply_transforms(&value[cap.at..end], &cap.transform) else {
            continue;
        };
        if let Some(decoded) = eval::decode_bytes(&bytes, cap.encoding) {
            ctx.scope.insert(cap.name.clone(), decoded);
            ctx.encodings.insert(cap.name.clone(), cap.encoding);
        }
    }
}

/// Execute a `bleAwaitUntil` step (§11.15): observe the source until `until`
/// holds over scope, running `on_each` between unsatisfied iterations.
/// Deterministic timeout = source exhaustion or [`MAX_AWAIT_ITERS`].
fn run_await_until(
    ctx: &mut WalkCtx<'_>,
    s: &BleAwaitUntilStep,
    here: &str,
) -> Result<(), WalkError> {
    let err = |message: String| WalkError {
        step: here.to_string(),
        kind: RetryFailureKind::Other,
        message,
        context: BTreeMap::new(),
    };
    // CCCD-enable once up front for a notify source (the camera then streams).
    if let AwaitSource::Notify { gatt, mode, .. } = &s.source {
        ensure_subscribed(ctx, gatt, *mode).map_err(|e| err(e.to_string()))?;
    }
    let mut seed_pending = matches!(
        &s.source,
        AwaitSource::Notify {
            seed_read: true,
            ..
        }
    );
    for _ in 0..MAX_AWAIT_ITERS {
        // Observe one value.
        let value = match &s.source {
            AwaitSource::Read { gatt } => {
                ctx.responder.read(gatt).map_err(|e| err(e.to_string()))?
            }
            AwaitSource::Notify { gatt, .. } => {
                if seed_pending {
                    seed_pending = false;
                    ctx.responder.read(gatt).map_err(|e| err(e.to_string()))?
                } else {
                    match ctx.responder.take_notification(gatt) {
                        Some(p) => p,
                        None => {
                            return Err(await_deadline_error(
                                here,
                                "awaited notification never arrived (source exhausted before `until`)"
                                    .into(),
                            ));
                        }
                    }
                }
            }
        };
        apply_value_captures(ctx, &value, &s.capture_as, &s.capture);

        // Satisfied? `until` is a Predicate over scope (the `if` vocabulary).
        let satisfied = match ctx.scope.get(&s.until.field) {
            Some(actual) => predicate_holds(actual, s.until.op, &s.until.value),
            // An unbound field can't satisfy the condition; keep observing
            // (a capture too short to bind it is the deterministic analogue
            // of "the camera hasn't reported it yet").
            None => false,
        };
        if satisfied {
            return Ok(());
        }
        if s.fail_when.as_ref().is_some_and(|predicate| {
            ctx.scope
                .get(&predicate.field)
                .is_some_and(|actual| predicate_holds(actual, predicate.op, &predicate.value))
        }) {
            let predicate = s.fail_when.as_ref().expect("matched above");
            let confirmed = match &s.failure_evidence {
                None => true,
                Some(evidence) => {
                    ctx.scope.remove(&evidence.when.field);
                    ctx.encodings.remove(&evidence.when.field);
                    walk_steps(
                        ctx,
                        &evidence.steps,
                        &format!("{here}.failureEvidence.steps"),
                    )?;
                    ctx.scope.get(&evidence.when.field).is_some_and(|actual| {
                        predicate_holds(actual, evidence.when.op, &evidence.when.value)
                    })
                }
            };
            if confirmed {
                let evidence = s
                    .failure_evidence
                    .as_ref()
                    .map_or(String::new(), |evidence| {
                        format!(
                            "; `failureEvidence.when` matched ({} {} {})",
                            evidence.when.field,
                            evidence.when.op.as_token(),
                            evidence.when.value
                        )
                    });
                return Err(WalkError {
                    step: here.to_string(),
                    kind: RetryFailureKind::ConditionRejected,
                    message: format!(
                        "`failWhen` matched ({} {} {}){}",
                        predicate.field,
                        predicate.op.as_token(),
                        predicate.value,
                        evidence
                    ),
                    context: BTreeMap::new(),
                });
            }
        }
        // Not yet: act, then observe again. interval_ms is dispatcher cadence
        // — the deterministic walker doesn't sleep.
        walk_steps(ctx, &s.on_each, &format!("{here}.onEach"))?;
    }
    Err(await_deadline_error(
        here,
        format!(
            "`until` ({} {} {}) not satisfied within {MAX_AWAIT_ITERS} observations",
            s.until.field,
            s.until.op.as_token(),
            s.until.value
        ),
    ))
}

fn await_deadline_error(here: &str, message: String) -> WalkError {
    WalkError {
        step: here.to_string(),
        kind: RetryFailureKind::DeadlineExceeded,
        message,
        context: BTreeMap::new(),
    }
}

/// Execute a `bleWriteChunk` (#112): frame + write ONE window of the host blob,
/// selected by the captured chunk index. The walker owns the slice math and the
/// frame assembly; the manifest declares only policy. Window layout mirrors

/// indexed `0..full`, then a final remainder window addressed by `sentinel_index`
/// (the camera's `0xffff` last chunk, which carries real data, not an empty frame).
fn run_write_chunk(
    ctx: &mut WalkCtx<'_>,
    s: &BleWriteChunkStep,
    here: &str,
) -> Result<(), WalkError> {
    let err = |message: String| WalkError {
        step: here.to_string(),
        kind: RetryFailureKind::Other,
        message,
        context: BTreeMap::new(),
    };

    // The host supplies the whole blob once as a bytes-raw hex param (#114).
    let raw = ctx
        .runtime_params
        .get(&s.source)
        .ok_or_else(|| err(format!("source slot '{}' unbound", s.source)))?;
    let blob = eval::scope_string_to_bytes(raw, Some(Encoding::BytesRaw))
        .ok_or_else(|| err(format!("source '{}' undecodable", s.source)))?;

    let size = s.size.max(1) as usize;
    let total = blob.len();
    // Count of full (non-final) windows: floor((total-1)/size). The final window
    // is the remainder (1..=size bytes; empty only for an empty blob).
    let full = if total == 0 { 0 } else { (total - 1) / size };
    if full + 1 > MAX_CHUNK_WINDOWS {
        return Err(err(format!(
            "blob of {total} bytes needs {} windows, exceeds cap {MAX_CHUNK_WINDOWS}",
            full + 1
        )));
    }

    // The captured chunk index (a bleAwaitUntil capture of the notification) names
    // which window to write — `sentinel_index` is the final remainder window.
    let captured = ctx
        .scope
        .get(&s.index)
        .ok_or_else(|| err(format!("index slot '{}' unbound in scope", s.index)))?;
    let idx: u64 = captured.parse().map_err(|_| {
        err(format!(
            "index '{}' = {captured:?} is not an integer",
            s.index
        ))
    })?;

    let (offset, len) = if idx == s.sentinel_index as u64 {
        (full * size, total - full * size)
    } else if (idx as usize) < full {
        (idx as usize * size, size)
    } else {
        return Err(err(format!(
            "chunk index {idx} out of range (full windows 0..{full}, sentinel {})",
            s.sentinel_index
        )));
    };

    // Assemble the declared header, then the window payload. The header carries
    // the chunk's own index (== the captured value) and its payload length.
    let mut frame = Vec::new();
    for f in &s.frame {
        let value = match f.field {
            ChunkField::Index => idx,
            ChunkField::Length => len as u64,
        };
        let encoded = eval::encode_uint(value, f.encoding).ok_or_else(|| {
            err(format!(
                "frame field {:?} needs an integer encoding (got {})",
                f.field,
                f.encoding.as_token()
            ))
        })?;
        frame.extend_from_slice(&encoded);
    }
    frame.extend_from_slice(&blob[offset..offset + len]);

    ctx.responder
        .write(&s.gatt, &frame)
        .map_err(|e| err(e.to_string()))
}

fn predicate_holds(actual: &str, op: PredicateOp, expected: &str) -> bool {
    // Numeric compare when both sides parse; string compare otherwise.
    let nums = (actual.parse::<i64>().ok(), expected.parse::<i64>().ok());
    match op {
        PredicateOp::Eq => actual == expected,
        PredicateOp::Ne => actual != expected,
        PredicateOp::Gt | PredicateOp::Gte | PredicateOp::Lt | PredicateOp::Lte => {
            let ord = match nums {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => actual.cmp(expected),
            };
            match op {
                PredicateOp::Gt => ord.is_gt(),
                PredicateOp::Gte => ord.is_ge(),
                PredicateOp::Lt => ord.is_lt(),
                PredicateOp::Lte => ord.is_le(),
                _ => unreachable!(),
            }
        }
        PredicateOp::In => expected.split(',').map(str::trim).any(|v| v == actual),
    }
}

fn resolve_value(ctx: &WalkCtx<'_>, value: &StepValue) -> Result<Vec<u8>, String> {
    match value {
        StepValue::Literal { literal } => eval::yaml_literal_to_bytes(literal, None)
            .ok_or_else(|| "literal undecodable".to_string()),
        StepValue::Template {
            template,
            transform,
        } => {
            let mut out = String::new();
            let mut rest = template.as_str();
            while let Some(open) = rest.find('{') {
                out.push_str(&rest[..open]);
                let Some(close) = rest[open..].find('}') else {
                    return Err(format!("template '{template}': unclosed brace"));
                };
                let name = &rest[open + 1..open + close];
                let v = ctx
                    .scope
                    .get(name)
                    .or_else(|| ctx.runtime_params.get(name))
                    .ok_or_else(|| format!("template ref '{{{name}}}' unbound"))?;
                out.push_str(v);
                rest = &rest[open + close + 1..];
            }
            out.push_str(rest);
            eval::apply_transforms(out.as_bytes(), transform)
                .ok_or_else(|| "transform chain failed".to_string())
        }
        StepValue::Runtime {
            runtime,
            encoding,
            transform,
        } => {
            let v = ctx
                .runtime_params
                .get(runtime)
                .ok_or_else(|| format!("runtime slot '{runtime}' unbound"))?;
            let bytes = eval::scope_string_to_bytes(v, *encoding)
                .ok_or_else(|| format!("runtime '{runtime}' undecodable"))?;
            eval::apply_transforms(&bytes, transform)
                .ok_or_else(|| "transform chain failed".to_string())
        }
        StepValue::Captured {
            captured,
            transform,
        } => {
            let v = ctx
                .scope
                .get(captured)
                .ok_or_else(|| format!("captured '{captured}' unbound in scope"))?;
            let bytes = eval::scope_string_to_bytes(v, ctx.encodings.get(captured).copied())
                .ok_or_else(|| format!("captured '{captured}' undecodable"))?;
            eval::apply_transforms(&bytes, transform)
                .ok_or_else(|| "transform chain failed".to_string())
        }
    }
}

fn bind_nikon_connection_configuration(
    ctx: &mut WalkCtx<'_>,
    step: &camera_config::index::NikonLssReadConnectionConfigurationStep,
    config: NikonConnectionConfiguration,
) {
    for name in [
        &step.ssid_capture_as,
        &step.password_capture_as,
        &step.security_mode_capture_as,
    ] {
        ctx.scope.remove(name);
        ctx.encodings.remove(name);
    }
    if let Some(name) = &step.spp_max_length_capture_as {
        ctx.scope.remove(name);
        ctx.encodings.remove(name);
    }
    ctx.scope
        .insert(step.flags_capture_as.clone(), config.flags.to_string());
    ctx.encodings
        .insert(step.flags_capture_as.clone(), Encoding::U8);
    if let Some(wifi) = config.wifi {
        ctx.scope.insert(step.ssid_capture_as.clone(), wifi.ssid);
        ctx.encodings
            .insert(step.ssid_capture_as.clone(), Encoding::Utf8);
        ctx.scope
            .insert(step.password_capture_as.clone(), wifi.password);
        ctx.encodings
            .insert(step.password_capture_as.clone(), Encoding::Utf8);
        ctx.scope.insert(
            step.security_mode_capture_as.clone(),
            wifi.security.as_token().to_string(),
        );
        ctx.encodings
            .insert(step.security_mode_capture_as.clone(), Encoding::Utf8);
    }
    if let (Some(name), Some(length)) = (&step.spp_max_length_capture_as, config.spp_maximum_length)
    {
        ctx.scope.insert(name.clone(), length.to_string());
        ctx.encodings.insert(name.clone(), Encoding::U32Le);
    }
}
