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
//!
//! The same link also carries the BLE-registered **device name** (#109): the host
//! writes its own name to `deviceNameString` during pairing, and the camera gates
//! the `InitCommandRequest` on the PTP/IP friendly name being CONSISTENT with it. A
//! mismatch is silently dropped. The name is `None` until a BLE write registers it,
//! so a standalone init (no registration) is ungated — same shape as arming.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Shared arming + identity state linking the BLE responder to the PTP/IP engine.
#[derive(Debug)]
pub struct CameraLink {
    prep_written: AtomicBool,
    armed: AtomicBool,
    device_name: Mutex<Option<String>>,
}

impl Default for CameraLink {
    fn default() -> Self {
        CameraLink {
            prep_written: AtomicBool::new(false),
            // A standalone camera (no AP handoff) is ready — the gate only applies
            // after a function-launch latches arming to the prep flag.
            armed: AtomicBool::new(true),
            // No BLE registration yet — the name gate is inert until one happens.
            device_name: Mutex::new(None),
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

    /// The BLE `deviceNameString` write (#109): record the device name the host
    /// registered during pairing — the PTP/IP friendly name must match it.
    pub fn note_device_name(&self, name: String) {
        *self.device_name.lock().unwrap() = Some(name);
    }

    /// The BLE-registered device name, if a pairing write set one. `None` on a
    /// standalone path (no registration) — the init handshake then skips the gate.
    pub fn device_name(&self) -> Option<String> {
        self.device_name.lock().unwrap().clone()
    }
}

/// A `CameraLink` shared (cloned) between the BLE responder and the engine.
pub type SharedLink = Arc<CameraLink>;
