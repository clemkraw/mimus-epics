//! Just enough of the PVData encoding to locate the timestamp in a value.
//!
//! Everything MIMUS sends after the handshake consists of learned bytes.
//! Steady-state updates carry a timestamp in a slot identified during
//! calibration by observing which bytes change. Initial monitor values
//! and GET responses are different: each channel sends only one, so there
//! is no change to observe. Replayed as recorded, they retain the calibration
//! time, possibly hours or days old. Archivers would store this stale time
//! as the first sample for every channel.
//!
//! Locating the timestamp requires the value's layout and the type descriptor
//! sent by the server. This module reads both at the encoding level: sizes,
//! strings, arrays, structures, and unions. It looks for `timeStamp` by its
//! two members, `secondsPastEpoch` and `nanoseconds`, rather than by type,
//! and for `dataTimeStamp`, the second stamp an NTNDArray carries, the same way.
//!
//! The same walk lists where a value's arrays lie, which calibration needs
//! to keep a large array whole: see [`Descriptor::arrays`]. It also reads
//! the fields a PUT writes, and puts them in the updates that follow: see
//! [`Descriptor::written`] and [`Descriptor::overwrite`].
//!
//! Anything it cannot understand is an error, never a guess. The caller then
//! sends the message unchanged, preserving the previous behavior.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use crate::codec::{CodecError, Reader, Size, put_size};
use crate::header::ByteOrder;

/// Maximum nesting depth for descriptors and values.
/// Deeper nesting is rejected to prevent unbounded recursion and stack exhaustion.
const MAX_DEPTH: usize = 32;

const NULL_TYPE: u8 = 0xFF;
const ONLY_ID: u8 = 0xFE;
const FULL_WITH_ID: u8 = 0xFD;
const FULL_TAGGED_ID: u8 = 0xFC;

/// Why a descriptor or value could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkError {
    Codec(CodecError),
    /// A type code not defined by PVData.
    UnknownType(u8),
    /// A reference to a cached type not defined in this descriptor.
    UnknownId(u16),
    /// The bytes do not match the shape described by the type.
    Malformed(&'static str),
}

impl From<CodecError> for WalkError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codec(e) => write!(f, "{e}"),
            Self::UnknownType(c) => write!(f, "unknown type code 0x{c:02x}"),
            Self::UnknownId(id) => write!(f, "reference to undefined type id {id}"),
            Self::Malformed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for WalkError {}

#[derive(Debug, Clone, Copy)]
enum Elem {
    Fixed(usize),
    Str,
}

#[derive(Debug, Clone)]
enum Kind {
    /// Boolean, integer or floating-point value with a fixed byte size.
    Fixed(usize),
    /// A string: a size followed by that many bytes.
    Str,
    /// An array of scalars. Fixed-size arrays have no count on the wire.
    Array {
        elem: Elem,
        fixed: Option<usize>,
    },
    Struct {
        fields: Vec<(String, Node)>,
    },
    Union(Vec<Node>),
    /// A variant union whose value carries its own type.
    Any,
    StructArray(Box<Node>),
    UnionArray(Box<Node>),
    AnyArray,
}

#[derive(Debug, Clone)]
struct Node {
    kind: Kind,
    /// Bit positions occupied by this field and, for structures, all its nested members.
    span: usize,
}

/// Previously defined types, indexed by registry ID.
/// Lookups fall back to the descriptor's own definitions, avoiding copies when walking a value.
struct Registry<'a> {
    base: Option<&'a HashMap<u16, Node>>,
    local: HashMap<u16, Node>,
}

impl Registry<'_> {
    fn get(&self, id: u16) -> Option<&Node> {
        self.local
            .get(&id)
            .or_else(|| self.base.and_then(|b| b.get(&id)))
    }
}

/// A parsed type descriptor, built once per template.
#[derive(Debug, Clone)]
pub struct Descriptor {
    root: Node,
    order: ByteOrder,
    registry: HashMap<u16, Node>,
    /// Bit positions of `timeStamp.secondsPastEpoch` and
    /// `timeStamp.nanoseconds`, if present at the top level.
    stamp_bits: Option<(usize, usize)>,
    /// The same for `dataTimeStamp`, which an NTNDArray carries beside
    /// `timeStamp`: when its frame was taken.
    data_stamp_bits: Option<(usize, usize)>,
}

/// Layout of a body: changed set, selected value, and optional overrun set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    /// Offset of the value, immediately after the changed set. An RPC reply
    /// carries the value from here, without the set.
    pub value_start: usize,
    /// Offset immediately after the value, where the overrun set starts.
    /// GET responses end here.
    pub value_end: usize,
    /// Byte offsets of the 8-byte seconds and 4-byte nanoseconds fields,
    /// if included in the changed set.
    pub stamp: Option<(usize, usize)>,
    /// The same for `dataTimeStamp`, if the type has one and the changed
    /// set includes it.
    pub data_stamp: Option<(usize, usize)>,
}

/// One field a value carries whole, where no member of it can be selected
/// on its own: a scalar, a string, an array, a union, an `any`, an array of
/// structures.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Leaf {
    /// The field's place in a changed set.
    bit: usize,
    /// Its encoding in the body: a size before its elements, a selector
    /// before a union's member, all of it.
    range: Range<usize>,
    shape: Shape,
}

/// What a field carried whole is made of, as far as its byte order goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A number or a boolean of this many bytes.
    Fixed(usize),
    Str,
    /// An array of numbers of `elem` bytes, or of strings if `None`, with a
    /// size before its elements unless its length is fixed by its type.
    Array {
        elem: Option<usize>,
        sized: bool,
    },
    /// A union, an `any`, an array of structures or unions: not reordered.
    Other,
}

impl Shape {
    fn of(kind: &Kind) -> Self {
        match kind {
            Kind::Fixed(w) => Shape::Fixed(*w),
            Kind::Str => Shape::Str,
            Kind::Array { elem, fixed } => Shape::Array {
                elem: match elem {
                    Elem::Fixed(w) => Some(*w),
                    Elem::Str => None,
                },
                sized: fixed.is_none(),
            },
            _ => Shape::Other,
        }
    }

