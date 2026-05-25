//! Standard PTP/IP TCP framing: a `u32` little-endian total length (including
//! the 8-byte header) and a `u32` packet type, followed by the payload. This is
//! the baseline codec; vendor framings (Fuji compressed, etc.) live in
//! `protocol-primitives` and reuse the same [`crate::container`] payloads.

use crate::codes::PacketType;
use crate::container::*;
use crate::datatype::{Reader, Writer};
use crate::error::{DecodeError, EncodeError};

/// One PTP/IP packet in the standard framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtpIpPacket {
    InitCommandRequest(InitCommandRequest),
    InitCommandAck(InitCommandAck),
    OperationRequest(OperationRequest),
    OperationResponse(OperationResponse),
    Event(EventPacket),
    StartData(StartData),
    Data(DataBlock),
    EndData(DataBlock),
}

/// Decode/encode by-value over a byte buffer. Encode is fallible only because
/// PTP strings have a 254-unit ceiling.
pub trait PtpCodec: Sized {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError>;
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeError>;
}

impl PtpIpPacket {
    fn packet_type(&self) -> PacketType {
        match self {
            PtpIpPacket::InitCommandRequest(_) => PacketType::InitCommandRequest,
            PtpIpPacket::InitCommandAck(_) => PacketType::InitCommandAck,
            PtpIpPacket::OperationRequest(_) => PacketType::OperationRequest,
            PtpIpPacket::OperationResponse(_) => PacketType::OperationResponse,
            PtpIpPacket::Event(_) => PacketType::Event,
            PtpIpPacket::StartData(_) => PacketType::StartData,
            PtpIpPacket::Data(_) => PacketType::Data,
            PtpIpPacket::EndData(_) => PacketType::EndData,
        }
    }

    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        match self {
            PtpIpPacket::InitCommandRequest(p) => p.encode_body(w)?,
            PtpIpPacket::InitCommandAck(p) => p.encode_body(w)?,
            PtpIpPacket::OperationRequest(p) => p.encode_body(w),
            PtpIpPacket::OperationResponse(p) => p.encode_body(w),
            PtpIpPacket::Event(p) => p.encode_body(w),
            PtpIpPacket::StartData(p) => p.encode_body(w),
            PtpIpPacket::Data(p) => p.encode_body(w),
            PtpIpPacket::EndData(p) => p.encode_body(w),
        }
        Ok(())
    }
}

impl PtpCodec for PtpIpPacket {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        let length = r.u32()? as usize;
        let type_raw = r.u32()?;
        if length != bytes.len() {
            return Err(DecodeError::LengthMismatch { declared: length, actual: bytes.len() });
        }
        let ptype = PacketType::from_u32(type_raw).ok_or(DecodeError::UnknownPacketType(type_raw))?;
        Ok(match ptype {
            PacketType::InitCommandRequest => {
                PtpIpPacket::InitCommandRequest(InitCommandRequest::decode_body(&mut r)?)
            }
            PacketType::InitCommandAck => {
                PtpIpPacket::InitCommandAck(InitCommandAck::decode_body(&mut r)?)
            }
            PacketType::OperationRequest => {
                PtpIpPacket::OperationRequest(OperationRequest::decode_body(&mut r)?)
            }
            PacketType::OperationResponse => {
                PtpIpPacket::OperationResponse(OperationResponse::decode_body(&mut r)?)
            }
            PacketType::Event => PtpIpPacket::Event(EventPacket::decode_body(&mut r)?),
            PacketType::StartData => PtpIpPacket::StartData(StartData::decode_body(&mut r)?),
            PacketType::Data => PtpIpPacket::Data(DataBlock::decode_body(&mut r)?),
            PacketType::EndData => PtpIpPacket::EndData(DataBlock::decode_body(&mut r)?),
            other => return Err(DecodeError::UnknownPacketType(other as u32)),
        })
    }

    fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        let mut w = Writer::new();
        w.u32(0); // length placeholder
        w.u32(self.packet_type() as u32);
        self.encode_body(&mut w)?;
        let len = w.len() as u32;
        w.patch_u32(0, len);
        out.extend_from_slice(w.as_slice());
        Ok(())
    }
}

