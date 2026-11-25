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
use mimus_emit::{Activity, Backpressure, Counters, EpicsTime, Instance, RateMix, Schedule};
use mimus_profile::DescriptorPolicy;
use mimus_wire::codec::put_i32;
use mimus_wire::header::{ByteOrder, Direction, Header, Segmentation};
use mimus_wire::pvdata::{Descriptor, Located};
use mimus_wire::{
    ConnectionValidation, ConnectionValidationResponse, CreateChannelRequest,
    CreateChannelResponse, Frame, Framer, GUID_LEN, GetFieldRequest, GetRequest, MonitorRequest,
    PipelineAck, SearchRequest, SearchResponse, commands, connection_validated, put_status_error,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::fleet::Fleet;
use crate::registry::Registry;
use crate::shared::{Dropper, SharedCounters};
use crate::stage::{Stage, Stages};
use crate::written::Written;

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
    /// Shared request to close one live session without warning, to
    /// exercise a consumer's reconnection. `None` disables connection drops.
    pub dropper: Option<Arc<Dropper>>,
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
    /// The stages a scenario sets for each virtual IOC, if the run plays
    /// one. The session follows the stage of its own IOC.
    pub stages: Option<Arc<Stages>>,
    /// The highest rate factor the scenario asks for, at least 1.
    ///
    /// The schedule is built at the configured rates times this factor and
    /// thinned back to the factor of the moment. A raised rate then spreads
    /// its updates over the wheel as the configured one does, instead of
    /// arriving in clumps once per configured period, and no wheel has to be
    /// rebuilt while clients are connected.
    pub headroom: f64,
    /// Whether channels post in bursts and fall silent in between, and how.
    /// `None` sends every update a channel is due.
    pub activity: Option<Activity>,
    /// Alarm transitions per channel per hour, for the channels whose type
    /// carries an alarm variant. `None` sends the ordinary update only.
    pub alarms: Option<f64>,
    /// What a PUT does.
    pub put: Put,
    /// What an RPC gets.
    pub rpc: Rpc,
    /// Where what is written is kept, shared by every connection of a run so
    /// that each sees what another wrote. `None` keeps it to the session.
    pub written: Option<Arc<Written>>,
}

/// What a PUT does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Put {
    /// What is written under `value` is what the channel carries from then
    /// on: in its updates on every connection, in a read, in an RPC reply.
    /// What is written elsewhere is answered and counted, and left out, as
    /// an IOC leaves a write to a field no record holds.
    #[default]
    Apply,
    /// Answered and counted, and changes nothing.
    Accept,
    /// Refused with an error status, as before MIMUS answered writes.
    Refuse,
}

/// What an RPC gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rpc {
    /// The channel's value, as a read gets it, whatever the request: MIMUS
    /// runs no service, and this gives the RPC path of a client or a gateway
    /// replies of the size and type the channel has.
    #[default]
    Value,
    /// Refused with an error status.
    Refuse,
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
            dropper: None,
            coalesce_us: 0,
            debug: false,
            pipeline: Pipeline::default(),
            stages: None,
            headroom: 1.0,
            activity: None,
            alarms: None,
            put: Put::default(),
            rpc: Rpc::default(),
            written: None,
        }
    }
}

/// Controls flow control for pipelined subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pipeline {
    /// Decode and count credits without withholding updates.
    ///
    /// This is the default: every subscription gets the offered rate,
    /// whatever its client acknowledges. A PVXS server would not send to a
    /// pipelined monitor with no credit left; for a monitor that is not
    /// pipelined, the two policies are the same.
    #[default]
    Permissive,
    /// Stop emitting updates when a pipelined monitor runs out of credit,
    /// matching the behavior of a PVXS server.
    ///
    /// The credit starts at the queue size the client gave on INIT and
    /// grows with each acknowledgement, as in PVXS's `servermon.cpp`.
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

impl std::fmt::Display for Put {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Apply => "apply",
            Self::Accept => "accept",
            Self::Refuse => "refuse",
        })
    }
}

