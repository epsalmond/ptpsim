//! Fuji's compressed PTP/IP command framing (id `fuji-compressed-v1`).
//!
//! After `InitCommandRequest` (which uses the standard framing in `ptp-core`),
//! the GFX switches the command channel to a narrower frame — the PIMA/USB
//! container shape without the standard PTP/IP outer wrapper:
//!
//! ```text
//! offset 0x00  u32  total length (incl. this 12-byte header)
//! offset 0x04  u16  packet type (1=OpRequest, 2=Data, 3=OpResponse)
//! offset 0x06  u16  PTP opcode (request/data) / response code (response)
//! offset 0x08  u32  transaction id
//! offset 0x0c  ...  parameters (u32 each) or data payload
//! ```
//!
//! The data phase is a **single** length-prefixed type-2 frame — even a 14.5 MB
//! `GetObject` or a ~250 KB `0x9018` live-view frame arrives whole; there is no
//! `StartData`/`Data`/`EndData` (9/10/12) sequence on this channel. The type-2
//! Data frame's code field echoes the operation opcode. This mirrors `usb_ptp`'s
//! container model — Fuji reuses the PIMA type numbering; only events differ, as
//! they ride a separate socket and this channel carries none.
//!
//! Reconciled byte-exact against the wireless-tether PCSS captures

//! reference app `:55740` capture `app_real_run_fw0230_wirelevel_v6_20260518T114931Z`
//! (both channels use the identical 1/2/3 single-frame model). See ptpsim #143.

use crate::error::FramingError;
use ptp_core::container::*;
use ptp_core::datatype::{Reader, Writer};
use ptp_core::error::DecodeError;
use ptp_core::PtpIpPacket;

mod ty {
    pub const OP_REQUEST: u16 = 1;
    pub const DATA: u16 = 2;
    pub const OP_RESPONSE: u16 = 3;
}

/// Encode an operation request/response in Fuji compressed framing. A data frame
/// carries the echoed opcode, which the generic `DataBlock` does not hold, so
/// callers build it with [`encode_data`]. `InitCommandRequest`/`Ack` (standard
/// framing) and `Event` (separate socket) are not part of this channel.
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
        PtpIpPacket::StartData(_)
        | PtpIpPacket::Data(_)
        | PtpIpPacket::EndData(_)
        | PtpIpPacket::InitCommandRequest(_)
        | PtpIpPacket::InitCommandAck(_)
        | PtpIpPacket::InitFail(_)
        | PtpIpPacket::Event(_) => {
            // A data phase needs the opcode (use `encode_data`); Fuji has no
            // StartData/EndData; init is standard-framed; events ride their own
            // socket.
            return Err(FramingError::NotOnCompressedChannel);
        }
    }
    let len = w.len() as u32;
    w.patch_u32(0, len);
    Ok(w.into_vec())
}

/// The 12-byte header of a type-2 Data frame carrying `payload_len` payload bytes
/// for operation `op`, transaction `txn`. A streaming emitter writes this then
/// streams the body over the socket (so a multi-MB object never lands in memory
/// yet the wire still sees one frame); [`encode_data`] is this plus the payload.
pub fn data_frame_header(op: u16, txn: u32, payload_len: u32) -> [u8; 12] {
    let total = payload_len
        .checked_add(12)
        .expect("data frame length overflows u32");
    let mut h = [0u8; 12];
    h[0..4].copy_from_slice(&total.to_le_bytes());
    h[4..6].copy_from_slice(&ty::DATA.to_le_bytes());
    h[6..8].copy_from_slice(&op.to_le_bytes());
    h[8..12].copy_from_slice(&txn.to_le_bytes());
    h
}

/// Encode a whole data-phase frame (packet type 2): the [`data_frame_header`]
/// followed by `payload`. The compressed channel delivers the entire data phase
/// in this one length-prefixed frame whose code field echoes the operation `op`;
/// there is no `StartData`/`EndData`.
pub fn encode_data(op: u16, txn: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + payload.len());
    v.extend_from_slice(&data_frame_header(op, txn, payload.len() as u32));
    v.extend_from_slice(payload);
    v
}

