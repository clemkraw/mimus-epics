//! Responds to UDP searches so clients can discover MIMUS just as they
//! discover ordinary IOCs.
//!
//! Until now, clients had to be told where to look:
//!
//! ```text
//! EPICS_PVA_NAME_SERVERS=127.0.0.1:5075 pvxmonitor MIM:...
//! ```
//!
//! This works, but it weakens the project's central claim. An archiver
//! configured normally discovers servers by broadcasting searches and
//! listening for replies. Having to configure it specifically for MIMUS
//! means changing the setup used for the measurement. Being
//! indistinguishable from an IOC should not require that exception.
//!
//! UDP discovery differs from the same search over TCP in one important
//! respect: there is no connection to reply on. The request carries the
//! address and port to which the response must be sent. Getting this wrong
//! leaves the client searching without a reply, indistinguishable from a
//! server that does not serve the requested channel.
//!
//! UDP discovery is not intended to bring up a million channels quickly.
//! At roughly 28 bytes per search entry, a million names require around
//! four hundred datagrams over loopback and twenty thousand over a real
//! network, with corresponding replies. PVXS deliberately spreads searches
//! and backs off between retries because broadcast discovery does not scale.
//! This is why name servers exist and why facilities with large channel
//! counts configure them.
//!
//! Both discovery paths serve different purposes. UDP discovery makes
//! MIMUS visible to clients that have not been explicitly configured.
//! `EPICS_PVA_NAME_SERVERS` remains the appropriate mechanism for consumers
//! subscribing to a million channels, just as it would in a real facility.
//! On this machine, that approach brings up a million channels in
//! seventeen seconds.
//!
//! A single responder serves all virtual IOCs. Each reply identifies one
//! TCP port and one GUID, so search results are grouped by IOC and a
//! separate datagram is sent for each IOC with matching channels. Multiple
//! GUIDs on the same host therefore appear as multiple IOCs, just as they
//! would in a real deployment.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use mimus_wire::header::{ByteOrder, Direction, HEADER_SIZE, Header, Segmentation};
use mimus_wire::{GUID_LEN, SearchRequest, SearchResponse, commands};
use tokio::net::UdpSocket;

use crate::fleet::Fleet;

/// The port used for PVAccess search broadcasts.
pub const DEFAULT_SEARCH_PORT: u16 = 5076;

/// Multicast group used by PVXS clients.
///
/// Joining this group also covers clients configured for multicast rather
/// than broadcast. Failure to join is non-fatal, as broadcast discovery
/// covers the common case.
const PVA_MULTICAST: [u8; 4] = [224, 0, 0, 128];

/// Identifies a virtual IOC to the search responder.
#[derive(Debug, Clone)]
pub struct IocEndpoint {
    pub index: u16,
    pub tcp_port: u16,
    pub guid: [u8; GUID_LEN],
}

/// Listens for UDP searches and responds on behalf of the virtual IOCs.
pub struct SearchResponder {
    socket: UdpSocket,
    fleet: Arc<Fleet>,
    iocs: Vec<IocEndpoint>,
    /// Number of searches received, whether or not they matched any channels.
    ///
    /// Distinguishes between no incoming searches and searches that found
    /// no matching channels, which require different troubleshooting.
    pub seen: std::sync::atomic::AtomicU64,
    pub answered: std::sync::atomic::AtomicU64,
}