    /// The same field in another byte order, or `None` for a field whose
    /// shape this does not cover or whose bytes do not match it.
    fn reorder(self, bytes: &[u8], from: ByteOrder, to: ByteOrder) -> Option<Vec<u8>> {
        if from == to {
            return Some(bytes.to_vec());
        }
        let mut r = Reader::new(bytes, from);
        let mut out = Vec::with_capacity(bytes.len());
        let string = |r: &mut Reader<'_>, out: &mut Vec<u8>| -> Option<()> {
            let s = r.string().ok()?;
            put_size(out, s.len(), to);
            out.extend_from_slice(s.as_bytes());
            Some(())
        };
        match self {
            Shape::Fixed(w) => out.extend(r.take_exact(w).ok()?.iter().rev()),
            Shape::Str => string(&mut r, &mut out)?,
            Shape::Array { elem, sized } => {
                let count = if sized {
                    let n = r.size().ok()?.or_empty();
                    put_size(&mut out, n, to);
                    n
                } else {
                    // A fixed length: the elements alone, as many as fit.
                    elem.map_or(0, |w| bytes.len() / w.max(1))
                };
                for _ in 0..count {
                    match elem {
                        Some(w) => out.extend(r.take_exact(w).ok()?.iter().rev()),
                        None => string(&mut r, &mut out)?,
                    }
                }
            }
            Shape::Other => return None,
        }
        r.is_empty().then_some(out)
    }
}

/// What follows the value in a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tail {
    /// A monitor update: the overrun set.
    Overrun,
    /// A PUT: nothing.
    Nothing,
}

impl Descriptor {
    /// Parses a type description from a server reply: a status followed
    /// by the type descriptor, as sent in monitor and GET INIT replies.
    pub fn from_init_reply(blob: &[u8], order: ByteOrder) -> Result<Self, WalkError> {
        let mut r = Reader::new(blob, order);
        let status = r.u8()?;
        if status != NULL_TYPE {
            // Non-OK statuses carry a message and a call tree.
            // Only OK and warning statuses are followed by a type.
            skip_string(&mut r)?;
            skip_string(&mut r)?;
            if status > 1 {
                return Err(WalkError::Malformed("the reply carries an error status"));
            }
        }
        let mut reg = Registry {
            base: None,
            local: HashMap::new(),
        };
        let root = read_type(&mut r, &mut reg, 0)?
            .ok_or(WalkError::Malformed("the descriptor is the null type"))?;
        if !matches!(root.kind, Kind::Struct { .. }) {
            return Err(WalkError::Malformed("the top level is not a structure"));
        }
        Ok(Self {
            stamp_bits: stamp_bits(&root, "timeStamp"),
            data_stamp_bits: stamp_bits(&root, "dataTimeStamp"),
            root,
            order,
            registry: reg.local,
        })
    }

    /// Returns whether the top-level structure contains a rewritable `timeStamp`.
    pub fn has_stamp(&self) -> bool {
        self.stamp_bits.is_some()
    }

    /// Locates the end of the value and the timestamp in a body.
    /// Expects a changed set, a value, and an overrun set, with no trailing bytes.
    pub fn locate(&self, body: &[u8]) -> Result<Located, WalkError> {
        self.walk(body, self.order, Tail::Overrun, None, None)
    }

    /// The fields a PUT writes, from its changed set and value: what
    /// follows the sub-command in the request, with nothing after it, in
    /// the byte order of the request. Each field comes with its bit and its
    /// encoding in this descriptor's byte order, ready for
    /// [`Descriptor::overwrite`].
    ///
    /// A structure written whole is listed member by member, so that a
    /// write of `value` and a write of `value.index` name the same fields.
    /// A union, an `any` or an array of structures written in the other
    /// byte order is left out: MIMUS does not reorder their insides.
    pub fn written(
        &self,
        put: &[u8],
        order: ByteOrder,
    ) -> Result<Vec<(usize, Vec<u8>)>, WalkError> {
        let mut found = Vec::new();
        self.walk(put, order, Tail::Nothing, None, Some(&mut found))?;
        Ok(found
            .into_iter()
            .filter_map(|leaf| {
                let bytes = leaf.shape.reorder(&put[leaf.range], order, self.order)?;
                Some((leaf.bit, bytes))
            })
            .collect())
    }

    /// The value a monitor update or a read carries, encoded in full: every
    /// field of the type, in order, without a changed set, as an RPC reply
    /// carries its value.
    ///
    /// An IOC's opening message is a partial value: QSRV marks the fields a
    /// record holds and leaves the rest out. The fields left out are written
    /// as a client that applied the partial value would hold them: zero, an
    /// empty string or array, a union with no member selected, a null `any`.
    pub fn full_value(&self, body: &[u8]) -> Result<Vec<u8>, WalkError> {
        let mut carried = Vec::new();
        self.walk(body, self.order, Tail::Overrun, None, Some(&mut carried))?;
        let carried: HashMap<usize, Range<usize>> =
            carried.into_iter().map(|l| (l.bit, l.range)).collect();
        let mut out = Vec::with_capacity(body.len());
        write_full(&self.root, 0, &carried, body, self.order, &mut out);
        Ok(out)
    }

    /// The bits of a top-level field and of its members, if the type has a
    /// field of that name: `value` holds bits 1 to 5 of an NTEnum.
    pub fn field_bits(&self, name: &str) -> Option<Range<usize>> {
        let Kind::Struct { fields } = &self.root.kind else {
            return None;
        };
        let mut off = 1;
        for (n, f) in fields {
            if n == name {
                return Some(off..off + f.span);
            }
            off += f.span;
        }
        None
    }

    /// Writes the fields of `written`, by bit, over those the monitor update
    /// at `buf[at..]` carries, and returns how many it replaced. A field the
    /// update does not carry is left out; one of another length, a string
    /// or an array, moves what follows it.
    ///
    /// Each replacement is a whole field's encoding in place of another of
    /// the same type, so the update stays a value of its type.
    pub fn overwrite(
        &self,
        buf: &mut Vec<u8>,
        at: usize,
        written: &BTreeMap<usize, Vec<u8>>,
    ) -> Result<usize, WalkError> {
        let mut carried = Vec::new();
        self.walk(
            &buf[at..],
            self.order,
            Tail::Overrun,
            None,
            Some(&mut carried),
        )?;
        let mut replaced = 0;
        // From the last field back, so that a change of length leaves the
        // fields still to replace where they were found.
        for leaf in carried.iter().rev() {
            if let Some(bytes) = written.get(&leaf.bit) {
                let range = at + leaf.range.start..at + leaf.range.end;
                buf.splice(range, bytes.iter().copied());
                replaced += 1;
            }
        }
        Ok(replaced)
    }

