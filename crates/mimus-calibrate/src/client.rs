//! A client that monitors a reference IOC.
//!
//! Performs standard PVAccess client operations: connect, validate, search,
//! open channels, start monitors and capture the messages received.
//! It uses [`mimus_wire`] for all protocol framing, ensuring that both
//! client and server use the same implementation.
//!
//! Two deliberate design choices:
//!
//! **Request the complete structure.** The monitor INIT uses an empty
//! `pvRequest`. A narrower request would produce smaller messages and cause
//! templates to underestimate the data received by a real subscriber.
//!
//! **Do not interpret payloads.** Message bodies are passed verbatim to the
//! observer, and type descriptors are stored as bytes. The client does not
//! need to know how PVXS represents an NTScalar or any other structure.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use mimus_wire::codec::Reader;
use mimus_wire::header::{ByteOrder, Direction, Header, Segmentation};
use mimus_wire::{
    ConnectionValidationResponse, CreateChannelRequest, CreateChannelResponse, Framer,
    MonitorRequest, SearchFound, SearchRequest, commands,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::observe::ChannelObserver;

/// Why observation stopped.
#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    /// The stream became invalid. PVA has no resynchronization point,
    /// so the connection must be closed.
    Protocol(String),
    /// The IOC did not complete the handshake in time.
    HandshakeTimeout,
    /// None of the requested channels were found.
    ///
    /// The counts distinguish three failure cases:
    /// no replies indicate a search failure, undecodable replies indicate
    /// a codec issue, and decoded replies with no matches indicate that
    /// the IOC does not have the requested channels.
    NothingFound {
        asked: usize,
        replies: usize,
        undecodable: usize,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Protocol(e) => write!(f, "protocol error: {e}"),
            Self::HandshakeTimeout => f.write_str("the IOC never finished the handshake"),
            Self::NothingFound {
                asked,
                replies,
                undecodable,
            } => {
                write!(f, "none of the {asked} channels were found. ")?;
                match (replies, undecodable) {
                    (0, _) => f.write_str(
                        "The IOC never answered the search at all, which points at \
                         the search message rather than at the catalogue.",
                    ),
                    (_, u) if *u > 0 => write!(
                        f,
                        "{replies} reply(ies) arrived and {u} did not decode, which \
                         points at the codec rather than at the IOC."
                    ),
                    _ => write!(
                        f,
                        "{replies} reply(ies) arrived, claiming nothing. The IOC is \
                         reachable but does not serve these names — check it loaded \
                         the catalogue."
                    ),
                }
            }
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Observation duration and retention limits.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// Maximum number of message bodies retained per channel.
    ///
    /// Prevents long runs from exhausting memory. The observer evicts
    /// duplicate samples first to preserve variety.
    pub max_updates_per_channel: usize,
    /// Interval between progress callback invocations.
    pub poll: Duration,
    /// Maximum observation duration.
    ///
    /// Reaching this limit is not necessarily a failure; the completion
    /// criterion determines the outcome. It also prevents calibration
    /// from waiting indefinitely for inactive channels.
    pub deadline: Duration,
    /// Quiet period after which observation stops once all active channels
    /// have stopped sending updates.
    ///
    /// Channels absent from the IOC never become active, so waiting for
    /// them until the deadline would waste time without adding information.
    /// They are still reported as missing.
    pub quiet_exit: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_updates_per_channel: 64,
            poll: Duration::from_secs(1),
            deadline: Duration::from_secs(600),
            quiet_exit: Duration::from_secs(45),
        }
    }
}

/// A monitor subscription in progress.
#[derive(Debug, Clone, Copy)]
struct Subscription {
    /// Index into the observer's channel list.
    channel: usize,
    /// Whether the monitor has received its start response.
    started: bool,
}

