//! Randomised tests of every decoder in the crate.
//!
//! Each target is fed what a peer may send: valid messages cut at random
//! places, valid messages with bytes changed, and bytes with no structure at
//! all. A decoder may refuse any of these; it may not panic, loop, allocate
//! without bound, or read a valid message differently from how it was
//! written.
//!
//! The generator is seeded, so a failure names the seed that reproduces it.
//! `MIMUS_FUZZ_ITERATIONS` sets how many cases each target runs: a few
//! thousand by default, enough for every run of the suite; set it to a
//! million to hunt.

use std::collections::BTreeMap;

use bytes::BytesMut;
use mimus_wire::codec::{put_i32, put_size, put_string};
use mimus_wire::header::flags;
use mimus_wire::pvdata::{Descriptor, RequestTypes};
use mimus_wire::{
    ByteOrder, ConnectionValidation, ConnectionValidationResponse, CreateChannelRequest,
    CreateChannelResponse, Direction, Frame, Framer, GetFieldRequest, GetRequest, HEADER_SIZE,
    Header, MonitorRequest, PipelineAck, Reader, SearchFound, SearchRequest, SearchResponse,
    Segmentation, Size, write_frame,
};

// ------------------------------------------------------------- generator

/// SplitMix64: small, fast, and the same everywhere.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// A number below `n`, which must not be zero.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }
    /// Fewer than `max` random bytes.
    fn some_bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.below(max);
        self.bytes(n)
    }
    fn order(&mut self) -> ByteOrder {
        if self.chance(2) {
            ByteOrder::Little
        } else {
            ByteOrder::Big
        }
    }
    /// A short name: what channel and field names look like, plus the odd
    /// character outside ASCII.
    fn name(&mut self) -> String {
        let n = 1 + self.below(12);
        (0..n)
            .map(|_| match self.below(40) {
                0 => 'é',
                1 => ':',
                2 => '_',
                k => (b'a' + (k % 26) as u8) as char,
            })
            .collect()
    }
}

fn iterations() -> u64 {
    std::env::var("MIMUS_FUZZ_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000)
}

/// Runs `case` for each seed, naming the seed if it panics.
fn each_seed(target: &str, case: impl Fn(&mut Rng)) {
    for seed in 0..iterations() {
        let mut rng = Rng::new(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ target.len() as u64);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(&mut rng)));
        if let Err(e) = result {
            let why = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            panic!("{target}: seed {seed}: {why}");
        }
    }
}

/// A copy of `bytes` damaged the way a broken peer or a bit flip would.
fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    for _ in 0..1 + rng.below(4) {
        match rng.below(6) {
            0 if !b.is_empty() => {
                let i = rng.below(b.len());
                b[i] ^= 1 << rng.below(8);
            }
            1 if !b.is_empty() => {
                let i = rng.below(b.len());
                b[i] = rng.byte();
            }
            2 if !b.is_empty() => {
                let i = rng.below(b.len());
                b.truncate(i);
            }
            3 => {
                let i = rng.below(b.len() + 1);
                let extra = {
                    let n = 1 + rng.below(8);
                    rng.bytes(n)
                };
                b.splice(i..i, extra);
            }
            4 if !b.is_empty() => {
                // Sizes are where decoders go wrong: a byte that becomes
                // 0xFE announces four more, 0xFF a null.
                let i = rng.below(b.len());
                b[i] = [0xFE, 0xFF, 0x00, 0x7F, 0x80][rng.below(5)];
            }
            _ => {
                let i = rng.below(b.len() + 1);
                b.truncate(i);
                b.extend(rng.some_bytes(16));
            }
        }
    }
    b
}

// ------------------------------------------------------------- the codec

#[test]
fn a_size_reads_back_as_written_at_every_length() {
    for order in [ByteOrder::Little, ByteOrder::Big] {
        for n in [
            0usize,
            1,
            253,
            254,
            255,
            256,
            65_535,
            65_536,
            i32::MAX as usize,
        ] {
            let mut out = Vec::new();
            put_size(&mut out, n, order);
            let mut r = Reader::new(&out, order);
            assert_eq!(r.size(), Ok(Size::Len(n)), "{n} in {order}");
            assert!(r.is_empty());
        }
    }
    each_seed("size", |rng| {
        let order = rng.order();
        let n = rng.below(1 << 31);
        let mut out = Vec::new();
        put_size(&mut out, n, order);
        let mut r = Reader::new(&out, order);
        assert_eq!(r.size(), Ok(Size::Len(n)));
        let s = rng.name();
        let mut out = Vec::new();
        put_string(&mut out, &s, order);
        assert_eq!(Reader::new(&out, order).string(), Ok(s.as_str()));
    });
}

#[test]
fn the_reader_refuses_what_it_cannot_read_and_never_panics() {
    each_seed("reader", |rng| {
        let bytes = rng.some_bytes(24);
        let order = rng.order();
        let mut r = Reader::new(&bytes, order);
        for _ in 0..12 {
            let before = r.remaining();
            let _ = match rng.below(9) {
                0 => r.u8().map(|_| ()),
                1 => r.i16().map(|_| ()),
                2 => r.u16().map(|_| ()),
                3 => r.i32().map(|_| ()),
                4 => r.size().map(|_| ()),
                5 => r.string().map(|_| ()),
                6 => r.skip(rng.below(10)),
                7 => r.take_exact(rng.below(10)).map(|_| ()),
                _ => Ok(()),
            };
            assert!(r.remaining() <= before);
            assert_eq!(r.position() + r.remaining(), bytes.len());
        }
    });
}