    /// Returns where the elements of each array of fixed-width elements
    /// lie in a body, in the order the value carries them: the bytes after
    /// the array's size, empty for an empty array. Arrays of strings,
    /// structures and unions are not listed; a scalar array a union or an
    /// `any` carries is, like the pixels of an NTNDArray.
    ///
    /// Calibration keeps a large array whole rather than comparing it
    /// byte by byte, which would describe a megabyte image as hundreds of
    /// thousands of one-byte fields.
    pub fn arrays(&self, body: &[u8]) -> Result<Vec<Range<usize>>, WalkError> {
        let mut found = Vec::new();
        self.walk(body, self.order, Tail::Overrun, Some(&mut found), None)?;
        Ok(found)
    }

    fn walk(
        &self,
        body: &[u8],
        order: ByteOrder,
        tail: Tail,
        arrays: Option<&mut Vec<Range<usize>>>,
        leaves: Option<&mut Vec<Leaf>>,
    ) -> Result<Located, WalkError> {
        let mut w = Walker {
            r: Reader::new(body, order),
            reg: Registry {
                base: Some(&self.registry),
                local: HashMap::new(),
            },
            stamp_bits: self.stamp_bits,
            data_stamp_bits: self.data_stamp_bits,
            seconds: None,
            nanos: None,
            data_seconds: None,
            data_nanos: None,
            arrays,
            leaves,
        };
        let changed = read_bitset(&mut w.r, order)?;
        let value_start = w.r.position();
        w.partial(&self.root, 0, &changed, 0)?;
        let value_end = w.r.position();
        if tail == Tail::Overrun {
            read_bitset(&mut w.r, order)?;
        }
        if !w.r.is_empty() {
            return Err(WalkError::Malformed("bytes follow the value"));
        }
        Ok(Located {
            value_start,
            value_end,
            stamp: w.seconds.zip(w.nanos),
            data_stamp: w.data_seconds.zip(w.data_nanos),
        })
    }

    /// Names the top-level fields a changed set touches, in the order the
    /// type lists them: `value`, `alarm` and `timeStamp` for an update that
    /// crosses an alarm limit. `bits` holds the changed set's bytes, without
    /// the size before them. A set with the structure's own bit names every
    /// field.
    pub fn changed_fields(&self, bits: &[u8]) -> Vec<String> {
        let mut words = vec![0u64; bits.len().div_ceil(8)];
        for (i, &b) in bits.iter().enumerate() {
            words[i / 8] |= u64::from(b) << (8 * (i % 8));
        }
        let Kind::Struct { fields } = &self.root.kind else {
            return Vec::new();
        };
        let whole = next_set(&words, 0) == Some(0);
        let mut out = Vec::new();
        let mut off = 1;
        for (name, f) in fields {
            if whole || next_set(&words, off).is_some_and(|b| b < off + f.span) {
                out.push(name.clone());
            }
            off += f.span;
        }
        out
    }

    /// Writes a timestamp at the given offsets, using the descriptor's byte order.
    pub fn write_stamp(&self, body: &mut [u8], at: (usize, usize), seconds: i64, nanos: i32) {
        let (s, n) = at;
        let (sb, nb) = match self.order {
            ByteOrder::Little => (seconds.to_le_bytes(), nanos.to_le_bytes()),
            ByteOrder::Big => (seconds.to_be_bytes(), nanos.to_be_bytes()),
        };
        body[s..s + 8].copy_from_slice(&sb);
        body[n..n + 4].copy_from_slice(&nb);
    }
}

/// Writes `node`, at `bit`, in full: the fields `body` carries as they are,
/// the others as their type's empty value.
fn write_full(
    node: &Node,
    bit: usize,
    carried: &HashMap<usize, Range<usize>>,
    body: &[u8],
    order: ByteOrder,
    out: &mut Vec<u8>,
) {
    if let Kind::Struct { fields } = &node.kind {
        let mut off = bit + 1;
        for (_, f) in fields {
            write_full(f, off, carried, body, order, out);
            off += f.span;
        }
        return;
    }
    if let Some(range) = carried.get(&bit) {
        out.extend_from_slice(&body[range.clone()]);
        return;
    }
    match &node.kind {
        Kind::Fixed(w) => out.resize(out.len() + w, 0),
        Kind::Str => put_size(out, 0, order),
        Kind::Array { elem, fixed } => match (fixed, elem) {
            (None, _) => put_size(out, 0, order),
            (Some(n), Elem::Fixed(w)) => out.resize(out.len() + n * w, 0),
            (Some(n), Elem::Str) => {
                for _ in 0..*n {
                    put_size(out, 0, order);
                }
            }
        },
        // No member selected; a null type.
        Kind::Union(_) | Kind::Any => out.push(NULL_TYPE),
        Kind::StructArray(_) | Kind::UnionArray(_) | Kind::AnyArray => put_size(out, 0, order),
        Kind::Struct { .. } => unreachable!("written member by member above"),
    }
}

/// Returns the bit positions of a timestamp's two members, if the
/// top-level structure contains a field of that name.
fn stamp_bits(root: &Node, field: &str) -> Option<(usize, usize)> {
    let Kind::Struct { fields } = &root.kind else {
        return None;
    };
    let mut off = 1;
    for (name, f) in fields {
        if name == field {
            let Kind::Struct { fields: inner } = &f.kind else {
                return None;
            };
            let (mut sec, mut ns) = (None, None);
            let mut o = off + 1;
            for (n, m) in inner {
                match (n.as_str(), &m.kind) {
                    ("secondsPastEpoch", Kind::Fixed(8)) => sec = Some(o),
                    ("nanoseconds", Kind::Fixed(4)) => ns = Some(o),
                    _ => {}
                }
                o += m.span;
            }
            return sec.zip(ns);
        }
        off += f.span;
    }
    None
}

