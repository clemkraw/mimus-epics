//! Control-plane messages exchanged by a PVA server before data flows.
//!
//! This includes the handshake, search, channel creation, and monitor setup.
//! MIMUS must implement these messages rather than replay them: clients choose
//! their own channel and request IDs, so the corresponding responses cannot
//! be taken from a recording.
//!
//! Everything that follows (the monitor updates themselves) is learned
//! as raw bytes and is not decoded here.

use crate::codec::{CodecError, Reader, put_i16, put_i32, put_size, put_status_ok, put_string};
use crate::header::ByteOrder;

/// Server GUID: 12 bytes identifying a server instance.
pub const GUID_LEN: usize = 12;

/// A client request to search for channels on a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub sequence_id: i32,
    pub flags: u8,
    /// Port on which the client wants to receive the response.
    /// Zero over TCP, where the response is sent over the same connection.
    pub response_port: u16,
    /// Search instance IDs paired with the channel names being queried.
    pub channels: Vec<(i32, String)>,
}

impl SearchRequest {
    /// Decodes a search request payload.
    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        let sequence_id = r.i32()?;
        let flags = r.u8()?;
        r.skip(3)?; // reserved
        r.skip(16)?; // response address, IPv6 form
        let response_port = r.u16()?;

        // Transport protocols supported by the client. MIMUS supports TCP
        // only, so the list is consumed but otherwise ignored. Skipping it
        // is necessary to keep subsequent fields correctly aligned.
        let protocols = r.size()?.or_empty();
        for _ in 0..protocols {
            let _ = r.string()?;
        }

        let count = r.i16()?;
        if count < 0 {
            return Err(CodecError::BadCount(count as i32));
        }
        let mut channels = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let id = r.i32()?;
            let name = r.string()?.to_string();
            channels.push((id, name));
        }
        Ok(Self {
            sequence_id,
            flags,
            response_port,
            channels,
        })
    }
}

impl SearchRequest {
    /// Encodes a search request as sent by a client over TCP.
    ///
    /// The client-side protocol implementation exists so that
    /// `mimus calibrate` can communicate with a reference IOC.
    /// It is used with `softIocPVX`, never with `mimus serve`.
    /// The integration suite uses its own hand-written client to ensure
    /// that bugs in this encoder cannot be masked by matching bugs
    /// in the server implementation.
    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.channels.len() * 24);
        put_i32(&mut out, self.sequence_id, order);
        out.push(self.flags);
        out.extend_from_slice(&[0u8; 3]);
        // A zeroed response address asks the server to reply over the
        // connection it was reached on, as expected for a TCP search.
        out.extend_from_slice(&[0u8; 16]);
        let p = self.response_port;
        match order {
            ByteOrder::Little => out.extend_from_slice(&p.to_le_bytes()),
            ByteOrder::Big => out.extend_from_slice(&p.to_be_bytes()),
        }
        put_size(&mut out, 1, order);
        put_string(&mut out, "tcp", order);
        put_i16(&mut out, self.channels.len() as i16, order);
        for (id, name) in &self.channels {
            put_i32(&mut out, *id, order);
            put_string(&mut out, name, order);
        }
        out
    }
}

/// The server's response to a channel search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResponse<'a> {
    pub guid: &'a [u8; GUID_LEN],
    pub sequence_id: i32,
    pub server_port: u16,
    /// Search instance IDs claimed by the server, selected from the request.
    pub found: &'a [i32],
}

impl SearchResponse<'_> {
    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::with_capacity(48 + self.found.len() * 4);
        out.extend_from_slice(self.guid);
        put_i32(&mut out, self.sequence_id, order);
        // A zeroed server address tells the client to reuse the address it
        // connected to. This is important when MIMUS is behind a port
        // mapping and cannot reliably determine its externally visible address.
        out.extend_from_slice(&[0u8; 16]);
        let p = self.server_port;
        match order {
            ByteOrder::Little => out.extend_from_slice(&p.to_le_bytes()),
            ByteOrder::Big => out.extend_from_slice(&p.to_be_bytes()),
        }
        put_string(&mut out, "tcp", order);
        out.push(u8::from(!self.found.is_empty()));
        put_i16(&mut out, self.found.len() as i16, order);
        for id in self.found {
            put_i32(&mut out, *id, order);
        }
        out
    }
}

/// A decoded server response to a channel search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchFound {
    pub guid: [u8; GUID_LEN],
    pub sequence_id: i32,
    pub server_port: u16,
    /// Search instance IDs reported by the server.
    pub found: Vec<i32>,
}