/// Observes a set of channels on a single IOC.
///
/// `should_stop` is called every [`Settings::poll`] with the current
/// observers and elapsed time. It decides when enough data has been
/// collected. This keeps observation policy outside the client, which
/// is responsible only for connecting, listening and recording.
pub async fn observe(
    addr: &str,
    channels: &[String],
    settings: Settings,
    mut should_stop: impl FnMut(&[ChannelObserver], f64) -> bool,
) -> Result<Vec<ChannelObserver>, ClientError> {
    let mut observers: Vec<ChannelObserver> = channels
        .iter()
        .map(|pv| ChannelObserver::new(pv.clone(), settings.max_updates_per_channel))
        .collect();

    let mut stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;

    let order = ByteOrder::Little;
    let mut framer = Framer::new();
    let mut inbox = BytesMut::with_capacity(256 * 1024);
    let started = Instant::now();

    handshake(&mut stream, &mut framer, &mut inbox, order).await?;

    // Search for all channels in one request, as a real client would.
    // This also reveals whether the IOC has the requested channels.
    let search = SearchRequest {
        sequence_id: 1,
        // Request replies without setting the unicast flag.
        //
        // Bit 0 is replyRequired; bit 7 marks unicast. Replies are
        // essential because silence would otherwise be indistinguishable
        // from a search request that never reached the IOC.
        flags: 0x01,
        response_port: 0,
        channels: channels
            .iter()
            .enumerate()
            .map(|(i, pv)| (i as i32, pv.clone()))
            .collect(),
    };
    send(&mut stream, commands::SEARCH, &search.encode(order), order).await?;

    let mut found: Vec<usize> = Vec::new();
    let mut channels_by_cid: HashMap<i32, usize> = HashMap::new();
    let mut subscriptions: HashMap<i32, Subscription> = HashMap::new();
    let mut next_request_id: i32 = 1;
    let mut created = 0usize;
    let mut last_poll = Instant::now();
    // PVXS closes idle connections after 30 seconds by default.
    // Keep the connection alive while waiting for channel updates.
    let mut last_echo = Instant::now();
    let mut search_replies = 0usize;
    let mut undecodable = 0usize;
    let mut unknown_descriptors = 0usize;

    loop {
        if started.elapsed() > settings.deadline {
            break;
        }

        if last_echo.elapsed() >= Duration::from_secs(10) {
            last_echo = Instant::now();
            send(&mut stream, commands::ECHO, b"mimus", order).await?;
        }

        let read = tokio::time::timeout(settings.poll, stream.read_buf(&mut inbox)).await;
        match read {
            Ok(Ok(0)) => break,
            Ok(Err(e)) => return Err(ClientError::Io(e)),
            Ok(Ok(_)) => {}
            // A timeout is normal while waiting for slow channels.
            Err(_) => {}
        }

        loop {
            let frame = match framer.next_frame(&mut inbox) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => return Err(ClientError::Protocol(e.to_string())),
            };
            if frame.is_control() {
                // A byte-count marker requires an acknowledgement with
                // the same value. Without it, server-side flow control
                // remains blocked.
                if frame.header.command == commands::CTRL_MARK_TOTAL_SENT {
                    let ack = Header::control(
                        commands::CTRL_ACK_RECEIVED,
                        frame.header.payload_size,
                        order,
                        Direction::FromClient,
                    );
                    stream.write_all(&ack.encode()).await?;
                }
                continue;
            }
            let at = started.elapsed().as_secs_f64();
            let forder = frame.header.byte_order();

            match frame.command() {
                commands::SEARCH_RESPONSE => {
                    search_replies += 1;
                    let reply = match SearchFound::decode(&frame.payload, forder) {
                        Ok(r) => r,
                        Err(e) => {
                            // Track undecodable replies separately from
                            // missing replies to distinguish search failures
                            // from decoding failures.
                            undecodable += 1;
                            if undecodable == 1 {
                                eprintln!("  warning: a search reply did not decode: {e}");
                            }
                            continue;
                        }
                    };
                    // Open all channels found by this search response in a single request.
                    let mut req = CreateChannelRequest {
                        channels: Vec::new(),
                    };
                    for id in reply.found {
                        let Some(pv) = channels.get(id as usize) else {
                            continue;
                        };
                        found.push(id as usize);
                        channels_by_cid.insert(id, id as usize);
                        req.channels.push((id, pv.clone()));
                    }
                    if !req.channels.is_empty() {
                        send(
                            &mut stream,
                            commands::CREATE_CHANNEL,
                            &req.encode(order),
                            order,
                        )
                        .await?;
                    }
                }

                commands::CREATE_CHANNEL => {
                    let Ok(resp) = CreateChannelResponse::decode(&frame.payload, forder) else {
                        continue;
                    };
                    let Some(&channel) = channels_by_cid.get(&resp.client_cid) else {
                        continue;
                    };
                    created += 1;
                    let request_id = next_request_id;
                    next_request_id += 1;
                    subscriptions.insert(
                        request_id,
                        Subscription {
                            channel,
                            started: false,
                        },
                    );
                    let init = MonitorRequest {
                        server_cid: resp.server_cid,
                        request_id,
                        sub_command: commands::monitor::INIT,
                    };
                    send(&mut stream, commands::MONITOR, &init.encode(order), order).await?;
                    // Start immediately. The INIT response carries the
                    // descriptor, so waiting for it would add a round trip
                    // for every channel.
                    let start = MonitorRequest {
                        server_cid: resp.server_cid,
                        request_id,
                        sub_command: commands::monitor::START,
                    };
                    send(&mut stream, commands::MONITOR, &start.encode(order), order).await?;
                    if let Some(s) = subscriptions.get_mut(&request_id) {
                        s.started = true;
                    }
                }

                commands::MONITOR => {
                    let mut r = Reader::new(&frame.payload, forder);
                    let (Ok(request_id), Ok(sub)) = (r.i32(), r.u8()) else {
                        continue;
                    };
                    let Some(subscription) = subscriptions.get(&request_id) else {
                        continue;
                    };
                    let obs = &mut observers[subscription.channel];
                    let body = r.rest().to_vec();

                    if sub & commands::monitor::INIT != 0 {
                        // The descriptor form matters: an identifier-only response means
                        // this channel reuses a descriptor already sent on this connection.
                        let shape = descriptor_shape(&body);
                        if shape == Descriptor::Unrecognised && unknown_descriptors == 0 {
                            // Report the bytes received rather than guessing
                            // which descriptor format they represent.
                            let head: Vec<String> =
                                body.iter().take(12).map(|b| format!("{b:02x}")).collect();
                            eprintln!(
                                "  note: monitor INIT for {} begins {} — no known \
                                 descriptor marker; treating it as a full descriptor",
                                obs.pv,
                                head.join(" ")
                            );
                        }
                        if shape == Descriptor::Unrecognised {
                            unknown_descriptors += 1;
                        }
                        // Only the explicitly cached form omits the inline
                        // type descriptor. Other forms contain a full one.
                        obs.observe_introspection(body, shape != Descriptor::Cached);
                    } else {
                        obs.observe_update(body, at);
                    }
                }

                _ => {}
            }
        }

        if last_poll.elapsed() >= settings.poll {
            last_poll = Instant::now();
            let now = started.elapsed().as_secs_f64();
            if created > 0 && should_stop(&observers, now) {
                break;
            }
            // Channels that never started will not become active later.
            // Once all active channels have been quiet long enough, further
            // observation is unlikely to provide useful data.
            if created > 0 && now > settings.quiet_exit.as_secs_f64() {
                let anything_moving = observers
                    .iter()
                    .any(|o| o.total_updates() > 0 && o.quiet_for(now) < 5.0);
                if !anything_moving {
                    break;
                }
            }
        }
    }

    if found.is_empty() {
        return Err(ClientError::NothingFound {
            asked: channels.len(),
            replies: search_replies,
            undecodable,
        });
    }
    Ok(observers)
}