impl std::str::FromStr for Put {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "apply" => Ok(Self::Apply),
            "accept" => Ok(Self::Accept),
            "refuse" => Ok(Self::Refuse),
            other => Err(format!(
                "unknown PUT policy {other:?}: apply, accept or refuse"
            )),
        }
    }
}

impl std::fmt::Display for Rpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Value => "value",
            Self::Refuse => "refuse",
        })
    }
}

impl std::str::FromStr for Rpc {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "value" => Ok(Self::Value),
            "refuse" => Ok(Self::Refuse),
            other => Err(format!("unknown RPC policy {other:?}: value or refuse")),
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
    /// A delta that also carries an alarm transition.
    Alarm,
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
    /// Fraction of an update owed to this monitor while the schedule is
    /// thinned: the factor of the moment over the headroom, added on every
    /// due step, one update sent per whole unit. It starts at a fraction
    /// drawn from the channel's seed, so thinned channels do not all send on
    /// the same steps.
    credit: f64,
    /// Whether the activity model has the channel active, and so sending.
    active: bool,
    /// Updates the channel has been due on this monitor: the count the
    /// activity model draws from.
    due: u64,
    /// Alarm transitions sent on this monitor: the place in the alarm
    /// variant's recorded transitions, replayed in the order observed.
    alarm_step: u64,
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
    /// MIMUS closed the connection on purpose, under `--drop-connections`.
    Dropped,
    /// The connection's virtual IOC went down in a scenario.
    IocDown,
}

/// Completes when this session has claimed a drop request, and never when
/// connection drops are disabled.
async fn claim_drop(dropper: Option<&Dropper>) {
    match dropper {
        Some(d) => d.claim().await,
        None => std::future::pending().await,
    }
}

/// Completes with the new stage when the session's IOC changes stage, and
/// never when the run plays no scenario or the scenario has ended.
async fn stage_changed(rx: &mut Option<tokio::sync::watch::Receiver<Stage>>) -> Stage {
    if let Some(r) = rx
        && r.changed().await.is_ok()
    {
        return *r.borrow_and_update();
    }
    std::future::pending().await
}

impl std::fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientClosed => f.write_str("client closed"),
            Self::Protocol(e) => write!(f, "protocol error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Dropped => f.write_str("dropped by MIMUS"),
            Self::IocDown => f.write_str("closed: its IOC went down"),
        }
    }
}

/// State for one client connection.
pub struct Session {
    cfg: SessionConfig,
    fleet: Arc<Fleet>,
    /// Where what clients write is kept: the run's, shared by every
    /// connection, or the session's own. `None` unless PUTs are applied.
    written: Option<Arc<Written>>,
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
    /// The scenario stage of this session's IOC, as last seen.
    stage: Stage,
    /// Where stage changes arrive, if the run plays a scenario.
    stage_rx: Option<tokio::sync::watch::Receiver<Stage>>,
    order: ByteOrder,
    inbox: BytesMut,
    outbox: BytesMut,
    framer: Framer,
}