// ------------------------------------------------------------- headers and frames

#[test]
fn a_header_decodes_to_what_encodes_back_to_its_bytes() {
    each_seed("header", |rng| {
        let mut raw = [0u8; HEADER_SIZE];
        raw.copy_from_slice(&rng.bytes(HEADER_SIZE));
        if rng.chance(2) {
            raw[0] = mimus_wire::MAGIC;
        }
        if let Ok(h) = Header::decode(&raw) {
            assert_eq!(h.encode(), raw, "{h}");
            assert!(h.payload_size >= 0 || h.is_control());
            let _ = h.to_string();
        }
    });
}

/// What a framer must hand back from a stream: each control message as it
/// comes, and each application message once whole.
#[derive(Debug, PartialEq)]
struct Expected {
    command: u8,
    flags_without_segments: u8,
    payload: Vec<u8>,
}

fn expected(frame: &Frame) -> Expected {
    Expected {
        command: frame.header.command,
        flags_without_segments: frame.header.flags & !flags::SEG_MASK,
        payload: frame.payload.to_vec(),
    }
}

#[test]
fn the_framer_hands_back_every_message_whatever_the_reads() {
    each_seed("framer", |rng| {
        let mut stream = BytesMut::new();
        let mut want = Vec::new();
        for _ in 0..1 + rng.below(12) {
            let order = rng.order();
            let dir = if rng.chance(2) {
                Direction::FromClient
            } else {
                Direction::FromServer
            };
            if rng.chance(4) {
                let h = Header::control(rng.byte(), rng.next() as i32, order, dir);
                write_frame(&mut stream, &h, &[]);
                want.push(Expected {
                    command: h.command,
                    flags_without_segments: h.flags,
                    payload: Vec::new(),
                });
                continue;
            }
            let command = rng.byte();
            let payload = rng.some_bytes(300);
            let parts = if rng.chance(3) { 2 + rng.below(4) } else { 1 };
            if parts == 1 {
                let h = Header::application(
                    command,
                    payload.len() as i32,
                    order,
                    dir,
                    Segmentation::None,
                );
                write_frame(&mut stream, &h, &payload);
            } else {
                // Cut the payload into segments, with control messages
                // between them, which PVA allows.
                let mut cuts: Vec<usize> = (0..parts - 1)
                    .map(|_| rng.below(payload.len() + 1))
                    .collect();
                cuts.sort_unstable();
                cuts.insert(0, 0);
                cuts.push(payload.len());
                for (i, w) in cuts.windows(2).enumerate() {
                    let seg = match i {
                        0 => Segmentation::First,
                        i if i == parts - 1 => Segmentation::Last,
                        _ => Segmentation::Middle,
                    };
                    let piece = &payload[w[0]..w[1]];
                    let h = Header::application(command, piece.len() as i32, order, dir, seg);
                    write_frame(&mut stream, &h, piece);
                    if i < parts - 1 && rng.chance(3) {
                        let c = Header::control(rng.byte(), rng.next() as i32, order, dir);
                        write_frame(&mut stream, &c, &[]);
                        want.push(Expected {
                            command: c.command,
                            flags_without_segments: c.flags,
                            payload: Vec::new(),
                        });
                    }
                }
            }
            want.push(Expected {
                command,
                flags_without_segments: order.to_bits() | dir.to_bits(),
                payload,
            });
        }

        // Feed it in pieces of any size, a byte at a time included.
        let mut framer = Framer::new();
        let mut buf = BytesMut::new();
        let mut got = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let n = (1 + rng.below(64)).min(stream.len() - at);
            buf.extend_from_slice(&stream[at..at + n]);
            at += n;
            while let Some(frame) = framer.next_frame(&mut buf).expect("a valid stream") {
                got.push(expected(&frame));
            }
        }
        assert!(buf.is_empty(), "{} bytes left", buf.len());
        assert!(!framer.is_reassembling());
        assert_eq!(got, want);
    });
}

#[test]
fn the_framer_refuses_garbage_without_panicking_or_looping() {
    each_seed("framer garbage", |rng| {
        let mut stream = Vec::new();
        for _ in 0..1 + rng.below(6) {
            let size = rng.below(64);
            let h = Header::application(
                rng.byte(),
                size as i32,
                rng.order(),
                Direction::FromClient,
                [
                    Segmentation::None,
                    Segmentation::First,
                    Segmentation::Middle,
                    Segmentation::Last,
                ][rng.below(4)],
            );
            stream.extend_from_slice(&h.encode());
            stream.extend(rng.bytes(size));
        }
        let stream = if rng.chance(2) {
            mutate(rng, &stream)
        } else {
            rng.some_bytes(200)
        };
        let mut framer = Framer::new().with_max_payload(1 + rng.below(256) as u32);
        let mut buf = BytesMut::new();
        let mut at = 0;
        'feed: while at < stream.len() {
            let n = (1 + rng.below(32)).min(stream.len() - at);
            buf.extend_from_slice(&stream[at..at + n]);
            at += n;
            // Every call either takes bytes, waits for more, or fails.
            for _ in 0..stream.len() + 2 {
                let before = buf.len();
                match framer.next_frame(&mut buf) {
                    Ok(Some(f)) => {
                        assert!(buf.len() < before, "a frame without bytes");
                        if f.header.is_application() {
                            assert_eq!(f.payload.len(), f.header.payload_size as usize);
                        }
                    }
                    Ok(None) => continue 'feed,
                    Err(_) => break 'feed,
                }
            }
            panic!("the framer did not settle");
        }
    });
}