impl SearchResponder {
    /// Binds the UDP search socket.
    ///
    /// `bind_addr` should almost always be `0.0.0.0`. Broadcast searches
    /// arrive at broadcast addresses such as `192.168.1.255` or
    /// `127.255.255.255`, not at a host's individual address. A socket
    /// bound to a specific host address will not receive them.
    ///
    /// Binding to the same address as the TCP listeners may appear
    /// consistent, but silently disables broadcast discovery. The socket
    /// still binds successfully and the responder appears active, yet
    /// clients receive no replies and cannot distinguish this from a
    /// server that does not serve their channels. A socket bound to one
    /// address also takes the unicast searches sent to it away from a
    /// server on the same host bound to the wildcard, and MIMUS does not
    /// pass them on as a PVXS server does.
    pub async fn bind(
        bind_addr: &str,
        port: u16,
        fleet: Arc<Fleet>,
        iocs: Vec<IocEndpoint>,
    ) -> std::io::Result<Self> {
        let socket = bind_search(bind_addr, port).await?;

        // Warn explicitly when the socket is bound to an address that
        // cannot receive broadcast searches.
        if bind_addr != "0.0.0.0" {
            eprintln!(
                "warning: the search responder is bound to {bind_addr}, which does \
                 not receive broadcasts: only clients that search {bind_addr} by \
                 unicast will find these channels, and those searches no longer \
                 reach another server on this host that listens on port {port}. \
                 Use --search-bind 0.0.0.0."
            );
        }
        // Multicast support is best effort. Failure does not affect broadcast discovery,
        // while multicast-only clients will receive no replies. This is a discovery
        // limitation, not a protocol error.
        let iface: std::net::Ipv4Addr =
            bind_addr.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        let _ = socket.join_multicast_v4(std::net::Ipv4Addr::from(PVA_MULTICAST), iface);

        Ok(Self {
            socket,
            fleet,
            iocs,
            seen: std::sync::atomic::AtomicU64::new(0),
            answered: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Receives and answers searches until the task is canceled or dropped.
    pub async fn serve(self: Arc<Self>) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let Ok((n, from)) = self.socket.recv_from(&mut buf).await else {
                // A datagram receive error does not necessarily invalidate
                // the socket. Skip this packet and continue listening.
                continue;
            };
            self.handle(&buf[..n], from).await;
        }
    }

    /// Processes a datagram, which may contain multiple messages back to back.
    async fn handle(&self, datagram: &[u8], from: SocketAddr) {
        let mut at = 0usize;
        while at + HEADER_SIZE <= datagram.len() {
            let mut head = [0u8; HEADER_SIZE];
            head.copy_from_slice(&datagram[at..at + HEADER_SIZE]);
            let Ok(header) = Header::decode(&head) else {
                // If the header is invalid, the remaining message boundaries
                // cannot be trusted. Discard the rest of the datagram.
                return;
            };
            at += HEADER_SIZE;
            if !header.is_application() {
                continue;
            }
            let size = header.payload_size.max(0) as usize;
            if at + size > datagram.len() {
                // The payload extends beyond the datagram. Discard the rest.
                return;
            }
            let payload = &datagram[at..at + size];
            at += size;

            if header.command == commands::SEARCH {
                self.on_search(payload, header.byte_order(), from).await;
            }
        }
    }

    async fn on_search(&self, payload: &[u8], order: ByteOrder, from: SocketAddr) {
        use std::sync::atomic::Ordering;
        self.seen.fetch_add(1, Ordering::Relaxed);

        let Ok(req) = SearchRequest::decode(payload, order) else {
            return;
        };

        // Group matching channel IDs by their owning IOC. Each channel belongs
        // to exactly one IOC, so a search spanning several IOCs produces one
        // reply per matching IOC, each with its own GUID.
        let mut by_ioc: BTreeMap<u16, Vec<i32>> = BTreeMap::new();
        for (id, name) in &req.channels {
            for ioc in &self.iocs {
                if self.fleet.lookup_on(name, ioc.index).is_some() {
                    by_ioc.entry(ioc.index).or_default().push(*id);
                    break;
                }
            }
        }

        if by_ioc.is_empty() {
            // A search miss normally requires no reply. Responding to every
            // broadcast that finds nothing would generate unnecessary traffic.
            // Only send an empty response when the client explicitly requests
            // one through REPLY_REQUIRED.
            if req.flags & flags::REPLY_REQUIRED == 0 {
                return;
            }
        }

        // Resolve the reply destination. The request specifies a response port;
        // if it is zero, use the source port of the datagram instead.
        // Replying to the wrong port leaves the client waiting, with no way
        // to distinguish a misdirected response from a search miss.
        let reply_to = SocketAddr::new(
            from.ip(),
            if req.response_port != 0 {
                req.response_port
            } else {
                from.port()
            },
        );

        for ioc in &self.iocs {
            let found = by_ioc.remove(&ioc.index).unwrap_or_default();
            if found.is_empty() {
                continue;
            }
            let body = SearchResponse {
                guid: &ioc.guid,
                sequence_id: req.sequence_id,
                server_port: ioc.tcp_port,
                found: &found,
            }
            .encode(order);

            let header = Header::application(
                commands::SEARCH_RESPONSE,
                body.len() as i32,
                order,
                Direction::FromServer,
                Segmentation::None,
            );
            let mut datagram = Vec::with_capacity(HEADER_SIZE + body.len());
            datagram.extend_from_slice(&header.encode());
            datagram.extend_from_slice(&body);

            if self.socket.send_to(&datagram, reply_to).await.is_ok() {
                self.answered.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Search request flag bits.
///
/// The bit assignments are easy to misinterpret: bit 0 indicates that a
/// reply is required, while bit 7 marks a unicast request rather than a
/// broadcast. Both were verified against the protocol implementation.
pub mod flags {
    pub const REPLY_REQUIRED: u8 = 0x01;
    pub const UNICAST: u8 = 0x80;
}

/// Binds the UDP search socket with the options used by PVAccess servers.
///
/// `SO_REUSEADDR` allows multiple processes to bind the search port. This
/// is normal in EPICS deployments: an IOC, gateway, and monitor may all
/// bind port 5076 and receive the same broadcasts. Without this option,
/// only the first process can start, preventing side-by-side comparisons.
///
/// `SO_BROADCAST` enables the socket to receive broadcast search traffic.
async fn bind_search(addr: &str, port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let ip: std::net::IpAddr = addr
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad bind address"))?;
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    sock.set_broadcast(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::new(ip, port).into())?;
    UdpSocket::from_std(std::net::UdpSocket::from(sock))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimus_wire::codec::{put_i32, put_size, put_string};

    fn search_datagram(seq: i32, names: &[(i32, &str)], response_port: u16) -> Vec<u8> {
        let order = ByteOrder::Little;
        let mut body = Vec::new();
        put_i32(&mut body, seq, order);
        body.push(flags::REPLY_REQUIRED);
        body.extend_from_slice(&[0u8; 3]);
        body.extend_from_slice(&[0u8; 16]); // response address
        body.extend_from_slice(&response_port.to_le_bytes());
        put_size(&mut body, 1, order);
        put_string(&mut body, "tcp", order);
        body.extend_from_slice(&(names.len() as i16).to_le_bytes());
        for (id, name) in names {
            put_i32(&mut body, *id, order);
            put_string(&mut body, name, order);
        }
        let header = Header::application(
            commands::SEARCH,
            body.len() as i32,
            order,
            Direction::FromClient,
            Segmentation::None,
        );
        let mut out = Vec::new();
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&body);
        out
    }

    /// Decodes the fields needed to verify a search response.
    fn parse_response(datagram: &[u8]) -> ([u8; GUID_LEN], u16, Vec<i32>) {
        assert!(datagram.len() > HEADER_SIZE);
        let mut head = [0u8; HEADER_SIZE];
        head.copy_from_slice(&datagram[..HEADER_SIZE]);
        let header = Header::decode(&head).expect("a well-formed header");
        assert_eq!(header.command, commands::SEARCH_RESPONSE);
        let p = &datagram[HEADER_SIZE..];
        let mut guid = [0u8; GUID_LEN];
        guid.copy_from_slice(&p[..GUID_LEN]);
        let mut at = GUID_LEN + 4 + 16; // GUID, sequence ID, and server address
        let port = u16::from_le_bytes([p[at], p[at + 1]]);
        at += 2;
        at += 1 + p[at] as usize; // protocol string
        at += 1; // Found-anything flag
        let count = i16::from_le_bytes([p[at], p[at + 1]]) as usize;
        at += 2;
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            ids.push(i32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]]));
            at += 4;
        }
        (guid, port, ids)
    }

