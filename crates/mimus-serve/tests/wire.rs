//! End-to-end tests over real sockets.
//!
//! The client is hand-written rather than reused from the server code.
//! If both sides shared an encoder, the same mistake could appear in
//! both and go undetected, allowing every test to pass against a wire
//! format that real clients do not accept.
//!
//! These tests verify that MIMUS can serve a peer implementing the
//! protocol independently. Compatibility with PVXS is verified
//! separately by the conformance suite.
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use mimus_emit::{Backpressure, RateMix};
use mimus_profile::{
    Assignment, DescriptorPolicy, Introspection, Manifest, Profile, ScalarType, SessionRules, Slot,
    SlotRole, SlotWidth, Template,
};
use mimus_serve::fleet::{Fleet, FleetSpec, Layout};
use mimus_serve::session::{Session, SessionConfig};
use mimus_wire::codec::{Reader, put_i16, put_i32, put_size, put_string};
use mimus_wire::header::{ByteOrder, Direction, Header, Segmentation};
use mimus_wire::{Frame, Framer, commands};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const LE: ByteOrder = ByteOrder::Little;
const MONITOR: u8 = 0x0D;

// ---------------------------------------------------------------- fixtures

fn template(name: &str, intro_tag: u8) -> Template {
    let mut t = Template::new(name, MONITOR);
    // Match the format stored by calibration: an OK status followed by
    // the descriptor. Omitting the status would shift the descriptor
    // offset and make the fixture inconsistent with real profiles.
    t.with_introspection(Introspection {
        full: vec![0xFF, 0xFD, intro_tag, 0x00, 0x80],
        id_only: Some(vec![0xFF, 0xFE, intro_tag, 0x00]),
    })
    .push_fixed(vec![0x03, 0x83, 0x01])
    .push_slot(
        Slot::new(1, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::Value {
            scalar: ScalarType::F64,
        }),
    )
    .push_slot(Slot::new(2, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::TimestampSeconds))
    .push_slot(Slot::new(3, SlotWidth::Fixed { bytes: 4 }).with_role(SlotRole::TimestampNanos));
    t.observations = 100;
    t
}

fn profile() -> Profile {
    let mut p = Profile::new(
        Manifest {
            pvxs_version: "1.4.2".into(),
            epics_base_version: "7.0.8".into(),
            catalogue_sha256: "d".repeat(64),
            host: "test".into(),
            captured_at: "2026-08-15T00:00:00Z".into(),
            mimus_version: "0.1.0".into(),
            study_duration_s: 1.0,
            pvs_observed: 2,
            synthetic: false,
        },
        SessionRules::default(),
    );
    p.templates.push(template("dbl", 1));
    p.templates.push(template("int", 2));
    p.assignments = vec![
        Assignment {
            pv: "A".into(),
            template: "dbl".into(),
        },
        Assignment {
            pv: "B".into(),
            template: "int".into(),
        },
    ];
    p
}

fn fleet(pvs: u32, iocs: u16) -> Arc<Fleet> {
    let p = profile();
    let spec = FleetSpec {
        iocs,
        pvs,
        layout: Layout::Blocked,
        mix: vec![("dbl".into(), 1), ("int".into(), 1)],
        rate_mix: mimus_emit::RateMix::uniform(1.0),
        names: mimus_serve::NameTemplate::default_pattern("MIM"),
    };
    Arc::new(Fleet::build(&p, &spec).unwrap())
}

/// Starts a session on an ephemeral port and returns its socket address.
async fn start(fleet: Arc<Fleet>, cfg: SessionConfig) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut cfg = cfg;
    cfg.port = addr.port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let session = Session::new(cfg, fleet).unwrap();
        session.run(stream).await;
    });
    addr
}

fn fast_config() -> SessionConfig {
    SessionConfig {
        // Use a 50 Hz update rate with a 1 ms tick. Tests finish quickly
        // while the scheduler still has enough buckets to exercise its wheel.
        rate_mix: RateMix::uniform(50.0),
        tick_us: 1000,
        ..Default::default()
    }
}

// ------------------------------------------------------------- test client

/// A minimal PVA client implemented independently of the server.
struct Client {
    stream: TcpStream,
    framer: Framer,
    inbox: BytesMut,
    next_id: i32,
}

