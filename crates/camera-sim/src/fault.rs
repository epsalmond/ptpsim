//! Fault injection — part of the simulator's public contract. Faults are how
//! the app exercises protocol-error, disconnect, and reconnect paths without
//! editing simulator code. Rules are checked before normal dispatch.

/// A single injected fault. Kept deliberately small; extend as scenarios need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Return `response` for operation `code` instead of handling it.
    FailOperation { code: u16, response: u16 },
    /// Close the command connection when operation `code` arrives.
    CloseOnOperation { code: u16 },
}

#[derive(Debug, Default, Clone)]
pub struct FaultSet {
    rules: Vec<Fault>,
}

impl FaultSet {
    pub fn install(&mut self, fault: Fault) {
        self.rules.push(fault);
    }

    pub fn clear(&mut self) {
        self.rules.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// First matching fault for an operation code, if any.
    pub fn match_op(&self, code: u16) -> Option<&Fault> {
        self.rules.iter().find(|f| match f {
            Fault::FailOperation { code: c, .. } => *c == code,
            Fault::CloseOnOperation { code: c } => *c == code,
        })
    }
}
