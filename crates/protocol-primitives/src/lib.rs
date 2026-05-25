//! `protocol-primitives` — the small set of code that genuinely is *not* data,
//! organized **by concern, not by brand**. Each primitive is referenced from a
//! manifest by id (`framing:`, `quirk:`, …). Adding a camera is a manifest +
//! captures; a new entry here is needed only for a genuinely new wire format or
//! computed quirk, and it lands as a shared peer — never a per-manufacturer
//! crate. This is what keeps ptpsim from becoming "vcam in `crates/`".

pub mod error;
pub mod fuji_framing;
pub mod liveview;
pub mod quirk;

pub use error::FramingError;