impl Client {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        stream.set_nodelay(true).unwrap();
        Self {
            stream,
            framer: Framer::new(),
            inbox: BytesMut::with_capacity(64 * 1024),
            next_id: 1,
        }
    }

    fn id(&mut self) -> i32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn send(&mut self, command: u8, payload: &[u8]) {
        let hdr = Header::application(
            command,
            payload.len() as i32,
            LE,
            Direction::FromClient,
            Segmentation::None,
        );
        self.stream.write_all(&hdr.encode()).await.unwrap();
        self.stream.write_all(payload).await.unwrap();
    }

    /// Reads the next complete frame, or returns `None` if the deadline
    /// expires or the connection encounters an error.
    async fn recv(&mut self, within: Duration) -> Option<Frame> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Ok(Some(frame)) = self.framer.next_frame(&mut self.inbox) {
                return Some(frame);
            }
            let read = tokio::time::timeout_at(deadline, self.stream.read_buf(&mut self.inbox));
            match read.await {
                Ok(Ok(0)) | Err(_) => return None,
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => return None,
            }
        }
    }

    /// Reads until an application frame with the requested command arrives,
    /// or the deadline expires.
    async fn recv_command(&mut self, command: u8, within: Duration) -> Option<Frame> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            let frame = self.recv(left).await?;
            if frame.header.is_application() && frame.command() == command {
                return Some(frame);
            }
        }
    }

    /// Performs the byte-order exchange and connection-validation handshake.
    async fn handshake(&mut self) {
        let marker = self.recv(Duration::from_secs(2)).await.expect("byte order");
        assert!(marker.is_control());
        assert_eq!(marker.header.command, commands::CTRL_SET_BYTE_ORDER);
        assert_eq!(
            marker.header.direction(),
            Direction::FromServer,
            "the server must mark its own messages"
        );

        let validation = self
            .recv_command(commands::CONNECTION_VALIDATION, Duration::from_secs(2))
            .await
            .expect("validation");
        let mut r = Reader::new(&validation.payload, LE);
        assert!(r.i32().unwrap() > 0, "buffer size");
        let _registry = r.i16().unwrap();
        assert_eq!(r.size().unwrap().or_empty(), 1);
        assert_eq!(r.string().unwrap(), "anonymous");

        let mut p = Vec::new();
        put_i32(&mut p, 32768, LE);
        put_i16(&mut p, 1024, LE);
        put_i16(&mut p, 0, LE);
        put_string(&mut p, "anonymous", LE);
        // The null type code represents absent authentication data.
        //
        // This independently implemented client is intended to catch
        // disagreements between the two sides, but it cannot catch a mistake
        // they share. Both implementations omitted this byte and therefore
        // agreed with each other, while PVXS rejected the connection.
        // Only comparison with the reference implementation can establish
        // compatibility with the actual protocol.
        p.push(0xFF);
        self.send(commands::CONNECTION_VALIDATION, &p).await;

        let validated = self
            .recv_command(commands::CONNECTION_VALIDATED, Duration::from_secs(2))
            .await
            .expect("validated");
        assert_eq!(validated.payload[0], 0xFF, "OK status");
    }

    async fn search(&mut self, names: &[&str]) -> Vec<i32> {
        let mut p = Vec::new();
        put_i32(&mut p, 42, LE);
        p.push(0x81);
        p.extend_from_slice(&[0, 0, 0]);
        p.extend_from_slice(&[0u8; 16]);
        p.extend_from_slice(&0u16.to_le_bytes());
        put_size(&mut p, 1, LE);
        put_string(&mut p, "tcp", LE);
        put_i16(&mut p, names.len() as i16, LE);
        let mut ids = Vec::new();
        for name in names {
            let id = self.id();
            ids.push(id);
            put_i32(&mut p, id, LE);
            put_string(&mut p, name, LE);
        }
        self.send(commands::SEARCH, &p).await;

        let Some(frame) = self
            .recv_command(commands::SEARCH_RESPONSE, Duration::from_millis(500))
            .await
        else {
            return Vec::new();
        };
        let mut r = Reader::new(&frame.payload, LE);
        r.skip(mimus_wire::GUID_LEN).unwrap();
        assert_eq!(r.i32().unwrap(), 42, "sequence id echoed");
        r.skip(16).unwrap();
        let _port = r.u16().unwrap();
        assert_eq!(r.string().unwrap(), "tcp");
        assert_eq!(r.u8().unwrap(), 1);
        let n = r.i16().unwrap();
        (0..n).map(|_| r.i32().unwrap()).collect()
    }

    /// Opens a channel and returns its server-assigned channel ID.
    ///
    /// Returns `None` if the server does not reply before the timeout.
    async fn create_channel(&mut self, name: &str) -> Option<i32> {
        let cid = self.id();
        let mut p = Vec::new();
        put_i16(&mut p, 1, LE);
        put_i32(&mut p, cid, LE);
        put_string(&mut p, name, LE);
        self.send(commands::CREATE_CHANNEL, &p).await;

        let frame = self
            .recv_command(commands::CREATE_CHANNEL, Duration::from_millis(500))
            .await?;
        let mut r = Reader::new(&frame.payload, LE);
        assert_eq!(r.i32().unwrap(), cid, "client channel id echoed");
        let server_cid = r.i32().unwrap();
        assert_eq!(r.u8().unwrap(), 0xFF, "OK status");
        Some(server_cid)
    }

    /// Sends a monitor INIT request and returns its request ID and descriptor.
    async fn monitor_init(&mut self, server_cid: i32) -> (i32, Vec<u8>) {
        let request_id = self.id();
        let mut p = Vec::new();
        put_i32(&mut p, server_cid, LE);
        put_i32(&mut p, request_id, LE);
        p.push(commands::monitor::INIT);
        p.push(0xFF); // empty pvRequest
        self.send(commands::MONITOR, &p).await;

        // Monitors started earlier may already be sending updates on this
        // connection. The INIT reply is therefore not necessarily the next
        // MONITOR frame, so filter by request ID and sub-command.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!left.is_zero(), "no monitor init reply for {request_id}");
            let Some(frame) = self.recv_command(commands::MONITOR, left).await else {
                panic!("connection closed before the init reply");
            };
            let mut r = Reader::new(&frame.payload, LE);
            if r.i32().unwrap() != request_id {
                continue;
            }
            if r.u8().unwrap() & commands::monitor::INIT == 0 {
                continue;
            }
            return (request_id, r.rest().to_vec());
        }
    }

    /// Starts a subscription.
    ///
    /// The sub-command is `0x44`, with both state bits set. A bare `0x04`
    /// means stop, not start. A permissive server may accept it as a start,
    /// allowing this mistake to go unnoticed until a real PVXS server
    /// rejected every subscription opened by MIMUS.
    async fn monitor_start(&mut self, server_cid: i32, request_id: i32) {
        self.monitor_start_with(server_cid, request_id, commands::monitor::START)
            .await;
    }

    /// Starts a pipelined subscription by combining the start state bits
    /// with the flow-control flag.
    async fn monitor_start_pipelined(&mut self, server_cid: i32, request_id: i32) {
        self.monitor_start_with(
            server_cid,
            request_id,
            commands::monitor::START | commands::monitor::PIPELINE,
        )
        .await;
    }

    async fn monitor_start_with(&mut self, server_cid: i32, request_id: i32, sub: u8) {
        let mut p = Vec::new();
        put_i32(&mut p, server_cid, LE);
        put_i32(&mut p, request_id, LE);
        p.push(sub);
        self.send(commands::MONITOR, &p).await;
    }

    /// Collects up to `n` steady-state monitor updates for the given request ID
    /// before the deadline expires.
    async fn collect_updates(
        &mut self,
        request_id: i32,
        n: usize,
        within: Duration,
    ) -> Vec<Vec<u8>> {
        let deadline = tokio::time::Instant::now() + within;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let Some(frame) = self.recv_command(commands::MONITOR, left).await else {
                break;
            };
            let mut r = Reader::new(&frame.payload, LE);
            if r.i32().unwrap() != request_id {
                continue;
            }
            let sub = r.u8().unwrap();
            if sub & commands::monitor::INIT != 0 {
                continue;
            }
            out.push(r.rest().to_vec());
        }
        out
    }

    async fn subscribe(&mut self, name: &str) -> (i32, Vec<u8>) {
        let cid = self.create_channel(name).await.expect("channel");
        let (request_id, intro) = self.monitor_init(cid).await;
        self.monitor_start(cid, request_id).await;
        (request_id, intro)
    }
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn a_client_can_handshake() {
    let addr = start(fleet(10, 1), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;
}

#[tokio::test]
async fn search_finds_channels_that_exist() {
    let f = fleet(10, 1);
    let name = f.names[3].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;
    assert_eq!(c.search(&[&name]).await.len(), 1);
}

#[tokio::test]
async fn search_stays_silent_for_channels_that_do_not() {
    // Replying to every search miss would turn a single broadcast round
    // into a flood of unnecessary responses.
    let addr = start(fleet(10, 1), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;
    assert!(c.search(&["NO:SUCH:PV"]).await.is_empty());
}

#[tokio::test]
async fn an_ioc_does_not_claim_another_ioc_s_channels() {
    // Each virtual IOC must answer only for its own channels. If every
    // IOC claimed every channel, multi-round client searches would always
    // succeed in the first round, leaving later search rounds untested.
    let f = fleet(100, 4);
    let mine = f.names[0].clone();
    let theirs = f.names[99].clone();
    assert_ne!(f.instances[0].ioc, f.instances[99].ioc);

    let mut cfg = fast_config();
    cfg.ioc = 0;
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    assert_eq!(c.search(&[&mine]).await.len(), 1);
    assert!(c.search(&[&theirs]).await.is_empty());
}

#[tokio::test]
async fn a_channel_can_be_opened_and_monitored() {
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, intro) = c.subscribe(&name).await;
    assert!(!intro.is_empty(), "init must carry a type descriptor");
    assert_eq!(intro[0], 0xFF, "an OK status comes first");
    assert_eq!(intro[1], 0xFD, "first channel gets the full descriptor");

    let updates = c
        .collect_updates(request_id, 3, Duration::from_secs(3))
        .await;
    assert_eq!(updates.len(), 3, "expected three updates");
}

#[tokio::test]
async fn updates_carry_changing_values() {
    // Repeating an identical frame could allow consumer-side
    // deduplication to hide a throughput problem. Verify that successive
    // updates contain changing values while retaining a fixed length.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 5, Duration::from_secs(3))
        .await;
    assert!(updates.len() >= 2);
    assert_ne!(updates[0], updates[1], "consecutive updates must differ");
    assert!(
        updates.iter().all(|u| u.len() == updates[0].len()),
        "a fixed-width template must produce fixed-length updates"
    );
}

#[tokio::test]
async fn the_second_channel_of_a_type_gets_the_cached_identifier() {
    // Descriptor caching avoids sending the full type descriptor for
    // every subscription. At million-channel scale, this reduces setup
    // traffic from one full descriptor per channel to one per type.
    let f = fleet(10, 1);
    let first = f.names[0].clone();
    let second = f.names[2].clone();
    assert_eq!(
        f.instances[0].template, f.instances[2].template,
        "fixture must use two channels of one type"
    );

    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (_, intro_a) = c.subscribe(&first).await;
    let (_, intro_b) = c.subscribe(&second).await;

    assert_eq!(intro_a[1], 0xFD, "full descriptor");
    assert_eq!(intro_b[1], 0xFE, "identifier only");
    assert!(intro_b.len() < intro_a.len());
}

#[tokio::test]
async fn each_type_introduces_itself_once() {
    let f = fleet(10, 1);
    let dbl = f.names[0].clone();
    let int = f.names[1].clone();
    assert_ne!(f.instances[0].template, f.instances[1].template);

    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (_, a) = c.subscribe(&dbl).await;
    let (_, b) = c.subscribe(&int).await;
    assert_eq!(a[1], 0xFD);
    assert_eq!(b[1], 0xFD, "a different type introduces itself too");
    assert_ne!(a, b, "the two descriptors must be distinguishable");
}

#[tokio::test]
async fn a_fresh_connection_starts_with_an_empty_type_cache() {
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;

    let mut c = Client::connect(addr).await;
    c.handshake().await;
    let (_, first) = c.subscribe(&name).await;
    assert_eq!(first[1], 0xFD);
    drop(c);

    // The type cache belongs to the session, not the fleet. A new
    // connection must receive the full descriptor even when another
    // client has already subscribed to a channel of the same type.
    let addr2 = start(Arc::clone(&f), fast_config()).await;
    let mut c2 = Client::connect(addr2).await;
    c2.handshake().await;
    let (_, again) = c2.subscribe(&name).await;
    assert_eq!(again[1], 0xFD, "the new client's cache is empty");
}

#[tokio::test]
async fn nothing_is_emitted_before_start() {
    // INIT establishes the monitor and provides its descriptor.
    // Updates must not be sent until the client explicitly starts it.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;

    let updates = c
        .collect_updates(request_id, 1, Duration::from_millis(300))
        .await;
    assert!(updates.is_empty(), "no updates before START");
}

#[tokio::test]
async fn an_echo_comes_back_unchanged() {
    // PVXS clients use echo requests to detect inactive connections.
    // If the server ignores them, clients eventually close their sessions,
    // making the disconnection difficult to diagnose from either side.
    let addr = start(fleet(10, 1), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    c.send(commands::ECHO, b"ping").await;
    let reply = c
        .recv_command(commands::ECHO, Duration::from_millis(500))
        .await
        .expect("echo reply");
    assert_eq!(&reply.payload[..], b"ping");
}

#[tokio::test]
async fn many_channels_all_flow() {
    let f = fleet(50, 1);
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let mut ids = Vec::new();
    for i in 0..20 {
        let (request_id, _) = c.subscribe(&f.names[i].clone()).await;
        ids.push(request_id);
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut seen = std::collections::HashSet::new();
    while seen.len() < ids.len() {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let Some(frame) = c.recv_command(commands::MONITOR, left).await else {
            break;
        };
        let mut r = Reader::new(&frame.payload, LE);
        let request_id = r.i32().unwrap();
        if r.u8().unwrap() & commands::monitor::INIT == 0 {
            seen.insert(request_id);
        }
    }
    assert_eq!(seen.len(), ids.len(), "every subscribed channel must emit");
}

#[tokio::test]
async fn a_malformed_message_does_not_kill_the_connection() {
    // A malformed payload does not corrupt the frame boundaries, so the
    // server can safely process the next message. Closing the session
    // instead would turn a recoverable request error into a disconnect.
    let addr = start(fleet(10, 1), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    c.send(commands::SEARCH, &[0x01, 0x02]).await;

    c.send(commands::ECHO, b"still here").await;
    let reply = c
        .recv_command(commands::ECHO, Duration::from_millis(500))
        .await
        .expect("connection survived");
    assert_eq!(&reply.payload[..], b"still here");
}

#[tokio::test]
async fn stopping_a_monitor_stops_the_updates() {
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;
    c.monitor_start(cid, request_id).await;
    assert!(
        !c.collect_updates(request_id, 1, Duration::from_secs(2))
            .await
            .is_empty()
    );

    let mut p = Vec::new();
    put_i32(&mut p, cid, LE);
    put_i32(&mut p, request_id, LE);
    p.push(commands::monitor::STOP);
    c.send(commands::MONITOR, &p).await;

    // Drain updates that may already be queued before checking for silence.
    let _ = c
        .collect_updates(request_id, 100, Duration::from_millis(200))
        .await;
    let after = c
        .collect_updates(request_id, 1, Duration::from_millis(500))
        .await;
    assert!(after.is_empty(), "no updates after STOP");
}

#[tokio::test]
async fn the_drop_policy_is_selectable() {
    // This verifies that the Drop policy is passed through to the session;
    // it does not test saturation, which requires a client that stops
    // reading from the socket.
    let cfg = SessionConfig {
        backpressure: Backpressure::Drop,
        high_water: 4096,
        ..fast_config()
    };
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;
    let (request_id, _) = c.subscribe(&name).await;
    assert!(
        !c.collect_updates(request_id, 1, Duration::from_secs(2))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn always_full_policy_never_caches() {
    let cfg = SessionConfig {
        descriptor_policy: DescriptorPolicy::AlwaysFull,
        ..fast_config()
    };
    let f = fleet(10, 1);
    let a = f.names[0].clone();
    let b = f.names[2].clone();
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (_, first) = c.subscribe(&a).await;
    let (_, second) = c.subscribe(&b).await;
    assert_eq!(first[1], 0xFD);
    assert_eq!(second[1], 0xFD, "AlwaysFull must not use the cache");
}

#[tokio::test]
async fn a_big_endian_search_is_answered() {
    // PVXS sends search requests in big-endian order, regardless of the
    // byte order declared by the server in CTRL_SET_BYTE_ORDER. Decoding
    // them with a connection-wide byte order misreads the channel count
    // and can silently suppress the response. The little-endian test
    // client above could not expose this mismatch; it only became apparent
    // when testing against a real PVXS client.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    const BE: ByteOrder = ByteOrder::Big;
    let mut p = Vec::new();
    put_i32(&mut p, 42, BE);
    p.push(0x81);
    p.extend_from_slice(&[0, 0, 0]);
    p.extend_from_slice(&[0u8; 16]);
    p.extend_from_slice(&0u16.to_be_bytes());
    put_size(&mut p, 1, BE);
    put_string(&mut p, "tcp", BE);
    put_i16(&mut p, 1, BE);
    put_i32(&mut p, 7, BE);
    put_string(&mut p, &name, BE);

    let hdr = Header::application(
        commands::SEARCH,
        p.len() as i32,
        BE,
        Direction::FromClient,
        Segmentation::None,
    );
    c.stream.write_all(&hdr.encode()).await.unwrap();
    c.stream.write_all(&p).await.unwrap();

    let frame = c
        .recv_command(commands::SEARCH_RESPONSE, Duration::from_millis(500))
        .await
        .expect("a big-endian search must be answered");
    let mut r = Reader::new(&frame.payload, LE);
    r.skip(mimus_wire::GUID_LEN).unwrap();
    assert_eq!(r.i32().unwrap(), 42, "sequence id echoed");
    r.skip(16).unwrap();
    let _port = r.u16().unwrap();
    assert_eq!(r.string().unwrap(), "tcp");
    assert_eq!(r.u8().unwrap(), 1, "found flag");
    assert_eq!(r.i16().unwrap(), 1);
    assert_eq!(r.i32().unwrap(), 7, "the search instance id we sent");
}

#[tokio::test]
async fn a_big_endian_channel_and_monitor_work_too() {
    // The byte-order requirement applies to every client message, not just searches.
    // Exercise the full channel and monitor setup path in big-endian order so a
    // search-only fix cannot pass this test.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    const BE: ByteOrder = ByteOrder::Big;

    let mut p = Vec::new();
    put_i16(&mut p, 1, BE);
    put_i32(&mut p, 555, BE);
    put_string(&mut p, &name, BE);
    let hdr = Header::application(
        commands::CREATE_CHANNEL,
        p.len() as i32,
        BE,
        Direction::FromClient,
        Segmentation::None,
    );
    c.stream.write_all(&hdr.encode()).await.unwrap();
    c.stream.write_all(&p).await.unwrap();

    let frame = c
        .recv_command(commands::CREATE_CHANNEL, Duration::from_millis(500))
        .await
        .expect("a big-endian create must be answered");
    let mut r = Reader::new(&frame.payload, LE);
    assert_eq!(r.i32().unwrap(), 555, "client channel id echoed");
    let server_cid = r.i32().unwrap();

    for sub in [commands::monitor::INIT, commands::monitor::START | 0x40] {
        let mut p = Vec::new();
        put_i32(&mut p, server_cid, BE);
        put_i32(&mut p, 9, BE);
        p.push(sub);
        if sub == commands::monitor::INIT {
            p.push(0xFF);
        }
        let hdr = Header::application(
            commands::MONITOR,
            p.len() as i32,
            BE,
            Direction::FromClient,
            Segmentation::None,
        );
        c.stream.write_all(&hdr.encode()).await.unwrap();
        c.stream.write_all(&p).await.unwrap();
    }

    let updates = c.collect_updates(9, 2, Duration::from_secs(3)).await;
    assert_eq!(updates.len(), 2, "big-endian setup must still produce data");
}

#[tokio::test]
async fn the_first_update_arrives_immediately_after_start() {
    // A real IOC posts the current value as soon as a monitor starts. MIMUS previously
    // waited for the next scheduled tick, adding up to one full period of startup
    // latency at low update rates. This delay was also visible in archiver
    // startup profiles at scale.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    // At one update per second, no scheduled update can arrive within
    // the 300 ms window below. Only an immediate post can satisfy it.
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(1.0),
        tick_us: 1000,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 1, Duration::from_millis(300))
        .await;
    assert_eq!(
        updates.len(),
        1,
        "START must post the current value at once"
    );
}

#[tokio::test]
async fn the_immediate_update_has_the_same_shape_as_the_rest() {
    // The immediate and scheduled updates deliberately use the same code
    // path. Their payloads must have the same structure so that clients
    // can process the first update just like every subsequent one.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 3, Duration::from_secs(3))
        .await;
    assert_eq!(updates.len(), 3);
    assert_eq!(
        updates[0].len(),
        updates[1].len(),
        "the first update must be the same shape as the second"
    );
    assert_ne!(updates[0], updates[1], "and it must not repeat it");
}

#[tokio::test]
async fn starting_twice_does_not_double_the_rate() {
    // A duplicate START could schedule the same monitor twice, doubling
    // its update rate for the remainder of the test.
    //
    // Counting updates over a short window would not reliably detect this.
    // The monitor is assigned to a bucket by the spreading logic, so its
    // first scheduled update can occur anywhere within one full period.
    // This is also consistent with real IOC scan threads, which run on
    // their own schedules. The reliable signal is the sustained update rate.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;
    c.monitor_start(cid, request_id).await;
    c.monitor_start(cid, request_id).await;

    // At 50 Hz, expect about 50 scheduled updates plus the immediate post.
    // A duplicate subscription would produce roughly twice as many.
    let updates = c
        .collect_updates(request_id, 200, Duration::from_millis(1000))
        .await;
    assert!(
        updates.len() < 75,
        "expected about 50 updates, got {} — the monitor is scheduled twice",
        updates.len()
    );
}

// ---------------------------------------------------- scheduling and rates

#[tokio::test]
async fn a_channel_faster_than_the_tick_bursts() {
    // At 5 kHz with a 1 ms tick, the channel must produce five updates
    // per tick. The old wheel rejected this rate and limited each channel
    // to one update per tick.
    //
    // Check the timestamps rather than the wall-clock arrival rate.
    // Counting arrivals measures the test client's throughput, which can
    // be much lower than the server's and varies across machines. An
    // earlier throughput-based assertion failed on a laptop even though
    // the server was behaving correctly.
    //
    // Updates within a burst are separated by less than one tick. A
    // scheduler limited to one update per tick cannot produce such a gap,
    // regardless of the client's speed.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(5000.0),
        tick_us: 1000,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 20, Duration::from_secs(3))
        .await;
    assert!(updates.len() >= 6, "got only {} updates", updates.len());

    // The fixture body is [changed-set 3][value 8][seconds 8][nanos 4].
    let at = |u: &Vec<u8>| {
        let n = u.len();
        let secs = i64::from_le_bytes(u[n - 12..n - 4].try_into().unwrap());
        let nanos = i32::from_le_bytes(u[n - 4..].try_into().unwrap()) as i64;
        secs * 1_000_000_000 + nanos
    };
    let smallest_gap = updates
        .windows(2)
        .map(|w| (at(&w[1]) - at(&w[0])).abs())
        .min()
        .expect("at least two updates");
    assert!(
        smallest_gap < 1_000_000,
        "closest updates are {smallest_gap} ns apart; a tick is 1 ms, so nothing burst"
    );
}

#[tokio::test]
async fn burst_updates_do_not_share_a_timestamp() {
    // An IOC does not assign the same timestamp to every update in a
    // burst. Repeated timestamps would also make latency measurements
    // based on this field unreliable.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(4000.0),
        tick_us: 1000,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 8, Duration::from_millis(500))
        .await;
    assert!(updates.len() >= 4);

    // The fixture template is [changed-set][value 8][seconds 8][nanos 4].
    let nanos_of = |u: &Vec<u8>| {
        let n = u.len();
        i32::from_le_bytes(u[n - 4..].try_into().unwrap())
    };
    let distinct: std::collections::HashSet<i32> = updates.iter().map(nanos_of).collect();
    assert!(
        distinct.len() > 1,
        "every update in the burst carried the same timestamp"
    );
}

#[tokio::test]
async fn rate_groups_fire_at_their_own_rates() {
    // A rate mix allows a small group of fast channels to coexist with
    // a larger group of slow channels, as in a real facility. A uniform
    // rate cannot represent this distribution.
    let p = profile();
    let spec = FleetSpec {
        iocs: 1,
        pvs: 100,
        layout: Layout::Blocked,
        mix: vec![("dbl".into(), 1)],
        // Half the channels run at 50 Hz and half at 1 Hz.
        rate_mix: RateMix::parse("50:1,1:1").unwrap(),
        names: mimus_serve::NameTemplate::default_pattern("MIM"),
    };
    let f = Arc::new(Fleet::build(&p, &spec).unwrap());

    let cfg = SessionConfig {
        rate_mix: spec.rate_mix.clone(),
        tick_us: 1000,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    // Channel 0 belongs to the first rate group; channel 1 belongs to the second.
    assert_eq!(f.instances[0].rate_group, 0);
    assert_eq!(f.instances[1].rate_group, 1);

    let (fast_id, _) = c.subscribe(&f.names[0].clone()).await;
    let (slow_id, _) = c.subscribe(&f.names[1].clone()).await;

    let fast = c
        .collect_updates(fast_id, 20, Duration::from_millis(1200))
        .await;
    let slow = c
        .collect_updates(slow_id, 20, Duration::from_millis(200))
        .await;
    assert!(
        fast.len() > slow.len() * 3,
        "fast group produced {} updates, slow group {}",
        fast.len(),
        slow.len()
    );
}

#[tokio::test]
async fn names_follow_the_template() {
    let p = profile();
    let spec = FleetSpec {
        iocs: 4,
        pvs: 100,
        layout: Layout::Blocked,
        mix: vec![("dbl".into(), 1)],
        rate_mix: RateMix::uniform(50.0),
        names: mimus_serve::NameTemplate::new("SR:C{ioc:02}:DEV-{n:04}:{i:05}").unwrap(),
    };
    let f = Arc::new(Fleet::build(&p, &spec).unwrap());
    assert_eq!(f.names[0], "SR:C00:DEV-0000:00000");
    assert_eq!(f.names[99], "SR:C03:DEV-0024:00099");

    // Verify that a client can search for a generated name.
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(50.0),
        ioc: 0,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;
    assert_eq!(c.search(&["SR:C00:DEV-0000:00000"]).await.len(), 1);
}

// ---------------------------------------------------- pipeline flow control

#[tokio::test]
async fn a_strict_pipeline_stops_when_credit_runs_out() {
    // A PVXS server withholds updates when the client has no available
    // credit. Sending regardless of credit would flood a consumer with
    // updates it has not yet requested capacity to receive.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(200.0),
        pipeline: mimus_serve::session::Pipeline::Strict,
        initial_credits: 3,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;
    c.monitor_start_pipelined(cid, request_id).await;

    // The three initial credits cover the immediate START update and two subsequent updates.
    // No more should arrive until credit is returned by the client.
    let updates = c
        .collect_updates(request_id, 20, Duration::from_millis(1500))
        .await;
    assert!(
        updates.len() <= 3,
        "expected at most the initial credit of 3, got {}",
        updates.len()
    );
}

#[tokio::test]
async fn credits_resume_a_stalled_pipeline() {
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(200.0),
        pipeline: mimus_serve::session::Pipeline::Strict,
        initial_credits: 2,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;
    c.monitor_start_pipelined(cid, request_id).await;

    let before = c
        .collect_updates(request_id, 10, Duration::from_millis(400))
        .await;
    assert!(before.len() <= 2);

    // Hand back twenty more.
    let mut p = Vec::new();
    put_i32(&mut p, cid, LE);
    put_i32(&mut p, request_id, LE);
    p.push(commands::monitor::PIPELINE);
    put_i32(&mut p, 20, LE);
    c.send(commands::MONITOR, &p).await;

    let after = c
        .collect_updates(request_id, 20, Duration::from_millis(1500))
        .await;
    assert!(
        after.len() > before.len(),
        "the subscription did not resume: {} then {}",
        before.len(),
        after.len()
    );
}

#[tokio::test]
async fn the_permissive_policy_ignores_credit() {
    // Permissive is the default. Incorrect credit encoding could stall
    // subscriptions entirely, which is more disruptive than sending a few
    // extra updates. Strict flow control therefore remains opt-in until
    // real-client acknowledgements have been validated.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(200.0),
        initial_credits: 1,
        ..Default::default()
    };
    assert_eq!(cfg.pipeline, mimus_serve::session::Pipeline::Permissive);

    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;
    c.monitor_start_pipelined(cid, request_id).await;
    let updates = c
        .collect_updates(request_id, 20, Duration::from_millis(1500))
        .await;
    assert!(
        updates.len() > 5,
        "permissive must keep sending regardless of credit, got {}",
        updates.len()
    );
}

#[tokio::test]
async fn a_non_pipelined_monitor_is_never_flow_controlled() {
    // Credit-based flow control applies only to subscriptions that request
    // pipelining. A regular monitor must not be affected by a policy it
    // did not opt into.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(200.0),
        pipeline: mimus_serve::session::Pipeline::Strict,
        initial_credits: 0,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;

    // Send a plain START without the pipeline bit.
    let mut p = Vec::new();
    put_i32(&mut p, cid, LE);
    put_i32(&mut p, request_id, LE);
    p.push(commands::monitor::START);
    c.send(commands::MONITOR, &p).await;

    let updates = c
        .collect_updates(request_id, 10, Duration::from_millis(500))
        .await;
    assert!(
        updates.len() > 3,
        "a non-pipelined monitor must flow freely, got {}",
        updates.len()
    );
}

#[tokio::test]
async fn a_pipelined_start_both_credits_and_starts() {
    // 0xC4 combines START with a credit update. Handling the credit bit
    // as a separate command, for example by returning early, can leave
    // the subscription initialized but never started. The connection
    // still succeeds, so the failure appears only as missing updates.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(50.0),
        pipeline: mimus_serve::session::Pipeline::Strict,
        initial_credits: 5,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;
    c.monitor_start_pipelined(cid, request_id).await;

    let updates = c
        .collect_updates(request_id, 3, Duration::from_secs(2))
        .await;
    assert!(
        !updates.is_empty(),
        "a pipelined start produced nothing at all"
    );
}

#[tokio::test]
async fn a_bare_stop_subcommand_stops_rather_than_starts() {
    // 0x04 is the STOP subcommand. Treating its bits as independent flags
    // can cause a server to interpret it as START, masking an invalid
    // client command until it connects to a stricter implementation.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;
    c.monitor_start_with(cid, request_id, commands::monitor::STOP)
        .await;

    let updates = c
        .collect_updates(request_id, 3, Duration::from_millis(600))
        .await;
    assert!(
        updates.is_empty(),
        "0x04 started a subscription instead of stopping it: {} updates",
        updates.len()
    );
}

#[tokio::test]
async fn a_subscription_opens_with_the_complete_value() {
    // A subscription starts with a complete value, followed by deltas.
    // If the initial complete value is missing, fields not covered by
    // subsequent deltas remain undefined for the lifetime of the connection.
    // In comparisons with pvxmonitor, this left only four fields populated
    // where a real IOC populated thirty-four.
    let p = profile_with_initial();
    let spec = FleetSpec {
        iocs: 1,
        pvs: 4,
        layout: Layout::Blocked,
        mix: vec![("dbl".into(), 1)],
        rate_mix: RateMix::uniform(50.0),
        names: mimus_serve::NameTemplate::default_pattern("MIM"),
    };
    let f = Arc::new(Fleet::build(&p, &spec).unwrap());
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&f.names[0].clone()).await;
    let updates = c
        .collect_updates(request_id, 3, Duration::from_secs(2))
        .await;
    assert!(updates.len() >= 2);
    assert_eq!(
        updates[0][0], 0xAA,
        "the first update must be the complete value"
    );
    assert!(
        updates[0].len() > updates[1].len(),
        "the opening message carries more than a delta: {} then {}",
        updates[0].len(),
        updates[1].len()
    );
    for later in &updates[1..] {
        assert_eq!(later[0], 0x03, "everything after it is a delta");
    }
}

#[tokio::test]
async fn two_channels_do_not_open_with_identical_bytes() {
    // The initial sample is specific to each channel, not just its type.
    // Reusing one sample for every channel of a type would make their
    // opening values identical, unlike independent PVs in a real facility.
    let p = profile_with_initial();
    let spec = FleetSpec {
        iocs: 1,
        pvs: 4,
        layout: Layout::Blocked,
        mix: vec![("dbl".into(), 1)],
        rate_mix: RateMix::uniform(50.0),
        names: mimus_serve::NameTemplate::default_pattern("MIM"),
    };
    let f = Arc::new(Fleet::build(&p, &spec).unwrap());
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (a, _) = c.subscribe(&f.names[0].clone()).await;
    let first_a = c.collect_updates(a, 1, Duration::from_secs(1)).await;
    let (b, _) = c.subscribe(&f.names[1].clone()).await;
    let first_b = c.collect_updates(b, 1, Duration::from_secs(1)).await;

    assert_eq!(first_a.len(), 1);
    assert_eq!(first_b.len(), 1);
    assert_ne!(first_a[0], first_b[0]);
}

/// Builds a profile with an initial message that can be distinguished from
/// subsequent deltas by its first byte and payload length.
fn profile_with_initial() -> Profile {
    let mut steady = template("dbl", 1);
    let mut opening = Template::new("dbl_initial", 0x0D);
    opening
        .with_introspection(Introspection {
            full: vec![0xFF, 0xFD, 0x01],
            id_only: Some(vec![0xFF, 0xFE, 0x01]),
        })
        .push_fixed(vec![0xAA, 0xBB, 0xCC, 0xDD])
        .push_slot(
            Slot::new(1, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::Value {
                scalar: ScalarType::F64,
            }),
        )
        .push_fixed(vec![0xEE; 16]);
    opening.observations = 100;
    steady.with_initial(opening);

    let mut p = Profile::new(
        Manifest {
            pvxs_version: "1.4.2".into(),
            epics_base_version: "7.0.8.1".into(),
            catalogue_sha256: "e".repeat(64),
            host: "test".into(),
            captured_at: "posix:0".into(),
            mimus_version: "0.1.0".into(),
            study_duration_s: 30.0,
            pvs_observed: 2,
            synthetic: false,
        },
        SessionRules::default(),
    );
    p.templates.push(steady);
    p.assignments.push(Assignment {
        pv: "A".into(),
        template: "dbl".into(),
    });
    p
}

#[tokio::test]
async fn a_channel_can_be_described_without_subscribing() {
    // This is how `pvxinfo` queries a channel descriptor and how clients
    // discover the fields a server exposes. Without GET_FIELD support,
    // MIMUS would appear to offer no introspection, and the same
    // `pvxinfo -v` comparison against a reference IOC and MIMUS could
    // not be performed using PVXS as the independent verifier.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let mut p = Vec::new();
    put_i32(&mut p, cid, LE);
    put_i32(&mut p, 77, LE);
    put_string(&mut p, "", LE);
    c.send(commands::GET_FIELD, &p).await;

    let frame = c
        .recv_command(commands::GET_FIELD, Duration::from_millis(500))
        .await
        .expect("no reply to GET_FIELD");
    let mut r = Reader::new(&frame.payload, LE);
    assert_eq!(r.i32().unwrap(), 77, "request id echoed");
    let rest = r.rest();
    assert_eq!(rest[0], 0xFF, "OK status");
    assert_eq!(rest[1], 0xFD, "then the descriptor the monitor would carry");
}

#[tokio::test]
async fn asking_for_a_sub_field_is_refused_rather_than_answered_wrongly() {
    // MIMUS replays descriptors without parsing their contents, so it cannot resolve
    // individual fields. Returning the full descriptor instead would answer a
    // different request without the client being able to detect the mismatch.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let mut p = Vec::new();
    put_i32(&mut p, cid, LE);
    put_i32(&mut p, 78, LE);
    put_string(&mut p, "timeStamp", LE);
    c.send(commands::GET_FIELD, &p).await;

    let frame = c
        .recv_command(commands::GET_FIELD, Duration::from_millis(500))
        .await
        .expect("no reply");
    let mut r = Reader::new(&frame.payload, LE);
    assert_eq!(r.i32().unwrap(), 78);
    assert_ne!(r.u8().unwrap(), 0xFF, "must not claim success");
}

#[tokio::test]
async fn describing_an_unknown_channel_says_nothing() {
    // The channel ID was never issued by the server. A response would
    // imply that the server knows the type of a nonexistent channel.
    let f = fleet(10, 1);
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let mut p = Vec::new();
    put_i32(&mut p, 4242, LE);
    put_i32(&mut p, 79, LE);
    put_string(&mut p, "", LE);
    c.send(commands::GET_FIELD, &p).await;
    assert!(
        c.recv_command(commands::GET_FIELD, Duration::from_millis(300))
            .await
            .is_none()
    );

    // The connection remains usable after the invalid request.
    c.send(commands::ECHO, b"alive").await;
    assert!(
        c.recv_command(commands::ECHO, Duration::from_millis(500))
            .await
            .is_some()
    );
}

// ------------------------------------------------------------- reading once

#[tokio::test]
async fn a_channel_can_be_read_without_subscribing() {
    // This is how `pvxget` requests a value. The exchange mirrors a monitor:
    // an INIT carrying the type, followed by a value carrying the changed-set
    // and data. MIMUS replies with both using the data it already holds.
    let p = profile_with_initial();
    let spec = FleetSpec {
        iocs: 1,
        pvs: 4,
        layout: Layout::Blocked,
        mix: vec![("dbl".into(), 1)],
        rate_mix: RateMix::uniform(50.0),
        names: mimus_serve::NameTemplate::default_pattern("MIM"),
    };
    let f = Arc::new(Fleet::build(&p, &spec).unwrap());
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&f.names[0].clone()).await.unwrap();

    let mut req = Vec::new();
    put_i32(&mut req, cid, LE);
    put_i32(&mut req, 55, LE);
    req.push(commands::get::INIT);
    req.push(0xFF);
    c.send(commands::GET, &req).await;

    let frame = c
        .recv_command(commands::GET, Duration::from_millis(500))
        .await
        .expect("no reply to a get init");
    let mut r = Reader::new(&frame.payload, LE);
    assert_eq!(r.i32().unwrap(), 55);
    assert_eq!(r.u8().unwrap(), commands::get::INIT);
    let rest = r.rest();
    assert_eq!(rest[0], 0xFF, "OK status");
    assert_eq!(rest[1], 0xFD, "then the type");

    let mut read = Vec::new();
    put_i32(&mut read, cid, LE);
    put_i32(&mut read, 55, LE);
    read.push(0x00);
    c.send(commands::GET, &read).await;

    let frame = c
        .recv_command(commands::GET, Duration::from_millis(500))
        .await
        .expect("no reply to a get");
    let mut r = Reader::new(&frame.payload, LE);
    assert_eq!(r.i32().unwrap(), 55);
    assert_eq!(r.u8().unwrap(), 0x00);
    let body = r.rest();
    assert_eq!(body[0], 0xFF, "OK status");
    assert_eq!(
        body[1], 0xAA,
        "then the complete value, not a delta with nothing to apply it to"
    );
}

#[tokio::test]
async fn a_read_without_a_complete_value_says_so() {
    // This profile was calibrated before opening messages were captured,
    // so it has no complete initial value. Sending a delta would leave the
    // client with data it cannot interpret as a complete value.
    let f = fleet(10, 1);
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&f.names[0].clone()).await.unwrap();
    let mut read = Vec::new();
    put_i32(&mut read, cid, LE);
    put_i32(&mut read, 56, LE);
    read.push(0x00);
    c.send(commands::GET, &read).await;

    let frame = c
        .recv_command(commands::GET, Duration::from_millis(500))
        .await
        .expect("no reply");
    let mut r = Reader::new(&frame.payload, LE);
    assert_eq!(r.i32().unwrap(), 56);
    assert_eq!(r.u8().unwrap(), 0x00);
    assert_ne!(r.u8().unwrap(), 0xFF, "must not claim success");
}

