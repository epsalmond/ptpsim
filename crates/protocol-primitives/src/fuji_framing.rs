//! Fuji's compressed PTP/IP command framing (id `fuji-compressed-v1`).
//!
//! After `InitCommandRequest` (which uses the standard framing in `ptp-core`),
//! the GFX switches the command channel to a narrower frame:
//!
//! ```text
//! offset 0x00  u32  total length (incl. this 12-byte header)
//! offset 0x04  u16  packet type (1=OpRequest, 2=OpResponse, 9=StartData, 10=Data, 12=EndData)
//! offset 0x06  u16  PTP opcode (request) / response code (response)
//! offset 0x08  u32  transaction id
//! offset 0x0c  ...  parameters (u32 each) or data payload
//! ```
//!
//! Source: `client application/apps/apple/docs/parse_v6_ptpip.py`, corroborated against the
//! fw0230 wire capture. Note there is no `DataPhaseInfo` field here (unlike the
//! standard framing). The data-phase payload layout (StartData total length,
//! Data/EndData bodies) is modelled self-consistently and is flagged for
//! byte-exact reconciliation against the event/data capture (see DESIGN open
//! decisions).

use crate::error::FramingError;
use ptp_core::container::*;
use ptp_core::datatype::{Reader, Writer};
use ptp_core::error::DecodeError;
use ptp_core::PtpIpPacket;

mod ty {
    pub const OP_REQUEST: u16 = 1;
    pub const OP_RESPONSE: u16 = 2;
    pub const START_DATA: u16 = 9;
    pub const DATA: u16 = 10;
    pub const END_DATA: u16 = 12;
}

/// Encode a packet in Fuji compressed framing. `InitCommandRequest`/`Ack` and
/// `Event` are not part of this channel and return an error.
pub fn encode(pkt: &PtpIpPacket) -> Result<Vec<u8>, FramingError> {
    let mut w = Writer::new();
    w.u32(0); // length placeholder
    match pkt {
        PtpIpPacket::OperationRequest(p) => {
            w.u16(ty::OP_REQUEST);
            w.u16(p.code);
            w.u32(p.transaction_id);
            for v in &p.params {
                w.u32(*v);
            }
        }
        PtpIpPacket::OperationResponse(p) => {
            w.u16(ty::OP_RESPONSE);
            w.u16(p.code);
            w.u32(p.transaction_id);
            for v in &p.params {
                w.u32(*v);
            }
        }
        PtpIpPacket::StartData(p) => {
            w.u16(ty::START_DATA);
            w.u16(0);
            w.u32(p.transaction_id);
            w.u64(p.total_length);
        }
        PtpIpPacket::Data(p) => {
            w.u16(ty::DATA);
            w.u16(0);
            w.u32(p.transaction_id);
            w.bytes(&p.payload);
        }
        PtpIpPacket::EndData(p) => {
            w.u16(ty::END_DATA);
            w.u16(0);
            w.u32(p.transaction_id);
            w.bytes(&p.payload);
        }
        PtpIpPacket::InitCommandRequest(_)
        | PtpIpPacket::InitCommandAck(_)
        | PtpIpPacket::Event(_) => {
            // Init uses standard framing; events ride the event socket.
            return Err(FramingError::NotOnCompressedChannel);
        }
    }
    let len = w.len() as u32;
    w.patch_u32(0, len);
    Ok(w.into_vec())
}

/// Decode one Fuji compressed frame.
pub fn decode(bytes: &[u8]) -> Result<PtpIpPacket, DecodeError> {
    let mut r = Reader::new(bytes);
    let length = r.u32()? as usize;
    if length != bytes.len() {
        return Err(DecodeError::LengthMismatch {
            declared: length,
            actual: bytes.len(),
        });
    }
    let ptype = r.u16()?;
    let code = r.u16()?;
    let tid = r.u32()?;
    Ok(match ptype {
        ty::OP_REQUEST => {
            let mut params = Vec::new();
            while r.remaining() >= 4 {
                params.push(r.u32()?);
            }
            // Compressed framing carries no DataPhaseInfo; default to 1.
            PtpIpPacket::OperationRequest(OperationRequest {
                data_phase_info: 1,
                code,
                transaction_id: tid,
                params,
            })
        }
        ty::OP_RESPONSE => {
            let mut params = Vec::new();
            while r.remaining() >= 4 {
                params.push(r.u32()?);
            }
            PtpIpPacket::OperationResponse(OperationResponse {
                code,
                transaction_id: tid,
                params,
            })
        }
        ty::START_DATA => PtpIpPacket::StartData(StartData {
            transaction_id: tid,
            total_length: r.u64()?,
        }),
        ty::DATA => PtpIpPacket::Data(DataBlock {
            transaction_id: tid,
            payload: r.rest(),
        }),
        ty::END_DATA => PtpIpPacket::EndData(DataBlock {
            transaction_id: tid,
            payload: r.rest(),
        }),
        other => return Err(DecodeError::UnknownPacketType(other as u32)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(pkt: PtpIpPacket) {
        let bytes = encode(&pkt).unwrap();
        assert_eq!(decode(&bytes).unwrap(), pkt);
    }

    #[test]
    fn open_session_compressed_byte_exact() {
        // OpenSession (0x1002), tid=1, param=1. No DataPhaseInfo in this framing.
        let pkt = PtpIpPacket::OperationRequest(OperationRequest {
            data_phase_info: 1,
            code: 0x1002,
            transaction_id: 1,
            params: vec![1],
        });
        let bytes = encode(&pkt).unwrap();
        assert_eq!(
            bytes,
            vec![
                0x10, 0, 0, 0, // length = 16
                0x01, 0x00, // type = OpRequest
                0x02, 0x10, // opcode 0x1002
                0x01, 0, 0, 0, // tid
                0x01, 0, 0, 0, // param
            ]
        );
        rt(pkt);
    }

    #[test]
    fn response_and_data_phases_round_trip() {
        rt(PtpIpPacket::OperationResponse(OperationResponse {
            code: 0x2001,
            transaction_id: 7,
            params: vec![],
        }));
        rt(PtpIpPacket::StartData(StartData {
            transaction_id: 7,
            total_length: 10_485_760,
        }));
        rt(PtpIpPacket::Data(DataBlock {
            transaction_id: 7,
            payload: vec![1, 2, 3, 4],
        }));
        rt(PtpIpPacket::EndData(DataBlock {
            transaction_id: 7,
            payload: vec![0xFF; 8],
        }));
    }

    #[test]
    fn get_partial_object_compressed_round_trips() {
        rt(PtpIpPacket::OperationRequest(OperationRequest {
            data_phase_info: 1,
            code: 0x101b,
            transaction_id: 14,
            params: vec![5, 0, 0x00a0_0000],
        }));
    }
}
