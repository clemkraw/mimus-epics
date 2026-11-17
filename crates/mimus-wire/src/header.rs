//! The 8-byte PVAccess message header.
//!
//! ```text
//! ┌───────┬─────────┬───────┬─────────┬──────────────┐
//! │ magic │ version │ flags │ command │ payload_size │
//! │  1 B  │   1 B   │  1 B  │   1 B   │     4 B      │
//! └───────┴─────────┴───────┴─────────┴──────────────┘
//!   0xCA                                 i32, byte order per flags
//! ```
//!
//! Flags byte:
//!
//! ```text
//! bit 0    0 = application message, 1 = control message
//! bits 1-3 reserved (zero)
//! bits 4-5 segmentation: 00 none, 01 first, 10 last, 11 middle
//! bit 6    0 = from client, 1 = from server
//! bit 7    0 = little endian, 1 = big endian
//! ```
//!
//! For a control message the last four bytes are not a payload length but a
//! command-specific payload carried inside the header itself; a control
//! message never has bytes following it on the wire.

use std::fmt;

/// Every PVA message starts with this byte.
pub const MAGIC: u8 = 0xCA;

/// Protocol version MIMUS speaks.
pub const VERSION: u8 = 2;

/// The header is a fixed 8 bytes.
pub const HEADER_SIZE: usize = 8;

/// Reject frames whose payload exceeds this limit.
/// Protects against corrupted or malicious lengths causing excessive allocations.
/// Can be increased via [`Framer::with_max_payload`](crate::framing::Framer::with_max_payload) if needed.
pub const DEFAULT_MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

/// Raw bit masks of the flags byte.
pub mod flags {
    pub const CONTROL: u8 = 0x01;
    pub const SEG_MASK: u8 = 0x30;
    pub const SEG_NONE: u8 = 0x00;
    pub const SEG_FIRST: u8 = 0x10;
    pub const SEG_LAST: u8 = 0x20;
    pub const SEG_MIDDLE: u8 = 0x30;
    pub const FROM_SERVER: u8 = 0x40;
    pub const BIG_ENDIAN: u8 = 0x80;
}

/// Wire byte order, negotiated once per connection by `CTRL_SET_BYTE_ORDER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum ByteOrder {
    #[default]
    Little,
    Big,
}

impl ByteOrder {
    #[inline]
    pub const fn from_flags(f: u8) -> Self {
        if f & flags::BIG_ENDIAN != 0 {
            Self::Big
        } else {
            Self::Little
        }
    }

    #[inline]
    pub const fn to_bits(self) -> u8 {
        match self {
            Self::Little => 0,
            Self::Big => flags::BIG_ENDIAN,
        }
    }

    #[inline]
    pub const fn read_i32(self, b: [u8; 4]) -> i32 {
        match self {
            Self::Little => i32::from_le_bytes(b),
            Self::Big => i32::from_be_bytes(b),
        }
    }

    #[inline]
    pub const fn write_i32(self, v: i32) -> [u8; 4] {
        match self {
            Self::Little => v.to_le_bytes(),
            Self::Big => v.to_be_bytes(),
        }
    }
}

impl fmt::Display for ByteOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Little => "LE",
            Self::Big => "BE",
        })
    }
}

/// Which side of the connection produced a message.
///
/// MIMUS cares about this in a way an ordinary client does not: while
/// calibrating it consumes server-produced frames, while serving it produces them.
/// A profile built from frames with the wrong direction bit would be wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    FromClient,
    FromServer,
}

impl Direction {
    #[inline]
    pub const fn from_flags(f: u8) -> Self {
        if f & flags::FROM_SERVER != 0 {
            Self::FromServer
        } else {
            Self::FromClient
        }
    }

    #[inline]
    pub const fn to_bits(self) -> u8 {
        match self {
            Self::FromClient => 0,
            Self::FromServer => flags::FROM_SERVER,
        }
    }
}

/// Segmentation state. A large message is split across several frames:
/// `First`, zero or more `Middle`, then `Last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Segmentation {
    None,
    First,
    Middle,
    Last,
}

