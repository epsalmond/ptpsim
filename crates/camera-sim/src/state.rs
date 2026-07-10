//! Mutable per-session camera state: open flag, property values, and the
//! current workflow phase. Everything here is generic; what the values *mean*
//! comes from the manifest.

use camera_config::CameraManifest;
use ptp_core::codes::datatype_code as dt;
use ptp_core::dataset::{DevicePropDesc, PropForm, PropValue};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Fuji function-mode selector properties.
pub const PROP_DF00: u16 = 0xdf00;
pub const PROP_DF01: u16 = 0xdf01;
pub const DF01_IMAGE_IMPORT: u32 = 20;
pub const DF01_LIVE_VIEW: u32 = 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Disconnected,
    SessionOpen,
    ImageImport,
    QueuedReceive,
    LiveView,
    Streaming,
    Closed,
}

impl Phase {
    pub fn state_name(self) -> &'static str {
        match self {
            Phase::Disconnected => "disconnected",
            Phase::SessionOpen => "sessionOpen",
            Phase::ImageImport => "imageImport",
            Phase::QueuedReceive => "queuedReceive",
            Phase::LiveView => "liveView",
            Phase::Streaming => "streaming",
            Phase::Closed => "closed",
        }
    }

    pub fn from_state_name(name: &str) -> Option<Self> {
        match name {
            "disconnected" => Some(Phase::Disconnected),
            "sessionOpen" | "session_open" | "session-open" => Some(Phase::SessionOpen),
            "imageImport" | "image_import" | "image-import" => Some(Phase::ImageImport),
            "queuedReceive" | "queued_receive" | "queued-receive" => Some(Phase::QueuedReceive),
            "liveView" | "live_view" | "live-view" => Some(Phase::LiveView),
            "streaming" => Some(Phase::Streaming),
            "closed" => Some(Phase::Closed),
            _ => None,
        }
    }
}

pub struct CameraState {
    pub session_open: bool,
    pub phase: Phase,
    /// Manifest mode path detected from current property state. This keeps
    /// generic modes available without adding selector constants to the engine.
    pub active_mode: Option<String>,
    /// Current property values, keyed by property code.
    pub props: BTreeMap<u16, PropValue>,
    /// Deferred op-effect transitions awaiting their settle poll (the §5.5 AF
    /// delay analogue). Keyed by target prop code; resolved by `resolve_pending`
    /// on each `GetDevicePropValue` of that code. Internal — driven only via
    /// [`CameraState::arm_effect`] / [`CameraState::resolve_pending`].
    pending: BTreeMap<u16, PendingTransition>,
    /// Completion/lifecycle events pushed by operation `emits` (#54), FIFO. The
    /// in-memory analogue of the camera's `0xC0xx` push channel: drained by the
    /// event socket (`drain_events`) and by the reference executor's event-source
    /// `awaitUntil` (`take_event`). Internal — pushed via [`push_event`], drained
    /// via [`take_event`] / [`drain_events`].
    ///
    /// [`push_event`]: CameraState::push_event
    /// [`take_event`]: CameraState::take_event
    /// [`drain_events`]: CameraState::drain_events
    events: VecDeque<u16>,
    /// Sequence gates satisfied in the current session. These are manifest-named
    /// ordered bootstrap preconditions, not camera properties.
    satisfied_gates: BTreeSet<String>,
    /// Progress through each manifest-declared gate sequence, keyed by
    /// `(gate_name, sequence_index)`.
    gate_progress: BTreeMap<(String, usize), usize>,
}

/// A scheduled op-effect: the value `set_prop` settles to, and how many more
/// polls of it until the new value is visible.
struct PendingTransition {
    value: PropValue,
    remaining: u32,
}

impl CameraState {
    /// Seed property values from manifest descriptors (enum -> first value,
    /// range -> min) so reads have something defined to return.
    pub fn from_manifest(manifest: &CameraManifest) -> Self {
        let mut props = BTreeMap::new();
        for (code_key, prop) in &manifest.properties {
            let Some(code) = camera_config::parse_hex_code(code_key) else {
                continue;
            };
            let datatype = datatype_of(prop.ptype.as_deref());
            if let Some(value) = prop.initial_value {
                props.insert(code, typed(datatype, value));
            } else if let Some(desc) = &prop.descriptor {
                if let Some(first) = desc.values.first() {
                    props.insert(code, typed(datatype, *first));
                }
            }
        }
        CameraState {
            session_open: false,
            phase: Phase::Disconnected,
            active_mode: None,
            props,
            pending: BTreeMap::new(),
            events: VecDeque::new(),
            satisfied_gates: BTreeSet::new(),
            gate_progress: BTreeMap::new(),
        }
    }

    /// Arm an op-effect on `code`. Immediate (`settle_after_polls == 0`) writes
    /// the value now; otherwise it becomes visible after that many
    /// `GetDevicePropValue` polls of `code` (see [`resolve_pending`]). Re-arming
    /// replaces any in-flight transition for the same prop.
    ///
    /// [`resolve_pending`]: CameraState::resolve_pending
    pub fn arm_effect(&mut self, code: u16, value: PropValue, settle_after_polls: u32) {
        if settle_after_polls == 0 {
            self.props.insert(code, value);
            self.pending.remove(&code);
        } else {
            self.pending.insert(
                code,
                PendingTransition {
                    value,
                    remaining: settle_after_polls,
                },
            );
        }
    }

