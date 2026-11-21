//! Handles one accepted connection, from handshake through steady-state
//! emission.
//!
//! The loop drains all buffered frames before returning to the timer. A single
//! read may contain thousands of small messages; yielding to the scheduler
//! after each frame would make throughput depend on scheduling rather than
//! memory and processing speed.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::{Buf, BytesMut};
use mimus_emit::{Backpressure, Counters, EpicsTime, Instance, RateMix, Schedule};
use mimus_profile::DescriptorPolicy;
use mimus_wire::codec::put_i32;
use mimus_wire::header::{ByteOrder, Direction, Header, Segmentation};
use mimus_wire::pvdata::Descriptor;
use mimus_wire::{
    ConnectionValidation, ConnectionValidationResponse, CreateChannelRequest,
    CreateChannelResponse, Frame, Framer, GUID_LEN, GetFieldRequest, GetRequest, MonitorRequest,
    PipelineAck, SearchRequest, SearchResponse, commands, connection_validated, put_status_error,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::fleet::Fleet;
use crate::registry::Registry;
use crate::shared::SharedCounters;

/// Configuration shared by sessions that belong to the same virtual IOC.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub guid: [u8; GUID_LEN],
    /// Index of the virtual IOC this session belongs to.
    ///
    /// A session only claims channels served by its own IOC.
    pub ioc: u16,
    pub port: u16,
    pub buffer_size: i32,
    pub registry_size: i16,
    pub descriptor_policy: DescriptorPolicy,
    pub backpressure: Backpressure,
    /// Distribution of the population across update rates.
    ///
    /// A single update rate is represented by a mix with one group.
    pub rate_mix: RateMix,
    pub tick_us: u64,
    /// Maximum number of queued bytes before the drop policy takes effect.
    ///
    /// Models the monitor queue depth of an IOC.
    pub high_water: usize,
    /// Interval at which to drop a live session to exercise client
    /// reconnection behavior. `None` disables connection drops.
    pub drop_connections: Option<std::time::Duration>,
    /// Maximum delay before buffered updates are written.
    ///
    /// A value of zero writes on every tick, matching IOC behavior and
    /// preserving the meaning of timestamps. It also requires one write
    /// syscall per connection per tick. In measurements, sending the same
    /// 42 MB across eight connections instead of one used 3.6 times the CPU,
    /// because each write carried one eighth as much data.
    ///
    /// A nonzero value reduces syscall overhead at the cost of added latency.
    /// It is not the default and is recorded in the run manifest, since it
    /// affects end-to-end timing measured against the timestamp field.
    pub coalesce_us: u64,
    /// Log the first outgoing message of each shape in hexadecimal.
    ///
    /// The bytes rejected by a client are the most direct evidence for diagnosing a mismatch,
    /// and reconstructing them from a capture requires an additional round trip.
    pub debug: bool,
    /// Whether to enforce pipeline credits or only count them.
    pub pipeline: Pipeline,
    /// Initial credits for a pipelined monitor, before any acknowledgements.
    ///
    /// PVXS clients request a queue depth in their `pvRequest`. MIMUS does
    /// not parse that field, so this value provides a configurable substitute.
    pub initial_credits: i32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            guid: *b"MIMUS-------",
            ioc: 0,
            port: 5075,
            buffer_size: 16384,
            registry_size: 0x7FFF,
            descriptor_policy: DescriptorPolicy::FullThenId,
            backpressure: Backpressure::Block,
            rate_mix: RateMix::uniform(1.0),
            tick_us: 1000,
            high_water: 1 << 20,
            drop_connections: None,
            coalesce_us: 0,
            debug: false,
            pipeline: Pipeline::default(),
            initial_credits: 4,
        }
    }
}

/// Controls flow control for pipelined subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pipeline {
    /// Decode and count credits without withholding updates.
    ///
    /// This is MIMUS's default behavior. It avoids disrupting clients if
    /// the assumed credit encoding differs from the actual protocol.
    #[default]
    Permissive,
    /// Stop emitting updates when a pipelined monitor runs out of credit,
    /// matching the behavior of a PVXS server.
    ///
    /// This mode remains opt-in until calibration confirms that clients send
    /// acknowledgements as expected. Incorrect credit handling can stall a subscription entirely.
    Strict,
}

impl std::fmt::Display for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Permissive => "permissive",
            Self::Strict => "strict",
        })
    }
}

impl std::str::FromStr for Pipeline {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "permissive" | "off" => Ok(Self::Permissive),
            "strict" => Ok(Self::Strict),
            other => Err(format!("unknown pipeline policy {other:?}")),
        }
    }
}

/// Selects which of the two subscription message shapes to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// The complete value sent once when the monitor starts.
    Initial,
    /// A delta containing only the fields that changed.
    SteadyState,
}