#[tokio::test]
async fn the_last_read_of_an_operation_is_still_answered() {
    // The destroy bit on a GET marks the client's last request on that
    // operation, not a teardown without a read. PVXS's server executes the
    // request and then releases the operation (`lastRequest` in serverget.cpp).
    // core-pva, the client used by Phoebus and the Archiver Appliance,
    // sends every read this way. MIMUS once silently ignored such requests,
    // leaving the Archiver's engine waiting indefinitely for a response.
    let p = profile_with_initial();
    let spec = FleetSpec {
        iocs: 1,
        pvs: 4,
        layout: Layout::Blocked,
        mix: vec![("dbl".into(), 1)],
        rate_mix: RateMix::uniform(50.0),
        names: mimus_serve::NameTemplate::default_pattern("MIM"),
    };
    let f = Arc::new(Fleet::build(&p, &spec).unwrap());
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&f.names[0].clone()).await.unwrap();
    let mut init = Vec::new();
    put_i32(&mut init, cid, LE);
    put_i32(&mut init, 57, LE);
    init.push(commands::get::INIT);
    init.push(0xFF);
    c.send(commands::GET, &init).await;
    c.recv_command(commands::GET, Duration::from_millis(500))
        .await
        .expect("no reply to a get init");

    let mut last = Vec::new();
    put_i32(&mut last, cid, LE);
    put_i32(&mut last, 57, LE);
    last.push(commands::get::DESTROY);
    c.send(commands::GET, &last).await;

    let frame = c
        .recv_command(commands::GET, Duration::from_millis(500))
        .await
        .expect("a last read must still be answered");
    let mut r = Reader::new(&frame.payload, LE);
    assert_eq!(r.i32().unwrap(), 57);
    assert_eq!(
        r.u8().unwrap(),
        commands::get::DESTROY,
        "the sub-command is echoed"
    );
    let body = r.rest();
    assert_eq!(body[0], 0xFF, "OK status");
    assert_eq!(body[1], 0xAA, "then the complete value");

    c.send(commands::ECHO, b"alive").await;
    assert!(
        c.recv_command(commands::ECHO, Duration::from_millis(500))
            .await
            .is_some()
    );
}