impl Segmentation {
    #[inline]
    pub const fn from_flags(f: u8) -> Self {
        match f & flags::SEG_MASK {
            flags::SEG_FIRST => Self::First,
            flags::SEG_MIDDLE => Self::Middle,
            flags::SEG_LAST => Self::Last,
            _ => Self::None,
        }
    }

    #[inline]
    pub const fn to_bits(self) -> u8 {
        match self {
            Self::None => flags::SEG_NONE,
            Self::First => flags::SEG_FIRST,
            Self::Middle => flags::SEG_MIDDLE,
            Self::Last => flags::SEG_LAST,
        }
    }

    #[inline]
    pub const fn is_segmented(self) -> bool {
        !matches!(self, Self::None)
    }
}

impl fmt::Display for Segmentation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "complete",
            Self::First => "first",
            Self::Middle => "middle",
            Self::Last => "last",
        })
    }
}

/// A decoded message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub flags: u8,
    pub command: u8,
    /// Payload length for an application message; command-specific data for a control message.
    pub payload_size: i32,
}

/// Why a header could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// First byte was not [`MAGIC`].
    BadMagic(u8),
    /// Payload length was negative.
    NegativePayload(i32),
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic(b) => write!(f, "bad magic byte 0x{b:02X}, expected 0x{MAGIC:02X}"),
            Self::NegativePayload(n) => write!(f, "negative payload size {n}"),
        }
    }
}

impl std::error::Error for HeaderError {}

impl Header {
    /// Decodes a header from [`HEADER_SIZE`] bytes.
    ///
    /// The byte order is determined by the header's flags, independently
    /// of the connection state. This allows `CTRL_SET_BYTE_ORDER` to be
    /// decoded before negotiation is complete.
    pub fn decode(buf: &[u8; HEADER_SIZE]) -> Result<Self, HeaderError> {
        if buf[0] != MAGIC {
            return Err(HeaderError::BadMagic(buf[0]));
        }
        let flags = buf[2];
        let order = ByteOrder::from_flags(flags);
        let payload_size = order.read_i32([buf[4], buf[5], buf[6], buf[7]]);
        let header = Self {
            version: buf[1],
            flags,
            command: buf[3],
            payload_size,
        };
        if !header.is_control() && payload_size < 0 {
            return Err(HeaderError::NegativePayload(payload_size));
        }
        Ok(header)
    }

    /// Encode into a fixed-size array.
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[0] = MAGIC;
        out[1] = self.version;
        out[2] = self.flags;
        out[3] = self.command;
        let n = self.byte_order().write_i32(self.payload_size);
        out[4..8].copy_from_slice(&n);
        out
    }

    /// An application message.
    pub fn application(
        command: u8,
        payload_size: i32,
        order: ByteOrder,
        direction: Direction,
        segmentation: Segmentation,
    ) -> Self {
        Self {
            version: VERSION,
            flags: order.to_bits() | direction.to_bits() | segmentation.to_bits(),
            command,
            payload_size,
        }
    }

    /// Creates a control message with no trailing payload.
    /// The `data` value is stored in the header's last four bytes.
    pub fn control(command: u8, data: i32, order: ByteOrder, direction: Direction) -> Self {
        Self {
            version: VERSION,
            flags: flags::CONTROL | order.to_bits() | direction.to_bits(),
            command,
            payload_size: data,
        }
    }

    #[inline]
    pub const fn is_control(&self) -> bool {
        self.flags & flags::CONTROL != 0
    }

    #[inline]
    pub const fn is_application(&self) -> bool {
        !self.is_control()
    }

    #[inline]
    pub const fn byte_order(&self) -> ByteOrder {
        ByteOrder::from_flags(self.flags)
    }

    #[inline]
    pub const fn direction(&self) -> Direction {
        Direction::from_flags(self.flags)
    }

    #[inline]
    pub const fn segmentation(&self) -> Segmentation {
        Segmentation::from_flags(self.flags)
    }

    /// Bytes this message occupies on the wire with header included.
    #[inline]
    pub const fn wire_size(&self) -> usize {
        if self.is_control() {
            HEADER_SIZE
        } else {
            HEADER_SIZE + self.payload_size as usize
        }
    }
}