impl SearchFound {
    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        let mut guid = [0u8; GUID_LEN];
        guid.copy_from_slice(r.take_exact(GUID_LEN)?);
        let sequence_id = r.i32()?;
        r.skip(16)?;
        let server_port = r.u16()?;
        let _protocol = r.string()?;
        let any = r.u8()?;
        let count = r.i16()?;
        if count < 0 {
            return Err(CodecError::BadCount(count as i32));
        }
        let mut found = Vec::with_capacity(count as usize);
        for _ in 0..count {
            found.push(r.i32()?);
        }
        // The flag indicates whether the server found any channels.
        // Accept both an empty list and a zero flag as "no channels found".
        if any == 0 {
            found.clear();
        }
        Ok(Self {
            guid,
            sequence_id,
            server_port,
            found,
        })
    }
}

/// A client request to create channels.
/// PVXS clients commonly batch multiple channels into a single request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateChannelRequest {
    /// Client channel IDs paired with channel names.
    pub channels: Vec<(i32, String)>,
}

impl CreateChannelRequest {
    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        let count = r.i16()?;
        if count < 0 {
            return Err(CodecError::BadCount(count as i32));
        }
        let mut channels = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let cid = r.i32()?;
            let name = r.string()?.to_string();
            channels.push((cid, name));
        }
        Ok(Self { channels })
    }
}

impl CreateChannelRequest {
    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.channels.len() * 24);
        put_i16(&mut out, self.channels.len() as i16, order);
        for (cid, name) in &self.channels {
            put_i32(&mut out, *cid, order);
            put_string(&mut out, name, order);
        }
        out
    }
}

/// Response granting access to a single channel.
/// One response is sent per channel, even when the request is batched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateChannelResponse {
    pub client_cid: i32,
    pub server_cid: i32,
}

impl CreateChannelResponse {
    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        put_i32(&mut out, self.client_cid, order);
        put_i32(&mut out, self.server_cid, order);
        put_status_ok(&mut out);
        // Access rights, deprecated but still on the wire.
        put_i16(&mut out, 0, order);
        out
    }
}

impl CreateChannelResponse {
    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        Ok(Self {
            client_cid: r.i32()?,
            server_cid: r.i32()?,
        })
    }
}

/// A monitor request. Only the routing fields are decoded; the `pvRequest`
/// structure following an INIT is left untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRequest {
    pub server_cid: i32,
    pub request_id: i32,
    pub sub_command: u8,
}

impl MonitorRequest {
    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        Ok(Self {
            server_cid: r.i32()?,
            request_id: r.i32()?,
            sub_command: r.u8()?,
        })
    }

    #[inline]
    pub fn is_init(&self) -> bool {
        self.sub_command & crate::commands::monitor::INIT != 0
    }

    /// Transition from Stopped to Running.
    ///
    /// Both bits of the state mask are required, not just the low bit.
    /// `0x04` alone means STOP. Treating the sub-command as a set of
    /// independent flags would mistake STOP for START. A permissive server
    /// may silently accept this, while a conforming server does not.
    #[inline]
    pub fn is_start(&self) -> bool {
        use crate::commands::monitor;
        self.sub_command & monitor::STATE_MASK == monitor::START
    }

    /// Transition from Running to Stopped.
    #[inline]
    pub fn is_stop(&self) -> bool {
        use crate::commands::monitor;
        self.sub_command & monitor::STATE_MASK == monitor::STOP
    }

    /// Whether this message terminates the monitor subscription.
    #[inline]
    pub fn is_destroy(&self) -> bool {
        self.sub_command & crate::commands::monitor::DESTROY != 0
    }

    /// Whether this message carries a flow-control window update.
    ///
    /// The bit is `0x80`. It was previously `0x40`, which is part of
    /// the START encoding and unrelated to pipelining. As a result,
    /// every ordinary START was incorrectly treated as pipelined.
    #[inline]
    pub fn wants_pipeline(&self) -> bool {
        self.sub_command & crate::commands::monitor::PIPELINE != 0
    }

    /// Whether this message carries a flow-control credit update.
    ///
    /// With pipelining, the client periodically tells the server how much
    /// capacity remains in its receive window. A server that ignores these
    /// updates may send data without respecting the client's available
    /// capacity, which a real PVXS server does not do.
    #[inline]
    pub fn is_pipeline_ack(&self) -> bool {
        self.sub_command & crate::commands::monitor::PIPELINE != 0
    }
}