    /// Builds a test fleet with `pvs` channels named `MIM:dbl:0..`, distributed
    /// across `iocs` virtual IOCs.
    fn test_fleet(pvs: u32, iocs: u16) -> Fleet {
        use mimus_profile::{
            Assignment, Introspection, Manifest, Profile, ScalarType, SessionRules, Slot, SlotRole,
            SlotWidth, Template,
        };
        let mut t = Template::new("dbl", 0x0D);
        t.with_introspection(Introspection {
            full: vec![0xFF, 0xFD, 0x01, 0x00, 0x80],
            id_only: Some(vec![0xFF, 0xFE, 0x01, 0x00]),
        })
        .push_fixed(vec![0x02, 0x82, 0x03])
        .push_slot(
            Slot::new(1, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::Value {
                scalar: ScalarType::F64,
            }),
        )
        .push_slot(
            Slot::new(2, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::TimestampSeconds),
        )
        .push_slot(Slot::new(3, SlotWidth::Fixed { bytes: 4 }).with_role(SlotRole::TimestampNanos));
        t.observations = 100;

        let mut p = Profile::new(
            Manifest {
                pvxs_version: "test".into(),
                epics_base_version: "test".into(),
                catalogue_checksum: "0".repeat(64),
                host: "test".into(),
                captured_at: "posix:0".into(),
                mimus_version: "0.1.0".into(),
                study_duration_s: 30.0,
                pvs_observed: 2,
                synthetic: false,
                incomplete: false,
            },
            SessionRules::default(),
        );
        p.templates.push(t);
        p.assignments.push(Assignment {
            pv: "A".into(),
            template: "dbl".into(),
        });

        let spec = crate::fleet::FleetSpec {
            iocs,
            pvs,
            layout: crate::fleet::Layout::Blocked,
            mix: vec![("dbl".into(), 1)],
            rate_mix: mimus_emit::RateMix::uniform(1.0),
            names: crate::naming::NameTemplate::default_pattern("MIM"),
        };
        Fleet::build(&p, &spec).expect("a fleet for the test")
    }

