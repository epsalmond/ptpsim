//! Little-endian byte cursor plus the PTP wire datatypes that everything else
//! is built from: fixed-width integers, the length-prefixed UTF-16 PTP string,
//! and the `u32`-counted PTP array.

use crate::error::{DecodeError, EncodeError};

/// Sanity ceiling on a decoded array/string count so a corrupt length field
/// can't trigger a huge allocation.
const MAX_ELEMENTS: u32 = 1 << 24;

/// Cursor over a byte slice. All multi-byte reads are little-endian (PTP/IP).
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::UnexpectedEof {
                offset: self.pos,
                needed: n - self.remaining(),
            });
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    pub fn bytes(&mut self, n: usize) -> Result<Vec<u8>, DecodeError> {
        Ok(self.take(n)?.to_vec())
    }

    /// Read the rest of the buffer.
    pub fn rest(&mut self) -> Vec<u8> {
        let s = self.buf[self.pos..].to_vec();
        self.pos = self.buf.len();
        s
    }

    /// PTP string: `u8` count of UTF-16 code units *including* the trailing
    /// NUL, then that many code units. A count of 0 means the empty string.
    pub fn ptp_string(&mut self) -> Result<String, DecodeError> {
        let count = self.u8()? as usize;
        if count == 0 {
            return Ok(String::new());
        }
        let mut units = Vec::with_capacity(count);
        for _ in 0..count {
            units.push(self.u16()?);
        }
        // Drop the trailing NUL terminator if present.
        if units.last() == Some(&0) {
            units.pop();
        }
        String::from_utf16(&units).map_err(|_| DecodeError::InvalidString("not valid UTF-16"))
    }

    /// PTP array: `u32` element count followed by `count` elements decoded by
    /// `f`.
    pub fn ptp_array<T, F>(&mut self, mut f: F) -> Result<Vec<T>, DecodeError>
    where
        F: FnMut(&mut Reader<'a>) -> Result<T, DecodeError>,
    {
        let count = self.u32()?;
        if count > MAX_ELEMENTS {
            return Err(DecodeError::ArrayTooLong(count));
        }
        let mut out = Vec::with_capacity(count as usize);
        for _ in 0..count {
            out.push(f(self)?);
        }
        Ok(out)
    }
}

/// Append-only little-endian writer.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// Patch a previously written `u32` at `offset` (used to backfill a length
    /// prefix once the body size is known).
    pub fn patch_u32(&mut self, offset: usize, v: u32) {
        self.buf[offset..offset + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// PTP string (see [`Reader::ptp_string`]). Empty string writes a single
    /// `0x00` count byte.
    pub fn ptp_string(&mut self, s: &str) -> Result<(), EncodeError> {
        if s.is_empty() {
            self.u8(0);
            return Ok(());
        }
        let units: Vec<u16> = s.encode_utf16().collect();
        let count = units.len() + 1; // include NUL terminator
        if count > 255 {
            return Err(EncodeError::StringTooLong(count));
        }
        self.u8(count as u8);
        for u in units {
            self.u16(u);
        }
        self.u16(0); // NUL terminator
        Ok(())
    }

    pub fn ptp_array<T, F>(&mut self, items: &[T], mut f: F)
    where
        F: FnMut(&mut Writer, &T),
    {
        self.u32(items.len() as u32);
        for it in items {
            f(self, it);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints_round_trip_le() {
        let mut w = Writer::new();
        w.u8(0x12);
        w.u16(0x3456);
        w.u32(0x789a_bcde);
        w.u64(0x0102_0304_0506_0708);
        assert_eq!(
            w.as_slice(),
            &[
                0x12, 0x56, 0x34, 0xde, 0xbc, 0x9a, 0x78, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02,
                0x01
            ]
        );
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 0x12);
        assert_eq!(r.u16().unwrap(), 0x3456);
        assert_eq!(r.u32().unwrap(), 0x789a_bcde);
        assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn ptp_string_byte_exact() {
        // "Hi" -> count=3 (H,i,NUL), then 'H' 'i' NUL as UTF-16LE.
        let mut w = Writer::new();
        w.ptp_string("Hi").unwrap();
        assert_eq!(w.as_slice(), &[0x03, b'H', 0x00, b'i', 0x00, 0x00, 0x00]);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.ptp_string().unwrap(), "Hi");
    }

    #[test]
    fn empty_ptp_string_is_single_zero() {
        let mut w = Writer::new();
        w.ptp_string("").unwrap();
        assert_eq!(w.as_slice(), &[0x00]);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.ptp_string().unwrap(), "");
    }

    #[test]
    fn ptp_array_round_trips() {
        let vals: Vec<u16> = vec![100, 200, 400, 800];
        let mut w = Writer::new();
        w.ptp_array(&vals, |w, v| w.u16(*v));
        // count=4 then four LE u16
        assert_eq!(w.as_slice()[0..4], [0x04, 0, 0, 0]);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        let back = r.ptp_array(|r| r.u16()).unwrap();
        assert_eq!(back, vals);
    }

    #[test]
    fn eof_is_reported_with_offset() {
        let mut r = Reader::new(&[0x01, 0x02]);
        assert_eq!(r.u16().unwrap(), 0x0201);
        assert_eq!(
            r.u8(),
            Err(DecodeError::UnexpectedEof {
                offset: 2,
                needed: 1
            })
        );
    }
}