/// A flow-control credit update indicating how many more updates the client can accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineAck {
    pub request_id: i32,
    pub free: i32,
}

impl PipelineAck {
    /// Decodes the credit count following the sub-command byte.
    ///
    /// This layout must be confirmed by observing a real PVXS client
    /// during calibration. Until then, strict parsing is opt-in.
    /// A decoding failure must be treated as unknown credit, not zero:
    /// zero credit would stall the subscription.
    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        let _server_cid = r.i32()?;
        let request_id = r.i32()?;
        let _sub_command = r.u8()?;
        let free = r.i32()?;
        Ok(Self { request_id, free })
    }
}

/// Encodes the monitor INIT pvRequest `field()`.
///
/// This is a structure containing one empty structure named `field`,
/// which selects all fields of the channel, matching what `pvxmonitor`
/// requests.
///
/// A null marker also means "no restriction", but is not equivalent.
/// Against QSRV2, a null request produced a complete value in every
/// update, with all thirty-odd leaves marked as changed: 174 bytes
/// instead of the 41-byte delta sent to `pvxmonitor`.
///
/// Calibrating with a null request would therefore make MIMUS emit
/// updates roughly four times larger than those received by a real
/// subscriber, invalidating all byte-rate measurements.
pub fn field_request() -> Vec<u8> {
    vec![
        0x80, // structure
        0x00, // no type id
        0x01, // one member
        0x05, b'f', b'i', b'e', b'l', b'd', 0x80, // the member is itself a structure
        0x00, // no type id
        0x00, // with no members: select everything
    ]
}

impl MonitorRequest {
    /// Encodes a monitor request.
    ///
    /// An INIT request includes a pvRequest describing the fields
    /// requested by the client. See [`field_request`] for why MIMUS
    /// uses `field()` instead of the null marker.
    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        put_i32(&mut out, self.server_cid, order);
        put_i32(&mut out, self.request_id, order);
        out.push(self.sub_command);
        if self.is_init() {
            out.extend_from_slice(&field_request());
        }
        out
    }
}

/// A client request to retrieve a channel's type description.
///
/// `pvxinfo` uses this request to inspect a channel without subscribing
/// to it. If the server does not respond, the channel's type cannot be
/// discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetFieldRequest {
    pub server_cid: i32,
    pub request_id: i32,
    /// Sub-field to describe. Empty selects the entire structure,
    /// as requested by `pvxinfo`.
    pub sub_field: String,
}

impl GetFieldRequest {
    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        Ok(Self {
            server_cid: r.i32()?,
            request_id: r.i32()?,
            sub_field: r.string()?.to_string(),
        })
    }

    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.sub_field.len());
        put_i32(&mut out, self.server_cid, order);
        put_i32(&mut out, self.request_id, order);
        put_string(&mut out, &self.sub_field, order);
        out
    }
}

/// A client request to read a channel once, without subscribing.
///
/// `pvxget` uses this request. The exchange consists of two messages:
/// an INIT describing the type, followed by a GET carrying the value.
/// This is the same sequence used by monitors, allowing MIMUS to answer
/// using data it already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetRequest {
    pub server_cid: i32,
    pub request_id: i32,
    pub sub_command: u8,
}

impl GetRequest {
    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        Ok(Self {
            server_cid: r.i32()?,
            request_id: r.i32()?,
            sub_command: r.u8()?,
        })
    }

    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        put_i32(&mut out, self.server_cid, order);
        put_i32(&mut out, self.request_id, order);
        out.push(self.sub_command);
        if self.is_init() {
            out.extend_from_slice(&field_request());
        }
        out
    }

    /// Whether this message describes the requested type.
    /// The first step of the exchange.
    #[inline]
    pub fn is_init(&self) -> bool {
        self.sub_command & crate::commands::get::INIT != 0
    }

    /// Whether this message requests the channel value.
    /// The second step of the exchange. Any non-INIT sub-command is a GET:
    /// `0x40` explicitly requests the value, while DESTROY also requests
    /// the value before releasing the operation.
    #[inline]
    pub fn is_get(&self) -> bool {
        !self.is_init()
    }

    /// Whether this is the client's last request for this operation.
    ///
    /// The server sends the requested value before releasing the operation;
    /// DESTROY does not mean teardown without a read. PVXS calls this
    /// `lastRequest`, and core-pva sets it on every read.
    #[inline]
    pub fn is_destroy(&self) -> bool {
        self.sub_command & crate::commands::get::DESTROY != 0
    }
}