/// A monitor established by the client on this connection.
#[derive(Debug, Clone, Copy)]
struct Monitor {
    instance: u32,
    request_id: i32,
    started: bool,
    /// Whether the client requested a pipelined subscription.
    pipelined: bool,
    /// Number of updates the client can receive.
    ///
    /// Only used when [`Pipeline::Strict`] is enabled.
    credits: i32,
    /// Per-monitor update counter.
    ///
    /// Kept separate from the instance because the same channel can be monitored
    /// on multiple connections, each with its own update stream.
    step: u64,
}

/// Describes why a session ended.
#[derive(Debug)]
pub enum SessionEnd {
    /// The client closed the connection cleanly.
    ClientClosed,
    /// The incoming stream violated the protocol.
    ///
    /// PVA provides no resynchronization point, so the connection cannot
    /// safely continue after a protocol error.
    Protocol(String),
    Io(std::io::Error),
}

impl std::fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientClosed => f.write_str("client closed"),
            Self::Protocol(e) => write!(f, "protocol error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

/// State for one client connection.
pub struct Session {
    cfg: SessionConfig,
    fleet: Arc<Fleet>,
    registry: Registry,
    /// Maps server-assigned channel IDs to fleet instance indices.
    channels: HashMap<i32, u32>,
    /// Maps monitor request IDs to their monitor state.
    monitors: HashMap<i32, Monitor>,
    schedule: Schedule,
    next_server_cid: i32,
    counters: Counters,
    /// Counters accumulated since the last flush.
    ///
    /// Kept separate so shared atomics are updated once per flush rather
    /// than once per individual update.
    pending: Counters,
    shared: Option<Arc<SharedCounters>>,
    /// Time of the previous tick, used to detect late ticks.
    last_tick: Option<std::time::Instant>,
    /// Time of the most recent write of the outbox to the socket.
    last_flush: Option<std::time::Instant>,
    /// Template and message-shape combinations already logged in debug mode.
    debugged: Vec<(u16, Shape)>,
    order: ByteOrder,
    inbox: BytesMut,
    outbox: BytesMut,
    framer: Framer,
}

impl Session {
    /// Creates a session with its own channel, monitor, and registry state.
    pub fn new(cfg: SessionConfig, fleet: Arc<Fleet>) -> Result<Self, mimus_emit::WheelError> {
        let schedule = Schedule::new(&cfg.rate_mix, cfg.tick_us)?;
        let registry = Registry::new(fleet.templates.len(), cfg.descriptor_policy);
        Ok(Self {
            cfg,
            fleet,
            registry,
            channels: HashMap::new(),
            monitors: HashMap::new(),
            schedule,
            next_server_cid: 1,
            counters: Counters::default(),
            pending: Counters::default(),
            shared: None,
            last_tick: None,
            last_flush: None,
            debugged: Vec::new(),
            // Outgoing messages use little-endian byte order. Incoming
            // messages are decoded using the byte order in their own headers.
            order: ByteOrder::Little,
            inbox: BytesMut::with_capacity(64 * 1024),
            outbox: BytesMut::with_capacity(256 * 1024),
            framer: Framer::new(),
        })
    }

    /// Publishes session progress to shared counters while the session runs.
    pub fn with_shared(mut self, shared: Arc<SharedCounters>) -> Self {
        self.shared = Some(shared);
        self
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Serves the connection until the client disconnects or an error occurs.
    pub async fn run(mut self, mut stream: TcpStream) -> (SessionEnd, Counters) {
        // Frames are small, and batching is handled by the outbox.
        // Disable Nagle's algorithm to avoid adding latency.
        let _ = stream.set_nodelay(true);

        if let Err(e) = self.handshake(&mut stream).await {
            return (e, self.counters);
        }

        // The timer drives emission and is only needed while there are
        // subscriptions to serve.
        //
        // Previously, idle connections still woke up a thousand times per
        // second. Profiling an idle session showed timer processing and
        // `Sleep::poll` among the largest costs, outweighing MIMUS's own
        // work. An archiver opening hundreds of connections incurred this
        // overhead on every connection, even before receiving an update.
        //
        // Create the interval once and let the select guard disable polling
        // while the schedule is empty. It is not rebuilt on each tick.
        let mut ticker = tokio::time::interval(std::time::Duration::from_micros(self.cfg.tick_us));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let end = loop {
            // When there is nothing scheduled, wait only for socket input.
            // `pending()` never completes, so the timer branch is disabled
            // until the schedule contains work.
            let idle = self.schedule.is_empty();
            tokio::select! {
                _ = ticker.tick(), if !idle => {
                    self.emit_due();
                    if self.should_flush()
                        && let Err(e) = self.flush(&mut stream).await {
                            break SessionEnd::Io(e);
                        }
                }
                read = stream.read_buf(&mut self.inbox) => {
                    match read {
                        Ok(0) => break SessionEnd::ClientClosed,
                        Ok(_) => {
                            // Process every complete frame buffered by this
                            // read before returning to the scheduler.
                            loop {
                                match self.framer.next_frame(&mut self.inbox) {
                                    Ok(Some(frame)) => {
                                        // A malformed message does not
                                        // necessarily invalidate the stream.
                                        // If framing remains intact, continue
                                        // with the next frame.
                                        let _ = self.dispatch(&frame);
                                    }
                                    Ok(None) => break,
                                    Err(e) => return (SessionEnd::Protocol(e.to_string()), self.counters),
                                }
                            }
                            // Flush replies according to the session's
                            // buffering policy, rather than blocking after
                            // each dispatched message.
                            //
                            // A blocking flush can stall the read loop when
                            // the peer stops reading. The socket fills, the
                            // flush never completes, and the session stops
                            // processing incoming subscriptions. In one
                            // test, a burst of twenty thousand subscription
                            // starts produced fewer than three thousand live
                            // subscriptions, leaving the offered load seven
                            // times below target with no obvious error.
                            //
                            // If the peer cannot receive replies, blocking
                            // the session does not help. The handshake still
                            // uses a blocking flush because no updates are
                            // flowing at that stage.
                            if let Err(e) = self.flush(&mut stream).await {
                                break SessionEnd::Io(e);
                            }
                        }
                        Err(e) => break SessionEnd::Io(e),
                    }
                }
            }
        };
        // A closed session releases all its subscriptions. Otherwise, the
        // shared live-subscription gauge would keep counting monitors from
        // disconnected clients and drift upward across reconnections.
        if let Some(shared) = &self.shared {
            let live = self.monitors.values().filter(|m| m.started).count() as u64;
            for _ in 0..live {
                shared.subscription_stopped();
            }
        }
        (end, self.counters)
    }

    async fn handshake(&mut self, stream: &mut TcpStream) -> Result<(), SessionEnd> {
        // Send the byte-order marker first. Its own flags determine how the
        // message is decoded.
        let hdr = Header::control(
            commands::CTRL_SET_BYTE_ORDER,
            0,
            ByteOrder::Little,
            Direction::FromServer,
        );
        self.outbox.extend_from_slice(&hdr.encode());

        let payload = ConnectionValidation {
            buffer_size: self.cfg.buffer_size,
            registry_size: self.cfg.registry_size,
        }
        .encode(self.order);
        self.push_app(commands::CONNECTION_VALIDATION, &payload);
        self.flush_reply(stream).await.map_err(SessionEnd::Io)?;

        // Wait for the client's connection-validation response.
        loop {
            match self.framer.next_frame(&mut self.inbox) {
                Ok(Some(frame)) => {
                    if frame.header.is_application()
                        && frame.command() == commands::CONNECTION_VALIDATION
                    {
                        // Decode the response using the byte order declared in its own header.
                        // Although the only field we use is the authentication string, whose
                        // length is one byte, other fields may depend on byte order.
                        // Using a connection-wide order here would fail on later messages
                        // such as search requests.
                        let _ = ConnectionValidationResponse::decode(
                            &frame.payload,
                            frame.header.byte_order(),
                        );
                        break;
                    }
                    // Ignore other messages received before validation completes.
                }
                Ok(None) => {
                    let n = stream
                        .read_buf(&mut self.inbox)
                        .await
                        .map_err(SessionEnd::Io)?;
                    if n == 0 {
                        return Err(SessionEnd::ClientClosed);
                    }
                }
                Err(e) => return Err(SessionEnd::Protocol(e.to_string())),
            }
        }

        self.push_app(commands::CONNECTION_VALIDATED, &connection_validated());
        self.flush_reply(stream).await.map_err(SessionEnd::Io)?;
        Ok(())
    }

    fn dispatch(&mut self, frame: &Frame) -> Result<(), ()> {
        if frame.is_control() {
            // Flow-control markers and echo requests are not handled here yet.
            // Their acknowledgements remain an explicit protocol-fidelity item
            // rather than being simulated without the required behavior.
            return Ok(());
        }
        // Byte order is specified per message, not per connection. PVXS sends
        // search requests in big-endian order even after the server declares
        // little-endian in CTRL_SET_BYTE_ORDER. Using a connection-wide order
        // would misread counts and lengths, potentially producing plausible
        // but incorrect values instead of an obvious decoding error.
        let order = frame.header.byte_order();
        match frame.command() {
            commands::SEARCH => self.on_search(&frame.payload, order),
            commands::CREATE_CHANNEL => self.on_create_channel(&frame.payload, order),
            commands::MONITOR => self.on_monitor(&frame.payload, order),
            commands::ECHO => {
                // Return the payload unchanged. The client uses the response to measure round-trip
                // time and may close the connection if it does not receive the expected echo.
                self.push_app(commands::ECHO, &frame.payload);
                Ok(())
            }
            commands::GET => self.on_get(&frame.payload, order),
            commands::GET_FIELD => self.on_get_field(&frame.payload, order),
            commands::DESTROY_CHANNEL => self.on_destroy_channel(&frame.payload, order),
            commands::DESTROY_REQUEST => self.on_destroy_request(&frame.payload, order),
            _ => Ok(()),
        }
    }

    fn on_search(&mut self, payload: &[u8], order: ByteOrder) -> Result<(), ()> {
        let Ok(req) = SearchRequest::decode(payload, order) else {
            return Err(());
        };
        let found: Vec<i32> = req
            .channels
            .iter()
            .filter(|(_, name)| self.fleet.lookup_on(name, self.cfg.ioc).is_some())
            .map(|(id, _)| *id)
            .collect();
        if found.is_empty() {
            // Silence is the correct response when no requested channel is found.
            // Replying to every miss would generate a large number of unnecessary responses
            // during broadcast searches.
            return Ok(());
        }
        let resp = SearchResponse {
            guid: &self.cfg.guid,
            sequence_id: req.sequence_id,
            server_port: self.cfg.port,
            found: &found,
        }
        .encode(self.order);
        self.push_app(commands::SEARCH_RESPONSE, &resp);
        Ok(())
    }

    fn on_create_channel(&mut self, payload: &[u8], order: ByteOrder) -> Result<(), ()> {
        let Ok(req) = CreateChannelRequest::decode(payload, order) else {
            return Err(());
        };
        for (client_cid, name) in req.channels {
            let Some(instance) = self.fleet.lookup_on(&name, self.cfg.ioc) else {
                // Explicitly reject channels this IOC does not serve.
                //
                // Unlike a search miss, an unanswered create-channel request leaves the client
                // waiting indefinitely. A load test exposed this when requests for unknown
                // names stalled at the first missing channel, reporting no data delivered.
                let mut refusal = Vec::with_capacity(24);
                put_i32(&mut refusal, client_cid, self.order);
                put_i32(&mut refusal, 0, self.order);
                put_status_error(&mut refusal, "no such channel", self.order);
                self.push_app(commands::CREATE_CHANNEL, &refusal);
                continue;
            };
            let server_cid = self.next_server_cid;
            self.next_server_cid += 1;
            self.channels.insert(server_cid, instance);
            let resp = CreateChannelResponse {
                client_cid,
                server_cid,
            }
            .encode(self.order);
            self.push_app(commands::CREATE_CHANNEL, &resp);
        }
        Ok(())
    }

    fn on_monitor(&mut self, payload: &[u8], order: ByteOrder) -> Result<(), ()> {
        let Ok(req) = MonitorRequest::decode(payload, order) else {
            return Err(());
        };

        if req.is_init() {
            let Some(&instance) = self.channels.get(&req.server_cid) else {
                return Ok(());
            };
            // Borrow the registry and fleet separately so the descriptor can
            // be copied directly into the response without cloning it to work
            // around the borrow checker.
            //
            // Duplicating a 446-byte descriptor twice per subscription is
            // negligible for a single channel, but creates substantial
            // allocator traffic at million-subscription scale, particularly
            // during the initial subscription burst.
            let mut out = Vec::new();
            {
                let Self {
                    registry,
                    fleet,
                    order,
                    ..
                } = self;
                let template = fleet.instances[instance as usize].template;
                let compiled = &fleet.templates[template as usize];
                let Some(bytes) = registry.descriptor(
                    template,
                    &compiled.intro_full,
                    compiled.intro_id_only.as_deref(),
                ) else {
                    // Without an identifier-only variant, this type cannot
                    // be described on a warm connection. Fail rather than
                    // send a descriptor that has not been calibrated.
                    return Err(());
                };
                out.reserve(bytes.len() + 8);
                put_i32(&mut out, req.request_id, *order);
                out.push(commands::monitor::INIT);
                out.extend_from_slice(bytes);
            }
            self.push_app(commands::MONITOR, &out);

            self.monitors.insert(
                req.request_id,
                Monitor {
                    instance,
                    request_id: req.request_id,
                    started: false,
                    pipelined: false,
                    credits: 0,
                    step: 0,
                },
            );
            return Ok(());
        }

        // A non-init monitor message can combine up to three actions:
        // granting flow-control credit, changing subscription state, and
        // terminating the request. Process each applicable action.
        //
        // Returning early after handling a credit would skip the state change
        // for pipelined monitor starts. The 0xC4 start flag combines both
        // operations; treating them as alternatives left subscriptions
        // connected but never started, so no updates were sent.
        if req.is_pipeline_ack() {
            // Ignore undecodable credit updates rather than treating them as
            // zero. A decoding error should not stall an otherwise active
            // subscription.
            if let Ok(ack) = PipelineAck::decode(payload, order)
                && let Some(mon) = self.monitors.get_mut(&ack.request_id)
            {
                mon.credits = mon.credits.saturating_add(ack.free);
                self.counters.pipeline_acks += 1;
                self.pending.pipeline_acks += 1;
            }
        }

        if req.is_destroy() || req.is_stop() {
            if self
                .monitors
                .remove(&req.request_id)
                .is_some_and(|m| m.started)
                && let Some(shared) = &self.shared
            {
                shared.subscription_stopped();
            }
            self.schedule.remove(req.request_id);
            return Ok(());
        }

        if req.is_start() {
            let mut emit_now = false;
            let mut group = 0u16;
            let initial = self.cfg.initial_credits;
            if let Some(mon) = self.monitors.get_mut(&req.request_id)
                && !mon.started
            {
                mon.started = true;
                mon.pipelined = req.wants_pipeline();
                mon.credits = if mon.pipelined { initial } else { 0 };
                group = self.fleet.instances[mon.instance as usize].rate_group;
                emit_now = true;
            }
            if emit_now {
                self.schedule.insert(group, req.request_id);
                if let Some(shared) = &self.shared {
                    shared.subscription_started();
                }
            }
            // Send the current value as soon as the monitor starts, as a real
            // IOC does. Waiting for the next scan can delay the first update
            // by a full period and makes subscription startup noticeably
            // slower than softIocPVX.
            if emit_now {
                // Send the complete value before any deltas. The client needs
                // an initial value to apply subsequent updates; fields not
                // included in those deltas would otherwise remain undefined.
                self.emit_one(req.request_id, EpicsTime::now(), Shape::Initial);
            }
        }
        Ok(())
    }

    /// Reads a channel once, without creating a subscription.
    ///
    /// This is the request used by `pvxget`, and one of the two ways to inspect
    /// the value held by a server. The exchange mirrors a monitor: an INIT
    /// response containing the type descriptor, followed by a value response
    /// containing the changed-field set and data.
    ///
    /// MIMUS builds both responses from data already available in the profile.
    /// The INIT response uses the stored descriptor. The value response uses
    /// the opening message that a subscription would receive. This is a
    /// complete value, stamped with the channel's last processing time (see
    /// [`last_processed`]), and does not include the overrun set used only in
    /// monitor updates. Profiles calibrated before opening messages were
    /// captured have no complete value; MIMUS reports this rather than sending
    /// a delta that the client cannot apply.
    ///
    /// The destroy bit does not cancel the read. It marks this as the client's
    /// final request for the operation: the server sends the response and then
    /// releases the request. This matches how PVXS interprets `lastRequest`
    /// in `serverget.cpp`. Core PVA, used by Phoebus and the Archiver Appliance,
    /// sends the bit on its only GET after INIT. Treating it as an immediate
    /// teardown leaves those clients waiting for a response. MIMUS keeps no
    /// per-read state, so there is nothing to release. The response echoes the
    /// sub-command, as PVXS does.
    fn on_get(&mut self, payload: &[u8], order: ByteOrder) -> Result<(), ()> {
        let Ok(req) = GetRequest::decode(payload, order) else {
            return Err(());
        };
        let Some(&instance) = self.channels.get(&req.server_cid) else {
            return Ok(());
        };
        let template = self.fleet.instances[instance as usize].template;
        let compiled = &self.fleet.templates[template as usize];

        let mut out = Vec::with_capacity(compiled.max_len() + 16);
        put_i32(&mut out, req.request_id, self.order);
        out.push(req.sub_command);

        if req.is_init() {
            out.extend_from_slice(&compiled.intro_full);
        } else {
            match compiled.initial.as_deref() {
                Some(initial) => {
                    mimus_wire::codec::put_status_ok(&mut out);
                    let inst = self.fleet.instances[instance as usize];
                    let when = last_processed(&self.schedule, &inst, EpicsTime::now());
                    let body = out.len();
                    initial.render(inst.seed, inst.seed, when, &mut out);
                    if let Some(desc) = &self.fleet.openings[template as usize]
                        && let Some(value_end) = restamp(desc, &mut out[body..], when)
                    {
                        out.truncate(body + value_end);
                    }
                }
                None => put_status_error(
                    &mut out,
                    "this profile carries no complete value for the channel",
                    self.order,
                ),
            }
        }
        self.push_app(commands::GET, &out);
        Ok(())
    }

    /// Describes a channel's type without creating a subscription.
    ///
    /// This is the request used by `pvxinfo` to inspect the types a server
    /// provides. Without a response, MIMUS appears to expose no introspection
    /// information. It also prevents comparison against the reference PVXS IOC:
    /// `pvxinfo -v` should report the same type for a channel on both servers.
    ///
    /// The response contains the request ID, a status, and the type descriptor.
    /// These are the same fields carried by a monitor INIT after its sub-command,
    /// so MIMUS can replay the stored descriptor unchanged.
    fn on_get_field(&mut self, payload: &[u8], order: ByteOrder) -> Result<(), ()> {
        let Ok(req) = GetFieldRequest::decode(payload, order) else {
            return Err(());
        };
        let Some(&instance) = self.channels.get(&req.server_cid) else {
            return Ok(());
        };

        let mut out = Vec::with_capacity(64);
        put_i32(&mut out, req.request_id, self.order);

        if req.sub_field.is_empty() {
            let template = self.fleet.instances[instance as usize].template;
            out.extend_from_slice(&self.fleet.templates[template as usize].intro_full);
        } else {
            // MIMUS replays stored descriptors without parsing their structure.
            // It therefore cannot select and describe an individual sub-field.
            // Returning the full descriptor would not answer the client's
            // request, so report the limitation explicitly.
            put_status_error(
                &mut out,
                "mimus replays whole type descriptors and cannot select a sub-field",
                self.order,
            );
        }
        self.push_app(commands::GET_FIELD, &out);
        Ok(())
    }

    fn on_destroy_channel(&mut self, payload: &[u8], order: ByteOrder) -> Result<(), ()> {
        let mut r = mimus_wire::Reader::new(payload, order);
        let Ok(server_cid) = r.i32() else {
            return Err(());
        };
        self.channels.remove(&server_cid);
        Ok(())
    }

    fn on_destroy_request(&mut self, payload: &[u8], order: ByteOrder) -> Result<(), ()> {
        let mut r = mimus_wire::Reader::new(payload, order);
        let (Ok(_cid), Ok(request_id)) = (r.i32(), r.i32()) else {
            return Err(());
        };
        if self.monitors.remove(&request_id).is_some_and(|m| m.started)
            && let Some(shared) = &self.shared
        {
            shared.subscription_stopped();
        }
        self.schedule.remove(request_id);
        Ok(())
    }

    /// Calculates how many scheduler steps are due based on elapsed time.
    ///
    /// The wheel previously advanced by one step per timer wakeup, making
    /// the delivered rate depend on timer accuracy rather than the configured
    /// rate. In one measurement, a 1 ms interval fired at about 660 Hz, so a
    /// fleet configured for 20 Hz delivered only 13.9 Hz. Nearly a third of
    /// the requested load was missing, despite the machine being 93% idle.
    ///
    /// Advancing according to elapsed time keeps the delivered rate aligned
    /// with the configured rate, regardless of timer granularity or jitter.
    /// This is essential for a load generator: an unreported shortfall makes
    /// the consumer's measurements misleading.
    ///
    /// Catch-up is capped to avoid emitting a large burst after a pause, such
    /// as a debugger stop, laptop suspension, or long GC pause. Any excess
    /// elapsed time is skipped, and the tick is counted as late.
    fn steps_due(&mut self, clock: std::time::Instant) -> u32 {
        const MAX_CATCH_UP: u32 = 16;

        let period = std::time::Duration::from_micros(self.cfg.tick_us.max(1));
        let Some(prev) = self.last_tick else {
            self.last_tick = Some(clock);
            return 1;
        };
        let elapsed = clock.duration_since(prev);
        let steps = (elapsed.as_nanos() / period.as_nanos().max(1)) as u64;
        if steps == 0 {
            // The timer woke before the next step was due. Do not advance
            // the wheel, or it would run faster than the configured rate.
            return 0;
        }
        let capped = steps.min(MAX_CATCH_UP as u64) as u32;
        if steps > MAX_CATCH_UP as u64 {
            self.counters.late_ticks += 1;
            self.pending.late_ticks += 1;
        }
        // Advance only by the steps processed. Any excess time is skipped
        // rather than being counted again on the next tick.
        self.last_tick = Some(prev + period * capped);
        capped
    }

    fn emit_due(&mut self) {
        let clock = std::time::Instant::now();
        let steps = self.steps_due(clock);
        for _ in 0..steps {
            self.emit_one_step();
        }
    }

    fn emit_one_step(&mut self) {
        // Update both counters: `counters` tracks this session, while
        // `pending` is accumulated into the shared totals. Updating only one
        // leaves session reports and shared reports inconsistent.
        self.counters.wheel_steps += 1;
        self.pending.wheel_steps += 1;
        let over_high_water = self.outbox.len() >= self.cfg.high_water;
        let due = self.schedule.tick();
        if due.is_empty() {
            return;
        }

        if over_high_water && self.cfg.backpressure == Backpressure::Drop {
            // The consumer is not keeping up. Drop this tick's updates and
            // account for them, as an IOC does when its monitor queue overflows.
            let n: u64 = due.iter().map(|d| d.repeats as u64).sum();
            self.counters.dropped_backpressure += n;
            self.pending.dropped_backpressure += n;
            self.publish();
            return;
        }

        let now = EpicsTime::now();
        for d in due {
            // A channel configured above the tick rate may be due for multiple
            // updates. Spread their timestamps across the tick so consumers
            // see a sequence of updates rather than a simultaneous burst.
            for k in 0..d.repeats {
                // In nanoseconds: above 1 MHz per channel the spacing is
                // under a microsecond, and counted in microseconds it was
                // zero, so the whole burst shared one timestamp.
                let at = if d.spacing_ns == 0 {
                    now
                } else {
                    now.plus_nanos(k as u64 * d.spacing_ns)
                };
                self.emit_one(d.request_id, at, Shape::SteadyState);
            }
        }
    }

    /// Renders one monitor update into the outbox.
    ///
    /// Used both by the scheduler and when a monitor starts. Keeping framing,
    /// counters, and flow control in one place ensures both paths behave
    /// consistently. The caller selects whether to render the initial value
    /// or a steady-state update.
    fn emit_one(&mut self, request_id: i32, now: EpicsTime, shape: Shape) {
        let strict = self.cfg.pipeline == Pipeline::Strict;
        let Some(mon) = self.monitors.get_mut(&request_id) else {
            return;
        };
        if strict && mon.pipelined {
            if mon.credits <= 0 {
                // The client has exhausted its available credits. A PVXS
                // server waits for more credit instead of sending an update.
                self.counters.flow_control_holds += 1;
                self.pending.flow_control_holds += 1;
                return;
            }
            mon.credits -= 1;
        }
        let instance = self.fleet.instances[mon.instance as usize];
        let steady = &self.fleet.templates[instance.template as usize];
        // Profiles captured before initial updates were recorded have no
        // initial body. Fall back to the steady-state body rather than
        // suppressing the update entirely.
        let compiled = match shape {
            Shape::Initial => steady.initial.as_deref().unwrap_or(steady),
            Shape::SteadyState => steady,
        };

        let mut payload = Vec::with_capacity(compiled.max_len() + 8);
        put_i32(&mut payload, mon.request_id, self.order);
        payload.push(0); // Sub-command: a standard monitor update.
        // Initial values are seeded by channel, so channels of the same
        // template do not all start with identical data.
        let step = match shape {
            Shape::Initial => instance.seed,
            Shape::SteadyState => mon.step,
        };
        // Steady-state updates are stamped with their emission time. Initial
        // values represent the channel's current value and use its last
        // processing time, as they do in a GET response.
        let when = match shape {
            Shape::Initial => last_processed(&self.schedule, &instance, now),
            Shape::SteadyState => now,
        };
        compiled.render(step, instance.seed, when, &mut payload);
        if shape == Shape::SteadyState {
            mon.step += 1;
        } else if steady.initial.is_some()
            && let Some(desc) = &self.fleet.openings[instance.template as usize]
        {
            restamp(desc, &mut payload[MONITOR_BODY..], when);
        }

        let hdr = Header::application(
            commands::MONITOR,
            payload.len() as i32,
            self.order,
            Direction::FromServer,
            Segmentation::None,
        );
        if self.cfg.debug && !self.debugged.contains(&(instance.template, shape)) {
            self.debugged.push((instance.template, shape));
            let head: Vec<String> = payload
                .iter()
                .take(24)
                .map(|b| format!("{b:02x}"))
                .collect();
            eprintln!(
                "  emit {:<24} {:?} {:>4} B  {}",
                compiled.name,
                shape,
                payload.len(),
                head.join(" ")
            );
        }

        self.outbox.extend_from_slice(&hdr.encode());
        self.outbox.extend_from_slice(&payload);

        self.counters.emitted += 1;
        self.pending.emitted += 1;
        let n = (payload.len() + mimus_wire::HEADER_SIZE) as u64;
        self.counters.bytes += n;
        self.pending.bytes += n;
    }

    /// Determines whether the outbox should be flushed.
    ///
    /// Without coalescing, flush every time, matching IOC behavior. When
    /// coalescing is enabled, flush once the outbox reaches the size threshold
    /// or the time limit expires, whichever comes first. This ensures that
    /// even a quiet connection does not hold messages indefinitely.
    fn should_flush(&self) -> bool {
        if self.cfg.coalesce_us == 0 {
            return true;
        }
        // Flush before the outbox reaches the high-water mark and the drop
        // policy may discard updates. Coalescing should trade latency for
        // fewer writes, not cause avoidable data loss.
        if self.outbox.len() >= self.cfg.high_water / 2 {
            return true;
        }
        match self.last_flush {
            None => true,
            Some(at) => at.elapsed() >= std::time::Duration::from_micros(self.cfg.coalesce_us),
        }
    }

    fn push_app(&mut self, command: u8, payload: &[u8]) {
        let hdr = Header::application(
            command,
            payload.len() as i32,
            self.order,
            Direction::FromServer,
            Segmentation::None,
        );
        self.outbox.extend_from_slice(&hdr.encode());
        self.outbox.extend_from_slice(payload);
    }

    // Publish the counters accumulated since the previous call.
    fn publish(&mut self) {
        if let Some(shared) = &self.shared {
            shared.add(&self.pending);
        }
        self.pending = Counters::default();
    }

    /// Returns the buffered replies to the client.
    ///
    /// Replies are protocol-critical and must be delivered in full. Unlike
    /// monitor updates, they are not subject to the backpressure drop policy:
    /// dropping part of a handshake, echo, or channel response can leave the
    /// client waiting indefinitely.
    ///
    /// This distinction is important when writing to a socket that is not
    /// immediately writable. An earlier non-blocking implementation allowed
    /// `WouldBlock` to interrupt the handshake, leaving the byte-order marker
    /// in the outbox while the client waited for it.
    async fn flush_reply(&mut self, stream: &mut TcpStream) -> std::io::Result<()> {
        self.publish();
        if self.outbox.is_empty() {
            return Ok(());
        }
        stream.write_all(&self.outbox).await?;
        self.outbox.clear();
        self.last_flush = Some(std::time::Instant::now());
        Ok(())
    }

    async fn flush(&mut self, stream: &mut TcpStream) -> std::io::Result<()> {
        self.publish();
        if self.outbox.is_empty() {
            return Ok(());
        }
        match self.cfg.backpressure {
            // Let TCP apply backpressure naturally. The resulting throughput
            // reflects the consumer's actual receive capacity, which is the
            // measurement this policy is intended to capture.
            Backpressure::Block => {
                stream.write_all(&self.outbox).await?;
                self.outbox.clear();
            }

            // Write only what the socket can accept immediately.
            //
            // Using `write_all` here would defeat the drop policy: if the
            // consumer stops reading, the socket fills and the session blocks
            // inside the write. The tick loop then stops, so updates are
            // neither emitted nor counted as dropped.
            //
            // In a test where the client subscribed and then stopped reading,
            // MIMUS emitted only 2,910 updates per second out of 400,000
            // offered, while reporting zero drops.
            //
            // Retain any unwritten bytes so the buffer remains above the
            // high-water threshold on subsequent ticks. New updates can then
            // be dropped and counted as intended.
            Backpressure::Drop => loop {
                match stream.try_write(&self.outbox) {
                    Ok(0) => break,
                    Ok(n) => {
                        self.outbox.advance(n);
                        if self.outbox.is_empty() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            },
        }
        self.last_flush = Some(std::time::Instant::now());
        Ok(())
    }
}

/// Offset of the value body in a monitor update, after the request ID and sub-command.
const MONITOR_BODY: usize = 5;

/// Replaces the timestamp in a replayed opening message with the current
/// run's timestamp.
///
/// Opening messages are captured once per channel during calibration.
/// Replaying one unchanged would preserve the calibration timestamp as the
/// channel's first sample. This function updates the timestamp when present
/// and returns the end offset of the value body.
///
/// Returns `None` if the descriptor cannot locate the value in the body.
/// In that case, the body is left unchanged.
fn restamp(desc: &Descriptor, body: &mut [u8], when: EpicsTime) -> Option<usize> {
    let at = desc.locate(body).ok()?;
    if let Some(site) = at.stamp {
        desc.write_stamp(body, site, when.seconds, when.nanos);
    }
    Some(at.value_end)
}

/// Returns the last processing time for a channel, used to timestamp GET
/// responses and monitor opening messages.
///
/// Both messages represent the channel's current value. An IOC timestamps
/// that value with the record's last processing time, not the time of the
/// request. A client that reads a channel and then subscribes therefore
/// receives the same timestamp twice. An archiver that discards samples
/// with duplicate timestamps stores the value only once. Using the send
/// time instead makes the two samples appear distinct, causing both to
/// be stored.
///
/// MIMUS has no records, so it approximates the last processing time with
/// the most recent boundary of the channel's update period. The boundary
/// is offset by the channel's seed, giving each channel a stable phase.
/// All values sent within one period share the same timestamp. The result
/// is never in the future and is at most one period old. If the rate does
/// not yield a usable period, the current time is returned.
fn last_processed(schedule: &Schedule, instance: &Instance, now: EpicsTime) -> EpicsTime {
    let rate = schedule
        .wheel(usize::from(instance.rate_group))
        .map_or(0.0, |w| w.rate_hz());
    let period_ns = 1e9 / rate;
    if !(period_ns.is_finite() && period_ns >= 1.0) {
        return now;
    }
    let period_ns = period_ns.round() as u64;
    now.last_boundary(period_ns, phase_of(instance.seed) % period_ns)
}

/// Computes a stable phase within the update period from a channel seed.
///
/// Uses the finalizer from SplitMix64 to spread channels across the period,
/// avoiding identical processing timestamps for consecutive seeds.
fn phase_of(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