/// Seconds encoded in the recorded opening message: the calibration timestamp,
/// long past by the time the data is served.
const RECORDED_SECONDS: i64 = 1_000_000;

/// Encodes a descriptor for an NTScalar-like type with a double value, an
/// alarm whose message is a string, and a timestamp. The descriptor is cached
/// under ID 3 so a second channel can refer to it. Since the string precedes
/// the timestamp, locating the timestamp requires traversing the value rather
/// than relying on fixed byte offsets.
fn ntscalar_descriptor() -> Vec<u8> {
    let mut d = vec![0xFF, 0xFD, 0x03, 0x00, 0x80];
    put_string(&mut d, "epics:nt/NTScalar:1.0", LE);
    put_size(&mut d, 3, LE);
    put_string(&mut d, "value", LE);
    d.push(0x43);
    put_string(&mut d, "alarm", LE);
    d.push(0x80);
    put_string(&mut d, "alarm_t", LE);
    put_size(&mut d, 3, LE);
    put_string(&mut d, "severity", LE);
    d.push(0x22);
    put_string(&mut d, "status", LE);
    d.push(0x22);
    put_string(&mut d, "message", LE);
    d.push(0x60);
    put_string(&mut d, "timeStamp", LE);
    d.push(0x80);
    put_string(&mut d, "time_t", LE);
    put_size(&mut d, 3, LE);
    put_string(&mut d, "secondsPastEpoch", LE);
    d.push(0x23);
    put_string(&mut d, "nanoseconds", LE);
    d.push(0x22);
    put_string(&mut d, "userTag", LE);
    d.push(0x22);
    d
}