/// An error status with a message.
///
/// Used when MIMUS cannot correctly answer a request.
/// Returning an incorrect result would be worse: a client receiving
/// a plausible descriptor for the wrong field has no way to detect it.
pub fn put_status_error(out: &mut Vec<u8>, message: &str, order: ByteOrder) {
    out.push(2); // ERROR
    put_string(out, message, order);
    put_string(out, "", order); // no call tree
}

/// The server's initial offer during connection validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionValidation {
    pub buffer_size: i32,
    pub registry_size: i16,
}

impl ConnectionValidation {
    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        put_i32(&mut out, self.buffer_size, order);
        put_i16(&mut out, self.registry_size, order);
        // Authentication methods offered. Anonymous only: MIMUS is a
        // benchmarking tool, and credentials would add failure modes
        // without making the measurements more representative.
        put_size(&mut out, 1, order);
        put_string(&mut out, "anonymous", order);
        out
    }
}

impl ConnectionValidation {
    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        Ok(Self {
            buffer_size: r.i32()?,
            registry_size: r.i16()?,
        })
    }
}

/// What the client answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionValidationResponse {
    pub buffer_size: i32,
    pub registry_size: i16,
    pub qos: i16,
    pub auth: String,
}

impl ConnectionValidationResponse {
    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        put_i32(&mut out, self.buffer_size, order);
        put_i16(&mut out, self.registry_size, order);
        put_i16(&mut out, self.qos, order);
        put_string(&mut out, &self.auth, order);
        // Authentication data: a type descriptor followed by a value.
        // The anonymous method has no data, but still requires the null
        // type code (0xFF).
        //
        // Omitting this byte caused PVXS to reject the response with
        // `Truncated/Invalid ConnValid from client`. MIMUS's server and
        // handwritten test client both ignored the trailing data, so
        // they agreed on the same incorrect format. Only testing against
        // a reference IOC exposed the mistake.
        out.push(0xFF);
        out
    }

    pub fn decode(payload: &[u8], order: ByteOrder) -> Result<Self, CodecError> {
        let mut r = Reader::new(payload, order);
        Ok(Self {
            buffer_size: r.i32()?,
            registry_size: r.i16()?,
            qos: r.i16()?,
            auth: r.string()?.to_string(),
        })
    }
}

