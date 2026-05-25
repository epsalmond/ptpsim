//! PTP-over-USB container framing (id `usb-ptp`). The classic PIMA 15740 USB
//! transport container — distinct from both PTP/IP standard framing and Fuji's
//! compressed framing:
//!
//! ```text
//! offset 0x00  u32  container length (incl. this 12-byte header)
//! offset 0x04  u16  container type (1=Command/Op, 2=Data, 3=Response, 4=Event)
//! offset 0x06  u16  operation / response / event code
//! offset 0x08  u32  transaction id
//! offset 0x0c  ...  parameters (u32 each) or data payload
//! ```
//!
//! On USB the data phase is its own bulk transfer (one type-2 container, often
//! split across URBs by the host stack), not the StartData/EndData sequence of
//! PTP/IP. We model the control containers (op/response/event) here; a decoded
//! type-2 container surfaces as `Data` (its `code` is dropped — only the bytes
//! matter to the simulator, and the bulk is never a golden packet).

use crate::error::FramingError;
use ptp_core::container::*;
use ptp_core::datatype::{Reader, Writer};
use ptp_core::error::DecodeError;
use ptp_core::PtpIpPacket;

mod ty {
    pub const COMMAND: u16 = 1;
    pub const DATA: u16 = 2;
    pub const RESPONSE: u16 = 3;
    pub const EVENT: u16 = 4;
}

/// Encode a control container (op/response/event). Data phases are a transport
/// concern on USB, not a single re-emittable logical container, so they are not
/// encodable here.
pub fn encode(pkt: &PtpIpPacket) -> Result<Vec<u8>, FramingError> {
    let mut w = Writer::new();
    w.u32(0); // length placeholder
    match pkt {
        PtpIpPacket::OperationRequest(p) => {
            w.u16(ty::COMMAND);
            w.u16(p.code);
            w.u32(p.transaction_id);
            for v in &p.params {
                w.u32(*v);
            }
        }
        PtpIpPacket::OperationResponse(p) => {
            w.u16(ty::RESPONSE);
            w.u16(p.code);
            w.u32(p.transaction_id);
            for v in &p.params {
                w.u32(*v);
            }
        }
        PtpIpPacket::Event(p) => {
            w.u16(ty::EVENT);
            w.u16(p.code);
            w.u32(p.transaction_id);
            for v in &p.params {
                w.u32(*v);
            }
        }
        _ => return Err(FramingError::UnsupportedPacket),
    }
    let len = w.len() as u32;
    w.patch_u32(0, len);
    Ok(w.into_vec())
}

/// Decode one USB-PTP container.
pub fn decode(bytes: &[u8]) -> Result<PtpIpPacket, DecodeError> {
    let mut r = Reader::new(bytes);
    let length = r.u32()? as usize;
    if length != bytes.len() {
        return Err(DecodeError::LengthMismatch { declared: length, actual: bytes.len() });
    }
    let ctype = r.u16()?;
    let code = r.u16()?;
    let tid = r.u32()?;
    let params = |r: &mut Reader| -> Result<Vec<u32>, DecodeError> {
        let mut v = Vec::new();
        while r.remaining() >= 4 {
            v.push(r.u32()?);
        }
        Ok(v)
    };
    Ok(match ctype {
        ty::COMMAND => PtpIpPacket::OperationRequest(OperationRequest {
            data_phase_info: 1,
            code,
            transaction_id: tid,
            params: params(&mut r)?,
        }),
        ty::RESPONSE => PtpIpPacket::OperationResponse(OperationResponse {
            code,
            transaction_id: tid,
            params: params(&mut r)?,
        }),
        ty::EVENT => PtpIpPacket::Event(EventPacket {
            code,
            transaction_id: tid,
            params: params(&mut r)?,
        }),
        ty::DATA => PtpIpPacket::Data(DataBlock { transaction_id: tid, payload: r.rest() }),
        other => return Err(DecodeError::UnknownPacketType(other as u32)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_device_info_command_byte_exact() {
        // GetDeviceInfo (0x1001), tid 0, no params — the first frame seen in the

        let pkt = PtpIpPacket::OperationRequest(OperationRequest {
            data_phase_info: 1,
            code: 0x1001,
            transaction_id: 0,
            params: vec![],
        });
        let bytes = encode(&pkt).unwrap();
        assert_eq!(
            bytes,
            vec![0x0c, 0, 0, 0, 0x01, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(decode(&bytes).unwrap(), pkt);
    }

    #[test]
    fn response_and_event_round_trip() {
        for pkt in [
            PtpIpPacket::OperationResponse(OperationResponse { code: 0x2001, transaction_id: 5, params: vec![] }),
            PtpIpPacket::Event(EventPacket { code: 0x4002, transaction_id: 0, params: vec![3] }),
        ] {
            let bytes = encode(&pkt).unwrap();
            assert_eq!(decode(&bytes).unwrap(), pkt);
        }
    }

    #[test]
    fn data_container_decodes_as_data() {
        // len=16, type=2 (data), code=0x1009, tid=7, 4 payload bytes.
        let bytes = vec![0x10, 0, 0, 0, 0x02, 0x00, 0x09, 0x10, 0x07, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef];
        match decode(&bytes).unwrap() {
            PtpIpPacket::Data(d) => {
                assert_eq!(d.transaction_id, 7);
                assert_eq!(d.payload, vec![0xde, 0xad, 0xbe, 0xef]);
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }
}
