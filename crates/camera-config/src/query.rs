//! Compatibility queries the simulator and app both use. All lookups are by the
//! parsed `u16` code so callers don't deal in hex strings.

use crate::model::{parse_hex_code, CameraManifest, Control, Operation, Property, Workflow};

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
}