// ------------------------------------------------------------- control messages

#[test]
fn every_message_decoder_reads_back_what_was_written() {
    each_seed("messages", |rng| {
        let order = rng.order();
        let search = SearchRequest {
            sequence_id: rng.next() as i32,
            flags: rng.byte(),
            response_port: rng.next() as u16,
            channels: (0..rng.below(6))
                .map(|_| (rng.next() as i32, rng.name()))
                .collect(),
        };
        assert_eq!(
            SearchRequest::decode(&search.encode(order), order),
            Ok(search.clone())
        );

        let guid: [u8; 12] = rng.bytes(12).try_into().unwrap();
        let found: Vec<i32> = (0..rng.below(6)).map(|_| rng.next() as i32).collect();
        let response = SearchResponse {
            guid: &guid,
            sequence_id: search.sequence_id,
            server_port: rng.next() as u16,
            found: &found,
        };
        let back = SearchFound::decode(&response.encode(order), order).unwrap();
        assert_eq!(back.guid, guid);
        assert_eq!(back.sequence_id, response.sequence_id);
        assert_eq!(back.server_port, response.server_port);
        assert_eq!(back.found, found);

        let create = CreateChannelRequest {
            channels: (0..1 + rng.below(3))
                .map(|_| (rng.next() as i32, rng.name()))
                .collect(),
        };
        assert_eq!(
            CreateChannelRequest::decode(&create.encode(order), order),
            Ok(create.clone())
        );
        let created = CreateChannelResponse {
            client_cid: rng.next() as i32,
            server_cid: rng.next() as i32,
        };
        assert_eq!(
            CreateChannelResponse::decode(&created.encode(order), order),
            Ok(created)
        );

        let monitor = MonitorRequest {
            server_cid: rng.next() as i32,
            request_id: rng.next() as i32,
            sub_command: rng.byte(),
        };
        let encoded = monitor.encode(order);
        let back = MonitorRequest::decode(&encoded, order).unwrap();
        assert_eq!(back, monitor);
        let _ = back.initial_credit(&encoded, order);

        let field = GetFieldRequest {
            server_cid: rng.next() as i32,
            request_id: rng.next() as i32,
            sub_field: rng.name(),
        };
        assert_eq!(
            GetFieldRequest::decode(&field.encode(order), order),
            Ok(field.clone())
        );
        let get = GetRequest {
            server_cid: rng.next() as i32,
            request_id: rng.next() as i32,
            sub_command: rng.byte(),
        };
        assert_eq!(GetRequest::decode(&get.encode(order), order), Ok(get));

        let validation = ConnectionValidation {
            buffer_size: rng.next() as i32,
            registry_size: rng.next() as i16,
        };
        let back = ConnectionValidation::decode(&validation.encode(order), order).unwrap();
        assert_eq!(back.buffer_size, validation.buffer_size);
        assert_eq!(back.registry_size, validation.registry_size);
        let reply = ConnectionValidationResponse {
            buffer_size: rng.next() as i32,
            registry_size: rng.next() as i16,
            qos: rng.next() as i16,
            auth: rng.name(),
        };
        let back = ConnectionValidationResponse::decode(&reply.encode(order), order).unwrap();
        assert_eq!(back.auth, reply.auth);
        assert_eq!(back.buffer_size, reply.buffer_size);
    });
}

#[test]
fn every_message_decoder_survives_damaged_and_random_bytes() {
    each_seed("message garbage", |rng| {
        let order = rng.order();
        let valid = match rng.below(4) {
            0 => SearchRequest {
                sequence_id: 1,
                flags: 0x81,
                response_port: 5076,
                channels: vec![(1, rng.name()), (2, rng.name())],
            }
            .encode(order),
            1 => CreateChannelRequest {
                channels: vec![(7, rng.name())],
            }
            .encode(order),
            2 => {
                let mut m = MonitorRequest {
                    server_cid: 1,
                    request_id: 2,
                    sub_command: 0x88,
                }
                .encode(order);
                put_i32(&mut m, 4, order);
                m
            }
            _ => rng.some_bytes(40),
        };
        let bytes = if rng.chance(4) {
            rng.some_bytes(64)
        } else {
            mutate(rng, &valid)
        };
        let _ = SearchRequest::decode(&bytes, order);
        let _ = SearchFound::decode(&bytes, order);
        let _ = CreateChannelRequest::decode(&bytes, order);
        let _ = CreateChannelResponse::decode(&bytes, order);
        if let Ok(m) = MonitorRequest::decode(&bytes, order) {
            let _ = m.initial_credit(&bytes, order);
            let _ = (m.is_init(), m.is_start(), m.is_stop(), m.is_destroy());
            let _ = (m.wants_pipeline(), m.is_pipeline_ack());
        }
        let _ = PipelineAck::decode(&bytes, order);
        let _ = GetFieldRequest::decode(&bytes, order);
        let _ = GetRequest::decode(&bytes, order);
        let _ = ConnectionValidation::decode(&bytes, order);
        let _ = ConnectionValidationResponse::decode(&bytes, order);
    });
}

