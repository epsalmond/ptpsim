//! Fault injection — part of the simulator's public contract. Faults are how
//! the app exercises protocol-error, disconnect, and reconnect paths without
//! editing simulator code. Rules are checked before normal dispatch.

/// A single injected fault. Kept deliberately small; extend as scenarios need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Return `response` for operation `code` instead of handling it.
    FailOperation { code: u16, response: u16 },
    /// Return `response` for the next `remaining` matching operations, then
    /// resume normal dispatch. Useful for transient camera-settling scenarios.
    FailOperationTimes {
        code: u16,
        response: u16,
        remaining: u32,
    },
    /// Return `response` for the next `remaining` operations whose code and
    /// complete parameter list match. Lets tests target one property read or
    /// object handle without catching neighboring operations with the same code.
    FailOperationParamsTimes {
        code: u16,
        params: Vec<u32>,
        response: u16,
        remaining: u32,
    },
    /// Close the command connection when operation `code` arrives.
    CloseOnOperation { code: u16 },
    /// Handle the next `remaining` operations whose code and complete parameter
    /// list match normally, but truncate the data payload to `keep` bytes while
    /// keeping the OK response. Models a camera serving a framing-valid but
    /// short payload while settling into a mode.
    TruncateDataParamsTimes {
        code: u16,
        params: Vec<u32>,
        keep: usize,
        remaining: u32,
    },
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
    pub fn match_op(&self, code: u16, params: &[u32]) -> Option<&Fault> {
        self.rules.iter().find(|f| match f {
            Fault::FailOperation { code: c, .. } => *c == code,
            Fault::FailOperationTimes {
                code: c, remaining, ..
            } => *c == code && *remaining > 0,
            Fault::FailOperationParamsTimes {
                code: c,
                params: expected,
                remaining,
                ..
            } => *c == code && expected == params && *remaining > 0,
            Fault::CloseOnOperation { code: c } => *c == code,
            Fault::TruncateDataParamsTimes { .. } => false,
        })
    }

    /// Consume one use of a finite fault, or clone a persistent fault.
    /// Truncation faults are not dispatch replacements; they are consumed by
    /// [`Self::take_truncation`] after normal handling.
    pub fn take_op(&mut self, code: u16, params: &[u32]) -> Option<Fault> {
        let fault = self.rules.iter_mut().find(|fault| match fault {
            Fault::FailOperation {
                code: candidate, ..
            }
            | Fault::CloseOnOperation { code: candidate } => *candidate == code,
            Fault::FailOperationTimes {
                code: candidate,
                remaining,
                ..
            } => *candidate == code && *remaining > 0,
            Fault::FailOperationParamsTimes {
                code: candidate,
                params: expected,
                remaining,
                ..
            } => *candidate == code && expected == params && *remaining > 0,
            Fault::TruncateDataParamsTimes { .. } => false,
        })?;
        match fault {
            Fault::FailOperationTimes { remaining, .. }
            | Fault::FailOperationParamsTimes { remaining, .. } => *remaining -= 1,
            Fault::FailOperation { .. }
            | Fault::CloseOnOperation { .. }
            | Fault::TruncateDataParamsTimes { .. } => {}
        }
        Some(fault.clone())
    }

    /// Consume one use of a matching truncation fault, returning the byte
    /// count to keep from the reply's data payload.
    pub fn take_truncation(&mut self, code: u16, params: &[u32]) -> Option<usize> {
        let fault = self.rules.iter_mut().find(|fault| match fault {
            Fault::TruncateDataParamsTimes {
                code: candidate,
                params: expected,
                remaining,
                ..
            } => *candidate == code && expected == params && *remaining > 0,
            _ => false,
        })?;
        match fault {
            Fault::TruncateDataParamsTimes {
                keep, remaining, ..
            } => {
                *remaining -= 1;
                Some(*keep)
            }
            _ => unreachable!("find matched a truncation fault"),
        }
    }
}