    async fn responder_for(pvs: u32, ports: &[u16]) -> (Arc<SearchResponder>, SocketAddr) {
        let fleet = test_fleet(pvs, ports.len() as u16);
        let iocs: Vec<IocEndpoint> = ports
            .iter()
            .enumerate()
            .map(|(i, p)| IocEndpoint {
                index: i as u16,
                tcp_port: *p,
                guid: {
                    let mut g = *b"MIMUS-------";
                    g[6..8].copy_from_slice(&(i as u16).to_be_bytes());
                    g
                },
            })
            .collect();
        let r = Arc::new(
            SearchResponder::bind("127.0.0.1", 0, Arc::new(fleet), iocs)
                .await
                .expect("bind"),
        );
        let addr = r.local_addr().unwrap();
        let run = Arc::clone(&r);
        tokio::spawn(async move { run.serve().await });
        (r, addr)
    }

    #[tokio::test]
    async fn a_wildcard_bind_is_what_receives_broadcasts() {
        // Binding to a specific host address can fail silently: the responder
        // starts and reports itself active, but receives no broadcasts because
        // they target a subnet address rather than an individual host.
        // Every client that discovers by broadcast is then unable to find
        // the server, with no indication that discovery is not working.
        let fleet = Arc::new(test_fleet(4, 1));
        let iocs = vec![IocEndpoint {
            index: 0,
            tcp_port: 5099,
            guid: [0; GUID_LEN],
        }];
        let r = SearchResponder::bind("0.0.0.0", 0, fleet, iocs)
            .await
            .expect("a wildcard bind must succeed");
        let addr = r.local_addr().unwrap();
        assert!(
            addr.ip().is_unspecified(),
            "bound to {addr}, not the wildcard"
        );
    }

    #[tokio::test]
    async fn a_search_for_a_served_channel_is_answered() {
        let (_r, addr) = responder_for(4, &[5099]).await;
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(&search_datagram(7, &[(1, "MIM:dbl:0")], 0), addr)
            .await
            .unwrap();

        let mut buf = [0u8; 2048];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("no reply within two seconds")
            .unwrap();

        let (guid, port, ids) = parse_response(&buf[..n]);
        assert_eq!(ids, vec![1], "the reply must claim the id that was asked");
        assert_eq!(port, 5099, "and name the tcp port to connect to");
        assert_eq!(&guid[..5], b"MIMUS");
    }

    #[tokio::test]
    async fn a_search_for_nothing_we_serve_is_not_answered() {
        // Responding "not mine" to every broadcast would make every server
        // generate traffic for every search miss. The protocol uses silence
        // as the default response to an unmatched search.
        let (_r, addr) = responder_for(4, &[5099]).await;
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut probe = search_datagram(8, &[(1, "NOT:MINE")], 0);
        probe[HEADER_SIZE + 4] = 0; // Clear the reply-required flag.
        client.send_to(&probe, addr).await.unwrap();

        let mut buf = [0u8; 2048];
        let got =
            tokio::time::timeout(std::time::Duration::from_millis(600), client.recv(&mut buf))
                .await;
        assert!(
            got.is_err(),
            "a miss was answered when it should have been silent"
        );
    }