/// Decode one Fuji compressed frame. A type-2 Data frame surfaces as `Data`; its
/// echoed opcode is dropped (redundant — the consumer correlates by `txn` with
/// the operation request).
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
        ty::DATA => PtpIpPacket::Data(DataBlock {
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

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn open_session_compressed_byte_exact() {
        // OpenSession (0x1002), tid=1, param=1. Type 1 = OpRequest.
        let pkt = PtpIpPacket::OperationRequest(OperationRequest {
            data_phase_info: 1,
            code: 0x1002,
            transaction_id: 1,
            params: vec![1],
        });
        assert_eq!(
            encode(&pkt).unwrap(),
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
    fn request_and_response_round_trip() {
        rt(PtpIpPacket::OperationRequest(OperationRequest {
            data_phase_info: 1,
            code: 0x101b,
            transaction_id: 14,
            params: vec![5, 0, 0x00a0_0000],
        }));
        rt(PtpIpPacket::OperationResponse(OperationResponse {
            code: 0x2001,
            transaction_id: 7,
            params: vec![],
        }));
    }

    #[test]
    fn data_frame_carries_the_opcode_and_round_trips() {
        let bytes = encode_data(0x9018, 362, &[0xff, 0xd8, 0xff, 0xe0]);
        assert_eq!(
            decode(&bytes).unwrap(),
            PtpIpPacket::Data(DataBlock {
                transaction_id: 362,
                payload: vec![0xff, 0xd8, 0xff, 0xe0],
            })
        );
        // The opcode lands in the code field (offset 0x06).
        assert_eq!(&bytes[4..8], &[0x02, 0x00, 0x18, 0x90]);
    }

    #[test]
    fn encode_rejects_frames_not_on_this_channel() {
        assert!(matches!(
            encode(&PtpIpPacket::Data(DataBlock {
                transaction_id: 1,
                payload: vec![],
            })),
            Err(FramingError::NotOnCompressedChannel)
        ));
        assert!(matches!(
            encode(&PtpIpPacket::StartData(StartData {
                transaction_id: 1,
                total_length: 4,
            })),
            Err(FramingError::NotOnCompressedChannel)
        ));
    }

    // Byte-exact goldens lifted from the wireless-tether PCSS capture
    // 2026-06-02-pcss-ptpip-fuji-original.pcapng (stream 207). See ptpsim #143.
    #[test]
    fn wire_capture_transaction_is_byte_exact() {
        // GetDevicePropValue(0x1015) request, tid 2, param 0xd16e.
        let req = PtpIpPacket::OperationRequest(OperationRequest {
            data_phase_info: 1,
            code: 0x1015,
            transaction_id: 2,
            params: vec![0x0000_d16e],
        });
        let req_bytes = hex("10000000 01001510 02000000 6ed10000");
        assert_eq!(encode(&req).unwrap(), req_bytes);
        assert_eq!(decode(&req_bytes).unwrap(), req);

        // Data(0x1015), tid 2, payload = the u16 value 5 (0x0005).
        let data_bytes = hex("0e000000 02001510 02000000 0500");
        assert_eq!(encode_data(0x1015, 2, &[0x05, 0x00]), data_bytes);
        assert_eq!(
            decode(&data_bytes).unwrap(),
            PtpIpPacket::Data(DataBlock {
                transaction_id: 2,
                payload: vec![0x05, 0x00],
            })
        );

        // OperationResponse(0x2001 OK), tid 2, no params.
        let resp = PtpIpPacket::OperationResponse(OperationResponse {
            code: 0x2001,
            transaction_id: 2,
            params: vec![],
        });
        let resp_bytes = hex("0c000000 03000120 02000000");
        assert_eq!(encode(&resp).unwrap(), resp_bytes);
        assert_eq!(decode(&resp_bytes).unwrap(), resp);
    }
}