// ------------------------------------------------------------- PVData

/// A PVData type, as the generator builds it.
#[derive(Debug, Clone)]
enum Ty {
    /// A scalar: its type code and width.
    Scalar(u8, usize),
    Str,
    /// A string with a bound, which changes nothing in its value.
    BoundedStr(usize),
    /// An array of scalars or strings: the element's code and width (0 for
    /// strings), then whether it has a bound, or a fixed count.
    Array(u8, usize, ArrayKind),
    Struct(String, Vec<(String, Ty)>),
    Union(String, Vec<(String, Ty)>),
    Any,
    StructArray(Box<Ty>),
    UnionArray(Box<Ty>),
    AnyArray,
}

#[derive(Debug, Clone, Copy)]
enum ArrayKind {
    Variable,
    Bounded(usize),
    Fixed(usize),
}

impl Ty {
    /// The bits the type takes in a changed set.
    fn span(&self) -> usize {
        match self {
            Ty::Struct(_, fields) => 1 + fields.iter().map(|(_, f)| f.span()).sum::<usize>(),
            _ => 1,
        }
    }

    fn is_struct(&self) -> bool {
        matches!(self, Ty::Struct(..))
    }
}

const SCALARS: [(u8, usize); 11] = [
    (0x00, 1),
    (0x20, 1),
    (0x21, 2),
    (0x22, 4),
    (0x23, 8),
    (0x24, 1),
    (0x25, 2),
    (0x26, 4),
    (0x27, 8),
    (0x42, 4),
    (0x43, 8),
];

fn gen_ty(rng: &mut Rng, depth: usize) -> Ty {
    let leaf = depth >= 3 || rng.chance(2);
    if leaf {
        return match rng.below(6) {
            0 | 1 => {
                let (c, w) = SCALARS[rng.below(SCALARS.len())];
                Ty::Scalar(c, w)
            }
            2 => Ty::Str,
            3 => Ty::BoundedStr(1 + rng.below(100)),
            _ => {
                let (c, w) = if rng.chance(4) {
                    (0x60, 0)
                } else {
                    SCALARS[rng.below(SCALARS.len())]
                };
                let kind = match rng.below(4) {
                    0 => ArrayKind::Bounded(1 + rng.below(100)),
                    1 => ArrayKind::Fixed(rng.below(4)),
                    _ => ArrayKind::Variable,
                };
                Ty::Array(c, w, kind)
            }
        };
    }
    match rng.below(10) {
        0..=4 => gen_struct(rng, depth),
        5 => Ty::Union(
            rng.name(),
            (0..1 + rng.below(3))
                .map(|_| (rng.name(), gen_ty(rng, depth + 1)))
                .collect(),
        ),
        6 => Ty::Any,
        7 => Ty::StructArray(Box::new(gen_struct(rng, depth + 1))),
        8 => Ty::UnionArray(Box::new(Ty::Union(
            rng.name(),
            (0..1 + rng.below(3))
                .map(|_| (rng.name(), gen_ty(rng, depth + 2)))
                .collect(),
        ))),
        _ => Ty::AnyArray,
    }
}

fn gen_struct(rng: &mut Rng, depth: usize) -> Ty {
    let n = rng.below(5);
    let mut fields: Vec<(String, Ty)> = Vec::new();
    for i in 0..n {
        // Names unique within the structure, as PVData requires.
        fields.push((format!("{}{i}", rng.name()), gen_ty(rng, depth + 1)));
    }
    Ty::Struct(rng.name(), fields)
}

/// An NT-like top level: what a channel's type looks like, sometimes with
/// a standard timestamp the walker must find.
fn gen_root(rng: &mut Rng) -> Ty {
    let Ty::Struct(id, mut fields) = gen_struct(rng, 0) else {
        unreachable!()
    };
    if rng.chance(2) {
        fields.insert(0, ("value".into(), gen_ty(rng, 1)));
    }
    if rng.chance(2) {
        let at = rng.below(fields.len() + 1);
        fields.insert(
            at,
            (
                "timeStamp".into(),
                Ty::Struct(
                    "time_t".into(),
                    vec![
                        ("secondsPastEpoch".into(), Ty::Scalar(0x23, 8)),
                        ("nanoseconds".into(), Ty::Scalar(0x22, 4)),
                        ("userTag".into(), Ty::Scalar(0x22, 4)),
                    ],
                ),
            ),
        );
    }
    Ty::Struct(id, fields)
}

