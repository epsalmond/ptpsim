//! The manufacturer-index schema and loader (BLE-MVP P0).
//!
//! This module is **additive** alongside the existing per-model + firmware-tier
//! schema in [`crate::model`]. It introduces the schema shape the new iOS app
//! consumes:
//!
//! * a `ManufacturerIndex` lists one or more **models**, each pointing at a body
//!   manifest and declaring `signatures` (BLE-advert detectors, today),
//! * **families** carry shared facts the models `inherits:` from — for BLE that
//!   is the universal GATT catalog, advert constants, and the establishment
//!   plan,
//! * **recognition** is observation→decision (pull model): the app pushes an
//!   advert at `recognize()`; the FFI returns a `Recognition` carrying the
//!   matched signature's `runtime_scope`,
//! * **establishment** is a sequence of step verbs (BLE-only in the MVP)
//!   walked by the app-side dispatcher.
//!
//! Plan: `docs/plans/ios-rewrite-p0-p1-ble-mvp.md`. §11 is the contract
//! tiebreaker; everywhere this module makes a contract decision the §11
//! reference is named in a doc comment so the source and the plan stay in
//! sync. Where §11 specifies more than this MVP needs (e.g. `bleNotify` with
//! `until: Matches { pattern }`), the type is present and parses; the
//! dispatcher side lands in P1.

pub mod eval;
pub mod parse;
pub mod types;

pub use parse::ResolvedManufacturerIndex;
pub use types::{
    AcquireFirmwareStep, AcquireSource, AcquireStep, BleAdvertConstants, BleAdvertMfgData,
    BleAdvertRequire, BleAdvertSignature, BleConnectStep, BleDiscoverServicesStep, BleNotifyStep,
    BleNotifyUntil, BleReadStep, BleRequestMtuStep, BleSubscribeStep, BleWriteStep, CccdMode,
    Confidence, Encoding, EstablishmentBlock, FamilyBleBlock, FamilyBlock, IfStep, IndexedModel,
    ManufacturerIndex, MfgByteAssertion, MfgByteCapture, ModelView, NotifyCapture, Predicate,
    PredicateOp, Signature, SignatureKind, Step, StepOptions, StepValue, SuggestsBlock, Transform,
};
