//! Binding generator entry point (uniffi library mode). Generates Swift / Kotlin
//! bindings from the compiled `camera-protocol-ffi` library. See
//! `docs/INTEGRATION.md` for the per-platform invocations.
fn main() {
    uniffi::uniffi_bindgen_main()
}
