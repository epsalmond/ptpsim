//! Logical PTP/IP packet payloads. These are the framing-independent bodies;
//! the standard TCP wrapper (length + type) is applied in [`crate::framing`].
//! Vendor wire variants (e.g. Fuji's compressed framing) reuse these same
//! payload structs from `protocol-primitives`.

use crate::datatype::{Reader, Writer};
use crate::error::{DecodeError, EncodeError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitCommandRequest {
    pub initiator_guid: [u8; 16],
    pub friendly_name: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitCommandAck {
    pub connection_number: u32,
    pub responder_guid: [u8; 16],
    pub friendly_name: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitFail {
    pub reason: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRequest {
    /// PTP/IP DataPhaseInfo (1 = no data or data-in, 2 = data-out).
    pub data_phase_info: u32,
    pub code: u16,
    pub transaction_id: u32,
    pub params: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationResponse {
    pub code: u16,
    pub transaction_id: u32,
    pub params: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPacket {
    pub code: u16,
    pub transaction_id: u32,
    pub params: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartData {
    pub transaction_id: u32,
    pub total_length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataBlock {
    pub transaction_id: u32,
    pub payload: Vec<u8>,
}

fn guid(r: &mut Reader) -> Result<[u8; 16], DecodeError> {
    let v = r.bytes(16)?;
    let mut g = [0u8; 16];
    g.copy_from_slice(&v);
    Ok(g)
}

fn remaining_params(r: &mut Reader) -> Result<Vec<u32>, DecodeError> {
    let mut params = Vec::with_capacity(r.remaining() / 4);
    while r.remaining() >= 4 {
        params.push(r.u32()?);
    }
    Ok(params)
}

impl InitCommandRequest {
    pub(crate) fn decode_body(r: &mut Reader) -> Result<Self, DecodeError> {
        let initiator_guid = guid(r)?;
        let friendly_name = r.ptp_string()?;
        let protocol_version = r.u32()?;
        Ok(Self {
            initiator_guid,
            friendly_name,
            protocol_version,
        })
    }
    pub(crate) fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.bytes(&self.initiator_guid);
        w.ptp_string(&self.friendly_name)?;
        w.u32(self.protocol_version);
        Ok(())
    }
}

impl InitCommandAck {
    pub(crate) fn decode_body(r: &mut Reader) -> Result<Self, DecodeError> {
        let connection_number = r.u32()?;
        let responder_guid = guid(r)?;
        let friendly_name = r.ptp_string()?;
        let protocol_version = r.u32()?;
        Ok(Self {
            connection_number,
            responder_guid,
            friendly_name,
            protocol_version,
        })
    }
    pub(crate) fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.u32(self.connection_number);
        w.bytes(&self.responder_guid);
        w.ptp_string(&self.friendly_name)?;
        w.u32(self.protocol_version);
        Ok(())
    }
}

impl InitFail {
    pub(crate) fn decode_body(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(Self { reason: r.u32()? })
    }
    pub(crate) fn encode_body(&self, w: &mut Writer) {
        w.u32(self.reason);
    }
}

impl OperationRequest {
    pub(crate) fn decode_body(r: &mut Reader) -> Result<Self, DecodeError> {
        let data_phase_info = r.u32()?;
        let code = r.u16()?;
        let transaction_id = r.u32()?;
        Ok(Self {
            data_phase_info,
            code,
            transaction_id,
            params: remaining_params(r)?,
        })
    }
    pub(crate) fn encode_body(&self, w: &mut Writer) {
        w.u32(self.data_phase_info);
        w.u16(self.code);
        w.u32(self.transaction_id);
        for p in &self.params {
            w.u32(*p);
        }
    }
}

impl OperationResponse {
    pub(crate) fn decode_body(r: &mut Reader) -> Result<Self, DecodeError> {
        let code = r.u16()?;
        let transaction_id = r.u32()?;
        Ok(Self {
            code,
            transaction_id,
            params: remaining_params(r)?,
        })
    }
    pub(crate) fn encode_body(&self, w: &mut Writer) {
        w.u16(self.code);
        w.u32(self.transaction_id);
        for p in &self.params {
            w.u32(*p);
        }
    }
}

impl EventPacket {
    pub(crate) fn decode_body(r: &mut Reader) -> Result<Self, DecodeError> {
        let code = r.u16()?;
        let transaction_id = r.u32()?;
        Ok(Self {
            code,
            transaction_id,
            params: remaining_params(r)?,
        })
    }
    pub(crate) fn encode_body(&self, w: &mut Writer) {
        w.u16(self.code);
        w.u32(self.transaction_id);
        for p in &self.params {
            w.u32(*p);
        }
    }
}

impl StartData {
    pub(crate) fn decode_body(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(Self {
            transaction_id: r.u32()?,
            total_length: r.u64()?,
        })
    }
    pub(crate) fn encode_body(&self, w: &mut Writer) {
        w.u32(self.transaction_id);
        w.u64(self.total_length);
    }
}

impl DataBlock {
    pub(crate) fn decode_body(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(Self {
            transaction_id: r.u32()?,
            payload: r.rest(),
        })
    }
    pub(crate) fn encode_body(&self, w: &mut Writer) {
        w.u32(self.transaction_id);
        w.bytes(&self.payload);
    }
}
