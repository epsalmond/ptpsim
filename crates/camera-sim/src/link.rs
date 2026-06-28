//! Cross-transport runtime link: the BLE side arms the PTP/IP session (#102).
//!
//! The GFX100 II Wi-Fi-AP handoff requires a BLE write to `IMAGE_TRANSFER_SETTING`
//! BEFORE the function-launch; without it the camera brings up the AP but **drops
//! the `InitCommandRequest`** — the session was never armed. The BLE responder and
//! the PTP/IP `Engine` are otherwise independent, so a host that skips the prep
//! write passes a green sim run. This shared link makes the sim model the refusal:
//! the prep write sets a flag, function-launch latches the session armed to it, and
//! the init handshake reads it.
//!
//! Default armed = a **standalone** camera (no BLE handoff in play, e.g. the
//! service's smoke path) is ready, so the gate only bites once a function-launch
//! has run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared arming state linking the BLE responder to the PTP/IP engine.
#[derive(Debug)]
pub struct CameraLink {
    prep_written: AtomicBool,
    armed: AtomicBool,
}

impl Default for CameraLink {
    fn default() -> Self {
        CameraLink {
            prep_written: AtomicBool::new(false),
            // A standalone camera (no AP handoff) is ready — the gate only applies
            // after a function-launch latches arming to the prep flag.
            armed: AtomicBool::new(true),
        }
    }
}

impl CameraLink {
    /// The BLE `IMAGE_TRANSFER_SETTING` write: the AP handoff that follows is armed.
    pub fn note_prep_write(&self) {
        self.prep_written.store(true, Ordering::SeqCst);
    }

    /// The BLE Wi-Fi-AP function-launch: latch the session armed IFF the prep write
    /// preceded it. A launch without the prep write leaves the session unarmed.
    pub fn launch_ap(&self) {
        let armed = self.prep_written.load(Ordering::SeqCst);
        self.armed.store(armed, Ordering::SeqCst);
    }

    /// Whether the PTP/IP `InitCommandRequest` should be answered.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }
}

/// A `CameraLink` shared (cloned) between the BLE responder and the engine.
pub type SharedLink = Arc<CameraLink>;