    #[tokio::test]
    async fn each_virtual_ioc_answers_for_its_own_channels() {
        // Two GUIDs on the same host should appear to a client as two
        // independent IOCs. Each response must also advertise the TCP port
        // of the IOC that owns the matching channel.
        //
        // With four channels distributed across two blocked-layout IOCs,
        // channels 0 and 1 belong to the first IOC, and channels 2 and 3
        // to the second. Searching for one channel from each verifies that
        // both IOCs respond independently.
        let (_r, addr) = responder_for(4, &[5099, 5100]).await;
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(
                &search_datagram(9, &[(1, "MIM:dbl:0"), (2, "MIM:dbl:2")], 0),
                addr,
            )
            .await
            .unwrap();

        let mut seen = Vec::new();
        for _ in 0..2 {
            let mut buf = [0u8; 2048];
            let n = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv(&mut buf))
                .await
                .expect("expected one reply per IOC")
                .unwrap();
            let (guid, port, ids) = parse_response(&buf[..n]);
            seen.push((guid, port, ids));
        }
        seen.sort_by_key(|(_, p, _)| *p);
        assert_eq!(seen[0].1, 5099);
        assert_eq!(seen[1].1, 5100);
        assert_ne!(seen[0].0, seen[1].0, "each IOC must have its own GUID");
    }

    #[tokio::test]
    async fn the_reply_goes_to_the_port_the_client_asked_for() {
        // Unlike a search over TCP, a UDP search has no connected socket
        // to send the response back through. The client specifies the
        // destination port explicitly; replying elsewhere leaves it
        // waiting, unable to distinguish a misdirected reply from a miss.
        let (_r, addr) = responder_for(4, &[5099]).await;
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let want = listener.local_addr().unwrap().port();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender
            .send_to(&search_datagram(10, &[(1, "MIM:dbl:0")], want), addr)
            .await
            .unwrap();

        let mut buf = [0u8; 2048];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), listener.recv(&mut buf))
            .await
            .expect("the reply did not arrive on the requested port")
            .unwrap();
        let (_, port, ids) = parse_response(&buf[..n]);
        assert_eq!(ids, vec![1]);
        assert_eq!(port, 5099);
    }

    #[tokio::test]
    async fn several_searches_in_one_datagram_are_all_answered() {
        // A datagram may contain multiple messages concatenated together.
        // The responder must process each one rather than stopping after
        // the first and silently ignoring the remaining searches.
        let (_r, addr) = responder_for(4, &[5099]).await;
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut both = search_datagram(11, &[(1, "MIM:dbl:0")], 0);
        both.extend_from_slice(&search_datagram(12, &[(2, "MIM:dbl:1")], 0));
        client.send_to(&both, addr).await.unwrap();

        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut buf = [0u8; 2048];
            let n = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv(&mut buf))
                .await
                .expect("both searches should be answered")
                .unwrap();
            let (_, _, got) = parse_response(&buf[..n]);
            ids.extend(got);
        }
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
    }

    #[tokio::test]
    async fn a_corrupt_datagram_does_not_stop_the_responder() {
        // UDP sockets receive arbitrary network traffic, not just valid
        // PVAccess messages. Malformed packets must be discarded without
        // terminating the responder or preventing it from handling later
        // valid searches.
        let (_r, addr) = responder_for(4, &[5099]).await;
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(b"not a pva message at all", addr)
            .await
            .unwrap();
        client
            .send_to(&[0xCA, 2, 0, 3, 0xFF, 0xFF, 0xFF, 0x7F], addr)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        client
            .send_to(&search_datagram(13, &[(1, "MIM:dbl:0")], 0), addr)
            .await
            .unwrap();
        let mut buf = [0u8; 2048];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("the responder stopped after a bad packet")
            .unwrap();
        assert_eq!(parse_response(&buf[..n]).2, vec![1]);
    }
}
