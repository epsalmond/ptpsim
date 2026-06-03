//! Compatibility queries the simulator and app both use. All lookups are by the
//! parsed `u16` code so callers don't deal in hex strings.

use crate::model::{
    parse_hex_code, Action, ActionVerb, CameraManifest, Control, Operation, Property, Workflow,
};
use crate::predicate::PropView;
use crate::version::VersionScheme;

/// Whether an operation is available, and in the queried workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Supported and listed for this workflow.
    InWorkflow,
    /// Defined by the camera but not for this workflow.
    WrongWorkflow,
    /// Not defined at all.
    Unsupported,
}

/// Availability of an operation across the orthogonal `(connection, mode)` axes
/// plus its runtime prerequisite — the precise reason it is or isn't usable now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Valid in this connection + mode and its `requires` predicate holds.
    Available,
    /// Defined, but not in this mode path.
    WrongMode,
    /// Defined, but not over this connection.
    WrongConnection,
    /// Valid here, but its `requires` prerequisite is unmet by observed state.
    Blocked,
    /// Not defined at all.
    Unavailable,
}

/// Does an operation's `modes` set (path-prefix matched) cover `mode_path`?
/// Empty = valid in all modes. A `Shooting` entry covers `Shooting/Stills`.
fn mode_matches(op_modes: &[String], mode_path: &str) -> bool {
    op_modes.is_empty()
        || op_modes
            .iter()
            .any(|m| mode_path == m || mode_path.starts_with(&format!("{m}/")))
}

impl CameraManifest {
    pub fn schema_version(&self) -> &str {
        &self.schema
    }

    /// Look up an operation by code regardless of workflow.
    pub fn operation(&self, code: u16) -> Option<&Operation> {
        self.operations
            .iter()
            .find(|(k, _)| parse_hex_code(k) == Some(code))
            .map(|(_, v)| v)
    }

    /// Is `code` supported, and is it valid in `workflow`?
    pub fn supports_operation(&self, workflow: &str, code: u16) -> Support {
        match self.operation(code) {
            None => Support::Unsupported,
            Some(op) => {
                if op.workflows.is_empty() || op.workflows.iter().any(|w| w == workflow) {
                    Support::InWorkflow
                } else {
                    Support::WrongWorkflow
                }
            }
        }
    }

    pub fn property(&self, code: u16) -> Option<&Property> {
        self.properties
            .iter()
            .find(|(k, _)| parse_hex_code(k) == Some(code))
            .map(|(_, v)| v)
    }

    /// Resolve the concrete control mechanism for a property in a given mode/
    /// workflow — the "intent → mechanism" lookup the app relies on.
    pub fn control_for(&self, property_code: u16, mode: &str) -> Option<&Control> {
        self.property(property_code)
            .and_then(|p| p.controls.get(mode))
    }

    /// Human label for a property value, if the manifest defines one.
    pub fn value_label(&self, property_code: u16, value: i64) -> Option<&str> {
        self.property(property_code)
            .and_then(|p| p.labels.get(&value.to_string()))
            .map(|s| s.as_str())
    }

    pub fn workflow(&self, id: &str) -> Option<&Workflow> {
        self.workflows.get(id)
    }

    /// Resolve a named action on a connection. The closed `ActionVerb` enum
    /// gates new verbs to schema PRs; if a connection doesn't declare an
    /// action for this verb, returns `None` and the caller surfaces it as
    /// "not supported here" (e.g. `ActionVerb::Shutter` on a read-only
    /// transport). Mode-gating against the action's `mode:` field is the
    /// caller's responsibility — same pattern as `control_for`. See
    /// `docs/plans/action-verbs.md`.
    pub fn action(&self, connection: &str, verb: ActionVerb) -> Option<&Action> {
        self.connections.get(connection)?.actions.get(&verb)
    }
}