/// Writes a type. Structures are now and then given an identifier, and a
/// structure written again is then named by it alone, as servers and
/// pvAccessCPP clients do.
fn put_ty(
    out: &mut Vec<u8>,
    ty: &Ty,
    order: ByteOrder,
    rng: &mut Rng,
    cache: &mut Vec<(u16, String)>,
) {
    match ty {
        Ty::Scalar(c, _) => out.push(*c),
        Ty::Str => out.push(0x60),
        Ty::BoundedStr(bound) => {
            out.push(0x83);
            put_size(out, *bound, order);
        }
        Ty::Array(c, _, kind) => match kind {
            ArrayKind::Variable => out.push(c | 0x08),
            ArrayKind::Bounded(bound) => {
                out.push(c | 0x10);
                put_size(out, *bound, order);
            }
            ArrayKind::Fixed(n) => {
                out.push(c | 0x18);
                put_size(out, *n, order);
            }
        },
        Ty::Struct(id, fields) | Ty::Union(id, fields) => {
            let signature = format!("{ty:?}");
            if let Some((cached, _)) = cache.iter().find(|(_, s)| *s == signature)
                && rng.chance(2)
            {
                out.push(0xFE);
                out.extend_from_slice(&match order {
                    ByteOrder::Little => cached.to_le_bytes(),
                    ByteOrder::Big => cached.to_be_bytes(),
                });
                return;
            }
            if rng.chance(3) {
                let id = rng.next() as u16;
                out.push(0xFD);
                out.extend_from_slice(&match order {
                    ByteOrder::Little => id.to_le_bytes(),
                    ByteOrder::Big => id.to_be_bytes(),
                });
                cache.retain(|(c, _)| *c != id);
                cache.push((id, signature));
            }
            out.push(if ty.is_struct() { 0x80 } else { 0x81 });
            put_string(out, id, order);
            put_size(out, fields.len(), order);
            for (name, f) in fields {
                put_string(out, name, order);
                put_ty(out, f, order, rng, cache);
            }
        }
        Ty::Any => out.push(0x82),
        Ty::StructArray(e) => {
            out.push(0x88);
            put_ty(out, e, order, rng, cache);
        }
        Ty::UnionArray(e) => {
            out.push(0x89);
            put_ty(out, e, order, rng, cache);
        }
        Ty::AnyArray => out.push(0x8A),
    }
}

fn put_fixed(out: &mut Vec<u8>, le: &[u8], order: ByteOrder) {
    match order {
        ByteOrder::Little => out.extend_from_slice(le),
        ByteOrder::Big => out.extend(le.iter().rev()),
    }
}

/// A value, kept apart from its encoding so that the same value can be
/// written in either byte order.
#[derive(Debug, Clone)]
enum Val {
    /// A scalar's bytes, least significant first.
    Fixed(Vec<u8>),
    Str(String),
    Array(Vec<Val>),
    Struct(Vec<Val>),
    /// The member selected, if any.
    Union(Option<(usize, Box<Val>)>),
    /// The type carried, if any, the seed its encoding draws from (so
    /// that it encodes the same way twice), and its value.
    Any(Option<(Ty, u64, Box<Val>)>),
    /// Elements, each null or not.
    Elements(Vec<Option<Val>>),
    AnyElements(Vec<Option<Option<(Ty, u64, Val)>>>),
}

fn gen_val(rng: &mut Rng, ty: &Ty, depth: usize) -> Val {
    let scalar = |rng: &mut Rng, w: usize| {
        if w == 0 {
            Val::Str(if rng.chance(5) {
                String::new()
            } else {
                rng.name()
            })
        } else {
            Val::Fixed(rng.bytes(w))
        }
    };
    match ty {
        Ty::Scalar(_, w) => Val::Fixed(rng.bytes(*w)),
        Ty::Str | Ty::BoundedStr(_) => scalar(rng, 0),
        Ty::Array(_, w, kind) => {
            let n = match kind {
                ArrayKind::Fixed(n) => *n,
                _ if rng.chance(30) => 254 + rng.below(4),
                _ => rng.below(5),
            };
            Val::Array((0..n).map(|_| scalar(rng, *w)).collect())
        }
        Ty::Struct(_, fields) => Val::Struct(
            fields
                .iter()
                .map(|(_, f)| gen_val(rng, f, depth + 1))
                .collect(),
        ),
        Ty::Union(_, members) => {
            if members.is_empty() || rng.chance(3) {
                Val::Union(None)
            } else {
                let i = rng.below(members.len());
                Val::Union(Some((i, Box::new(gen_val(rng, &members[i].1, depth + 1)))))
            }
        }
        Ty::Any => {
            if depth > 4 || rng.chance(3) {
                Val::Any(None)
            } else {
                let t = gen_ty(rng, 2);
                let v = gen_val(rng, &t, depth + 1);
                Val::Any(Some((t, rng.next(), Box::new(v))))
            }
        }
        Ty::StructArray(e) | Ty::UnionArray(e) => Val::Elements(
            (0..rng.below(4))
                .map(|_| (!rng.chance(4)).then(|| gen_val(rng, e, depth + 1)))
                .collect(),
        ),
        Ty::AnyArray => Val::AnyElements(
            (0..rng.below(4))
                .map(|_| {
                    if rng.chance(4) {
                        None
                    } else if depth > 4 || rng.chance(4) {
                        Some(None)
                    } else {
                        let t = gen_ty(rng, 2);
                        let v = gen_val(rng, &t, depth + 1);
                        Some(Some((t, rng.next(), v)))
                    }
                })
                .collect(),
        ),
    }
}