/// Payload of `CONNECTION_VALIDATED`: an OK status with no additional data.
pub fn connection_validated() -> Vec<u8> {
    let mut out = Vec::with_capacity(1);
    put_status_ok(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::put_i16;

    const LE: ByteOrder = ByteOrder::Little;

    fn search_payload(channels: &[(i32, &str)]) -> Vec<u8> {
        let mut p = Vec::new();
        put_i32(&mut p, 7, LE);
        p.push(0x81);
        p.extend_from_slice(&[0, 0, 0]);
        p.extend_from_slice(&[0u8; 16]);
        p.extend_from_slice(&5075u16.to_le_bytes());
        put_size(&mut p, 1, LE);
        put_string(&mut p, "tcp", LE);
        put_i16(&mut p, channels.len() as i16, LE);
        for (id, name) in channels {
            put_i32(&mut p, *id, LE);
            put_string(&mut p, name, LE);
        }
        p
    }

    #[test]
    fn decodes_a_search_request() {
        let p = search_payload(&[(1, "MIM:DBL:0"), (2, "MIM:DBL:1")]);
        let req = SearchRequest::decode(&p, LE).unwrap();
        assert_eq!(req.sequence_id, 7);
        assert_eq!(req.response_port, 5075);
        assert_eq!(
            req.channels,
            vec![(1, "MIM:DBL:0".to_string()), (2, "MIM:DBL:1".to_string())]
        );
    }

    #[test]
    fn decodes_a_search_with_no_channels() {
        let req = SearchRequest::decode(&search_payload(&[]), LE).unwrap();
        assert!(req.channels.is_empty());
    }

    #[test]
    fn a_search_with_several_protocols_stays_aligned() {
        // The protocol list has a variable length and must be fully consumed.
        // Otherwise, subsequent fields are read at the wrong offsets, causing
        // channel names to be corrupted rather than producing a decode error.
        let mut p = Vec::new();
        put_i32(&mut p, 1, LE);
        p.push(0);
        p.extend_from_slice(&[0, 0, 0]);
        p.extend_from_slice(&[0u8; 16]);
        p.extend_from_slice(&0u16.to_le_bytes());
        put_size(&mut p, 2, LE);
        put_string(&mut p, "tcp", LE);
        put_string(&mut p, "tls", LE);
        put_i16(&mut p, 1, LE);
        put_i32(&mut p, 42, LE);
        put_string(&mut p, "MIM:STR:0", LE);

        let req = SearchRequest::decode(&p, LE).unwrap();
        assert_eq!(req.channels, vec![(42, "MIM:STR:0".to_string())]);
    }

    #[test]
    fn a_truncated_search_is_an_error() {
        let p = search_payload(&[(1, "MIM:DBL:0")]);
        for cut in [0, 4, 10, 24, 30] {
            assert!(
                SearchRequest::decode(&p[..cut], LE).is_err(),
                "truncation at {cut} should fail"
            );
        }
    }

    #[test]
    fn encodes_a_search_response() {
        let guid = *b"MIMUS0000001";
        let found = [1i32, 2, 3];
        let out = SearchResponse {
            guid: &guid,
            sequence_id: 7,
            server_port: 5075,
            found: &found,
        }
        .encode(LE);

        assert_eq!(&out[..GUID_LEN], b"MIMUS0000001");
        let mut r = Reader::new(&out[GUID_LEN..], LE);
        assert_eq!(r.i32().unwrap(), 7);
        r.skip(16).unwrap();
        assert_eq!(r.u16().unwrap(), 5075);
        assert_eq!(r.string().unwrap(), "tcp");
        assert_eq!(r.u8().unwrap(), 1, "found flag");
        assert_eq!(r.i16().unwrap(), 3);
        assert_eq!(
            [r.i32().unwrap(), r.i32().unwrap(), r.i32().unwrap()],
            [1, 2, 3]
        );
        assert!(r.is_empty());
    }

    #[test]
    fn an_empty_search_response_clears_the_found_flag() {
        let guid = [0u8; GUID_LEN];
        let out = SearchResponse {
            guid: &guid,
            sequence_id: 1,
            server_port: 5075,
            found: &[],
        }
        .encode(LE);
        let mut r = Reader::new(&out[GUID_LEN..], LE);
        r.i32().unwrap();
        r.skip(16).unwrap();
        r.u16().unwrap();
        r.string().unwrap();
        assert_eq!(r.u8().unwrap(), 0, "nothing found");
    }

    #[test]
    fn create_channel_round_trips() {
        let mut p = Vec::new();
        put_i16(&mut p, 2, LE);
        put_i32(&mut p, 100, LE);
        put_string(&mut p, "MIM:DBL:0", LE);
        put_i32(&mut p, 101, LE);
        put_string(&mut p, "MIM:DBL:1", LE);

        let req = CreateChannelRequest::decode(&p, LE).unwrap();
        assert_eq!(req.channels.len(), 2);
        assert_eq!(req.channels[1], (101, "MIM:DBL:1".to_string()));

        let out = CreateChannelResponse {
            client_cid: 100,
            server_cid: 900,
        }
        .encode(LE);
        let mut r = Reader::new(&out, LE);
        assert_eq!(r.i32().unwrap(), 100);
        assert_eq!(r.i32().unwrap(), 900);
        assert_eq!(r.u8().unwrap(), 0xFF, "OK status");
        assert_eq!(r.i16().unwrap(), 0);
    }

    #[test]
    fn monitor_subcommands_are_read_as_bits() {
        let mut p = Vec::new();
        put_i32(&mut p, 900, LE);
        put_i32(&mut p, 5, LE);
        p.push(crate::commands::monitor::INIT);
        let m = MonitorRequest::decode(&p, LE).unwrap();
        assert_eq!(m.server_cid, 900);
        assert_eq!(m.request_id, 5);
        assert!(m.is_init());
        assert!(!m.is_start());
    }

    #[test]
    fn a_plain_start_is_a_start_and_not_a_stop() {
        // 0x44 is the START command sent by PVXS clients.
        // MIMUS must recognize it as a start, not as a pipeline update.
        let mut p = Vec::new();
        put_i32(&mut p, 1, LE);
        put_i32(&mut p, 1, LE);
        p.push(crate::commands::monitor::START);
        let m = MonitorRequest::decode(&p, LE).unwrap();
        assert!(m.is_start());
        assert!(!m.is_stop());
        assert!(!m.is_init());
        assert!(!m.wants_pipeline(), "an ordinary start is not pipelined");
    }

    #[test]
    fn a_bare_0x04_is_a_stop_not_a_start() {
        // Treating 0x04 as START creates the subscription and stops it
        // immediately. A permissive server may accept this mistake,
        // hiding the bug until the client connects to a real server.
        let mut p = Vec::new();
        put_i32(&mut p, 1, LE);
        put_i32(&mut p, 1, LE);
        p.push(0x04);
        let m = MonitorRequest::decode(&p, LE).unwrap();
        assert!(m.is_stop());
        assert!(!m.is_start());
    }

    #[test]
    fn a_pipelined_start_is_both() {
        let mut p = Vec::new();
        put_i32(&mut p, 1, LE);
        put_i32(&mut p, 1, LE);
        p.push(crate::commands::monitor::START | crate::commands::monitor::PIPELINE);
        let m = MonitorRequest::decode(&p, LE).unwrap();
        assert!(m.is_start());
        assert!(m.wants_pipeline());
    }

    #[test]
    fn a_pipeline_ack_is_recognised_and_decoded() {
        let mut p = Vec::new();
        put_i32(&mut p, 900, LE);
        put_i32(&mut p, 5, LE);
        p.push(crate::commands::monitor::PIPELINE);
        put_i32(&mut p, 16, LE);

        let m = MonitorRequest::decode(&p, LE).unwrap();
        assert!(m.is_pipeline_ack());
        assert!(!m.is_init());
        assert!(!m.is_start());

        let ack = PipelineAck::decode(&p, LE).unwrap();
        assert_eq!(ack.request_id, 5);
        assert_eq!(ack.free, 16);
    }

    #[test]
    fn a_truncated_ack_is_an_error_not_a_zero() {
        // Treating an undecodable credit update as zero credit would stall
        // the subscription. Returning an error is safer than silently
        // interpreting a malformed message as zero.
        let mut p = Vec::new();
        put_i32(&mut p, 900, LE);
        put_i32(&mut p, 5, LE);
        p.push(crate::commands::monitor::PIPELINE);
        assert!(PipelineAck::decode(&p, LE).is_err());
    }

    #[test]
    fn a_get_exchange_is_init_then_value() {
        let init = GetRequest {
            server_cid: 900,
            request_id: 3,
            sub_command: crate::commands::get::INIT,
        };
        assert!(init.is_init());
        assert!(!init.is_get());
        assert_eq!(GetRequest::decode(&init.encode(LE), LE).unwrap(), init);

        let read = GetRequest {
            server_cid: 900,
            request_id: 3,
            sub_command: 0x00,
        };
        assert!(read.is_get());
        assert!(!read.is_init());
        assert!(!read.is_destroy());

        let done = GetRequest {
            server_cid: 900,
            request_id: 3,
            sub_command: crate::commands::get::DESTROY,
        };
        assert!(done.is_destroy());
        assert!(done.is_get(), "a last read is still a read");
    }

    #[test]
    fn a_get_init_carries_the_same_request_a_monitor_does() {
        // Both request the full structure. Their pvRequest payloads must
        // match so that pvxget and pvxmonitor observe the same fields.
        let init = GetRequest {
            server_cid: 1,
            request_id: 1,
            sub_command: crate::commands::get::INIT,
        };
        assert_eq!(&init.encode(LE)[9..], &field_request()[..]);
    }

    #[test]
    fn a_get_field_request_round_trips() {
        let req = GetFieldRequest {
            server_cid: 900,
            request_id: 7,
            sub_field: String::new(),
        };
        assert_eq!(GetFieldRequest::decode(&req.encode(LE), LE).unwrap(), req);

        let sub = GetFieldRequest {
            server_cid: 900,
            request_id: 8,
            sub_field: "timeStamp".into(),
        };
        assert_eq!(
            GetFieldRequest::decode(&sub.encode(LE), LE)
                .unwrap()
                .sub_field,
            "timeStamp"
        );
    }

    #[test]
    fn an_error_status_is_distinguishable_from_ok() {
        let mut ok = Vec::new();
        put_status_ok(&mut ok);
        let mut err = Vec::new();
        put_status_error(&mut err, "no", LE);
        assert_eq!(ok[0], 0xFF);
        assert_ne!(err[0], 0xFF);
        let mut r = Reader::new(&err[1..], LE);
        assert_eq!(r.string().unwrap(), "no");
    }

    #[test]
    fn connection_validation_round_trips() {
        let out = ConnectionValidation {
            buffer_size: 16384,
            registry_size: 0x7FFF,
        }
        .encode(LE);
        let mut r = Reader::new(&out, LE);
        assert_eq!(r.i32().unwrap(), 16384);
        assert_eq!(r.i16().unwrap(), 0x7FFF);
        assert_eq!(r.size().unwrap().or_empty(), 1);
        assert_eq!(r.string().unwrap(), "anonymous");

        let mut p = Vec::new();
        put_i32(&mut p, 32768, LE);
        put_i16(&mut p, 1024, LE);
        put_i16(&mut p, 0, LE);
        put_string(&mut p, "anonymous", LE);
        let resp = ConnectionValidationResponse::decode(&p, LE).unwrap();
        assert_eq!(resp.buffer_size, 32768);
        assert_eq!(resp.auth, "anonymous");
    }

    #[test]
    fn validated_is_a_bare_ok() {
        assert_eq!(connection_validated(), vec![0xFF]);
    }

    #[test]
    fn everything_works_big_endian_too() {
        let guid = [1u8; GUID_LEN];
        let found = [9i32];
        let out = SearchResponse {
            guid: &guid,
            sequence_id: 3,
            server_port: 1,
            found: &found,
        }
        .encode(ByteOrder::Big);
        let mut r = Reader::new(&out[GUID_LEN..], ByteOrder::Big);
        assert_eq!(r.i32().unwrap(), 3);
    }
}
#[cfg(test)]
mod client_side_tests {
    use super::*;
    use crate::commands;

    const LE: ByteOrder = ByteOrder::Little;
    const BE: ByteOrder = ByteOrder::Big;

    /// Encoding and decoding live in the same file on purpose.
    /// A round trip only shows that both halves agree; it does not
    /// prove that either matches the protocol specification.
    /// Changes to one half should therefore prompt a review of the other.
    ///
    /// **It does not prove that the bytes match PVXS.** That is verified
    /// by calibrating against a real `softIocPVX` and by using a separate,
    /// hand-written client in the integration suite, so that a mistake
    /// here cannot be mirrored by `mimus serve`.
    #[test]
    fn a_search_round_trips_through_the_server_decoder() {
        for order in [LE, BE] {
            let req = SearchRequest {
                sequence_id: 42,
                flags: 0x81,
                response_port: 0,
                channels: vec![
                    (1, "MIM:F64:0".to_string()),
                    (2, "SR:C01:BPM-003:POS:X".to_string()),
                ],
            };
            let decoded = SearchRequest::decode(&req.encode(order), order).unwrap();
            assert_eq!(decoded.sequence_id, req.sequence_id);
            assert_eq!(decoded.channels, req.channels);
        }
    }

    #[test]
    fn a_search_with_no_channels_round_trips() {
        let req = SearchRequest {
            sequence_id: 1,
            flags: 0,
            response_port: 0,
            channels: Vec::new(),
        };
        assert!(
            SearchRequest::decode(&req.encode(LE), LE)
                .unwrap()
                .channels
                .is_empty()
        );
    }

    #[test]
    fn a_search_response_round_trips() {
        let guid = *b"MIMUS-------";
        let found = [7i32, 9, 11];
        let bytes = SearchResponse {
            guid: &guid,
            sequence_id: 42,
            server_port: 5075,
            found: &found,
        }
        .encode(LE);

        let got = SearchFound::decode(&bytes, LE).unwrap();
        assert_eq!(got.guid, guid);
        assert_eq!(got.sequence_id, 42);
        assert_eq!(got.server_port, 5075);
        assert_eq!(got.found, found);
    }

    #[test]
    fn a_search_response_finding_nothing_decodes_as_nothing() {
        // The response flag and channel count can disagree.
        // The decoder must honor the "no channels found" flag rather
        // than interpreting a non-zero count as valid channel results.
        let guid = [0u8; GUID_LEN];
        let bytes = SearchResponse {
            guid: &guid,
            sequence_id: 1,
            server_port: 5075,
            found: &[],
        }
        .encode(LE);
        assert!(SearchFound::decode(&bytes, LE).unwrap().found.is_empty());
    }

    #[test]
    fn channel_creation_round_trips_both_ways() {
        let req = CreateChannelRequest {
            channels: vec![(100, "MIM:F64:0".into()), (101, "MIM:F64:1".into())],
        };
        let back = CreateChannelRequest::decode(&req.encode(LE), LE).unwrap();
        assert_eq!(back.channels, req.channels);

        let resp = CreateChannelResponse {
            client_cid: 100,
            server_cid: 900,
        };
        assert_eq!(
            CreateChannelResponse::decode(&resp.encode(LE), LE).unwrap(),
            resp
        );
    }

    #[test]
    fn a_monitor_init_carries_a_field_request() {
        // Not the null marker. Both mean "no restriction", but with QSRV2
        // the null form produced a complete value in every update: 174 bytes
        // instead of the 41-byte delta sent to pvxmonitor for the same channel.
        // Calibrating against the null form would therefore inflate byte-rate
        // measurements by roughly a factor of four.
        let m = MonitorRequest {
            server_cid: 900,
            request_id: 5,
            sub_command: commands::monitor::INIT,
        };
        let bytes = m.encode(LE);
        assert_eq!(&bytes[9..], &field_request()[..]);
        assert!(bytes[9..].windows(5).any(|w| w == b"field"));
        let back = MonitorRequest::decode(&bytes, LE).unwrap();
        assert_eq!(back, m);
        assert!(back.is_init());
    }

    #[test]
    fn a_field_request_is_a_structure_holding_an_empty_field_structure() {
        let r = field_request();
        assert_eq!(r[0], 0x80, "outer structure");
        assert_eq!(r[2], 1, "one member");
        assert_eq!(&r[4..9], b"field");
        assert_eq!(r[9], 0x80, "the member is a structure");
        assert_eq!(*r.last().unwrap(), 0, "with no members: everything");
    }

    #[test]
    fn a_monitor_start_carries_no_request() {
        let m = MonitorRequest {
            server_cid: 900,
            request_id: 5,
            sub_command: commands::monitor::START,
        };
        let bytes = m.encode(LE);
        assert_eq!(bytes.len(), 9, "no pvRequest after a START");
        assert_eq!(MonitorRequest::decode(&bytes, LE).unwrap(), m);
    }

    #[test]
    fn connection_validation_round_trips_both_ways() {
        let offer = ConnectionValidation {
            buffer_size: 16384,
            registry_size: 0x7FFF,
        };
        assert_eq!(
            ConnectionValidation::decode(&offer.encode(LE), LE).unwrap(),
            offer
        );

        let answer = ConnectionValidationResponse {
            buffer_size: 32768,
            registry_size: 1024,
            qos: 0,
            auth: "anonymous".into(),
        };
        assert_eq!(
            ConnectionValidationResponse::decode(&answer.encode(LE), LE).unwrap(),
            answer
        );
    }

    #[test]
    fn a_truncated_search_response_is_an_error_not_a_guess() {
        let guid = [0u8; GUID_LEN];
        let found = [1i32];
        let bytes = SearchResponse {
            guid: &guid,
            sequence_id: 1,
            server_port: 1,
            found: &found,
        }
        .encode(LE);
        for cut in [0, 4, 12, 20, bytes.len() - 1] {
            assert!(
                SearchFound::decode(&bytes[..cut], LE).is_err(),
                "truncation at {cut} should fail"
            );
        }
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    const LE: ByteOrder = ByteOrder::Little;

    #[test]
    fn the_validation_response_ends_with_a_null_type_code() {
        // PVXS expects a type descriptor followed by a value after the
        // method string. The anonymous method has no authentication data,
        // so it uses the null type code.
        // Without this byte, PVXS reports `Truncated/Invalid ConnValid
        // from client` and closes the connection before any search request.
        let answer = ConnectionValidationResponse {
            buffer_size: 32768,
            registry_size: 1024,
            qos: 0,
            auth: "anonymous".to_string(),
        };
        let bytes = answer.encode(LE);
        assert_eq!(
            *bytes.last().unwrap(),
            0xFF,
            "no null type code after the method: {bytes:02x?}"
        );
        // Four, two, two, one length byte, nine characters, one marker.
        assert_eq!(bytes.len(), 4 + 2 + 2 + 1 + 9 + 1);
    }

    #[test]
    fn a_response_without_the_marker_is_still_accepted_on_decode() {
        // Strict when sending, lenient when receiving. MIMUS accepts
        // responses that omit the null type marker, but always includes
        // it in responses sent to the client.
        let mut short = Vec::new();
        put_i32(&mut short, 32768, LE);
        put_i16(&mut short, 1024, LE);
        put_i16(&mut short, 0, LE);
        put_string(&mut short, "anonymous", LE);
        let back = ConnectionValidationResponse::decode(&short, LE).unwrap();
        assert_eq!(back.auth, "anonymous");
    }
}
