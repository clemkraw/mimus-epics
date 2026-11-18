//! Reading and writing PVA primitive types.
//!
//! Implements only what MIMUS needs on the server side: handling searches,
//! creating channels, and accepting monitor requests. Monitor update values
//! remain opaque bytes and are never decoded, as they are learned directly
//! from captured frames.
//!
//! PVA byte order is negotiated per connection. Every function takes it
//! explicitly rather than relying on global state, since the handshake
//! happens before the byte order is agreed upon.

use crate::header::ByteOrder;

/// Represents a size field, including the absence of a value.
///
/// PVA encodes sizes using one to five bytes:
/// - 1 byte for values below 254
/// - 254 followed by a 4-byte length
/// - 255 for null
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Null,
    Len(usize),
}

impl Size {
    /// Length, treating null as empty.
    #[inline]
    pub fn or_empty(self) -> usize {
        match self {
            Self::Null => 0,
            Self::Len(n) => n,
        }
    }
}

/// Why a buffer could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// Ran off the end of the buffer.
    Truncated { need: usize, have: usize },
    /// A length field was negative.
    NegativeSize(i32),
    /// A string was not valid UTF-8.
    BadUtf8,
    /// A count field was outside what the protocol allows.
    BadCount(i32),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { need, have } => {
                write!(f, "needed {need} bytes, buffer had {have}")
            }
            Self::NegativeSize(n) => write!(f, "negative size {n}"),
            Self::BadUtf8 => f.write_str("string is not valid UTF-8"),
            Self::BadCount(n) => write!(f, "invalid element count {n}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// A cursor over a message payload.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    order: ByteOrder,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], order: ByteOrder) -> Self {
        Self { buf, pos: 0, order }
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.remaining() < n {
            return Err(CodecError::Truncated {
                need: n,
                have: self.remaining(),
            });
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Reads exactly `n` bytes or returns an error.
    /// On failure, consumes nothing, allowing the caller to retry when more data is available.
    pub fn take_exact(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        self.take(n)
    }

    pub fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    pub fn i16(&mut self) -> Result<i16, CodecError> {
        let b: [u8; 2] = self.take(2)?.try_into().expect("2 bytes");
        Ok(match self.order {
            ByteOrder::Little => i16::from_le_bytes(b),
            ByteOrder::Big => i16::from_be_bytes(b),
        })
    }

    pub fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(self.i16()? as u16)
    }

    pub fn i32(&mut self) -> Result<i32, CodecError> {
        let b: [u8; 4] = self.take(4)?.try_into().expect("4 bytes");
        Ok(self.order.read_i32(b))
    }

    /// Skip `n` bytes.
    pub fn skip(&mut self, n: usize) -> Result<(), CodecError> {
        self.take(n).map(|_| ())
    }

    /// Everything not yet consumed.
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    pub fn size(&mut self) -> Result<Size, CodecError> {
        let first = self.u8()?;
        match first {
            255 => Ok(Size::Null),
            254 => {
                let n = self.i32()?;
                if n < 0 {
                    return Err(CodecError::NegativeSize(n));
                }
                Ok(Size::Len(n as usize))
            }
            n => Ok(Size::Len(n as usize)),
        }
    }

    pub fn string(&mut self) -> Result<&'a str, CodecError> {
        let n = self.size()?.or_empty();
        let b = self.take(n)?;
        std::str::from_utf8(b).map_err(|_| CodecError::BadUtf8)
    }
}

/// Append a size field.
pub fn put_size(out: &mut Vec<u8>, n: usize, order: ByteOrder) {
    if n < 254 {
        out.push(n as u8);
    } else {
        out.push(254);
        out.extend_from_slice(&order.write_i32(n as i32));
    }
}

/// Append the null size marker.
pub fn put_null(out: &mut Vec<u8>) {
    out.push(255);
}

pub fn put_string(out: &mut Vec<u8>, s: &str, order: ByteOrder) {
    put_size(out, s.len(), order);
    out.extend_from_slice(s.as_bytes());
}

pub fn put_i16(out: &mut Vec<u8>, v: i16, order: ByteOrder) {
    match order {
        ByteOrder::Little => out.extend_from_slice(&v.to_le_bytes()),
        ByteOrder::Big => out.extend_from_slice(&v.to_be_bytes()),
    }
}