fn put_val(out: &mut Vec<u8>, ty: &Ty, val: &Val, order: ByteOrder) {
    match (ty, val) {
        (_, Val::Fixed(b)) => put_fixed(out, b, order),
        (_, Val::Str(s)) => put_string(out, s, order),
        (Ty::Array(_, _, kind), Val::Array(items)) => {
            if !matches!(kind, ArrayKind::Fixed(_)) {
                put_size(out, items.len(), order);
            }
            for i in items {
                put_val(out, ty, i, order);
            }
        }
        (Ty::Struct(_, fields), Val::Struct(vals)) => {
            for ((_, f), v) in fields.iter().zip(vals) {
                put_val(out, f, v, order);
            }
        }
        (Ty::Union(_, members), Val::Union(sel)) => match sel {
            None => out.push(0xFF),
            Some((i, v)) => {
                put_size(out, *i, order);
                put_val(out, &members[*i].1, v, order);
            }
        },
        (Ty::Any, Val::Any(sel)) => match sel {
            None => out.push(0xFF),
            Some((t, seed, v)) => {
                put_ty(out, t, order, &mut Rng::new(*seed), &mut Vec::new());
                put_val(out, t, v, order);
            }
        },
        (Ty::StructArray(e) | Ty::UnionArray(e), Val::Elements(items)) => {
            put_size(out, items.len(), order);
            for i in items {
                match i {
                    None => out.push(0),
                    Some(v) => {
                        out.push(1);
                        put_val(out, e, v, order);
                    }
                }
            }
        }
        (Ty::AnyArray, Val::AnyElements(items)) => {
            put_size(out, items.len(), order);
            for i in items {
                match i {
                    None => out.push(0),
                    Some(None) => {
                        out.push(1);
                        out.push(0xFF);
                    }
                    Some(Some((t, seed, v))) => {
                        out.push(1);
                        put_ty(out, t, order, &mut Rng::new(*seed), &mut Vec::new());
                        put_val(out, t, v, order);
                    }
                }
            }
        }
        (t, v) => unreachable!("{t:?} with {v:?}"),
    }
}

/// A leaf the walker reports: a field carried whole that is not a
/// structure, at its bit.
#[derive(Debug)]
struct Leaf<'a> {
    bit: usize,
    ty: &'a Ty,
    val: &'a Val,
}

/// Writes the fields a changed set selects, depth first, as PVA does: a
/// structure whose own bit is set is written whole. Returns the leaves
/// written, in order.
fn put_partial<'a>(
    out: &mut Vec<u8>,
    ty: &'a Ty,
    val: &'a Val,
    bit: usize,
    set: &[bool],
    order: ByteOrder,
    leaves: &mut Vec<Leaf<'a>>,
) {
    let selected_below = |from: usize, span: usize| set[from..from + span].iter().any(|&b| b);
    if set[bit] {
        put_whole(out, ty, val, bit, order, leaves);
        return;
    }
    let (Ty::Struct(_, fields), Val::Struct(vals)) = (ty, val) else {
        unreachable!()
    };
    let mut off = bit + 1;
    for ((_, f), v) in fields.iter().zip(vals) {
        if selected_below(off, f.span()) {
            put_partial(out, f, v, off, set, order, leaves);
        }
        off += f.span();
    }
}

fn put_whole<'a>(
    out: &mut Vec<u8>,
    ty: &'a Ty,
    val: &'a Val,
    bit: usize,
    order: ByteOrder,
    leaves: &mut Vec<Leaf<'a>>,
) {
    if let (Ty::Struct(_, fields), Val::Struct(vals)) = (ty, val) {
        let mut off = bit + 1;
        for ((_, f), v) in fields.iter().zip(vals) {
            put_whole(out, f, v, off, order, leaves);
            off += f.span();
        }
        return;
    }
    leaves.push(Leaf { bit, ty, val });
    put_val(out, ty, val, order);
}

fn put_bitset(out: &mut Vec<u8>, set: &[bool], order: ByteOrder, rng: &mut Rng) {
    let last = set.iter().rposition(|&b| b);
    let mut n = last.map_or(0, |l| l / 8 + 1);
    // Trailing zero bytes are legal, and some servers send them.
    if rng.chance(4) {
        n += rng.below(9);
    }
    let byte = |i: usize| -> u8 {
        (0..8)
            .filter(|j| set.get(i * 8 + j).copied().unwrap_or(false))
            .fold(0, |b, j| b | 1 << j)
    };
    put_size(out, n, order);
    let words = n / 8;
    for w in 0..words {
        let le: Vec<u8> = (0..8).map(|i| byte(w * 8 + i)).collect();
        put_fixed(out, &le, order);
    }
    for i in words * 8..n {
        out.push(byte(i));
    }
}

fn gen_set(rng: &mut Rng, span: usize) -> Vec<bool> {
    let p = 1 + rng.below(6);
    (0..span).map(|_| rng.chance(p)).collect()
}

/// A channel's type and an update of it, as a server sends them.
struct Case {
    root: Ty,
    order: ByteOrder,
    init: Vec<u8>,
}

fn gen_case(rng: &mut Rng) -> Case {
    let root = gen_root(rng);
    let order = rng.order();
    let mut init = vec![0xFF]; // status OK
    put_ty(&mut init, &root, order, rng, &mut Vec::new());
    Case { root, order, init }
}

fn encode_leaf(leaf: &Leaf<'_>, order: ByteOrder) -> Vec<u8> {
    let mut out = Vec::new();
    put_val(&mut out, leaf.ty, leaf.val, order);
    out
}

/// Whether the walker converts a field written in the other byte order:
/// scalars, strings and arrays of them.
fn converted(ty: &Ty) -> bool {
    matches!(
        ty,
        Ty::Scalar(..) | Ty::Str | Ty::BoundedStr(_) | Ty::Array(..)
    )
}

