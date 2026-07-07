//! `ptp-core` — PTP/IP protocol **syntax**: packet framing, container payloads,
//! PTP datatypes, dataset encoders, and standard code registries.
//!
//! Hard rule: this crate knows nothing about any camera's behavior. It does not
//! know that Fuji live view needs `0xdf01 = 22`, that a card has a `DCIM`
//! folder, or that a session is in image-import mode. Semantics live in
//! manifests; vendor wire variants (e.g. Fuji's compressed framing) live in
//! `protocol-primitives`. Everything here is round-trippable: decode ∘ encode is
//! the identity on well-formed input.

pub mod codes;
pub mod container;
pub mod dataset;
pub mod datatype;
pub mod error;
pub mod framing;

pub use container::{
    DataBlock, EventPacket, InitCommandAck, InitCommandRequest, InitFail, OperationRequest,
    OperationResponse, StartData,
};
pub use dataset::{DeviceInfo, DevicePropDesc, ObjectInfo, PropForm, PropValue, StorageInfo};
pub use datatype::{Reader, Writer};
pub use error::{DecodeError, EncodeError};
pub use framing::{encode, PtpCodec, PtpIpPacket};