/// The form of type descriptor carried by a monitor INIT reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descriptor {
    /// The complete type descriptor, accompanied by an ID for caching.
    FullWithId,
    /// The complete type descriptor, embedded directly in the reply.
    Inline,
    /// A reference to a type descriptor already cached by the peer.
    Cached,
    /// An unrecognised form.
    Unrecognised,
}

/// Identifies the type descriptor form in a monitor INIT reply.
///
/// The marker follows the status, whose length is variable. An empty
/// successful status is encoded as the single byte `0xFF`, but the
/// status may also contain a type byte, a message and a call tree,
/// both of which may be null. Assuming a one-byte status would place
/// the marker at the wrong offset and misidentify descriptors as cached.
///
/// The marker is not always a registry code. Without an introspection
/// registry, the server writes the type descriptor inline, starting
/// with a structure type code. This is a complete descriptor without
/// an attached ID. Only `0xFE` indicates a reference to a cached type.
pub fn descriptor_shape(body: &[u8]) -> Descriptor {
    const FULL_WITH_ID: u8 = 0xFD;
    const ONLY_ID: u8 = 0xFE;
    const STRUCTURE: u8 = 0x80;
    const UNION: u8 = 0x81;
    const VARIANT_UNION: u8 = 0x82;

    let Some(first) = body.first() else {
        return Descriptor::Unrecognised;
    };
    let offset = if *first == 0xFF {
        1
    } else {
        let mut r = Reader::new(body, ByteOrder::Little);
        if r.u8().is_err() || r.string().is_err() || r.string().is_err() {
            return Descriptor::Unrecognised;
        }
        r.position()
    };

    match body.get(offset) {
        Some(&FULL_WITH_ID) => Descriptor::FullWithId,
        Some(&ONLY_ID) => Descriptor::Cached,
        Some(&STRUCTURE) | Some(&UNION) | Some(&VARIANT_UNION) => Descriptor::Inline,
        _ => Descriptor::Unrecognised,
    }
}