/// Offset of the timestamp in the opening body: the changed set (two bytes),
/// the double, severity, status, and the empty message.
const OPENING_SECONDS_AT: usize = 2 + 8 + 4 + 4 + 1;

/// Builds a profile with a complete opening value carrying an old timestamp,
/// followed by the overrun set at the end of a monitor update.
fn profile_with_stamped_opening() -> Profile {
    let mut steady = template("nts", 3);
    steady.with_introspection(Introspection {
        full: ntscalar_descriptor(),
        id_only: Some(vec![0xFF, 0xFE, 0x03, 0x00]),
    });
    let mut tail = Vec::new();
    tail.extend_from_slice(&0i32.to_le_bytes()); // severity
    tail.extend_from_slice(&0i32.to_le_bytes()); // status
    tail.push(0x00); // empty message
    tail.extend_from_slice(&RECORDED_SECONDS.to_le_bytes());
    tail.extend_from_slice(&7i32.to_le_bytes()); // nanoseconds
    tail.extend_from_slice(&0i32.to_le_bytes()); // userTag
    tail.push(0x00); // empty overrun set
    let mut opening = Template::new("nts_initial", MONITOR);
    opening
        .push_fixed(vec![0x01, 0x01]) // changed set {0}: the whole value
        .push_slot(
            Slot::new(1, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::Value {
                scalar: ScalarType::F64,
            }),
        )
        .push_fixed(tail);
    opening.observations = 100;
    steady.with_initial(opening);

    let mut p = Profile::new(
        Manifest {
            pvxs_version: "1.4.2".into(),
            epics_base_version: "7.0.8.1".into(),
            catalogue_sha256: "f".repeat(64),
            host: "test".into(),
            captured_at: "posix:0".into(),
            mimus_version: "0.1.0".into(),
            study_duration_s: 30.0,
            pvs_observed: 2,
            synthetic: false,
        },
        SessionRules::default(),
    );
    p.templates.push(steady);
    p.assignments.push(Assignment {
        pv: "A".into(),
        template: "nts".into(),
    });
    p
}

