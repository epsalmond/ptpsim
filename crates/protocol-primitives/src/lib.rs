//! `protocol-primitives` — the small set of code that genuinely is *not* data,
//! organized **by concern, not by brand**. Each primitive is referenced from a
//! manifest by id (`framing:`, `quirk:`, …). Adding a camera is a manifest +
//! captures; a new entry here is needed only for a genuinely new wire format or
//! computed quirk, and it lands as a shared peer — never a per-manufacturer
//! crate. This is what keeps ptpsim from becoming "vcam in `crates/`".

pub mod client_identity;
pub mod error;
pub mod focus_area;
pub mod fuji_framing;
pub mod fuji_init;
pub mod liveview;
pub mod pcss;
pub mod quirk;
pub mod usb_ptp;
pub mod value_codec;

pub use client_identity::normalize_client_name;
pub use error::FramingError;
pub use focus_area::pack_af_area;
pub use fuji_init::{build_app_init, parse_app_init, validate_init_ack, AppInit};
pub use pcss::{
    callback_ack_message as pcss_callback_ack_message, discovery_message as pcss_discovery_message,
    notify_message as pcss_notify_message,
    notify_message_terminator as pcss_notify_message_terminator,
    parse_discovery as parse_pcss_discovery, parse_notify as parse_pcss_notify, parse_pcss_init,
    parse_pcss_init_ack, pcss_init_ack_message, pcss_init_message, PcssInitAck, PcssInitAckError,
    PcssMessageError, PcssNotify,
};
pub use value_codec::{encode_value, ValueWidth};