/// Returns a descriptor key with its registry ID cleared, for grouping
/// channels that share the same type.
///
/// PVXS assigns a fresh ID to each subscription. Descriptors for two
/// channels of the same type therefore differ only in the two ID bytes.
/// Grouping by raw bytes treats them as different types, potentially
/// producing one template per channel. At a million channels, that
/// would mean a million templates.
///
/// Clearing the ID compares the descriptor's actual contents rather
/// than the registry entry assigned by the server.
pub fn descriptor_key(body: &[u8]) -> Vec<u8> {
    let mut key = body.to_vec();
    if descriptor_shape(body) != Descriptor::FullWithId {
        return key;
    }
    // The two bytes after the marker contain the ID. Locate the marker
    // using the same status parsing as descriptor_shape.
    let offset = if body.first() == Some(&0xFF) {
        1
    } else {
        let mut r = Reader::new(body, ByteOrder::Little);
        if r.u8().is_err() || r.string().is_err() || r.string().is_err() {
            return key;
        }
        r.position()
    };
    if let Some(slot) = key.get_mut(offset + 1..offset + 3) {
        slot.fill(0);
    }
    key
}

async fn handshake(
    stream: &mut TcpStream,
    framer: &mut Framer,
    inbox: &mut BytesMut,
    order: ByteOrder,
) -> Result<(), ClientError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        while let Ok(Some(frame)) = framer.next_frame(inbox) {
            if frame.header.is_application() && frame.command() == commands::CONNECTION_VALIDATION {
                let answer = ConnectionValidationResponse {
                    buffer_size: 32768,
                    registry_size: 1024,
                    qos: 0,
                    auth: "anonymous".to_string(),
                };
                send(
                    stream,
                    commands::CONNECTION_VALIDATION,
                    &answer.encode(order),
                    order,
                )
                .await?;
            }
            if frame.header.is_application() && frame.command() == commands::CONNECTION_VALIDATED {
                return Ok(());
            }
        }
        if Instant::now() > deadline {
            return Err(ClientError::HandshakeTimeout);
        }
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read_buf(inbox))
            .await
            .map_err(|_| ClientError::HandshakeTimeout)??;
        if n == 0 {
            return Err(ClientError::HandshakeTimeout);
        }
    }
}

async fn send(
    stream: &mut TcpStream,
    command: u8,
    payload: &[u8],
    order: ByteOrder,
) -> Result<(), ClientError> {
    let header = Header::application(
        command,
        payload.len() as i32,
        order,
        Direction::FromClient,
        Segmentation::None,
    );
    stream.write_all(&header.encode()).await?;
    stream.write_all(payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_generous_enough_for_a_slow_catalogue() {
        // The reference catalogue updates once per second, and alarm
        // records need a full sine sweep to exhibit a transition.
        // A deadline shorter than a few minutes would cut the sweep short.
        let s = Settings::default();
        assert!(s.deadline >= Duration::from_secs(300));
        assert!(s.max_updates_per_channel >= 32);
    }

    #[test]
    fn an_unreachable_address_is_an_error_not_a_hang() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(async {
            observe(
                "127.0.0.1:1",
                &["MIM:F64:0".to_string()],
                Settings {
                    deadline: Duration::from_secs(2),
                    ..Default::default()
                },
                |_, _| false,
            )
            .await
            .unwrap_err()
        });
        assert!(matches!(err, ClientError::Io(_)), "{err}");
    }

    #[test]
    fn a_server_that_never_validates_times_out() {
        // Accepts the connection but never responds. Without a handshake
        // deadline, calibration would wait indefinitely for this server.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (_stream, _) = listener.accept().await.unwrap();
                tokio::time::sleep(Duration::from_secs(60)).await;
            });

            let err = observe(
                &addr.to_string(),
                &["MIM:F64:0".to_string()],
                Settings::default(),
                |_, _| false,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ClientError::HandshakeTimeout), "{err}");
        });
    }
}