fn stamped_fleet() -> Arc<Fleet> {
    stamped_fleet_at(50.0)
}

fn stamped_fleet_at(rate_hz: f64) -> Arc<Fleet> {
    let spec = FleetSpec {
        iocs: 1,
        pvs: 4,
        layout: Layout::Blocked,
        mix: vec![("nts".into(), 1)],
        rate_mix: RateMix::uniform(rate_hz),
        names: mimus_serve::NameTemplate::default_pattern("MIM"),
    };
    Arc::new(Fleet::build(&profile_with_stamped_opening(), &spec).unwrap())
}

/// Returns the opening body's timestamp as a single nanosecond count.
fn stamp_ns(body: &[u8]) -> i128 {
    let at = OPENING_SECONDS_AT;
    let s = i64::from_le_bytes(body[at..at + 8].try_into().unwrap());
    let n = i32::from_le_bytes(body[at + 8..at + 12].try_into().unwrap());
    s as i128 * 1_000_000_000 + n as i128
}

fn posix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn assert_current(body: &[u8], at: usize, what: &str) {
    let seconds = i64::from_le_bytes(body[at..at + 8].try_into().unwrap());
    let nanos = i32::from_le_bytes(body[at + 8..at + 12].try_into().unwrap());
    assert_ne!(
        seconds, RECORDED_SECONDS,
        "{what}: still the recorded instant"
    );
    assert!(
        (seconds - posix_now()).abs() <= 5,
        "{what}: {seconds} is not the current time"
    );
    assert!(
        (0..1_000_000_000).contains(&nanos),
        "{what}: nanoseconds {nanos}"
    );
}

#[test]
fn every_opening_of_the_fixture_can_be_stamped() {
    assert!(stamped_fleet().unstamped_openings().is_empty());
    // A template with an invalid descriptor is reported rather than silently accepted.
    let p = profile_with_initial();
    let spec = FleetSpec {
        iocs: 1,
        pvs: 1,
        layout: Layout::Blocked,
        mix: vec![("dbl".into(), 1)],
        rate_mix: RateMix::uniform(1.0),
        names: mimus_serve::NameTemplate::default_pattern("MIM"),
    };
    let f = Fleet::build(&p, &spec).unwrap();
    assert_eq!(f.unstamped_openings().len(), 1);
}

#[tokio::test]
async fn an_opening_message_carries_the_current_time() {
    let f = stamped_fleet();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    for name in [f.names[0].clone(), f.names[1].clone()] {
        let (request_id, _) = c.subscribe(&name).await;
        let updates = c
            .collect_updates(request_id, 2, Duration::from_secs(2))
            .await;
        assert_eq!(updates.len(), 2);
        let opening = &updates[0];
        assert_eq!(&opening[..2], &[0x01, 0x01], "the complete value");
        assert_current(opening, OPENING_SECONDS_AT, "opening message");
        // The user tag and overrun set remain unchanged, and the message retains
        // its original length.
        assert_eq!(opening.len(), OPENING_SECONDS_AT + 8 + 4 + 4 + 1);
        assert_eq!(&opening[OPENING_SECONDS_AT + 12..], &[0, 0, 0, 0, 0]);
    }
}

