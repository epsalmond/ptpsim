//! Computed quirks (id-referenced from manifests) — the cases where a property
//! value is *assembled or derived*, not looked up in a table, so it needs code.
//! Kept here as named, shared functions rather than baked into a per-brand
//! emulator.

/// Fuji `0xd212` live-status bundle: a packed snapshot the camera derives from
/// several live values during live view. Real layout TBD from capture; this
/// assembles a deterministic placeholder from the inputs so the engine has a
/// readback to return and tests can assert it changes with state.
pub fn status_d212(aperture: u16, iso: u32, focus_locked: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(8);
    v.extend_from_slice(&aperture.to_le_bytes());
    v.extend_from_slice(&iso.to_le_bytes());
    v.push(focus_locked as u8);
    v.push(0); // reserved
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_changes_with_inputs() {
        let a = status_d212(560, 800, false);
        let b = status_d212(280, 800, true);
        assert_ne!(a, b);
        assert_eq!(a.len(), 8);
    }
}