#[cfg(test)]
mod search_tests {
    use mimus_wire::{ByteOrder, SearchRequest};

    /// Search flag meanings, as defined by the protocol specification.
    ///
    /// > byte flags; // 0-bit for replyRequired, 7-th bit for "sent as
    /// > unicast" (1)/"sent as broadcast/multicast" (0)
    ///
    /// The bits are easy to misinterpret. Two incorrect attempts
    /// illustrate why: `0x81` enabled unicast and sent the reply to an
    /// empty response address, while `0x80` enabled unicast but did not
    /// request a reply.
    ///
    /// Broadcast searches leave reply-required clear to avoid responses
    /// from every server that does not own the requested channels.
    /// Calibration needs replies, however, because silence and an
    /// explicit "none found" response require different handling.
    #[test]
    fn a_tcp_search_asks_for_a_reply_and_is_not_unicast() {
        const REPLY_REQUIRED: u8 = 0x01;
        const UNICAST: u8 = 0x80;

        let req = SearchRequest {
            sequence_id: 1,
            flags: REPLY_REQUIRED,
            response_port: 0,
            channels: vec![(0, "MIM:F64:0".to_string())],
        };
        let bytes = req.encode(ByteOrder::Little);
        // Four bytes for the sequence ID, followed by the flags.
        assert_eq!(bytes[4] & REPLY_REQUIRED, REPLY_REQUIRED, "reply required");
        assert_eq!(bytes[4] & UNICAST, 0, "a TCP search is not a UDP unicast");
        // The response address is empty because the reply returns over the existing TCP connection.
        assert_eq!(&bytes[8..24], &[0u8; 16]);
        assert_eq!(&bytes[24..26], &[0u8; 2], "response port");
    }
}

#[cfg(test)]
mod descriptor_tests {
    use super::{Descriptor, descriptor_shape};

    #[test]
    fn a_one_byte_ok_status_is_understood() {
        // pvData encodes an empty OK status as a single 0xFF byte.
        assert_eq!(
            descriptor_shape(&[0xFF, 0xFD, 0x01, 0x00]),
            Descriptor::FullWithId
        );
        assert_eq!(
            descriptor_shape(&[0xFF, 0xFE, 0x01, 0x00]),
            Descriptor::Cached
        );
    }

    #[test]
    fn a_descriptor_written_inline_is_full_not_cached() {
        // Servers without an introspection registry write the type directly,
        // starting with the structure code. This is a complete descriptor.
        // Previously, it was treated as unrecognized and then as cached,
        // leaving channels that referenced a descriptor never received.
        assert_eq!(
            descriptor_shape(&[0xFF, 0x80, 0x15, b'e', b'p', b'i']),
            Descriptor::Inline
        );
        assert_eq!(
            descriptor_shape(&[0x00, 0xFF, 0xFF, 0x80]),
            Descriptor::Inline
        );
    }

    #[test]
    fn a_three_byte_ok_status_is_understood_too() {
        // Status type byte followed by null message and call tree.
        assert_eq!(
            descriptor_shape(&[0x00, 0xFF, 0xFF, 0xFD, 0x01, 0x00]),
            Descriptor::FullWithId
        );
        assert_eq!(
            descriptor_shape(&[0x00, 0xFF, 0xFF, 0xFE, 0x01, 0x00]),
            Descriptor::Cached
        );
    }

    #[test]
    fn a_status_carrying_text_is_skipped_correctly() {
        let mut body = vec![0x01]; // Warning status
        body.push(2);
        body.extend_from_slice(b"hi");
        body.push(0xFF); // Null call tree
        body.push(0xFD);
        assert_eq!(descriptor_shape(&body), Descriptor::FullWithId);
    }

    #[test]
    fn only_the_cached_marker_means_cached() {
        // Unknown or malformed shapes must not be mistaken for cached
        // descriptors, which could leave the calibration without a type.
        assert_eq!(descriptor_shape(&[]), Descriptor::Unrecognised);
        assert_eq!(descriptor_shape(&[0xFF]), Descriptor::Unrecognised);
        assert_ne!(descriptor_shape(&[0xFF, 0x43]), Descriptor::Cached);
    }
}