/// Encode to a fresh `Vec` (convenience over [`PtpCodec::encode`]).
pub fn encode(pkt: &PtpIpPacket) -> Result<Vec<u8>, EncodeError> {
    let mut v = Vec::new();
    pkt.encode(&mut v)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codes::{op, resp};

    fn round_trip(pkt: PtpIpPacket) {
        let bytes = encode(&pkt).unwrap();
        // Length prefix equals the whole frame.
        let declared = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(declared, bytes.len(), "length prefix must cover the frame");
        let back = PtpIpPacket::decode(&bytes).unwrap();
        assert_eq!(back, pkt);
    }

    #[test]
    fn open_session_request_byte_exact() {
        // OpenSession, tid=1, one param (session id = 1), data_phase=1.
        let pkt = PtpIpPacket::OperationRequest(OperationRequest {
            data_phase_info: 1,
            code: op::OPEN_SESSION,
            transaction_id: 1,
            params: vec![1],
        });
        let bytes = encode(&pkt).unwrap();
        assert_eq!(
            bytes,
            vec![
                0x16, 0, 0, 0, // length = 22 (8 header + 4 dataphase + 2 op + 4 tid + 4 param)
                0x06, 0, 0, 0, // type = OperationRequest(6)
                0x01, 0, 0, 0, // data phase info
                0x02, 0x10, // op 0x1002
                0x01, 0, 0, 0, // tid
                0x01, 0, 0, 0, // param: session id
            ]
        );
        round_trip(pkt);
    }

    #[test]
    fn response_ok_round_trips() {
        round_trip(PtpIpPacket::OperationResponse(OperationResponse {
            code: resp::OK,
            transaction_id: 7,
            params: vec![],
        }));
    }

    #[test]
    fn get_partial_object_request_round_trips() {
        round_trip(PtpIpPacket::OperationRequest(OperationRequest {
            data_phase_info: 1,
            code: op::GET_PARTIAL_OBJECT,
            transaction_id: 14,
            params: vec![0x0000_0005, 0x0000_0000, 0x00a0_0000],
        }));
    }

    #[test]
    fn init_command_request_round_trips() {
        round_trip(PtpIpPacket::InitCommandRequest(InitCommandRequest {
            initiator_guid: [0xAB; 16],
            friendly_name: "ptpsim".into(),
            protocol_version: 0x0001_0000,
        }));
    }

    #[test]
    fn init_command_ack_round_trips() {
        round_trip(PtpIpPacket::InitCommandAck(InitCommandAck {
            connection_number: 1,
            responder_guid: [0xCD; 16],
            friendly_name: "GFX100 II".into(),
            protocol_version: 0x0001_0000,
        }));
    }

    #[test]
    fn data_phase_and_event_round_trip() {
        round_trip(PtpIpPacket::StartData(StartData { transaction_id: 9, total_length: 10_485_760 }));
        round_trip(PtpIpPacket::Data(DataBlock { transaction_id: 9, payload: vec![1, 2, 3, 4, 5] }));
        round_trip(PtpIpPacket::EndData(DataBlock { transaction_id: 9, payload: vec![0xFF; 16] }));
        round_trip(PtpIpPacket::Event(EventPacket { code: 0x4002, transaction_id: 0, params: vec![5] }));
    }

    #[test]
    fn length_mismatch_is_rejected() {
        let mut bytes = encode(&PtpIpPacket::OperationResponse(OperationResponse {
            code: resp::OK,
            transaction_id: 1,
            params: vec![],
        }))
        .unwrap();
        bytes.push(0); // trailing junk -> declared length no longer matches
        assert!(matches!(PtpIpPacket::decode(&bytes), Err(DecodeError::LengthMismatch { .. })));
    }
}