fn skip_string(r: &mut Reader<'_>) -> Result<(), WalkError> {
    let n = r.size()?.or_empty();
    r.skip(n)?;
    Ok(())
}

fn read_name(r: &mut Reader<'_>) -> Result<String, WalkError> {
    let n = r.size()?.or_empty();
    let b = r.take_exact(n)?;
    Ok(String::from_utf8_lossy(b).into_owned())
}

/// Reads a type: null, cached reference, or new definition.
fn read_type(
    r: &mut Reader<'_>,
    reg: &mut Registry<'_>,
    depth: usize,
) -> Result<Option<Node>, WalkError> {
    if depth > MAX_DEPTH {
        return Err(WalkError::Malformed("types nested too deeply"));
    }
    let code = r.u8()?;
    match code {
        NULL_TYPE => Ok(None),
        ONLY_ID => {
            let id = r.u16()?;
            reg.get(id)
                .cloned()
                .map(Some)
                .ok_or(WalkError::UnknownId(id))
        }
        FULL_WITH_ID | FULL_TAGGED_ID => {
            let id = r.u16()?;
            if code == FULL_TAGGED_ID {
                r.i32()?;
            }
            let node = read_type(r, reg, depth + 1)?
                .ok_or(WalkError::Malformed("a cached definition of the null type"))?;
            reg.local.insert(id, node.clone());
            Ok(Some(node))
        }
        _ => read_code(code, r, reg, depth).map(Some),
    }
}

fn read_code(
    code: u8,
    r: &mut Reader<'_>,
    reg: &mut Registry<'_>,
    depth: usize,
) -> Result<Node, WalkError> {
    if code & 0xE0 == 0x80 {
        let kind = match code {
            0x80 | 0x81 => {
                let _type_id = read_name(r)?;
                let n = r.size()?.or_empty();
                let mut fields = Vec::new();
                for _ in 0..n {
                    let name = read_name(r)?;
                    let t = read_type(r, reg, depth + 1)?
                        .ok_or(WalkError::Malformed("a member of the null type"))?;
                    fields.push((name, t));
                }
                if code == 0x80 {
                    Kind::Struct { fields }
                } else {
                    Kind::Union(fields.into_iter().map(|(_, t)| t).collect())
                }
            }
            0x82 => Kind::Any,
            0x83 => {
                r.size()?; // the bound; the value is an ordinary string
                Kind::Str
            }
            0x88 | 0x89 => {
                let e = read_type(r, reg, depth + 1)?
                    .ok_or(WalkError::Malformed("an array of the null type"))?;
                match (code, &e.kind) {
                    (0x88, Kind::Struct { .. }) => Kind::StructArray(Box::new(e)),
                    (0x89, Kind::Union(_)) => Kind::UnionArray(Box::new(e)),
                    _ => return Err(WalkError::Malformed("array element of the wrong kind")),
                }
            }
            0x8A => Kind::AnyArray,
            _ => return Err(WalkError::UnknownType(code)),
        };
        let span = match &kind {
            Kind::Struct { fields } => 1 + fields.iter().map(|(_, f)| f.span).sum::<usize>(),
            _ => 1,
        };
        return Ok(Node { kind, span });
    }

    let elem = match code & !0x18 {
        0x00 => Elem::Fixed(1),
        c @ 0x20..=0x27 => Elem::Fixed(1 << (c & 0x03)),
        0x42 => Elem::Fixed(4),
        0x43 => Elem::Fixed(8),
        0x60 => Elem::Str,
        _ => return Err(WalkError::UnknownType(code)),
    };
    let kind = match code & 0x18 {
        0x00 => match elem {
            Elem::Fixed(w) => Kind::Fixed(w),
            Elem::Str => Kind::Str,
        },
        0x08 => Kind::Array { elem, fixed: None },
        0x10 => {
            r.size()?; // bound; the value still carries its count
            Kind::Array { elem, fixed: None }
        }
        _ => {
            let n = r.size()?.or_empty();
            Kind::Array {
                elem,
                fixed: Some(n),
            }
        }
    };
    Ok(Node { kind, span: 1 })
}

/// Reads a changed or overrun bitset: a byte count followed by 64-bit
/// words in message byte order and any remaining bytes in little-endian order.
fn read_bitset(r: &mut Reader<'_>, order: ByteOrder) -> Result<Vec<u64>, WalkError> {
    let n = r.size()?.or_empty();
    let bytes = r.take_exact(n)?;
    let full = n / 8;
    let mut words = vec![0u64; n.div_ceil(8)];
    for (i, w) in words.iter_mut().enumerate().take(full) {
        let b: [u8; 8] = bytes[i * 8..i * 8 + 8].try_into().expect("8 bytes");
        *w = match order {
            ByteOrder::Little => u64::from_le_bytes(b),
            ByteOrder::Big => u64::from_be_bytes(b),
        };
    }
    for (j, &b) in bytes[full * 8..].iter().enumerate() {
        words[full] |= u64::from(b) << (8 * j);
    }
    Ok(words)
}

/// First set bit at or after `from`.
fn next_set(bits: &[u64], from: usize) -> Option<usize> {
    let mut i = from / 64;
    if i >= bits.len() {
        return None;
    }
    let mut w = bits[i] & (u64::MAX << (from % 64));
    loop {
        if w != 0 {
            return Some(i * 64 + w.trailing_zeros() as usize);
        }
        i += 1;
        if i >= bits.len() {
            return None;
        }
        w = bits[i];
    }
}

struct Walker<'a> {
    r: Reader<'a>,
    reg: Registry<'a>,
    stamp_bits: Option<(usize, usize)>,
    data_stamp_bits: Option<(usize, usize)>,
    seconds: Option<usize>,
    nanos: Option<usize>,
    data_seconds: Option<usize>,
    data_nanos: Option<usize>,
    /// Where to list the arrays of fixed-width elements, when asked.
    arrays: Option<&'a mut Vec<Range<usize>>>,
    /// Where to list every field carried whole, by bit, when asked.
    leaves: Option<&'a mut Vec<Leaf>>,
}