pub fn put_i32(out: &mut Vec<u8>, v: i32, order: ByteOrder) {
    out.extend_from_slice(&order.write_i32(v));
}

/// Appends an OK status with no message.
///
/// PVA encodes this status as `0xFF`, with no following data.
/// MIMUS does not send other status types. Unavailable channels
/// are reported as not found rather than found and then rejected.
pub fn put_status_ok(out: &mut Vec<u8>) {
    out.push(0xFF);
}

#[cfg(test)]
mod tests {
    use super::*;

    const LE: ByteOrder = ByteOrder::Little;

    #[test]
    fn small_sizes_take_one_byte() {
        let mut out = Vec::new();
        put_size(&mut out, 5, LE);
        assert_eq!(out, vec![5]);
        assert_eq!(Reader::new(&out, LE).size().unwrap(), Size::Len(5));
    }

    #[test]
    fn large_sizes_use_the_escape() {
        let mut out = Vec::new();
        put_size(&mut out, 1000, LE);
        assert_eq!(out[0], 254);
        assert_eq!(out.len(), 5);
        assert_eq!(Reader::new(&out, LE).size().unwrap(), Size::Len(1000));
    }

    #[test]
    fn the_boundary_at_254_is_handled_both_ways() {
        for n in [253usize, 254, 255, 256] {
            let mut out = Vec::new();
            put_size(&mut out, n, LE);
            assert_eq!(
                Reader::new(&out, LE).size().unwrap(),
                Size::Len(n),
                "size {n} round trip"
            );
        }
        // 253 fits in one byte; 254 requires the extended encoding.
        let mut a = Vec::new();
        put_size(&mut a, 253, LE);
        assert_eq!(a.len(), 1);
        let mut b = Vec::new();
        put_size(&mut b, 254, LE);
        assert_eq!(b.len(), 5);
    }

    #[test]
    fn null_is_distinct_from_empty() {
        let mut null = Vec::new();
        put_null(&mut null);
        assert_eq!(Reader::new(&null, LE).size().unwrap(), Size::Null);

        let mut empty = Vec::new();
        put_size(&mut empty, 0, LE);
        assert_eq!(Reader::new(&empty, LE).size().unwrap(), Size::Len(0));
    }

    #[test]
    fn strings_round_trip() {
        for s in ["", "MIM:DBL:0", "a".repeat(300).as_str()] {
            let mut out = Vec::new();
            put_string(&mut out, s, LE);
            assert_eq!(Reader::new(&out, LE).string().unwrap(), s);
        }
    }

    #[test]
    fn integers_respect_byte_order() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut out = Vec::new();
            put_i32(&mut out, -12345, order);
            put_i16(&mut out, -321, order);
            let mut r = Reader::new(&out, order);
            assert_eq!(r.i32().unwrap(), -12345);
            assert_eq!(r.i16().unwrap(), -321);
            assert!(r.is_empty());
        }
    }

    #[test]
    fn big_endian_bytes_differ_from_little() {
        let mut le = Vec::new();
        put_i32(&mut le, 1, ByteOrder::Little);
        let mut be = Vec::new();
        put_i32(&mut be, 1, ByteOrder::Big);
        assert_ne!(le, be);
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let mut r = Reader::new(&[0x01, 0x02], LE);
        assert_eq!(r.i32(), Err(CodecError::Truncated { need: 4, have: 2 }));
        // The cursor did not move, so a retry after more data works.
        assert_eq!(r.remaining(), 2);
    }

    #[test]
    fn a_string_claiming_more_than_it_has_is_rejected() {
        // Length says 200, only two bytes follow.
        let buf = [200u8, b'h', b'i'];
        assert!(matches!(
            Reader::new(&buf, LE).string(),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn a_negative_escaped_size_is_rejected() {
        let mut buf = vec![254u8];
        buf.extend_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(
            Reader::new(&buf, LE).size(),
            Err(CodecError::NegativeSize(-1))
        );
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let buf = [2u8, 0xFF, 0xFE];
        assert_eq!(Reader::new(&buf, LE).string(), Err(CodecError::BadUtf8));
    }

    #[test]
    fn ok_status_is_a_single_byte() {
        let mut out = Vec::new();
        put_status_ok(&mut out);
        assert_eq!(out, vec![0xFF]);
    }
}