#[tokio::test]
async fn a_read_and_an_opening_message_carry_one_instant() {
    // An IOC stamps a GET reply and a monitor's opening message with the
    // record's last processing time. The Archiver Appliance's engine reads
    // a value and then subscribes to it. If the two messages carry different
    // timestamps, the engine may store the same value twice, milliseconds
    // apart. A slow channel makes the shared instant observable: at 0.1 Hz,
    // both messages fall within the same ten-second period, or in consecutive
    // periods if a period boundary falls between them.
    const PERIOD_NS: i128 = 10_000_000_000;
    let f = stamped_fleet_at(0.1);
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(0.1),
        ..fast_config()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let mut instants = Vec::new();
    for (k, name) in [f.names[0].clone(), f.names[1].clone()]
        .into_iter()
        .enumerate()
    {
        let cid = c.create_channel(&name).await.unwrap();
        let rid = 80 + k as i32;
        let mut init = Vec::new();
        put_i32(&mut init, cid, LE);
        put_i32(&mut init, rid, LE);
        init.push(commands::get::INIT);
        init.push(0xFF);
        c.send(commands::GET, &init).await;
        c.recv_command(commands::GET, Duration::from_millis(500))
            .await
            .expect("no reply to a get init");
        let mut read = Vec::new();
        put_i32(&mut read, cid, LE);
        put_i32(&mut read, rid, LE);
        read.push(commands::get::DESTROY);
        c.send(commands::GET, &read).await;
        let frame = c
            .recv_command(commands::GET, Duration::from_millis(500))
            .await
            .expect("no reply to a read");
        let got = stamp_ns(&frame.payload[6..]);

        let (request_id, _) = c.subscribe(&name).await;
        let updates = c
            .collect_updates(request_id, 1, Duration::from_secs(2))
            .await;
        let opened = stamp_ns(&updates[0]);

        let now = posix_now() as i128 * 1_000_000_000;
        assert!(
            got <= now + 1_000_000_000,
            "{name}: a read stamped in the future"
        );
        assert!(
            now - got < PERIOD_NS + 2_000_000_000,
            "{name}: a read stamped more than a period ago"
        );
        assert!(
            opened == got || opened - got == PERIOD_NS,
            "{name}: read at {got}, opened at {opened}: not one processing"
        );
        instants.push(got % PERIOD_NS);
    }
    assert_ne!(
        instants[0], instants[1],
        "two channels processed at the same phase"
    );
}

#[tokio::test]
async fn a_read_carries_the_current_time_and_ends_with_the_value() {
    // PVXS ends a GET reply with the value. The overrun set is specific to
    // monitor updates; it was present in the opening message MIMUS replayed.
    let f = stamped_fleet();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;
    let cid = c.create_channel(&f.names[0].clone()).await.unwrap();

    // 0x00 and 0x40 are plain reads; 0x10 and 0x50 are last reads.
    for (i, sub) in [0x00u8, 0x40, 0x10, 0x50].into_iter().enumerate() {
        let rid = 60 + i as i32;
        let mut init = Vec::new();
        put_i32(&mut init, cid, LE);
        put_i32(&mut init, rid, LE);
        init.push(commands::get::INIT);
        init.push(0xFF);
        c.send(commands::GET, &init).await;
        c.recv_command(commands::GET, Duration::from_millis(500))
            .await
            .expect("no reply to a get init");

        let mut read = Vec::new();
        put_i32(&mut read, cid, LE);
        put_i32(&mut read, rid, LE);
        read.push(sub);
        c.send(commands::GET, &read).await;
        let frame = c
            .recv_command(commands::GET, Duration::from_millis(500))
            .await
            .unwrap_or_else(|| panic!("no reply to a read with sub-command {sub:#04x}"));
        let mut r = Reader::new(&frame.payload, LE);
        assert_eq!(r.i32().unwrap(), rid);
        assert_eq!(r.u8().unwrap(), sub, "the sub-command is echoed");
        assert_eq!(r.u8().unwrap(), 0xFF, "OK status");
        let body = r.rest();
        assert_eq!(body.len(), OPENING_SECONDS_AT + 8 + 4 + 4, "no overrun set");
        assert_current(body, OPENING_SECONDS_AT, "read");
    }
}

#[tokio::test]
async fn an_idle_connection_does_not_tick() {
    // A connection with no active subscriptions used to wake up a thousand
    // times per second. With no data flowing, a captured profile showed
    // Tokio's timer machinery at the top of the profile: `process_at_time`
    // and `Sleep::poll` together accounted for more work than MIMUS itself.
    // An archiver opening hundreds of connections incurred this overhead
    // on every connection before receiving a single update.
    //
    // This is observable only through the absence of activity: a connection
    // with no subscriptions should send nothing and remain alive.
    let f = fleet(10, 1);
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(1000.0),
        tick_us: 1000,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    // A channel is open, but no monitor has started.
    let _cid = c.create_channel(&f.names[0].clone()).await.unwrap();
    assert!(
        c.recv_command(commands::MONITOR, Duration::from_millis(400))
            .await
            .is_none(),
        "an unsubscribed connection sent an update"
    );

    c.send(commands::ECHO, b"alive").await;
    assert!(
        c.recv_command(commands::ECHO, Duration::from_millis(500))
            .await
            .is_some(),
        "the connection died when its timer was disarmed"
    );
}

#[tokio::test]
async fn a_connection_starts_ticking_again_when_a_monitor_starts() {
    // The timer must also be re-armed when needed. A subscription arriving
    // after an idle period must restart it.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    // Stay idle long enough for the timer to be dropped.
    c.send(commands::ECHO, b"wait").await;
    let _ = c
        .recv_command(commands::ECHO, Duration::from_millis(500))
        .await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 3, Duration::from_secs(2))
        .await;
    assert!(
        updates.len() >= 2,
        "updates stopped after an idle period: {}",
        updates.len()
    );
}

#[tokio::test]
async fn stopping_the_last_monitor_makes_a_connection_idle_again() {
    // The connection must become idle again after its last monitor stops;
    // otherwise the optimization only applies before the first subscription.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let cid = c.create_channel(&name).await.unwrap();
    let (request_id, _) = c.monitor_init(cid).await;
    c.monitor_start(cid, request_id).await;
    assert!(
        !c.collect_updates(request_id, 2, Duration::from_millis(500))
            .await
            .is_empty()
    );

    c.monitor_start_with(cid, request_id, commands::monitor::STOP)
        .await;
    let after = c
        .collect_updates(request_id, 5, Duration::from_millis(600))
        .await;
    assert!(after.is_empty(), "{} updates after a stop", after.len());

    c.send(commands::ECHO, b"alive").await;
    assert!(
        c.recv_command(commands::ECHO, Duration::from_millis(500))
            .await
            .is_some()
    );
}

#[tokio::test]
async fn creating_an_unknown_channel_is_refused_not_ignored() {
    // Silence and refusal are indistinguishable to a client until it times
    // out. Previously, a load test requesting nonexistent channel names
    // waited indefinitely for replies and reported zero bytes delivered,
    // without any indication of the cause.
    let f = fleet(10, 1);
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let mut p = Vec::new();
    put_i16(&mut p, 1, LE);
    put_i32(&mut p, 4242, LE);
    put_string(&mut p, "NOT:A:CHANNEL", LE);
    c.send(commands::CREATE_CHANNEL, &p).await;

    let frame = c
        .recv_command(commands::CREATE_CHANNEL, Duration::from_millis(500))
        .await
        .expect("an unknown channel got no answer at all");
    let mut r = Reader::new(&frame.payload, LE);
    assert_eq!(r.i32().unwrap(), 4242, "the client's id is echoed back");
    let _server_cid = r.i32().unwrap();
    assert_ne!(r.u8().unwrap(), 0xFF, "must not report success");
}

#[tokio::test]
async fn a_refusal_does_not_disturb_the_channels_that_do_exist() {
    // A batch containing both known and unknown names must still grant
    // access to the known channels.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let mut p = Vec::new();
    put_i16(&mut p, 2, LE);
    put_i32(&mut p, 1, LE);
    put_string(&mut p, "NOT:A:CHANNEL", LE);
    put_i32(&mut p, 2, LE);
    put_string(&mut p, &name, LE);
    c.send(commands::CREATE_CHANNEL, &p).await;

    let mut seen = Vec::new();
    for _ in 0..2 {
        let frame = c
            .recv_command(commands::CREATE_CHANNEL, Duration::from_millis(500))
            .await
            .expect("a reply per channel asked for");
        let mut r = Reader::new(&frame.payload, LE);
        let cid = r.i32().unwrap();
        let _server = r.i32().unwrap();
        seen.push((cid, r.u8().unwrap()));
    }
    seen.sort();
    assert_ne!(seen[0].1, 0xFF, "the unknown one was refused");
    assert_eq!(seen[1].1, 0xFF, "the known one was granted");
}