/// Orthogonal-axis queries (decisions #4–#8): gating intersects connection × mode
/// and evaluates `requires`/`detect` predicates over client-supplied observed
/// state. All side-effect-free; the engine does no I/O.
impl CameraManifest {
    /// Is `code` usable over `connection` in `mode_path`, given `observed`
    /// property values? Intersects the operation's `connections` and `modes`
    /// sets, then evaluates its `requires` predicate.
    pub fn operation_available(
        &self,
        connection: &str,
        mode_path: &str,
        code: u16,
        observed: &PropView,
    ) -> Availability {
        let Some(op) = self.operation(code) else {
            return Availability::Unavailable;
        };
        if !op.connections.is_empty() && !op.connections.iter().any(|c| c == connection) {
            return Availability::WrongConnection;
        }
        if !mode_matches(&op.modes, mode_path) {
            return Availability::WrongMode;
        }
        if let Some(req) = &op.requires {
            if !req.eval(observed) {
                return Availability::Blocked;
            }
        }
        Availability::Available
    }

    /// Connection ids available under `firmware` (a connection with an
    /// `availableWhen` firmware range is filtered by [`VersionScheme`]). An
    /// unconditional connection is always listed.
    pub fn connections_available(&self, firmware: &str, scheme: VersionScheme) -> Vec<&str> {
        self.connections
            .iter()
            .filter(|(_, c)| {
                c.available_when
                    .as_ref()
                    .is_none_or(|w| w.matches(firmware, scheme))
            })
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Which mode does `observed` put the camera in? Evaluates each mode's
    /// `detect` predicate; returns the first match (BTreeMap path order). `None`
    /// → the client should fall back to a picker over [`Self::mode_paths`].
    pub fn detect_mode(&self, observed: &PropView) -> Option<&str> {
        self.modes
            .iter()
            .find(|(_, m)| m.detect.as_ref().is_some_and(|p| p.eval(observed)))
            .map(|(path, _)| path.as_str())
    }

    /// All defined mode paths.
    pub fn mode_paths(&self) -> Vec<&str> {
        self.modes.keys().map(|s| s.as_str()).collect()
    }

    /// Like [`Self::operation_available`], but also returns a [`ResolutionTrace`]
    /// explaining the decision (the gating checks + the `requires` predicate's leaf
    /// evaluations) — for telemetry / config iteration. Same outcome as the fast path.
    pub fn operation_available_explained(
        &self,
        connection: &str,
        mode_path: &str,
        code: u16,
        observed: &PropView,
    ) -> (Availability, crate::trace::ResolutionTrace) {
        use crate::trace::ResolutionTrace;
        let mut connection_ok = false;
        let mut mode_ok = false;
        let mut requires = None;

        let (availability, reason) = match self.operation(code) {
            None => (
                Availability::Unavailable,
                format!("operation 0x{code:04x} is not defined"),
            ),
            Some(op) => {
                connection_ok =
                    op.connections.is_empty() || op.connections.iter().any(|c| c == connection);
                mode_ok = mode_matches(&op.modes, mode_path);
                requires = op.requires.as_ref().map(|p| p.explain(observed));
                if !connection_ok {
                    (
                        Availability::WrongConnection,
                        format!("op valid over {:?}, not '{connection}'", op.connections),
                    )
                } else if !mode_ok {
                    (
                        Availability::WrongMode,
                        format!("op valid in modes {:?}, not '{mode_path}'", op.modes),
                    )
                } else if requires.as_ref().is_some_and(|r| !r.passed) {
                    (
                        Availability::Blocked,
                        "requires prerequisite unmet (see trace leaves)".to_string(),
                    )
                } else {
                    (Availability::Available, "available".to_string())
                }
            }
        };

        let trace = ResolutionTrace {
            query: "operation_available".to_string(),
            connection: connection.to_string(),
            mode: mode_path.to_string(),
            op: code,
            outcome: format!("{availability:?}"),
            connection_ok,
            mode_ok,
            requires,
            reason,
        };
        (availability, trace)
    }

    /// Capabilities in effect at `mode_path`, inheriting from ancestor paths
    /// (`Shooting` caps apply under `Shooting/Stills`). Order: root→leaf.
    pub fn capabilities(&self, mode_path: &str) -> Vec<&str> {
        let mut caps = Vec::new();
        let mut acc = String::new();
        for seg in mode_path.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            if let Some(m) = self.modes.get(&acc) {
                caps.extend(m.capabilities.iter().map(|s| s.as_str()));
            }
        }
        caps
    }
}