impl Session {
    /// Creates a session with its own channel, monitor, and registry state.
    pub fn new(cfg: SessionConfig, fleet: Arc<Fleet>) -> Result<Self, mimus_emit::WheelError> {
        let schedule = Schedule::new(&cfg.rate_mix.scaled(cfg.headroom), cfg.tick_us)?;
        let registry = Registry::new(fleet.templates.len(), cfg.descriptor_policy);
        let stage_rx = cfg.stages.as_ref().and_then(|s| s.watch(cfg.ioc));
        // Bursts asked for before the session existed are not its to send.
        let stage = stage_rx.as_ref().map(|r| *r.borrow()).unwrap_or_default();
        let written = match cfg.put {
            Put::Apply => Some(
                cfg.written
                    .clone()
                    .unwrap_or_else(|| Written::new(fleet.instances.len())),
            ),
            Put::Accept | Put::Refuse => None,
        };
        Ok(Self {
            written,
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
            stage,
            stage_rx,
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
        // An idle connection that still woke up a thousand times per second
        // would spend more on timer processing and `Sleep::poll` than MIMUS
        // spends on its own work, as profiling an idle session shows, and an
        // archiver opening hundreds of connections would pay that on every
        // one of them before receiving a single update.
        //
        // Create the interval once and let the select guard disable polling
        // while the schedule is empty. It is not rebuilt on each tick.
        let mut ticker = tokio::time::interval(std::time::Duration::from_micros(self.cfg.tick_us));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // One wait for a drop request, kept across iterations: recreating it
        // on every tick would register with the shared request a thousand
        // times a second per session.
        let dropper = self.cfg.dropper.clone();
        let mut dropped = std::pin::pin!(claim_drop(dropper.as_deref()));
        let mut stage_rx = self.stage_rx.take();

        let end = loop {
            // An IOC can go down between the accept and the first pass.
            if self.stage.down {
                break SessionEnd::IocDown;
            }
            // When there is nothing scheduled, wait only for socket input.
            // `pending()` never completes, so the timer branch is disabled
            // until the schedule contains work.
            let idle = self.schedule.is_empty();
            tokio::select! {
                () = &mut dropped => break SessionEnd::Dropped,
                stage = stage_changed(&mut stage_rx) => {
                    if self.follow(stage) && self.should_flush()
                        && let Err(e) = self.flush(&mut stream).await {
                            break SessionEnd::Io(e);
                        }
                }
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
            match end {
                SessionEnd::Dropped => shared.session_dropped(),
                SessionEnd::IocDown => shared.session_closed_down(),
                _ => {}
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
                        // Although the only field used is the authentication string, whose
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
            commands::PUT => self.on_put(&frame.payload, order),
            commands::RPC => self.on_rpc(&frame.payload, order),
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

            // A pipelined subscription is declared here, not at START: the
            // client sets the pipeline bit on INIT and appends its queue
            // size, which PVXS takes as the first window.
            let credit = req.initial_credit(payload, order);
            self.monitors.insert(
                req.request_id,
                Monitor {
                    instance,
                    request_id: req.request_id,
                    started: false,
                    pipelined: credit.is_some(),
                    credits: credit.unwrap_or(0),
                    step: 0,
                    credit: unit_fraction(self.fleet.instances[instance as usize].seed),
                    active: self.cfg.activity.is_none_or(|a| {
                        a.starts_active(self.fleet.instances[instance as usize].seed)
                    }),
                    due: 0,
                    alarm_step: 0,
                },
            );
            return Ok(());
        }

        // A non-init monitor message can combine up to three actions:
        // granting flow-control credit, changing subscription state, and
        // terminating the request. PVXS accepts a credit alongside a START or
        // a STOP, so process each applicable action rather than returning
        // after the first.
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
            let mut steady = false;
            if let Some(mon) = self.monitors.get_mut(&req.request_id)
                && !mon.started
            {
                mon.started = true;
                let instance = &self.fleet.instances[mon.instance as usize];
                group = instance.rate_group;
                steady = !self.fleet.templates[usize::from(instance.template)].is_static();
                emit_now = true;
            }
            if emit_now {
                // A static template has nothing to send after its opening.
                if steady {
                    self.schedule.insert(group, req.request_id);
                }
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

    /// Refuses an operation MIMUS does not serve, such as a PUT or an RPC.
    ///
    /// A PVXS server answers the INIT of an operation its channel does not
    /// implement with an error status, and the client reports it. Ignoring
    /// the request instead would leave the client waiting for its own timeout.
    fn refuse(
        &mut self,
        command: u8,
        what: &str,
        payload: &[u8],
        order: ByteOrder,
    ) -> Result<(), ()> {
        let Ok(req) = GetRequest::decode(payload, order) else {
            return Err(());
        };
        if !req.is_init() || !self.channels.contains_key(&req.server_cid) {
            return Ok(());
        }
        let mut out = Vec::with_capacity(64);
        put_i32(&mut out, req.request_id, self.order);
        out.push(req.sub_command);
        put_status_error(
            &mut out,
            &format!("{what} refused: this MIMUS run serves no {what}"),
            self.order,
        );
        self.push_app(command, &out);
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
        let compiled =
            &self.fleet.templates[self.fleet.instances[instance as usize].template as usize];

        let mut out = Vec::with_capacity(compiled.max_len() + 16);
        put_i32(&mut out, req.request_id, self.order);
        out.push(req.sub_command);

        if req.is_init() {
            out.extend_from_slice(&compiled.intro_full);
        } else {
            self.read(instance, &mut out);
        }
        self.push_app(commands::GET, &out);
        Ok(())
    }

    /// Appends a read's reply after its request ID and sub-command: an OK
    /// status and the channel's value, from its changed set to the end of
    /// the value, or an error status if the profile has no complete value.
    fn read(&self, instance: u32, out: &mut Vec<u8>) {
        let template = self.fleet.instances[instance as usize].template as usize;
        if self.fleet.templates[template].initial.is_none() {
            put_status_error(
                out,
                "this profile carries no complete value for the channel",
                self.order,
            );
            return;
        }
        mimus_wire::codec::put_status_ok(out);
        let body = out.len();
        if let Some(at) = self.current(instance, out) {
            out.truncate(body + at.value_end);
        }
    }

    /// Appends the channel's value as a read gets it, in the form of a
    /// monitor update: the opening message a subscription would receive,
    /// carrying what was written to the channel, stamped with its last
    /// processing time. Returns where it lies relative to where it was
    /// appended, or `None` if the profile has no complete value or its type
    /// cannot be read, in which case what was appended is sent unchanged.
    fn current(&self, instance: u32, out: &mut Vec<u8>) -> Option<Located> {
        let inst = self.fleet.instances[instance as usize];
        let template = inst.template as usize;
        let initial = self.fleet.templates[template].initial.as_deref()?;
        let rate = channel_rate(&self.cfg.rate_mix, self.stage.gain, &inst);
        let when = last_processed(rate, &inst, EpicsTime::now());
        let body = out.len();
        initial.render(inst.seed, inst.seed, when, out);
        apply_written(self.written.as_deref(), &self.fleet, instance, out, body);
        let desc = self.fleet.openings[template].as_ref()?;
        restamp(desc, &mut out[body..], when)
    }

    /// Answers a PUT.
    ///
    /// The exchange is a read's: an INIT reply carrying the channel's type,
    /// then, for each write, the value the client built from that type, after
    /// its changed set, answered with a status. A write with the `0x40` bit
    /// asks instead for the value about to be written over, as a read does;
    /// PVXS clients ask before they build a value from the one present.
    ///
    /// Under [`Put::Apply`], what is written under `value` is kept and
    /// carried from then on, by every update, read and RPC reply of the
    /// channel, on every connection sharing the store. MIMUS does not post
    /// an update on a write: the next one the channel sends carries it, as
    /// the next scan of a periodic record would. A read right after the
    /// write returns it at once.
    fn on_put(&mut self, payload: &[u8], order: ByteOrder) -> Result<(), ()> {
        if self.cfg.put == Put::Refuse {
            return self.refuse(commands::PUT, "PUT", payload, order);
        }
        let Ok(req) = GetRequest::decode(payload, order) else {
            return Err(());
        };
        let Some(&instance) = self.channels.get(&req.server_cid) else {
            return Ok(());
        };
        let template = self.fleet.instances[instance as usize].template as usize;
        let mut out = Vec::with_capacity(64);
        put_i32(&mut out, req.request_id, self.order);
        out.push(req.sub_command);
        if req.is_init() {
            out.extend_from_slice(&self.fleet.templates[template].intro_full);
        } else if req.sub_command & commands::put::GET != 0 {
            self.read(instance, &mut out);
        } else {
            let n = (payload.len() + mimus_wire::HEADER_SIZE) as u64;
            self.counters.puts += 1;
            self.pending.puts += 1;
            self.counters.put_bytes += n;
            self.pending.put_bytes += n;
            match self.write(instance, &payload[GPR_REQUEST..], order) {
                Ok(()) => mimus_wire::codec::put_status_ok(&mut out),
                Err(why) => put_status_error(&mut out, why, self.order),
            }
        }
        self.push_app(commands::PUT, &out);
        Ok(())
    }

    /// Keeps what a PUT writes under the channel's `value`, under
    /// [`Put::Apply`]. A value that is not of the channel's type is refused;
    /// one MIMUS cannot read, because the type's descriptor could not be,
    /// is accepted and changes nothing.
    fn write(&self, instance: u32, value: &[u8], order: ByteOrder) -> Result<(), &'static str> {
        let Some(written) = &self.written else {
            return Ok(());
        };
        let template = self.fleet.instances[instance as usize].template as usize;
        let Some(desc) = &self.fleet.descriptors[template] else {
            return Ok(());
        };
        let fields = desc
            .written(value, order)
            .map_err(|_| "the value written is not of the channel's type")?;
        let Some(bits) = desc.field_bits("value") else {
            return Ok(());
        };
        written.write(
            instance,
            fields.into_iter().filter(|(bit, _)| bits.contains(bit)),
        );
        Ok(())
    }

    /// Answers an RPC with the channel's value, under [`Rpc::Value`].
    ///
    /// The INIT reply is a status alone. Each request, whatever it carries,
    /// is answered with a status, the channel's type and its value in full,
    /// without a changed set: what a read returns, in the form an RPC reply
    /// takes, the fields a read leaves out written empty. The request is not
    /// read.
    fn on_rpc(&mut self, payload: &[u8], order: ByteOrder) -> Result<(), ()> {
        if self.cfg.rpc == Rpc::Refuse {
            return self.refuse(commands::RPC, "RPC", payload, order);
        }
        let Ok(req) = GetRequest::decode(payload, order) else {
            return Err(());
        };
        let Some(&instance) = self.channels.get(&req.server_cid) else {
            return Ok(());
        };
        let template = self.fleet.instances[instance as usize].template as usize;
        let mut out = Vec::with_capacity(self.fleet.templates[template].max_len() + 64);
        put_i32(&mut out, req.request_id, self.order);
        out.push(req.sub_command);
        if req.is_init() {
            mimus_wire::codec::put_status_ok(&mut out);
        } else {
            self.counters.rpcs += 1;
            self.pending.rpcs += 1;
            let mut value = Vec::new();
            let full = self.current(instance, &mut value).and_then(|_| {
                let desc = self.fleet.openings[template].as_ref()?;
                desc.full_value(&value).ok()
            });
            match full {
                Some(full) => {
                    // The stored descriptor begins with the OK status.
                    out.extend_from_slice(&self.fleet.templates[template].intro_full);
                    out.extend_from_slice(&full);
                }
                None => put_status_error(
                    &mut out,
                    "this profile carries no complete value for the channel",
                    self.order,
                ),
            }
        }
        self.push_app(commands::RPC, &out);
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
    /// The wheel advances by the time that has passed, not by one step per
    /// timer wakeup. A timer does not fire as often as asked: a 1 ms interval
    /// measured firing at about 660 Hz would turn a fleet configured for
    /// 20 Hz into one delivering 13.9 Hz, on a machine 93% idle. For a load
    /// generator that shortfall would be silent, and the consumer's
    /// measurements wrong.
    ///
    /// See [`catch_up`] for what happens when the session falls far behind.
    fn steps_due(&mut self, clock: std::time::Instant) -> u32 {
        let period = std::time::Duration::from_micros(self.cfg.tick_us.max(1));
        let Some(prev) = self.last_tick else {
            self.last_tick = Some(clock);
            return 1;
        };
        let (steps, now, late) = catch_up(prev, clock, period);
        if late {
            self.counters.late_ticks += 1;
            self.pending.late_ticks += 1;
        }
        self.last_tick = Some(now);
        steps
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
        let mut due = self.schedule.tick();
        if due.is_empty() {
            return;
        }
        self.thin(&mut due);
        self.keep_active(&mut due);

        if self.stage.frozen {
            // A frozen IOC processes nothing: what falls due is held, and
            // never sent, rather than queued for the resume.
            let n: u64 = due.iter().map(|d| u64::from(d.repeats)).sum();
            self.counters.held += n;
            self.pending.held += n;
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
                let shape = self.alarm_or_steady(d.request_id);
                self.emit_one(d.request_id, at, shape);
            }
        }
    }

    /// Thins this step's due monitors down to the rate of the moment.
    ///
    /// The schedule runs at the configured rates times the headroom. Each
    /// due monitor is owed the factor of the moment over the headroom for
    /// each update the schedule offers it, and sends one update per whole
    /// unit it is owed, its timestamps spread over the tick as the
    /// schedule's were. Without a scenario both are 1, and nothing is
    /// touched.
    fn thin(&mut self, due: &mut Vec<mimus_emit::Due>) {
        let ratio = self.stage.gain / self.cfg.headroom;
        if ratio == 1.0 {
            return;
        }
        due.retain_mut(|d| {
            let Some(mon) = self.monitors.get_mut(&d.request_id) else {
                return false;
            };
            mon.credit += ratio * f64::from(d.repeats);
            let owed = mon.credit.floor();
            mon.credit -= owed;
            let n = owed as u32;
            if n == 0 {
                return false;
            }
            d.spacing_ns = d.spacing_ns * u64::from(d.repeats) / u64::from(n);
            d.spacing_us = d.spacing_ns / 1000;
            d.repeats = n;
            true
        });
    }

    /// Decides whether the update a monitor is due carries an alarm
    /// transition, under `--alarms`.
    ///
    /// A channel of a type with an alarm variant sends a transition with the
    /// probability that gives it the transitions per hour asked for, at the
    /// rate it runs at. The draw comes from the channel's seed and its
    /// counts, so the same channel alarms at the same updates on every run.
    /// The transitions themselves follow one another as calibration saw
    /// them, a raise and later its clear.
    fn alarm_or_steady(&self, request_id: i32) -> Shape {
        let Some(per_hour) = self.cfg.alarms else {
            return Shape::SteadyState;
        };
        let Some(mon) = self.monitors.get(&request_id) else {
            return Shape::SteadyState;
        };
        let instance = &self.fleet.instances[mon.instance as usize];
        if self.fleet.templates[usize::from(instance.template)]
            .alarm
            .is_none()
        {
            return Shape::SteadyState;
        }
        let rate = channel_rate(&self.cfg.rate_mix, self.stage.gain, instance);
        if rate <= 0.0 {
            return Shape::SteadyState;
        }
        let chance = per_hour / 3600.0 / rate;
        let draw = unit_fraction(
            instance.seed ^ (mon.step + mon.alarm_step).wrapping_mul(0xD6E8_FEB8_6659_FD93),
        );
        if draw < chance {
            Shape::Alarm
        } else {
            Shape::SteadyState
        }
    }

    /// Keeps, of each due monitor's updates, those its channel would send:
    /// what falls due while the activity model has it quiet is counted as
    /// quiet and dropped from the step.
    ///
    /// The model moves on once per update the channel is due, so its bursts
    /// and silences are counted in updates at the rate of the moment.
    fn keep_active(&mut self, due: &mut Vec<mimus_emit::Due>) {
        let Some(activity) = self.cfg.activity.filter(|a| !a.is_constant()) else {
            return;
        };
        let mut quiet = 0u64;
        due.retain_mut(|d| {
            let Some(mon) = self.monitors.get_mut(&d.request_id) else {
                return false;
            };
            let seed = self.fleet.instances[mon.instance as usize].seed;
            let mut sent = 0u32;
            for _ in 0..d.repeats {
                if mon.active {
                    sent += 1;
                }
                mon.active = activity.next(mon.active, seed, mon.due);
                mon.due += 1;
            }
            quiet += u64::from(d.repeats - sent);
            if sent == 0 {
                return false;
            }
            d.spacing_ns = d.spacing_ns * u64::from(d.repeats) / u64::from(sent);
            d.spacing_us = d.spacing_ns / 1000;
            d.repeats = sent;
            true
        });
        self.counters.quiet += quiet;
        self.pending.quiet += quiet;
    }

    /// Takes in a new stage of this session's IOC, and returns whether
    /// anything was queued for the client.
    ///
    /// A burst is sent at once; whether it reaches the client in one write
    /// is then up to the flush policy, as for any tick.
    fn follow(&mut self, stage: Stage) -> bool {
        let burst = stage.bursts != self.stage.bursts;
        self.stage = stage;
        if burst && !stage.frozen && !stage.down {
            return self.burst(stage.burst_share) > 0;
        }
        false
    }

    /// Sends one update from each started monitor the burst selects, all
    /// stamped with the same instant, as when a beam dump moves thousands of
    /// channels at once. Returns how many were queued.
    ///
    /// The selection is drawn from each channel's seed and the burst's
    /// number, so a burst of half the channels picks the same half on every
    /// connection and in every run. A static channel has nothing to send.
    /// Under the drop policy, what does not fit under the high-water mark is
    /// dropped and counted, as for any tick.
    fn burst(&mut self, share: f64) -> u64 {
        let draw = self.stage.bursts;
        let mut selected: Vec<i32> = self
            .monitors
            .values()
            .filter(|m| m.started)
            .filter(|m| {
                let instance = &self.fleet.instances[m.instance as usize];
                !self.fleet.templates[usize::from(instance.template)].is_static()
                    && unit_fraction(instance.seed ^ draw.wrapping_mul(0xA24B_AED4_963E_E407))
                        < share
            })
            .map(|m| m.request_id)
            .collect();
        // In request order, so a burst's messages do not depend on how the
        // monitor table happens to be laid out.
        selected.sort_unstable();
        let now = EpicsTime::now();
        let mut queued = 0;
        for request_id in selected {
            if self.cfg.backpressure == Backpressure::Drop
                && self.outbox.len() >= self.cfg.high_water
            {
                self.counters.dropped_backpressure += 1;
                self.pending.dropped_backpressure += 1;
                continue;
            }
            let before = self.counters.emitted;
            self.emit_one(request_id, now, Shape::SteadyState);
            queued += self.counters.emitted - before;
        }
        self.counters.burst += queued;
        self.pending.burst += queued;
        self.publish();
        queued
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
            Shape::Alarm => steady.alarm.as_deref().unwrap_or(steady),
        };

        let mut payload = Vec::with_capacity(compiled.max_len() + 8);
        put_i32(&mut payload, mon.request_id, self.order);
        payload.push(0); // Sub-command: a standard monitor update.
        // Initial values are seeded by channel, so channels of the same
        // template do not all start with identical data.
        let step = match shape {
            Shape::Initial => instance.seed,
            Shape::SteadyState => mon.step,
            Shape::Alarm => mon.alarm_step,
        };
        // Steady-state updates are stamped with their emission time. Initial
        // values represent the channel's current value and use its last
        // processing time, as they do in a GET response.
        let when = match shape {
            Shape::Initial => last_processed(
                channel_rate(&self.cfg.rate_mix, self.stage.gain, &instance),
                &instance,
                now,
            ),
            Shape::SteadyState | Shape::Alarm => now,
        };
        compiled.render(step, instance.seed, when, &mut payload);
        apply_written(
            self.written.as_deref(),
            &self.fleet,
            mon.instance,
            &mut payload,
            MONITOR_BODY,
        );
        if shape == Shape::SteadyState {
            mon.step += 1;
        } else if shape == Shape::Alarm {
            mon.alarm_step += 1;
            self.counters.alarms += 1;
            self.pending.alarms += 1;
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
/// run's timestamp, and the data's timestamp beside it if there is one.
///
/// Opening messages are captured once per channel during calibration.
/// Replaying one unchanged would preserve the calibration timestamp as the
/// channel's first sample. This function updates the timestamp when present
/// and returns the end offset of the value body.
///
/// Returns `None` if the descriptor cannot locate the value in the body.
/// In that case, the body is left unchanged.
fn restamp(desc: &Descriptor, body: &mut [u8], when: EpicsTime) -> Option<Located> {
    let at = desc.locate(body).ok()?;
    // An NTNDArray's frame was taken when it was sent, as far as a consumer
    // can tell: its dataTimeStamp is restamped with its timeStamp.
    for site in [at.stamp, at.data_stamp].into_iter().flatten() {
        desc.write_stamp(body, site, when.seconds, when.nanos);
    }
    Some(at)
}

/// Writes what was written to a channel over the fields of the update at
/// `buf[at..]` that carries them, if anything was.
///
/// One atomic load for a channel nothing was written to, which is almost
/// every update of almost every run.
fn apply_written(
    written: Option<&Written>,
    fleet: &Fleet,
    instance: u32,
    buf: &mut Vec<u8>,
    at: usize,
) {
    let Some(fields) = written.and_then(|w| w.get(instance)) else {
        return;
    };
    let template = fleet.instances[instance as usize].template as usize;
    if let Some(desc) = &fleet.descriptors[template] {
        // An update the walker cannot read is sent as rendered.
        let _ = desc.overwrite(buf, at, &fields);
    }
}

/// Bytes before a GET, PUT or RPC request's data: the server channel ID,
/// the request ID and the sub-command.
const GPR_REQUEST: usize = 4 + 4 + 1;

/// The most steps a session runs in one wakeup to catch up.
const MAX_CATCH_UP: u32 = 16;

/// How far the wheel advances when the timer fires at `clock`, the wheel
/// having last advanced to `prev`.
///
/// Returns the steps to run now, the instant they bring the wheel to, and
/// whether the session fell so far behind that time was skipped. Up to
/// [`MAX_CATCH_UP`] steps are run, so a short delay is made up. Past that,
/// after a debugger stop, a suspended machine, or a session that cannot
/// keep up, the rest is skipped rather than sent later as a burst: the
/// wheel jumps to `clock`, and the caller counts a late tick. What was
/// skipped is never sent, so a late tick means the offered load was not
/// delivered in full.
fn catch_up(
    prev: std::time::Instant,
    clock: std::time::Instant,
    period: std::time::Duration,
) -> (u32, std::time::Instant, bool) {
    let elapsed = clock.saturating_duration_since(prev);
    let steps = elapsed.as_nanos() / period.as_nanos().max(1);
    if steps == 0 {
        // The timer woke before the next step was due. Advancing anyway
        // would run the wheel faster than the configured rate.
        return (0, prev, false);
    }
    if steps > u128::from(MAX_CATCH_UP) {
        return (MAX_CATCH_UP, clock, true);
    }
    let steps = steps as u32;
    (steps, prev + period * steps, false)
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
fn last_processed(rate: f64, instance: &Instance, now: EpicsTime) -> EpicsTime {
    let period_ns = 1e9 / rate;
    if !(period_ns.is_finite() && period_ns >= 1.0) {
        return now;
    }
    let period_ns = period_ns.round() as u64;
    now.last_boundary(period_ns, phase_of(instance.seed) % period_ns)
}

/// The rate a channel runs at at the moment: its group's configured rate
/// times the scenario's factor.
fn channel_rate(mix: &RateMix, gain: f64, instance: &Instance) -> f64 {
    let configured = mix
        .groups
        .get(usize::from(instance.rate_group))
        .map_or(0.0, |g| g.0);
    configured * gain
}

/// Maps a seed to a fraction in [0, 1), the same for the same seed.
fn unit_fraction(seed: u64) -> f64 {
    (phase_of(seed) >> 11) as f64 / (1u64 << 53) as f64
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_short_delay_is_made_up() {
        let t0 = Instant::now();
        let p = Duration::from_millis(1);
        assert_eq!(catch_up(t0, t0 + p / 2, p), (0, t0, false));
        let (steps, now, late) = catch_up(t0, t0 + p * 3 + p / 2, p);
        assert_eq!((steps, now, late), (3, t0 + p * 3, false));
        // The half period left over is kept for the next wakeup.
        assert_eq!(catch_up(now, t0 + p * 4, p).0, 1);
    }

    #[test]
    fn a_long_stall_is_skipped_not_sent_as_a_burst() {
        // 100 steps behind: 16 are run, the rest is skipped and the tick
        // counted late. Carrying the rest over would send it at 16 steps
        // per wakeup until caught up, a burst that the late count would
        // then misdescribe as load not delivered.
        let t0 = Instant::now();
        let p = Duration::from_millis(1);
        let clock = t0 + p * 100;
        assert_eq!(catch_up(t0, clock, p), (MAX_CATCH_UP, clock, true));
        assert_eq!(
            catch_up(clock, clock, p),
            (0, clock, false),
            "nothing is left over"
        );
    }
}