// ------------------------------------------------------------- coalescing

#[tokio::test]
async fn without_coalescing_updates_go_out_on_every_tick() {
    // This is the default behavior and matches an IOC: one write per connection per tick.
    // Each write incurs a syscall, but the update timestamp reflects when it was sent.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let addr = start(Arc::clone(&f), fast_config()).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 3, Duration::from_millis(400))
        .await;
    assert!(
        updates.len() >= 2,
        "an update should arrive within a tick or two, got {}",
        updates.len()
    );
}

#[tokio::test]
async fn coalescing_delays_delivery_and_says_so() {
    // Coalescing trades latency for fewer, larger writes. Measurements showed that sending
    // the same data across eight connections separately used 3.6 times as much CPU
    // because each write carried only one eighth of the data.
    //
    // Updates must not be held indefinitely, so this test checks that they
    // still arrive, albeit later and in batches.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(200.0),
        tick_us: 1000,
        coalesce_us: 100_000,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 40, Duration::from_millis(1200))
        .await;
    assert!(
        updates.len() > 10,
        "coalescing must delay delivery, not prevent it: {} updates",
        updates.len()
    );
}

#[tokio::test]
async fn a_reply_is_never_held_back_by_coalescing() {
    // Coalescing applies only to the update emission path. Echoes are
    // request-response exchanges, and a client waiting 100 ms for one
    // may conclude that the server is unresponsive.
    let f = fleet(10, 1);
    let cfg = SessionConfig {
        coalesce_us: 500_000,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    c.send(commands::ECHO, b"now").await;
    assert!(
        c.recv_command(commands::ECHO, Duration::from_millis(200))
            .await
            .is_some(),
        "an echo was held back by a setting meant for updates"
    );
}

#[tokio::test]
async fn coalescing_yields_before_the_drop_policy_would_bite() {
    // Delaying updates beyond the high-water mark turns a latency trade-off
    // into dropped updates, which is not the intended behavior.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(1000.0),
        tick_us: 1000,
        coalesce_us: 10_000_000,
        high_water: 4096,
        backpressure: Backpressure::Drop,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 20, Duration::from_millis(800))
        .await;
    assert!(
        !updates.is_empty(),
        "a ten-second coalescing window swallowed everything"
    );
}

#[tokio::test]
async fn the_drop_policy_keeps_its_cadence_when_a_client_stops_reading() {
    // This is the promise of `--backpressure drop`: maintain the offered rate and count
    // updates that cannot be queued. Previously, the behavior was the opposite.
    // Once the socket filled, `write_all` blocked the session, stopping the tick loop.
    // In a test where the client subscribed and then stopped reading, MIMUS
    // emitted only 2,910 updates per second out of 400,000 offered, and reported zero
    // drops because no updates reached the point where they could be dropped.
    let f = fleet(200, 1);
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(500.0),
        tick_us: 1000,
        backpressure: Backpressure::Drop,
        high_water: 4096,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    for i in 0..60 {
        let cid = c.create_channel(&f.names[i].clone()).await.unwrap();
        let (rid, _) = c.monitor_init(cid).await;
        c.monitor_start(cid, rid).await;
    }

    // Stop reading. The socket fills, but the server must keep running.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // The session must still respond, rather than being stuck in a write.
    c.send(commands::ECHO, b"alive").await;
    assert!(
        c.recv_command(commands::ECHO, Duration::from_secs(2))
            .await
            .is_some(),
        "the session blocked in a write and stopped serving"
    );
}

#[tokio::test]
async fn the_block_policy_still_waits_for_the_consumer() {
    // The other side of the contract: Block measures what the consumer can
    // absorb. Discarding updates instead would overstate its throughput.
    let f = fleet(10, 1);
    let name = f.names[0].clone();
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(50.0),
        backpressure: Backpressure::Block,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    let (request_id, _) = c.subscribe(&name).await;
    let updates = c
        .collect_updates(request_id, 5, Duration::from_secs(2))
        .await;
    assert!(updates.len() >= 3, "{} updates", updates.len());
}

#[tokio::test]
async fn subscriptions_keep_registering_while_a_client_stops_reading() {
    // A client that sends a burst of monitor starts and then stops reading
    // is a common load-test pattern, but it exposed this bug twice.
    //
    // The reply path used a blocking write to flush responses. Once the socket filled,
    // the session stalled inside that await and stopped reading commands,
    // leaving most of the burst unregistered. In one measurement, only 3,000 of 20,000
    // starts became live subscriptions. The server handled just one seventh of the
    // offered load, with no counter to report the shortfall.
    let f = fleet(400, 1);
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(200.0),
        tick_us: 1000,
        backpressure: Backpressure::Drop,
        high_water: 8192,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    // Open all channels first so the monitor starts can be sent in one burst.
    let mut pairs = Vec::new();
    for i in 0..300 {
        let cid = c.create_channel(&f.names[i].clone()).await.unwrap();
        let (rid, _) = c.monitor_init(cid).await;
        pairs.push((cid, rid));
    }
    for (cid, rid) in &pairs {
        c.monitor_start(*cid, *rid).await;
    }

    // Let the socket fill while the server continues processing monitor starts.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Verify that the session is still responsive after registering the burst.
    c.send(commands::ECHO, b"alive").await;
    assert!(
        c.recv_command(commands::ECHO, Duration::from_secs(2))
            .await
            .is_some(),
        "the session stalled while registering a burst of subscriptions"
    );
}

#[tokio::test]
async fn the_subscription_gauge_follows_what_is_live() {
    // This metric distinguishes low offered load from a slow scheduler.
    // Without it, producing only one seventh of the expected updates could
    // mean either that only one seventh of the subscriptions registered or
    // that the scheduler is seven times too slow. These require opposite
    // fixes, and the ambiguity caused two rounds of investigation before
    // anyone thought to count live subscriptions.
    let f = fleet(20, 1);
    let totals = mimus_serve::shared::SharedCounters::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = fast_config();
    let fleet = Arc::clone(&f);
    let shared = Arc::clone(&totals);
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let session = Session::new(cfg, fleet).unwrap().with_shared(shared);
        let _ = session.run(stream).await;
    });

    let mut c = Client::connect(addr).await;
    c.handshake().await;
    assert_eq!(totals.subscriptions(), 0);

    let mut pairs = Vec::new();
    for i in 0..5 {
        let cid = c.create_channel(&f.names[i].clone()).await.unwrap();
        let (rid, _) = c.monitor_init(cid).await;
        c.monitor_start(cid, rid).await;
        pairs.push((cid, rid));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(totals.subscriptions(), 5, "five starts, five live");

    // A channel that is opened and initialized but never started is not live.
    let cid = c.create_channel(&f.names[9].clone()).await.unwrap();
    let _ = c.monitor_init(cid).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(totals.subscriptions(), 5, "an init is not a subscription");

    let (cid, rid) = pairs[0];
    c.monitor_start_with(cid, rid, commands::monitor::STOP)
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(totals.subscriptions(), 4, "a stop puts one back");

    // Ending the session must release its remaining subscriptions.
    // Otherwise, the gauge keeps increasing across reconnections and can
    // report more live monitors than there are channels.
    drop(c);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        totals.subscriptions(),
        0,
        "a closed session releases its own"
    );
}

#[tokio::test]
async fn the_wheel_delivers_the_rate_it_was_asked_for() {
    // Measure updates per wheel step rather than per second. A per-second
    // rate divides by the entire run, including the period before any
    // subscriptions existed and the wheel was intentionally idle. This
    // calculation made a correctly functioning wheel appear 30% too slow
    // during two rounds of investigation and led to an unnecessary scheduler
    // change.
    let f = fleet(500, 1);
    let cfg = SessionConfig {
        rate_mix: RateMix::uniform(20.0),
        tick_us: 1000,
        ..Default::default()
    };
    let addr = start(Arc::clone(&f), cfg).await;
    let mut c = Client::connect(addr).await;
    c.handshake().await;

    for i in 0..100 {
        let cid = c.create_channel(&f.names[i].clone()).await.unwrap();
        let (rid, _) = c.monitor_init(cid).await;
        c.monitor_start(cid, rid).await;
    }

    // Measure one subscription's cadence directly: at 20 Hz, it should
    // receive one update every 50 ms.
    let (cid, rid) = {
        let cid = c.create_channel(&f.names[200].clone()).await.unwrap();
        let (rid, _) = c.monitor_init(cid).await;
        c.monitor_start(cid, rid).await;
        (cid, rid)
    };
    let _ = cid;

    let began = std::time::Instant::now();
    let updates = c.collect_updates(rid, 9, Duration::from_secs(3)).await;
    let span = began.elapsed().as_secs_f64();
    assert!(updates.len() >= 6, "only {} updates", updates.len());

    // The initial update arrives immediately, so there is one fewer interval than updates.
    let observed = (updates.len() - 1) as f64 / span;
    assert!(
        observed > 12.0 && observed < 30.0,
        "asked for 20 Hz, delivered {observed} Hz over {span} s"
    );
}
