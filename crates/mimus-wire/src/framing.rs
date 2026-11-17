//! Converts a TCP byte stream into PVA frames.
//!
//! The framer is independent of I/O: it consumes a [`BytesMut`] filled
//! by the caller and extracts frames until more data is needed. This keeps
//! framing tests independent of sockets and fast enough to run on every commit.
//!
//! Payloads are returned as [`Bytes`], split from the input buffer without
//! copying. This avoids per-frame allocations during learning, where MIMUS
//! inspects every update. Keeping this overhead low is essential to minimizing
//! per-event processing costs.
//!
use bytes::{Buf, Bytes, BytesMut};
use std::fmt;

use crate::header::{DEFAULT_MAX_PAYLOAD, HEADER_SIZE, Header, HeaderError, Segmentation};

/// A complete message. Segmented messages are reassembled into a single
/// frame using the header from the first segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: Header,
    /// Empty for control messages.
    pub payload: Bytes,
}

impl Frame {
    #[inline]
    pub fn is_control(&self) -> bool {
        self.header.is_control()
    }

    #[inline]
    pub fn command(&self) -> u8 {
        self.header.command
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}B", self.header, self.payload.len())
    }
}

/// Errors encountered while framing a stream.
/// All variants are fatal to the connection: PVA provides no
/// resynchronization point, so an invalid stream cannot be recovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Header(HeaderError),
    /// Payload length exceeds the configured limit.
    PayloadTooLarge {
        size: u32,
        max: u32,
    },
    /// A `Middle` or `Last` segment arrived without a preceding `First`.
    OrphanSegment(Segmentation),
    /// A `First` segment arrived before the previous message was fully reassembled.
    NestedSegment,
    /// Reassembled message exceeds the configured limit.
    ReassemblyTooLarge {
        size: usize,
        max: u32,
    },
}