#[test]
fn every_update_of_every_type_is_located_and_its_stamp_found() {
    each_seed("locate", |rng| {
        let case = gen_case(rng);
        let desc = Descriptor::from_init_reply(&case.init, case.order).expect("a valid type");
        let val = gen_val(rng, &case.root, 0);
        let set = gen_set(rng, case.root.span());
        let mut body = Vec::new();
        put_bitset(&mut body, &set, case.order, rng);
        let start = body.len();
        let mut leaves = Vec::new();
        put_partial(
            &mut body,
            &case.root,
            &val,
            0,
            &set,
            case.order,
            &mut leaves,
        );
        let end = body.len();
        let span = 1 + rng.below(20);
        let overrun = gen_set(rng, span);
        put_bitset(&mut body, &overrun, case.order, rng);

        let at = desc.locate(&body).expect("a valid update");
        assert_eq!((at.value_start, at.value_end), (start, end));
        // The stamp is found when the update carries both its members.
        if let Some((s, n)) = at.stamp {
            let seconds: [u8; 8] = body[s..s + 8].try_into().unwrap();
            let nanos: [u8; 4] = body[n..n + 4].try_into().unwrap();
            let mut copy = body.clone();
            desc.write_stamp(&mut copy, (s, n), 1_700_000_000, 123);
            assert_ne!(
                (seconds, nanos),
                (
                    copy[s..s + 8].try_into().unwrap(),
                    copy[n..n + 4].try_into().unwrap()
                )
            );
            assert_eq!(
                desc.locate(&copy).unwrap(),
                at,
                "a stamp written in place moves nothing"
            );
        }
        let _ = desc.arrays(&body).expect("arrays of a valid update");
        let bits: Vec<u8> = {
            let mut b = Vec::new();
            put_bitset(&mut b, &set, ByteOrder::Little, &mut Rng::new(0));
            let mut r = Reader::new(&b, ByteOrder::Little);
            let n = r.size().unwrap().or_empty();
            r.take_exact(n).unwrap().to_vec()
        };
        let _ = desc.changed_fields(&bits);
    });
}

#[test]
fn a_partial_value_encoded_in_full_keeps_every_field_it_carried() {
    each_seed("full value", |rng| {
        let case = gen_case(rng);
        let desc = Descriptor::from_init_reply(&case.init, case.order).unwrap();
        let val = gen_val(rng, &case.root, 0);
        let set = gen_set(rng, case.root.span());
        let mut body = Vec::new();
        put_bitset(&mut body, &set, case.order, rng);
        let mut carried = Vec::new();
        put_partial(
            &mut body,
            &case.root,
            &val,
            0,
            &set,
            case.order,
            &mut carried,
        );
        put_bitset(&mut body, &[], case.order, rng);

        let full = desc.full_value(&body).expect("a valid update");
        // The full value is an update with every bit set, as a PUT of the
        // whole structure.
        let mut put = Vec::new();
        put_bitset(&mut put, &[true], case.order, rng);
        put.extend_from_slice(&full);
        let leaves = desc
            .written(&put, case.order)
            .expect("the full value walks");
        let by_bit: BTreeMap<usize, Vec<u8>> = leaves.into_iter().collect();
        for leaf in &carried {
            assert_eq!(
                by_bit.get(&leaf.bit),
                Some(&encode_leaf(leaf, case.order)),
                "bit {} lost",
                leaf.bit
            );
        }
    });
}

#[test]
fn what_a_put_writes_is_read_in_its_order_and_written_over_an_update_in_ours() {
    each_seed("written", |rng| {
        let case = gen_case(rng);
        let desc = Descriptor::from_init_reply(&case.init, case.order).unwrap();

        // The PUT, in its own byte order.
        let put_order = rng.order();
        let put_val_tree = gen_val(rng, &case.root, 0);
        let put_set = gen_set(rng, case.root.span());
        let mut put = Vec::new();
        put_bitset(&mut put, &put_set, put_order, rng);
        let mut put_leaves = Vec::new();
        put_partial(
            &mut put,
            &case.root,
            &put_val_tree,
            0,
            &put_set,
            put_order,
            &mut put_leaves,
        );
        let written: BTreeMap<usize, Vec<u8>> = desc
            .written(&put, put_order)
            .expect("a valid put")
            .into_iter()
            .collect();
        for leaf in &put_leaves {
            let kept = written.get(&leaf.bit);
            if put_order == case.order || converted(leaf.ty) {
                assert_eq!(
                    kept,
                    Some(&encode_leaf(leaf, case.order)),
                    "bit {} of {:?}, put in {put_order}, channel in {}",
                    leaf.bit,
                    leaf.ty,
                    case.order
                );
            } else {
                assert_eq!(kept, None, "bit {} of {:?}", leaf.bit, leaf.ty);
            }
        }
        assert_eq!(written.len(), written.keys().len());

        // An update, in the channel's order, then the written fields over it.
        let val = gen_val(rng, &case.root, 0);
        let set = gen_set(rng, case.root.span());
        let mut update = Vec::new();
        put_bitset(&mut update, &set, case.order, rng);
        let mut leaves = Vec::new();
        put_partial(
            &mut update,
            &case.root,
            &val,
            0,
            &set,
            case.order,
            &mut leaves,
        );
        let mut overrun = Vec::new();
        put_bitset(&mut overrun, &gen_set(rng, 8), case.order, rng);
        update.extend_from_slice(&overrun);

        let prefix = rng.some_bytes(8);
        let mut buf = prefix.clone();
        buf.extend_from_slice(&update);
        let replaced = desc
            .overwrite(&mut buf, prefix.len(), &written)
            .expect("a valid update");
        let expected = leaves
            .iter()
            .filter(|l| written.contains_key(&l.bit))
            .count();
        assert_eq!(replaced, expected);
        assert_eq!(&buf[..prefix.len()], &prefix[..], "what precedes is kept");
        let body = &buf[prefix.len()..];
        desc.locate(body).expect("still a valid update");
        assert!(body.ends_with(&overrun));
        let as_put = &body[..body.len() - overrun.len()];
        let now: BTreeMap<usize, Vec<u8>> = desc
            .written(as_put, case.order)
            .unwrap()
            .into_iter()
            .collect();
        for leaf in &leaves {
            let want = written
                .get(&leaf.bit)
                .cloned()
                .unwrap_or_else(|| encode_leaf(leaf, case.order));
            assert_eq!(now.get(&leaf.bit), Some(&want), "bit {}", leaf.bit);
        }
    });
}