#[cfg(test)]
mod orthogonal_tests {
    use super::*;
    use crate::predicate::PropView;

    const M: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x902d": { name: StepFNumber, modes: [Shooting/Stills], connections: [xlv-http] }
  "0x1007": { name: GetObjectHandles, modes: [ImageTransfer] }
  "0x101c":
    name: InitiateOpenCapture
    modes: [Shooting]
    requires: { prop: "0xd212", mask: 0x00ff, ne: 0 }
modes:
  Shooting: { capabilities: [exposureControl] }
  Shooting/Stills: { capabilities: [liveView], detect: { prop: "0xdf01", eq: 0x1600 } }
  ImageTransfer: { detect: { prop: "0xdf01", eq: 0x1400 } }
connections:
  xlv-http: { kind: http }
  instax-printer: { ref: instax-printer, availableWhen: { firmware: { lt: "2.40" } } }
"#;

    fn m() -> CameraManifest {
        CameraManifest::from_yaml(M).unwrap()
    }

    #[test]
    fn gating_intersects_connection_and_mode() {
        let m = m();
        let any = PropView::new();
        // StepFNumber: valid in Shooting/Stills over xlv-http.
        assert_eq!(
            m.operation_available("xlv-http", "Shooting/Stills", 0x902d, &any),
            Availability::Available
        );
        // Wrong mode.
        assert_eq!(
            m.operation_available("xlv-http", "ImageTransfer", 0x902d, &any),
            Availability::WrongMode
        );
        // Wrong connection.
        assert_eq!(
            m.operation_available("usb", "Shooting/Stills", 0x902d, &any),
            Availability::WrongConnection
        );
        // Unknown op.
        assert_eq!(
            m.operation_available("xlv-http", "Shooting/Stills", 0x9999, &any),
            Availability::Unavailable
        );
    }

    #[test]
    fn mode_prefix_inheritance() {
        let m = m();
        let any = PropView::new();
        // 0x101c declares modes:[Shooting]; valid under the child Shooting/Stills.
        // requires 0xd212 low byte != 0:
        let ready = PropView::new().with(0xd212, 0x01);
        assert_eq!(
            m.operation_available("xlv-http", "Shooting/Stills", 0x101c, &ready),
            Availability::Available
        );
        // Prerequisite unmet -> Blocked, not WrongMode.
        assert_eq!(
            m.operation_available("xlv-http", "Shooting/Stills", 0x101c, &any),
            Availability::Blocked
        );
    }

    #[test]
    fn detect_mode_and_capabilities() {
        let m = m();
        assert_eq!(
            m.detect_mode(&PropView::new().with(0xdf01, 0x1600)),
            Some("Shooting/Stills")
        );
        assert_eq!(
            m.detect_mode(&PropView::new().with(0xdf01, 0x1400)),
            Some("ImageTransfer")
        );
        assert_eq!(m.detect_mode(&PropView::new().with(0xdf01, 0x9999)), None);
        // Inherited capability + own capability.
        let caps = m.capabilities("Shooting/Stills");
        assert!(caps.contains(&"exposureControl")); // from Shooting
        assert!(caps.contains(&"liveView")); // from Shooting/Stills
    }

    #[test]
    fn instax_connection_gated_by_firmware() {
        let m = m();
        let s = VersionScheme::DottedInt;
        assert!(m
            .connections_available("2.30", s)
            .contains(&"instax-printer"));
        assert!(!m
            .connections_available("2.40", s)
            .contains(&"instax-printer"));
        // xlv-http is unconditional.
        assert!(m.connections_available("2.40", s).contains(&"xlv-http"));
    }
}
