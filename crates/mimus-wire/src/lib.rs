//! PVAccess wire format: framing and codec, without I/O.
//!
//! This crate is the only part of MIMUS that handles the PVAccess wire
//! format. Both `mimus-calibrate` and `mimus-serve` depend on it, but
//! not on each other: calibration decodes frames from a real IOC,
//! while the server produces frames for clients.
//!
//! Keeping I/O out of this crate keeps its tests cheap: golden-packet
//! tests compare byte arrays and run on every commit.
//!
//! Targets PVXS (QSRV2). See `catalogue/README.md`.

pub mod codec;
pub mod commands;
pub mod framing;
pub mod header;
pub mod messages;
pub mod pvdata;

pub use codec::{CodecError, Reader, Size};
pub use framing::{Frame, FrameError, Framer, write_frame};
pub use header::{ByteOrder, Direction, HEADER_SIZE, Header, HeaderError, MAGIC, Segmentation};
pub use messages::{
    ConnectionValidation, ConnectionValidationResponse, CreateChannelRequest,
    CreateChannelResponse, GUID_LEN, GetFieldRequest, GetRequest, MonitorRequest, PipelineAck,
    SearchFound, SearchRequest, SearchResponse, connection_validated, field_request,
    put_status_error,
};