#[test]
fn the_walker_survives_damaged_types_and_values() {
    each_seed("walker garbage", |rng| {
        let case = gen_case(rng);
        let init = if rng.chance(3) {
            mutate(rng, &case.init)
        } else {
            case.init.clone()
        };
        let Ok(desc) = Descriptor::from_init_reply(&init, case.order) else {
            return;
        };
        let val = gen_val(rng, &case.root, 0);
        let set = gen_set(rng, case.root.span());
        let mut body = Vec::new();
        put_bitset(&mut body, &set, case.order, rng);
        put_partial(
            &mut body,
            &case.root,
            &val,
            0,
            &set,
            case.order,
            &mut Vec::new(),
        );
        put_bitset(&mut body, &[], case.order, rng);
        let body = if rng.chance(5) {
            rng.some_bytes(64)
        } else {
            mutate(rng, &body)
        };
        let _ = desc.locate(&body);
        let _ = desc.arrays(&body);
        let _ = desc.full_value(&body);
        let _ = desc.written(&body, rng.order());
        let mut buf = body.clone();
        let mut map = BTreeMap::new();
        map.insert(rng.below(case.root.span() + 2), rng.some_bytes(12));
        let _ = desc.overwrite(&mut buf, 0, &map);
        let _ = desc.changed_fields(&body);
        let _ = desc.field_bits("value");
    });
    each_seed("random types", |rng| {
        let blob = rng.some_bytes(80);
        if let Ok(d) = Descriptor::from_init_reply(&blob, rng.order()) {
            let _ = d.locate(&rng.some_bytes(40));
        }
    });
}

#[test]
fn a_monitor_request_s_queue_is_read_from_any_pv_request_without_panicking() {
    each_seed("queue size", |rng| {
        let order = rng.order();
        let mut types = RequestTypes::new();
        for _ in 0..1 + rng.below(3) {
            let ty = gen_root(rng);
            let mut req = Vec::new();
            put_ty(&mut req, &ty, order, rng, &mut Vec::new());
            put_val(&mut req, &ty, &gen_val(rng, &ty, 0), order);
            let req = if rng.chance(2) {
                mutate(rng, &req)
            } else {
                req
            };
            let _ = types.queue_size(&req, order);
        }
    });
}

/// A type that names an earlier one twice, and is named twice by the next:
/// each level doubles the type described, in a few bytes. Read naively, a
/// few dozen levels hold more nodes than any memory.
fn doubling_type(levels: u16, order: ByteOrder) -> Vec<u8> {
    let id = |out: &mut Vec<u8>, code: u8, id: u16| {
        out.push(code);
        out.extend_from_slice(&match order {
            ByteOrder::Little => id.to_le_bytes(),
            ByteOrder::Big => id.to_be_bytes(),
        });
    };
    let mut t = Vec::new();
    t.push(0x80);
    put_string(&mut t, "", order);
    put_size(&mut t, levels as usize + 1, order);
    put_string(&mut t, "f0", order);
    id(&mut t, 0xFD, 0);
    t.push(0x22);
    for level in 1..=levels {
        put_string(&mut t, &format!("f{level}"), order);
        id(&mut t, 0xFD, level);
        t.push(0x80);
        put_string(&mut t, "", order);
        put_size(&mut t, 2, order);
        put_string(&mut t, "a", order);
        id(&mut t, 0xFE, level - 1);
        put_string(&mut t, "b", order);
        id(&mut t, 0xFE, level - 1);
    }
    t
}

#[test]
fn a_client_defining_types_without_end_is_not_remembered_without_end() {
    // Each request defines a large type under a new identifier, within the
    // limit of one read; together they would hold far more.
    let order = ByteOrder::Little;
    let mut types = RequestTypes::new();
    let started = std::time::Instant::now();
    for id in 0..1000u16 {
        let mut req = vec![0xFD];
        req.extend_from_slice(&id.to_le_bytes());
        let t = doubling_type(10, order);
        req.extend_from_slice(&t);
        let _ = types.queue_size(&req, order);
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_type_that_doubles_at_each_level_is_refused_in_bounded_time() {
    let order = ByteOrder::Little;
    let started = std::time::Instant::now();
    // As a client's pvRequest.
    let mut req = doubling_type(40, order);
    req.extend_from_slice(&[0; 64]);
    assert_eq!(RequestTypes::new().queue_size(&req, order), None);
    // As a server's type.
    let mut init = vec![0xFF];
    init.extend(doubling_type(40, order));
    let _ = Descriptor::from_init_reply(&init, order);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}
