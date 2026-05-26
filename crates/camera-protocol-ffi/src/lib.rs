//! `camera-protocol-ffi` — the (optional) Swift/Ruby FFI boundary for the app
//! side: generated lookup tables first, FFI-backed manifest queries only where
//! logic would otherwise be duplicated incorrectly (intent → mechanism
//! resolution).
//!
//! Placeholder until the app-integration phase (P2). The Rust side it will wrap
//! (`camera-config` queries) is already in place; this crate adds the
//! `uniffi`/cbindgen surface when the app begins consuming it.

#![allow(clippy::missing_safety_doc)]

/// Crate version, exposed so an FFI consumer can assert ABI/build expectations.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_reported() {
        assert!(!super::version().is_empty());
    }
}