impl From<HeaderError> for FrameError {
    fn from(e: HeaderError) -> Self {
        Self::Header(e)
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header(e) => write!(f, "{e}"),
            Self::PayloadTooLarge { size, max } => {
                write!(f, "payload {size} B exceeds the {max} B limit")
            }
            Self::OrphanSegment(s) => write!(f, "{s} segment with no first segment"),
            Self::NestedSegment => write!(f, "first segment while one was already in progress"),
            Self::ReassemblyTooLarge { size, max } => {
                write!(f, "reassembled message {size} B exceeds the {max} B limit")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Decodes frames from a growing buffer and reassembles segmented messages.
///
/// One framer is used per connection and direction. It holds no socket
/// or byte-order state: the byte order is read from each header, allowing
/// `CTRL_SET_BYTE_ORDER` to be decoded before negotiation is complete.
#[derive(Debug)]
pub struct Framer {
    max_payload: u32,
    partial: Option<Partial>,
    frames_decoded: u64,
    bytes_decoded: u64,
}

#[derive(Debug)]
struct Partial {
    header: Header,
    data: BytesMut,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framer {
    pub fn new() -> Self {
        Self {
            max_payload: DEFAULT_MAX_PAYLOAD,
            partial: None,
            frames_decoded: 0,
            bytes_decoded: 0,
        }
    }

    /// Sets the maximum allowed frame and reassembled message size.
    pub fn with_max_payload(mut self, max: u32) -> Self {
        self.max_payload = max;
        self
    }

    /// Decodes the next complete message from `buf`, consuming it on success.
    ///
    /// Returns `Ok(None)` if the buffer does not contain a complete frame.
    /// No data is consumed, so the caller can append more bytes and retry.
    ///
    /// Segmented messages return `Ok(None)` until the last segment arrives,
    /// then yield the fully reassembled message.
    pub fn next_frame(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        loop {
            if buf.len() < HEADER_SIZE {
                return Ok(None);
            }

            let mut raw = [0u8; HEADER_SIZE];
            raw.copy_from_slice(&buf[..HEADER_SIZE]);
            let header = Header::decode(&raw)?;

            // Control messages have no payload and may appear between
            // segments without affecting message reassembly.
            if header.is_control() {
                buf.advance(HEADER_SIZE);
                self.frames_decoded += 1;
                self.bytes_decoded += HEADER_SIZE as u64;
                return Ok(Some(Frame {
                    header,
                    payload: Bytes::new(),
                }));
            }

            let size = header.payload_size as u32;
            if size > self.max_payload {
                return Err(FrameError::PayloadTooLarge {
                    size,
                    max: self.max_payload,
                });
            }

            let needed = HEADER_SIZE + size as usize;
            if buf.len() < needed {
                return Ok(None);
            }

            buf.advance(HEADER_SIZE);
            let payload = buf.split_to(size as usize).freeze();
            self.frames_decoded += 1;
            self.bytes_decoded += needed as u64;

            match header.segmentation() {
                Segmentation::None => {
                    if self.partial.is_some() {
                        // Unsegmented messages cannot interleave with segmented ones.
                        self.partial = None;
                        return Err(FrameError::NestedSegment);
                    }
                    return Ok(Some(Frame { header, payload }));
                }
                Segmentation::First => {
                    if self.partial.is_some() {
                        self.partial = None;
                        return Err(FrameError::NestedSegment);
                    }
                    let mut data = BytesMut::with_capacity(payload.len() * 2);
                    data.extend_from_slice(&payload);
                    self.partial = Some(Partial { header, data });
                    // Wait for the remaining segments.
                    continue;
                }
                Segmentation::Middle | Segmentation::Last => {
                    let seg = header.segmentation();
                    let Some(partial) = self.partial.as_mut() else {
                        return Err(FrameError::OrphanSegment(seg));
                    };
                    let total = partial.data.len() + payload.len();
                    if total > self.max_payload as usize {
                        self.partial = None;
                        return Err(FrameError::ReassemblyTooLarge {
                            size: total,
                            max: self.max_payload,
                        });
                    }
                    partial.data.extend_from_slice(&payload);
                    if seg == Segmentation::Middle {
                        continue;
                    }
                    let partial = self.partial.take().expect("checked above");
                    // Keep the first segment's header, clear segmentation flags,
                    // and update the payload length to the reassembled size.
                    let mut header = partial.header;
                    header.flags &= !crate::header::flags::SEG_MASK;
                    header.payload_size = partial.data.len() as i32;
                    return Ok(Some(Frame {
                        header,
                        payload: partial.data.freeze(),
                    }));
                }
            }
        }
    }

    /// Returns `true` if a segmented message is being reassembled.
    /// If the connection closes in this state, the message is incomplete.
    #[inline]
    pub fn is_reassembling(&self) -> bool {
        self.partial.is_some()
    }

    #[inline]
    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }

    #[inline]
    pub fn bytes_decoded(&self) -> u64 {
        self.bytes_decoded
    }

    /// Discards any incomplete message. Call when reconnecting.
    pub fn reset(&mut self) {
        self.partial = None;
    }
}

/// Writes a message to an output buffer, starting with its header.
///
/// Complements [`Framer::next_frame`]. Both functions must remain
/// consistent when framing logic changes.
pub fn write_frame(out: &mut BytesMut, header: &Header, payload: &[u8]) {
    out.extend_from_slice(&header.encode());
    if header.is_application() {
        out.extend_from_slice(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands;
    use crate::header::{ByteOrder, Direction};

    fn app(payload: &[u8], seg: Segmentation) -> BytesMut {
        let h = Header::application(
            commands::MONITOR,
            payload.len() as i32,
            ByteOrder::Little,
            Direction::FromServer,
            seg,
        );
        let mut b = BytesMut::new();
        write_frame(&mut b, &h, payload);
        b
    }

    fn ctrl(command: u8, data: i32) -> BytesMut {
        let h = Header::control(command, data, ByteOrder::Little, Direction::FromServer);
        let mut b = BytesMut::new();
        write_frame(&mut b, &h, &[]);
        b
    }

    #[test]
    fn decodes_a_single_message() {
        let mut buf = app(b"hello", Segmentation::None);
        let mut f = Framer::new();
        let frame = f.next_frame(&mut buf).unwrap().unwrap();
        assert_eq!(&frame.payload[..], b"hello");
        assert!(buf.is_empty());
        assert!(f.next_frame(&mut buf).unwrap().is_none());
    }

    #[test]
    fn decodes_back_to_back_messages_in_one_buffer() {
        // Simulates a single socket read containing many small frames.
        let mut buf = BytesMut::new();
        for i in 0..1000u32 {
            buf.extend_from_slice(&app(&i.to_le_bytes(), Segmentation::None));
        }
        let mut f = Framer::new();
        for i in 0..1000u32 {
            let frame = f.next_frame(&mut buf).unwrap().unwrap();
            assert_eq!(&frame.payload[..], &i.to_le_bytes());
        }
        assert!(f.next_frame(&mut buf).unwrap().is_none());
        assert_eq!(f.frames_decoded(), 1000);
    }

    #[test]
    fn a_partial_frame_consumes_nothing() {
        let full = app(b"payload", Segmentation::None);
        let mut f = Framer::new();
        // Feed one byte at a time and ensure no data is consumed
        // until the complete frame is available.
        let mut buf = BytesMut::new();
        for (i, b) in full.iter().enumerate() {
            buf.extend_from_slice(&[*b]);
            let got = f.next_frame(&mut buf).unwrap();
            if i + 1 < full.len() {
                assert!(got.is_none(), "frame emitted early at byte {i}");
                assert_eq!(buf.len(), i + 1, "buffer disturbed at byte {i}");
            } else {
                assert_eq!(&got.unwrap().payload[..], b"payload");
            }
        }
    }

    #[test]
    fn control_frames_have_no_payload() {
        let mut buf = ctrl(commands::CTRL_SET_BYTE_ORDER, 0);
        let mut f = Framer::new();
        let frame = f.next_frame(&mut buf).unwrap().unwrap();
        assert!(frame.is_control());
        assert!(frame.payload.is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn reassembles_a_segmented_message() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&app(b"aaa", Segmentation::First));
        buf.extend_from_slice(&app(b"bbb", Segmentation::Middle));
        buf.extend_from_slice(&app(b"ccc", Segmentation::Last));

        let mut f = Framer::new();
        let frame = f.next_frame(&mut buf).unwrap().unwrap();
        assert_eq!(&frame.payload[..], b"aaabbbccc");
        // The caller receives a complete, unsegmented message.
        assert_eq!(frame.header.segmentation(), Segmentation::None);
        assert_eq!(frame.header.payload_size, 9);
        assert!(!f.is_reassembling());
    }

    #[test]
    fn control_frames_may_interleave_with_segments() {
        // PVXS may send flow-control markers between message segments.
        // They must be delivered without interrupting reassembly.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&app(b"aaa", Segmentation::First));
        buf.extend_from_slice(&ctrl(commands::CTRL_MARK_TOTAL_SENT, 123));
        buf.extend_from_slice(&app(b"bbb", Segmentation::Last));

        let mut f = Framer::new();
        let marker = f.next_frame(&mut buf).unwrap().unwrap();
        assert!(marker.is_control());
        assert_eq!(marker.header.payload_size, 123);
        assert!(f.is_reassembling());

        let frame = f.next_frame(&mut buf).unwrap().unwrap();
        assert_eq!(&frame.payload[..], b"aaabbb");
    }

    #[test]
    fn segmented_message_split_across_reads() {
        let mut whole = BytesMut::new();
        whole.extend_from_slice(&app(b"aaa", Segmentation::First));
        whole.extend_from_slice(&app(b"bbb", Segmentation::Last));

        let mut f = Framer::new();
        let mut buf = BytesMut::new();
        let mut out = None;
        for b in whole.iter() {
            buf.extend_from_slice(&[*b]);
            if let Some(frame) = f.next_frame(&mut buf).unwrap() {
                out = Some(frame);
            }
        }
        assert_eq!(&out.unwrap().payload[..], b"aaabbb");
    }

    #[test]
    fn rejects_orphan_segments() {
        let mut buf = app(b"x", Segmentation::Last);
        let mut f = Framer::new();
        assert_eq!(
            f.next_frame(&mut buf),
            Err(FrameError::OrphanSegment(Segmentation::Last))
        );
    }

    #[test]
    fn rejects_nested_segments() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&app(b"a", Segmentation::First));
        buf.extend_from_slice(&app(b"b", Segmentation::First));
        let mut f = Framer::new();
        assert_eq!(f.next_frame(&mut buf), Err(FrameError::NestedSegment));
    }

    #[test]
    fn rejects_oversized_payload_without_allocating() {
        let h = Header::application(
            commands::MONITOR,
            1_000_000,
            ByteOrder::Little,
            Direction::FromServer,
            Segmentation::None,
        );
        // Only the header is present. The framer must reject the declared
        // payload size immediately, without waiting for the payload to arrive.
        let mut buf = BytesMut::from(&h.encode()[..]);
        let mut f = Framer::new().with_max_payload(1024);
        assert_eq!(
            f.next_frame(&mut buf),
            Err(FrameError::PayloadTooLarge {
                size: 1_000_000,
                max: 1024
            })
        );
    }

    #[test]
    fn rejects_oversized_reassembly() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&app(&[0u8; 40], Segmentation::First));
        buf.extend_from_slice(&app(&[0u8; 40], Segmentation::Last));
        let mut f = Framer::new().with_max_payload(64);
        assert!(matches!(
            f.next_frame(&mut buf),
            Err(FrameError::ReassemblyTooLarge { .. })
        ));
        assert!(!f.is_reassembling());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = app(b"x", Segmentation::None);
        buf[0] = 0x00;
        let mut f = Framer::new();
        assert!(matches!(
            f.next_frame(&mut buf),
            Err(FrameError::Header(HeaderError::BadMagic(0x00)))
        ));
    }

    #[test]
    fn empty_payload_is_a_valid_message() {
        let mut buf = app(b"", Segmentation::None);
        let mut f = Framer::new();
        let frame = f.next_frame(&mut buf).unwrap().unwrap();
        assert!(frame.payload.is_empty());
        assert!(frame.header.is_application());
    }

    #[test]
    fn reset_drops_partial_reassembly() {
        let mut buf = app(b"aaa", Segmentation::First);
        let mut f = Framer::new();
        assert!(f.next_frame(&mut buf).unwrap().is_none());
        assert!(f.is_reassembling());
        f.reset();
        assert!(!f.is_reassembling());
    }

    #[test]
    fn payload_is_split_not_copied() {
        // The payload must share the original buffer's allocation.
        // Copying it would add one allocation per update during calibration.
        let mut buf = app(b"0123456789abcdef", Segmentation::None);
        let base = buf.as_ptr();
        let mut f = Framer::new();
        let frame = f.next_frame(&mut buf).unwrap().unwrap();
        let expected = unsafe { base.add(HEADER_SIZE) };
        assert_eq!(frame.payload.as_ptr(), expected);
    }
}