impl fmt::Display for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}[cmd=0x{:02X} v{} {} {}",
            if self.is_control() { "ctrl" } else { "app" },
            self.command,
            self.version,
            self.byte_order(),
            match self.direction() {
                Direction::FromClient => "c->s",
                Direction::FromServer => "s->c",
            }
        )?;
        if self.segmentation().is_segmented() {
            write!(f, " {}", self.segmentation())?;
        }
        if self.is_control() {
            write!(f, " data={}]", self.payload_size)
        } else {
            write!(f, " {}B]", self.payload_size)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands;

    #[test]
    fn round_trip_application() {
        let h = Header::application(
            commands::MONITOR,
            42,
            ByteOrder::Little,
            Direction::FromServer,
            Segmentation::None,
        );
        let decoded = Header::decode(&h.encode()).unwrap();
        assert_eq!(h, decoded);
        assert!(decoded.is_application());
        assert_eq!(decoded.payload_size, 42);
        assert_eq!(decoded.direction(), Direction::FromServer);
        assert_eq!(decoded.wire_size(), HEADER_SIZE + 42);
    }

    #[test]
    fn round_trip_control() {
        let h = Header::control(
            commands::CTRL_SET_BYTE_ORDER,
            0,
            ByteOrder::Little,
            Direction::FromServer,
        );
        let decoded = Header::decode(&h.encode()).unwrap();
        assert_eq!(h, decoded);
        assert!(decoded.is_control());
        assert_eq!(decoded.wire_size(), HEADER_SIZE);
    }

    #[test]
    fn big_endian_length_is_read_back_correctly() {
        let h = Header::application(
            commands::MONITOR,
            0x0102_0304,
            ByteOrder::Big,
            Direction::FromServer,
            Segmentation::None,
        );
        let raw = h.encode();
        assert_eq!(&raw[4..8], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(Header::decode(&raw).unwrap().payload_size, 0x0102_0304);
    }

    #[test]
    fn segmentation_bits_round_trip() {
        for seg in [
            Segmentation::None,
            Segmentation::First,
            Segmentation::Middle,
            Segmentation::Last,
        ] {
            let h = Header::application(
                commands::MONITOR,
                8,
                ByteOrder::Little,
                Direction::FromServer,
                seg,
            );
            assert_eq!(Header::decode(&h.encode()).unwrap().segmentation(), seg);
        }
    }

    #[test]
    fn rejects_bad_magic() {
        let mut raw = Header::application(
            commands::MONITOR,
            0,
            ByteOrder::Little,
            Direction::FromServer,
            Segmentation::None,
        )
        .encode();
        raw[0] = 0xAB;
        assert_eq!(Header::decode(&raw), Err(HeaderError::BadMagic(0xAB)));
    }

    #[test]
    fn rejects_negative_payload_on_application_only() {
        let mut raw = Header::application(
            commands::MONITOR,
            0,
            ByteOrder::Little,
            Direction::FromServer,
            Segmentation::None,
        )
        .encode();
        raw[4..8].copy_from_slice(&(-1i32).to_le_bytes());
        assert!(matches!(
            Header::decode(&raw),
            Err(HeaderError::NegativePayload(-1))
        ));

        // A control message may legitimately carry a negative i32.
        let ctrl = Header::control(
            commands::CTRL_ACK_RECEIVED,
            -1,
            ByteOrder::Little,
            Direction::FromServer,
        );
        assert_eq!(Header::decode(&ctrl.encode()).unwrap().payload_size, -1);
    }

    #[test]
    fn flags_are_independent() {
        let h = Header::application(
            commands::MONITOR,
            1,
            ByteOrder::Big,
            Direction::FromClient,
            Segmentation::Middle,
        );
        assert_eq!(h.byte_order(), ByteOrder::Big);
        assert_eq!(h.direction(), Direction::FromClient);
        assert_eq!(h.segmentation(), Segmentation::Middle);
        assert!(h.is_application());
    }
}