    /// Advance any pending transition for `code` by one poll; commit the new
    /// value when its countdown reaches zero. No-op when nothing is pending.
    pub fn resolve_pending(&mut self, code: u16) {
        if let Some(p) = self.pending.get_mut(&code) {
            p.remaining = p.remaining.saturating_sub(1);
            if p.remaining == 0 {
                let value = p.value.clone();
                self.props.insert(code, value);
                self.pending.remove(&code);
            }
        }
    }

    /// Queue a completion/lifecycle event (an operation `emits` code). FIFO.
    pub fn push_event(&mut self, code: u16) {
        self.events.push_back(code);
    }

    /// Remove the first queued copy of `code` and return whether it was there.
    /// Order-tolerant: a step awaiting `0xC005` still matches if `0xC004`/`0xC001`
    /// are queued ahead of it. (This keeps the in-memory oracle order-tolerant; a
    /// real client reading the socket sees events in wire order.) The event-source
    /// `awaitUntil` drains here — the counterpart of BLE `take_notification`.
    pub fn take_event(&mut self, code: u16) -> bool {
        if let Some(i) = self.events.iter().position(|&c| c == code) {
            self.events.remove(i);
            true
        } else {
            false
        }
    }

    /// Drain all queued events in FIFO order — the event socket forwards these
    /// to connected clients after each operation.
    pub fn drain_events(&mut self) -> Vec<u16> {
        self.events.drain(..).collect()
    }

    pub fn gate_satisfied(&self, name: &str) -> bool {
        self.satisfied_gates.contains(name)
    }

    pub fn satisfy_gate(&mut self, name: &str) {
        self.satisfied_gates.insert(name.to_string());
    }

    pub fn clear_gate(&mut self, name: &str) {
        self.satisfied_gates.remove(name);
        self.gate_progress.retain(|(gate, _), _| gate != name);
    }

    pub fn reset_gates(&mut self) {
        self.satisfied_gates.clear();
        self.gate_progress.clear();
    }

    pub fn gate_progress(&self, name: &str, sequence: usize) -> usize {
        self.gate_progress
            .get(&(name.to_string(), sequence))
            .copied()
            .unwrap_or(0)
    }

    pub fn set_gate_progress(&mut self, name: &str, sequence: usize, progress: usize) {
        let key = (name.to_string(), sequence);
        if progress == 0 {
            self.gate_progress.remove(&key);
        } else {
            self.gate_progress.insert(key, progress);
        }
    }

    /// The manifest control-mode key matching the current phase, used to resolve
    /// "intent -> mechanism" the same way the app does.
    pub fn mode_key(&self) -> &'static str {
        match self.phase {
            Phase::LiveView | Phase::Streaming => "liveView",
            Phase::ImageImport => "imageImport",
            _ => "",
        }
    }

    /// The manifest mode path matching the current phase, used for scoped
    /// property capability profiles.
    pub fn manifest_mode_path(&self) -> &str {
        self.active_mode.as_deref().unwrap_or(match self.phase {
            Phase::SessionOpen | Phase::LiveView | Phase::Streaming => "shooting/stills",
            Phase::ImageImport => "image-transfer",
            _ => "",
        })
    }
}

pub fn datatype_of(ty: Option<&str>) -> u16 {
    match ty {
        Some("u8") => dt::UINT8,
        Some("u16") => dt::UINT16,
        Some("u32") => dt::UINT32,
        Some("u64") => dt::UINT64,
        Some("str") => dt::STR,
        _ => dt::UINT16,
    }
}

pub fn typed(datatype: u16, v: i64) -> PropValue {
    match datatype {
        dt::UINT8 => PropValue::U8(v as u8),
        dt::UINT16 => PropValue::U16(v as u16),
        dt::UINT32 => PropValue::U32(v as u32),
        dt::UINT64 => PropValue::U64(v as u64),
        _ => PropValue::U16(v as u16),
    }
}

/// Build a `DevicePropDesc` for `code` from the manifest property entry and the
/// current value in state.
pub fn build_prop_desc(
    manifest: &CameraManifest,
    state: &CameraState,
    code: u16,
) -> Option<DevicePropDesc> {
    let prop = manifest.property(code)?;
    let datatype = datatype_of(prop.ptype.as_deref());
    let current = state
        .props
        .get(&code)
        .cloned()
        .unwrap_or(typed(datatype, 0));
    let get_set = match prop.access.as_deref() {
        Some("readWrite") => 1,
        _ => 0,
    };
    let form = match &prop.descriptor {
        Some(d) if d.form == "enum" => {
            PropForm::Enum(d.values.iter().map(|v| typed(datatype, *v)).collect())
        }
        Some(d) if d.form == "range" && d.values.len() == 3 => PropForm::Range {
            min: typed(datatype, d.values[0]),
            max: typed(datatype, d.values[1]),
            step: typed(datatype, d.values[2]),
        },
        _ => PropForm::None,
    };
    Some(DevicePropDesc {
        code,
        datatype,
        get_set,
        factory_default: current.clone(),
        current,
        form,
    })
}