impl Walker<'_> {
    /// Walks the fields selected by the changed set.
    /// A set bit selects the whole field; bits set only in a nested
    /// structure select only the corresponding members.
    fn partial(
        &mut self,
        node: &Node,
        offset: usize,
        bits: &[u64],
        depth: usize,
    ) -> Result<(), WalkError> {
        if depth > MAX_DEPTH {
            return Err(WalkError::Malformed("value nested too deeply"));
        }
        let Kind::Struct { fields } = &node.kind else {
            return Err(WalkError::Malformed("a partial value of a non-structure"));
        };
        let Some(next) = next_set(bits, offset) else {
            return Ok(());
        };
        if next == offset {
            return self.full(node, Some(offset), depth);
        }
        let mut off = offset + 1;
        for (_, f) in fields {
            let Some(next) = next_set(bits, off) else {
                return Ok(());
            };
            if next < off + f.span {
                if f.span == 1 {
                    self.full(f, Some(off), depth + 1)?;
                } else {
                    self.partial(f, off, bits, depth + 1)?;
                }
            }
            off += f.span;
        }
        Ok(())
    }

    /// Walks a complete field. `offset` is its bit position, or `None`
    /// for array and union members, which have no bit positions.
    fn full(&mut self, node: &Node, offset: Option<usize>, depth: usize) -> Result<(), WalkError> {
        if depth > MAX_DEPTH {
            return Err(WalkError::Malformed("value nested too deeply"));
        }
        let start = self.r.position();
        self.field(node, offset, depth)?;
        // A structure's members are listed on their own; anything else is
        // carried whole. Members of a union or an `any` have no bit.
        if let (Some(bit), Some(found)) = (offset, self.leaves.as_deref_mut())
            && !matches!(node.kind, Kind::Struct { .. })
        {
            found.push(Leaf {
                bit,
                range: start..self.r.position(),
                shape: Shape::of(&node.kind),
            });
        }
        Ok(())
    }

    fn field(&mut self, node: &Node, offset: Option<usize>, depth: usize) -> Result<(), WalkError> {
        match &node.kind {
            Kind::Fixed(w) => {
                if let Some(o) = offset {
                    let at = Some(self.r.position());
                    if let Some((s, n)) = self.stamp_bits {
                        if o == s {
                            self.seconds = at;
                        } else if o == n {
                            self.nanos = at;
                        }
                    }
                    if let Some((s, n)) = self.data_stamp_bits {
                        if o == s {
                            self.data_seconds = at;
                        } else if o == n {
                            self.data_nanos = at;
                        }
                    }
                }
                self.r.skip(*w)?;
            }
            Kind::Str => skip_string(&mut self.r)?,
            Kind::Array { elem, fixed } => {
                let count = match fixed {
                    Some(n) => *n,
                    None => self.r.size()?.or_empty(),
                };
                match elem {
                    Elem::Fixed(w) => {
                        let bytes = count
                            .checked_mul(*w)
                            .ok_or(WalkError::Malformed("array larger than memory"))?;
                        let start = self.r.position();
                        self.r.skip(bytes)?;
                        if let Some(found) = self.arrays.as_deref_mut() {
                            found.push(start..start + bytes);
                        }
                    }
                    Elem::Str => {
                        for _ in 0..count {
                            skip_string(&mut self.r)?;
                        }
                    }
                }
            }
            Kind::Struct { fields } => {
                let mut off = offset.map(|o| o + 1);
                for (_, f) in fields {
                    self.full(f, off, depth + 1)?;
                    off = off.map(|o| o + f.span);
                }
            }
            Kind::Union(members) => {
                if let Size::Len(i) = self.r.size()? {
                    let m = members
                        .get(i)
                        .ok_or(WalkError::Malformed("union selector out of range"))?;
                    self.full(m, None, depth + 1)?;
                }
            }
            Kind::Any => {
                if let Some(t) = read_type(&mut self.r, &mut self.reg, depth + 1)? {
                    self.full(&t, None, depth + 1)?;
                }
            }
            Kind::StructArray(e) | Kind::UnionArray(e) => {
                let count = self.r.size()?.or_empty();
                for _ in 0..count {
                    if self.r.u8()? != 0 {
                        self.full(e, None, depth + 1)?;
                    }
                }
            }
            Kind::AnyArray => {
                let count = self.r.size()?.or_empty();
                for _ in 0..count {
                    if self.r.u8()? != 0
                        && let Some(t) = read_type(&mut self.r, &mut self.reg, depth + 1)?
                    {
                        self.full(&t, None, depth + 1)?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{put_size, put_string};

    const LE: ByteOrder = ByteOrder::Little;

    /// An NTScalar<double> descriptor based on PVXS, reduced to the
    /// relevant fields. The string before `timeStamp` tests offset calculation.
    fn ntscalar_descriptor() -> Vec<u8> {
        let mut d = vec![0xFF, 0x80];
        put_string(&mut d, "epics:nt/NTScalar:1.0", LE);
        put_size(&mut d, 3, LE);
        put_string(&mut d, "value", LE); // bit 1
        d.push(0x43);
        put_string(&mut d, "alarm", LE); // bit 2
        d.push(0x80);
        put_string(&mut d, "alarm_t", LE);
        put_size(&mut d, 3, LE);
        put_string(&mut d, "severity", LE); // bit 3
        d.push(0x22);
        put_string(&mut d, "status", LE); // bit 4
        d.push(0x22);
        put_string(&mut d, "message", LE); // bit 5
        d.push(0x60);
        put_string(&mut d, "timeStamp", LE); // bit 6
        d.push(0x80);
        put_string(&mut d, "time_t", LE);
        put_size(&mut d, 3, LE);
        put_string(&mut d, "secondsPastEpoch", LE); // bit 7
        d.push(0x23);
        put_string(&mut d, "nanoseconds", LE); // bit 8
        d.push(0x22);
        put_string(&mut d, "userTag", LE); // bit 9
        d.push(0x22);
        d
    }

    fn value_bytes(message: &str, sec: i64, ns: i32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&1.5f64.to_le_bytes());
        v.extend_from_slice(&2i32.to_le_bytes());
        v.extend_from_slice(&3i32.to_le_bytes());
        put_string(&mut v, message, LE);
        v.extend_from_slice(&sec.to_le_bytes());
        v.extend_from_slice(&ns.to_le_bytes());
        v.extend_from_slice(&0i32.to_le_bytes());
        v
    }

    #[test]
    fn finds_the_stamp_behind_a_variable_length_member() {
        let d = Descriptor::from_init_reply(&ntscalar_descriptor(), LE).unwrap();
        assert!(d.has_stamp());
        for msg in ["", "HIGH alarm with a long explanation"] {
            // Changed set {0}: the whole structure, then an empty overrun set.
            let mut body = vec![0x01, 0x01];
            let start = body.len();
            body.extend(value_bytes(msg, 1_000, 7));
            let end = body.len();
            body.push(0x00);
            let at = d.locate(&body).unwrap();
            assert_eq!(at.value_end, end);
            let (s, n) = at.stamp.expect("stamp found");
            assert_eq!(s, start + 8 + 4 + 4 + 1 + msg.len());
            assert_eq!(n, s + 8);
            d.write_stamp(&mut body, (s, n), 1_794_812_962, 530_111_386);
            assert_eq!(&body[s..s + 8], &1_794_812_962i64.to_le_bytes());
            assert_eq!(&body[n..n + 4], &530_111_386i32.to_le_bytes());
        }
    }

    #[test]
    fn a_changed_set_is_named_by_the_fields_it_touches() {
        let d = Descriptor::from_init_reply(&ntscalar_descriptor(), LE).unwrap();
        // Bits 1, 7, 8, 9: an ordinary update.
        assert_eq!(d.changed_fields(&[0x82, 0x03]), ["value", "timeStamp"]);
        // Bits 1, 3, 4, 5, 7, 8: an alarm limit crossed.
        assert_eq!(
            d.changed_fields(&[0xBA, 0x01]),
            ["value", "alarm", "timeStamp"]
        );
        // Bit 0: the whole structure, as an opening message sends it.
        assert_eq!(d.changed_fields(&[0x01]), ["value", "alarm", "timeStamp"]);
        assert!(d.changed_fields(&[]).is_empty());
    }

    #[test]
    fn follows_a_changed_set_that_selects_members() {
        // Bits 1, 7, 8, 9: value and the three timestamp members, as a
        // steady-state update sends them.
        let d = Descriptor::from_init_reply(&ntscalar_descriptor(), LE).unwrap();
        let mut body = vec![0x02, 0x82, 0x03];
        body.extend_from_slice(&1.5f64.to_le_bytes());
        let s = body.len();
        body.extend_from_slice(&5i64.to_le_bytes());
        body.extend_from_slice(&6i32.to_le_bytes());
        body.extend_from_slice(&0i32.to_le_bytes());
        let end = body.len();
        body.push(0x00);
        let at = d.locate(&body).unwrap();
        assert_eq!(
            at,
            Located {
                value_start: 3,
                value_end: end,
                stamp: Some((s, s + 8)),
                data_stamp: None,
            }
        );
    }

    #[test]
    fn a_value_without_its_stamp_is_located_but_not_stamped() {
        let d = Descriptor::from_init_reply(&ntscalar_descriptor(), LE).unwrap();
        let mut body = vec![0x01, 0x02];
        body.extend_from_slice(&1.5f64.to_le_bytes());
        body.push(0x00);
        let at = d.locate(&body).unwrap();
        assert_eq!(at.stamp, None);
        assert_eq!(at.value_end, body.len() - 1);
    }

    #[test]
    fn refuses_rather_than_guesses() {
        let d = Descriptor::from_init_reply(&ntscalar_descriptor(), LE).unwrap();
        // One byte short.
        let mut body = vec![0x01, 0x01];
        body.extend(value_bytes("", 1, 2));
        body.pop();
        body.push(0x00);
        assert!(d.locate(&body).is_err());
        // A trailing byte the encoding does not account for.
        let mut body = vec![0x01, 0x01];
        body.extend(value_bytes("", 1, 2));
        body.extend_from_slice(&[0x00, 0x00]);
        assert!(d.locate(&body).is_err());
        // A type code outside the encoding.
        assert!(matches!(
            Descriptor::from_init_reply(&[0xFF, 0x80, 0x00, 0x01, 0x01, b'x', 0x7E], LE),
            Err(WalkError::UnknownType(0x7E))
        ));
    }

    /// A structure like an NTNDArray's: a union of scalar arrays, then a
    /// string array, a double array and a counter.
    fn image_descriptor() -> Vec<u8> {
        let mut d = vec![0xFF, 0x80];
        put_string(&mut d, "image", LE);
        put_size(&mut d, 4, LE);
        put_string(&mut d, "value", LE); // bit 1
        d.push(0x81);
        put_string(&mut d, "", LE);
        put_size(&mut d, 2, LE);
        put_string(&mut d, "ubyteValue", LE);
        d.push(0x2C);
        put_string(&mut d, "doubleValue", LE);
        d.push(0x4B);
        put_string(&mut d, "names", LE); // bit 2
        d.push(0x68);
        put_string(&mut d, "wave", LE); // bit 3
        d.push(0x4B);
        put_string(&mut d, "uniqueId", LE); // bit 4
        d.push(0x22);
        d
    }

    #[test]
    fn finds_the_stamp_of_the_data_beside_the_stamp_of_the_update() {
        // An NTNDArray's two stamps: dataTimeStamp, then timeStamp.
        let mut d = vec![0xFF, 0x80];
        put_string(&mut d, "epics:nt/NTNDArray:1.0", LE);
        put_size(&mut d, 3, LE);
        put_string(&mut d, "uniqueId", LE); // bit 1
        d.push(0x22);
        for stamp in ["dataTimeStamp", "timeStamp"] {
            put_string(&mut d, stamp, LE); // bits 2 and 6
            d.push(0x80);
            put_string(&mut d, "time_t", LE);
            put_size(&mut d, 3, LE);
            put_string(&mut d, "secondsPastEpoch", LE);
            d.push(0x23);
            put_string(&mut d, "nanoseconds", LE);
            d.push(0x22);
            put_string(&mut d, "userTag", LE);
            d.push(0x22);
        }
        let desc = Descriptor::from_init_reply(&d, LE).unwrap();
        let mut body = vec![0x01, 0x01];
        body.extend_from_slice(&7i32.to_le_bytes());
        for sec in [10i64, 20] {
            body.extend_from_slice(&sec.to_le_bytes());
            body.extend_from_slice(&5i32.to_le_bytes());
            body.extend_from_slice(&0i32.to_le_bytes());
        }
        body.push(0x00);
        let at = desc.locate(&body).unwrap();
        assert_eq!(at.data_stamp, Some((6, 14)));
        assert_eq!(at.stamp, Some((22, 30)));

        // A delta that carries only timeStamp's seconds and nanoseconds,
        // bits 7 and 8, locates only it.
        let mut delta = vec![0x02, 0x80, 0x01];
        delta.extend_from_slice(&30i64.to_le_bytes());
        delta.extend_from_slice(&5i32.to_le_bytes());
        delta.push(0x00);
        let at = desc.locate(&delta).unwrap();
        assert_eq!(at.stamp, Some((3, 11)));
        assert_eq!(at.data_stamp, None);
    }

    #[test]
    fn a_put_lists_the_fields_it_writes_member_by_member() {
        let d = Descriptor::from_init_reply(&ntscalar_descriptor(), LE).unwrap();

        // pvxput writes the value alone: bit 1.
        let mut put = vec![0x01, 0x02];
        put.extend_from_slice(&2.5f64.to_le_bytes());
        assert_eq!(
            d.written(&put, LE).unwrap(),
            vec![(1, 2.5f64.to_le_bytes().to_vec())]
        );

        // The whole alarm, bit 2, is its three members.
        let mut put = vec![0x01, 0x04];
        put.extend_from_slice(&2i32.to_le_bytes());
        put.extend_from_slice(&3i32.to_le_bytes());
        put_string(&mut put, "HIHI", LE);
        let bits: Vec<usize> = d.written(&put, LE).unwrap().iter().map(|l| l.0).collect();
        assert_eq!(bits, vec![3, 4, 5]);

        // A PUT carries no overrun set: a byte after the value is an error.
        let mut put = vec![0x01, 0x02];
        put.extend_from_slice(&2.5f64.to_le_bytes());
        put.push(0x00);
        assert!(d.written(&put, LE).is_err());
    }

    #[test]
    fn a_put_in_the_other_byte_order_is_read_in_it_and_kept_in_ours() {
        const BE: ByteOrder = ByteOrder::Big;
        let d = Descriptor::from_init_reply(&ntscalar_descriptor(), LE).unwrap();
        // The value and the alarm's message, bits 1 and 5.
        let mut put = vec![0x01, 0x22];
        put.extend_from_slice(&2.5f64.to_be_bytes());
        put_string(&mut put, "HIHI", BE);
        let mut message = Vec::new();
        put_string(&mut message, "HIHI", LE);
        assert_eq!(
            d.written(&put, BE).unwrap(),
            vec![(1, 2.5f64.to_le_bytes().to_vec()), (5, message)]
        );

        // An array of numbers, each element turned around.
        let desc = Descriptor::from_init_reply(&image_descriptor(), LE).unwrap();
        let mut put = vec![0x01, 0x08];
        put_size(&mut put, 2, BE);
        put.extend_from_slice(&1f64.to_be_bytes());
        put.extend_from_slice(&2f64.to_be_bytes());
        let mut wave = Vec::new();
        put_size(&mut wave, 2, LE);
        wave.extend_from_slice(&1f64.to_le_bytes());
        wave.extend_from_slice(&2f64.to_le_bytes());
        assert_eq!(desc.written(&put, BE).unwrap(), vec![(3, wave)]);

        // A union is left out rather than half turned around.
        let mut put = vec![0x01, 0x02];
        put_size(&mut put, 0, BE);
        put_size(&mut put, 1, BE);
        put.push(7);
        assert!(desc.written(&put, BE).unwrap().is_empty());
    }

    #[test]
    fn a_partial_value_is_written_in_full_with_the_rest_empty() {
        let d = Descriptor::from_init_reply(&ntscalar_descriptor(), LE).unwrap();
        // An opening as QSRV sends it: the value and the stamp's seconds and
        // nanoseconds (bits 1, 7, 8), the alarm and the user tag left out.
        let mut body = vec![0x02, 0x82, 0x01];
        body.extend_from_slice(&1.5f64.to_le_bytes());
        body.extend_from_slice(&10i64.to_le_bytes());
        body.extend_from_slice(&5i32.to_le_bytes());
        body.push(0x00);

        let mut want = 1.5f64.to_le_bytes().to_vec();
        want.extend_from_slice(&[0; 8]); // severity, status
        want.push(0x00); // empty message
        want.extend_from_slice(&10i64.to_le_bytes());
        want.extend_from_slice(&5i32.to_le_bytes());
        want.extend_from_slice(&[0; 4]); // userTag
        assert_eq!(d.full_value(&body).unwrap(), want);

        // An empty union, array and string where the image leaves them out.
        let desc = Descriptor::from_init_reply(&image_descriptor(), LE).unwrap();
        let mut body = vec![0x01, 0x10];
        body.extend_from_slice(&7i32.to_le_bytes());
        body.push(0x00);
        let mut want = vec![0xFF, 0x00, 0x00];
        want.extend_from_slice(&7i32.to_le_bytes());
        assert_eq!(desc.full_value(&body).unwrap(), want);
    }

    #[test]
    fn a_field_s_bits_cover_its_members() {
        let d = Descriptor::from_init_reply(&ntscalar_descriptor(), LE).unwrap();
        assert_eq!(d.field_bits("value"), Some(1..2));
        assert_eq!(d.field_bits("alarm"), Some(2..6));
        assert_eq!(d.field_bits("timeStamp"), Some(6..10));
        assert_eq!(d.field_bits("display"), None);
    }

    #[test]
    fn written_fields_replace_those_an_update_carries() {
        let desc = Descriptor::from_init_reply(&image_descriptor(), LE).unwrap();
        // An update of the waveform (bit 3) and the counter (bit 4).
        let mut update = vec![0x01, 0x18];
        put_size(&mut update, 2, LE);
        update.extend_from_slice(&1f64.to_le_bytes());
        update.extend_from_slice(&2f64.to_le_bytes());
        update.extend_from_slice(&7i32.to_le_bytes());
        update.push(0x00);
        let mut buf = vec![0xAA, 0xBB];
        buf.extend_from_slice(&update);

        // A longer waveform, a counter, and a string array the update does
        // not carry.
        let mut wave = Vec::new();
        put_size(&mut wave, 3, LE);
        for v in [10f64, 20.0, 30.0] {
            wave.extend_from_slice(&v.to_le_bytes());
        }
        let mut names = Vec::new();
        put_size(&mut names, 0, LE);
        let written = BTreeMap::from([
            (2, names),
            (3, wave.clone()),
            (4, 9i32.to_le_bytes().to_vec()),
        ]);
        assert_eq!(desc.overwrite(&mut buf, 2, &written).unwrap(), 2);

        let mut want = vec![0xAA, 0xBB, 0x01, 0x18];
        want.extend_from_slice(&wave);
        want.extend_from_slice(&9i32.to_le_bytes());
        want.push(0x00);
        assert_eq!(buf, want);
        assert!(desc.locate(&buf[2..]).is_ok(), "still a value of its type");
    }

    #[test]
    fn lists_the_arrays_of_fixed_width_elements_a_value_carries() {
        let desc = Descriptor::from_init_reply(&image_descriptor(), LE).unwrap();

        // The whole structure: the pixels inside the union are listed, the
        // string array is not.
        let mut body = vec![0x01, 0x01];
        put_size(&mut body, 0, LE); // union member "ubyteValue"
        put_size(&mut body, 5, LE);
        let pixels = body.len();
        body.extend_from_slice(&[1, 2, 3, 4, 5]);
        put_size(&mut body, 1, LE);
        put_string(&mut body, "a", LE);
        put_size(&mut body, 2, LE);
        let wave = body.len();
        body.extend_from_slice(&[0; 16]);
        body.extend_from_slice(&7i32.to_le_bytes());
        body.push(0x00);
        assert_eq!(
            desc.arrays(&body).unwrap(),
            vec![pixels..pixels + 5, wave..wave + 16]
        );

        // A delta that changes only the waveform lists only the waveform.
        let mut delta = vec![0x01, 0x08];
        put_size(&mut delta, 2, LE);
        delta.extend_from_slice(&[0; 16]);
        delta.push(0x00);
        assert_eq!(desc.arrays(&delta).unwrap(), vec![3..19]);

        // An empty array is listed, empty.
        let mut empty = vec![0x01, 0x08];
        put_size(&mut empty, 0, LE);
        empty.push(0x00);
        assert_eq!(desc.arrays(&empty).unwrap(), vec![3..3]);

        // What the walker cannot read is an error, as for the stamp.
        assert!(desc.arrays(&delta[..10]).is_err());
    }

    #[test]
    fn reads_cached_definitions_and_arrays() {
        // A structure with a string array, a structure array using both
        // a new definition and a cached reference, and a union.
        // All precede the timestamp to test offset calculation.
        let mut d = vec![0xFF, 0xFD, 0x01, 0x00, 0x80];
        put_string(&mut d, "outer", LE);
        put_size(&mut d, 5, LE);
        put_string(&mut d, "names", LE); // bit 1
        d.push(0x68);
        put_string(&mut d, "rows", LE); // bit 2
        d.push(0x88);
        d.extend_from_slice(&[0xFD, 0x02, 0x00, 0x80]);
        put_string(&mut d, "row", LE);
        put_size(&mut d, 1, LE);
        put_string(&mut d, "x", LE);
        d.push(0x26);
        put_string(&mut d, "more", LE); // bit 3
        d.push(0x88);
        d.extend_from_slice(&[0xFE, 0x02, 0x00]);
        put_string(&mut d, "u", LE); // bit 4
        d.push(0x81);
        put_string(&mut d, "", LE);
        put_size(&mut d, 2, LE);
        put_string(&mut d, "a", LE);
        d.push(0x21);
        put_string(&mut d, "b", LE);
        d.push(0x60);
        put_string(&mut d, "timeStamp", LE); // bit 5
        d.push(0x80);
        put_string(&mut d, "time_t", LE);
        put_size(&mut d, 2, LE);
        put_string(&mut d, "secondsPastEpoch", LE); // bit 6
        d.push(0x23);
        put_string(&mut d, "nanoseconds", LE); // bit 7
        d.push(0x22);
        let desc = Descriptor::from_init_reply(&d, LE).unwrap();
        assert!(desc.has_stamp());

        let mut body = vec![0x01, 0x01];
        put_size(&mut body, 2, LE);
        put_string(&mut body, "a", LE);
        put_string(&mut body, "bc", LE);
        put_size(&mut body, 3, LE);
        body.extend_from_slice(&[1, 9, 0, 0, 0, 0, 1, 8, 0, 0, 0]);
        put_size(&mut body, 0, LE);
        put_size(&mut body, 1, LE); // union member "b"
        put_string(&mut body, "xyz", LE);
        let s = body.len();
        body.extend_from_slice(&42i64.to_le_bytes());
        body.extend_from_slice(&43i32.to_le_bytes());
        let end = body.len();
        body.push(0x00);
        let at = desc.locate(&body).unwrap();
        assert_eq!(
            at,
            Located {
                value_start: 2,
                value_end: end,
                stamp: Some((s, s + 8)),
                data_stamp: None,
            }
        );
    }

    #[test]
    fn a_wide_changed_set_is_read_word_by_word() {
        let mut bits = vec![9u8];
        bits.extend_from_slice(&(1u64 << 63 | 1).to_le_bytes());
        bits.push(0b100);
        let mut r = Reader::new(&bits, LE);
        let words = read_bitset(&mut r, LE).unwrap();
        assert_eq!(next_set(&words, 0), Some(0));
        assert_eq!(next_set(&words, 1), Some(63));
        assert_eq!(next_set(&words, 64), Some(66));
        assert_eq!(next_set(&words, 67), None);
    }
}
