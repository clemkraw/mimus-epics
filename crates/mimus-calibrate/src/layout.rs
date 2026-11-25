//! Turning observations into a message layout.
//!
//! [`crate::diff`] identifies changing bytes in messages that all have the
//! same length. This module handles the harder case: messages of different
//! lengths.
//!
//! A PVA string consists of a length prefix followed by its bytes. As a
//! result, a channel carrying a string produces messages of varying lengths.
//! In the reference catalogue, `MIM:STRV:*` varies between eleven and twelve
//! characters. Every subsequent field shifts, so byte positions are no
//! longer a reliable reference.
//!
//! The solution is to use two reference points. The prefix is analyzed from
//! the start of the message, while the tail is analyzed from the end. The
//! length difference is absorbed by a variable region between them:
//!
//! ```text
//!   [ common prefix ][ variable region ][ tail, analyzed right-aligned ]
//!    positions from      absorbs the        positions from the end
//!      the start        length change
//! ```
//!
//! The variable region is replayed without interpreting its contents.
//! This preserves exact fidelity: samples remain index-aligned, so replaying
//! sample **k** from every region reproduces observation **k** byte for byte.
//! Fields after a variable-length string, such as timestamps, can therefore
//! still be identified and populated with a live clock instead of being
//! swallowed by a single opaque tail region.

use std::ops::Range;

use crate::diff::{self, DiffError, InferredSlot, Kind};

/// A message segment: either constant bytes or a variable slot.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// Identical across all observations and copied verbatim into the template.
    Fixed(Vec<u8>),
    /// Bytes that changed in at least one observation.
    Slot(InferredSlot),
}

impl Piece {
    pub fn as_slot(&self) -> Option<&InferredSlot> {
        match self {
            Self::Slot(s) => Some(s),
            Self::Fixed(_) => None,
        }
    }
}

/// Describes how observations are aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    /// All messages have the same length; positions are measured from the start.
    Fixed { len: usize },
    /// Message lengths differ. `prefix` bytes are stable from the start,
    /// `tail` bytes are stable from the end, and the region between them
    /// absorbs the length difference.
    Variable {
        prefix: usize,
        tail: usize,
        shortest: usize,
        longest: usize,
    },
}

impl Alignment {
    pub fn is_variable(&self) -> bool {
        matches!(self, Self::Variable { .. })
    }
}

/// Returns the longest prefix identical across all observations.
fn common_prefix(observations: &[Vec<u8>]) -> usize {
    let shortest = observations.iter().map(Vec::len).min().unwrap_or(0);
    (0..shortest)
        .take_while(|&i| observations.windows(2).all(|w| w[0][i] == w[1][i]))
        .count()
}

/// Determines how a set of observations can be aligned.
pub fn align(observations: &[Vec<u8>]) -> Result<Alignment, DiffError> {
    if observations.len() < 2 {
        return Err(DiffError::TooFew {
            given: observations.len(),
        });
    }
    let shortest = observations.iter().map(Vec::len).min().expect("non-empty");
    let longest = observations.iter().map(Vec::len).max().expect("non-empty");
    if shortest == longest {
        return Ok(Alignment::Fixed { len: shortest });
    }

    let prefix = common_prefix(observations);
    // Analyze the remaining bytes from the end. Limit the tail to the
    // shortest message so that every observation has enough bytes.
    let tail = shortest - prefix;
    Ok(Alignment::Variable {
        prefix,
        tail,
        shortest,
        longest,
    })
}

/// Splits equal-length observations into fixed and variable pieces.
fn pieces_from(windows: &[Vec<u8>]) -> Result<Vec<Piece>, DiffError> {
    let slots = diff::infer(windows)?;
    let len = windows[0].len();

    let mut out = Vec::new();
    let mut cursor = 0usize;
    for slot in slots {
        if slot.range.start > cursor {
            out.push(Piece::Fixed(windows[0][cursor..slot.range.start].to_vec()));
        }
        cursor = slot.range.end;
        out.push(Piece::Slot(slot));
    }
    if cursor < len {
        out.push(Piece::Fixed(windows[0][cursor..len].to_vec()));
    }
    Ok(out)
}

/// Builds the complete message layout as an ordered list of pieces.
///
/// Handles both fixed- and variable-length observations. The resulting
/// piece sequence has the same structure in either case, keeping the
/// profile format independent of the alignment strategy.
pub fn layout(observations: &[Vec<u8>]) -> Result<Vec<Piece>, DiffError> {
    match align(observations)? {
        Alignment::Fixed { .. } => pieces_from(observations),

        Alignment::Variable { prefix, tail, .. } => {
            let mut out = Vec::new();

            if prefix > 0 {
                out.push(Piece::Fixed(observations[0][..prefix].to_vec()));
            }

            // The region between the prefix and tail absorbs the length
            // difference. Preserve it as opaque samples because its bytes
            // cannot be compared at fixed positions.
            let middles: Vec<Vec<u8>> = observations
                .iter()
                .map(|o| o[prefix..o.len() - tail].to_vec())
                .collect();
            out.push(Piece::Slot(InferredSlot {
                range: prefix..prefix + tail,
                kind: Kind::Opaque,
                samples: middles,
            }));

            // Analyze the tail from the end. Fields such as timestamps
            // remain aligned even when a variable-length string precedes
            // them, allowing them to retain their inferred types.
            if tail > 0 {
                let tails: Vec<Vec<u8>> = observations
                    .iter()
                    .map(|o| o[o.len() - tail..].to_vec())
                    .collect();
                out.extend(pieces_from(&tails)?);
            }

            Ok(out)
        }
    }
}

/// Builds the layout of observations that carry large arrays, keeping each
/// large array whole.
///
/// Compared byte by byte, a megabyte image is hundreds of thousands of
/// one-byte slots, each with a sample per observation: a profile larger
/// than the capture, describing nothing the bytes do not. Yet where the
/// array lies is known: `arrays[k]` lists the arrays of observation `k`,
/// as [`mimus_wire::pvdata::Descriptor::arrays`] finds them. An array of at
/// least `min_bytes` in any observation becomes one slot of
/// [`Kind::Array`], or fixed bytes if it never changed; the bytes around
/// the arrays are laid out as [`layout`] lays out a whole message, so the
/// timestamp beside an image is still found and filled live.
///
/// Observations that do not list the same number of arrays have no common
/// structure to cut along; they are laid out byte by byte, as before.
pub fn layout_with_arrays(
    observations: &[Vec<u8>],
    arrays: &[Vec<Range<usize>>],
    min_bytes: usize,
) -> Result<Vec<Piece>, DiffError> {
    let count = arrays.first().map(Vec::len).unwrap_or(0);
    if observations.len() < 2
        || arrays.len() != observations.len()
        || arrays.iter().any(|a| a.len() != count)
    {
        return layout(observations);
    }
    let large: Vec<usize> = (0..count)
        .filter(|&i| arrays.iter().any(|a| a[i].len() >= min_bytes))
        .collect();
    if large.is_empty() {
        return layout(observations);
    }

    let mut out = Vec::new();
    let mut cursors = vec![0usize; observations.len()];
    for &i in &large {
        let before: Vec<Vec<u8>> = observations
            .iter()
            .zip(arrays)
            .zip(&cursors)
            .map(|((o, a), &c)| o[c..a[i].start].to_vec())
            .collect();
        push_laid_out(&mut out, &before)?;

        let samples: Vec<Vec<u8>> = observations
            .iter()
            .zip(arrays)
            .map(|(o, a)| o[a[i].clone()].to_vec())
            .collect();
        if samples.windows(2).all(|w| w[0] == w[1]) {
            push_fixed(&mut out, &samples[0]);
        } else {
            out.push(Piece::Slot(InferredSlot {
                range: 0..samples[0].len(),
                kind: Kind::Array,
                samples,
            }));
        }
        for (c, a) in cursors.iter_mut().zip(arrays) {
            *c = a[i].end;
        }
    }
    let after: Vec<Vec<u8>> = observations
        .iter()
        .zip(&cursors)
        .map(|(o, &c)| o[c..].to_vec())
        .collect();
    push_laid_out(&mut out, &after)?;
    Ok(out)
}

/// Appends the layout of one stretch between arrays, merging constant
/// bytes with the constant bytes before them.
fn push_laid_out(out: &mut Vec<Piece>, windows: &[Vec<u8>]) -> Result<(), DiffError> {
    if windows.iter().all(Vec::is_empty) {
        return Ok(());
    }
    for piece in layout(windows)? {
        match piece {
            Piece::Fixed(bytes) => push_fixed(out, &bytes),
            slot => out.push(slot),
        }
    }
    Ok(())
}

fn push_fixed(out: &mut Vec<Piece>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(Piece::Fixed(prev)) => prev.extend_from_slice(bytes),
        _ => out.push(Piece::Fixed(bytes.to_vec())),
    }
}

/// Rebuilds observation `k` from the piece list.
///
/// Samples are index-aligned, so replaying sample `k` from every slot
/// reproduces observation `k` exactly, even if slot boundaries are
/// imperfectly inferred. Exact replay therefore does not depend on the
/// quality of the analysis, only on preserving sample order.
pub fn rebuild(pieces: &[Piece], k: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for p in pieces {
        match p {
            Piece::Fixed(bytes) => out.extend_from_slice(bytes),
            Piece::Slot(slot) => out.extend_from_slice(&slot.samples[k]),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds messages resembling `MIM:STRV:0`, with a variable-length PVA
    /// string followed by a timestamp and a constant tail.
    ///
    /// ```text
    ///   02 82 03            changed-set
    ///   <len> <chars>       string value, variable length
    ///   <secs 8>            POSIX seconds
    ///   <nanos 4>           nanoseconds
    ///   00 00 00 00 00      userTag and overrun bitset
    /// ```
    fn string_channel() -> Vec<Vec<u8>> {
        let texts = [
            "pos=-37.3877",
            "pos=1.68139",
            "pos=21.512",
            "pos=-8.30894",
            "pos=49.4113",
        ];
        texts
            .iter()
            .enumerate()
            .map(|(k, text)| {
                let mut m = vec![0x02, 0x82, 0x03];
                m.push(text.len() as u8);
                m.extend_from_slice(text.as_bytes());
                m.extend_from_slice(&(1_787_174_889i64 + k as i64).to_le_bytes());
                m.extend_from_slice(&(157_360_595u32 + k as u32 * 7).to_le_bytes());
                m.extend_from_slice(&[0u8; 5]);
                m
            })
            .collect()
    }

    /// Builds fixed-length messages from a real softIocPVX capture.
    fn fixed_channel() -> Vec<Vec<u8>> {
        [
            "028203 4e2f1da0a7ca5840 e91f866a00000000 d3236009 0000000000",
            "028203 24b95bf59f585840 ea1f866a00000000 9bb86009 0000000000",
            "028203 047c4c1052a85740 eb1f866a00000000 20596309 0000000000",
            "028203 d7d473e780bb5640 ec1f866a00000000 476e5f09 0000000000",
        ]
        .iter()
        .map(|s| {
            let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
            (0..clean.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap())
                .collect()
        })
        .collect()
    }

    #[test]
    fn equal_lengths_are_a_fixed_alignment() {
        assert_eq!(
            align(&fixed_channel()).unwrap(),
            Alignment::Fixed { len: 28 }
        );
    }

    #[test]
    fn differing_lengths_are_a_variable_alignment() {
        let obs = string_channel();
        let a = align(&obs).unwrap();
        assert!(a.is_variable());
        let Alignment::Variable {
            prefix,
            shortest,
            longest,
            ..
        } = a
        else {
            unreachable!()
        };
        // The changed-set is shared by all observations, but the string length
        // prefix that follows it varies.
        assert_eq!(prefix, 3);
        assert_eq!(longest - shortest, 2, "the texts differ by two characters");
    }

    #[test]
    fn the_fixed_case_still_finds_its_three_slots() {
        // Variable-length alignment must preserve the existing fixed-length case.
        let pieces = layout(&fixed_channel()).unwrap();
        let kinds: Vec<Kind> = pieces
            .iter()
            .filter_map(|p| p.as_slot())
            .map(|s| s.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![Kind::F64, Kind::TimestampSeconds, Kind::TimestampNanos]
        );
    }

    #[test]
    fn a_string_channel_keeps_its_timestamp_out_of_the_opaque_blob() {
        // Analyze the tail from the end so the timestamp remains a typed slot.
        // Treating everything after the string as opaque would preserve exact
        // replay, but would also replay calibration timestamps. A consumer
        // measuring latency would then use stale timestamps instead of live ones.
        let pieces = layout(&string_channel()).unwrap();
        let kinds: Vec<Kind> = pieces
            .iter()
            .filter_map(|p| p.as_slot())
            .map(|s| s.kind)
            .collect();
        assert!(
            kinds.contains(&Kind::TimestampSeconds),
            "the timestamp was swallowed by the variable region: {kinds:?}"
        );
        assert!(kinds.contains(&Kind::TimestampNanos));
    }

    #[test]
    fn the_variable_region_carries_the_length_change() {
        let obs = string_channel();
        let pieces = layout(&obs).unwrap();
        let variable = pieces
            .iter()
            .filter_map(|p| p.as_slot())
            .find(|s| s.samples.iter().any(|x| x.len() != s.samples[0].len()))
            .expect("no variable-length slot");
        let lengths: Vec<usize> = variable.samples.iter().map(Vec::len).collect();
        assert_eq!(
            lengths.iter().max().unwrap() - lengths.iter().min().unwrap(),
            2
        );
    }

    #[test]
    fn every_observation_round_trips_variable() {
        // Verify exact replay for every observation in the variable-length case.
        let obs = string_channel();
        let pieces = layout(&obs).unwrap();
        for (k, original) in obs.iter().enumerate() {
            assert_eq!(&rebuild(&pieces, k), original, "observation {k}");
        }
    }

    #[test]
    fn every_observation_round_trips_fixed() {
        let obs = fixed_channel();
        let pieces = layout(&obs).unwrap();
        for (k, original) in obs.iter().enumerate() {
            assert_eq!(&rebuild(&pieces, k), original, "observation {k}");
        }
    }

    #[test]
    fn pieces_are_contiguous_and_complete() {
        // Ensure every byte is represented exactly once. Gaps would drop bytes
        // during emission, while overlaps would emit bytes more than once.
        for obs in [fixed_channel(), string_channel()] {
            let pieces = layout(&obs).unwrap();
            assert_eq!(rebuild(&pieces, 0).len(), obs[0].len());
        }
    }

    #[test]
    fn a_constant_channel_is_all_template() {
        // A static PV has no changing bytes, so the entire message belongs
        // in the fixed template and no slots are needed.
        let obs = vec![vec![1, 2, 3, 4, 5]; 4];
        let pieces = layout(&obs).unwrap();
        assert_eq!(pieces.len(), 1);
        assert!(matches!(pieces[0], Piece::Fixed(_)));
    }

    #[test]
    fn a_message_that_is_only_a_variable_field_still_works() {
        // With no common prefix or stable tail, the entire message is variable.
        // This edge case must still preserve every byte without panicking.
        let obs = vec![vec![1u8, 2], vec![3, 4, 5], vec![6]];
        let pieces = layout(&obs).unwrap();
        for (k, expected) in obs.iter().enumerate() {
            assert_eq!(&rebuild(&pieces, k), expected);
        }
    }

    /// Waveform updates as an NTScalarArray of doubles sends them: a
    /// changed set, the array's size and elements, the timestamp, the
    /// overrun set. Each lists where its array lies.
    fn waveform_channel(len: usize) -> (Vec<Vec<u8>>, Vec<Vec<Range<usize>>>) {
        let mut obs = Vec::new();
        let mut arrays = Vec::new();
        for k in 0..5u32 {
            let mut m = vec![0x02, 0x42, 0x00];
            m.push(len as u8);
            let start = m.len();
            for i in 0..len {
                m.extend_from_slice(&((i as f64) * 0.5 + f64::from(k)).to_le_bytes());
            }
            // The one array this message carries.
            let array = start..m.len();
            arrays.push(vec![array]);
            m.extend_from_slice(&(1_787_174_889i64 + i64::from(k)).to_le_bytes());
            m.extend_from_slice(&(157_360_595u32 + k * 7_919).to_le_bytes());
            m.extend_from_slice(&[0u8; 5]);
            obs.push(m);
        }
        (obs, arrays)
    }

    #[test]
    fn a_large_array_is_one_slot_and_the_stamp_beside_it_is_still_found() {
        let (obs, arrays) = waveform_channel(100);
        let pieces = layout_with_arrays(&obs, &arrays, 512).unwrap();
        let kinds: Vec<Kind> = pieces
            .iter()
            .filter_map(|p| p.as_slot())
            .map(|s| s.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![Kind::Array, Kind::TimestampSeconds, Kind::TimestampNanos]
        );
        for (k, original) in obs.iter().enumerate() {
            assert_eq!(&rebuild(&pieces, k), original, "observation {k}");
        }
    }

    #[test]
    fn a_small_array_is_compared_byte_by_byte_as_before() {
        let (obs, arrays) = waveform_channel(10);
        assert_eq!(
            layout_with_arrays(&obs, &arrays, 512).unwrap(),
            layout(&obs).unwrap()
        );
    }

    #[test]
    fn an_array_that_never_changed_is_fixed_bytes_merged_with_its_neighbours() {
        let (mut obs, arrays) = waveform_channel(100);
        let first = obs[0][arrays[0][0].clone()].to_vec();
        for (o, a) in obs.iter_mut().zip(&arrays) {
            o[a[0].clone()].copy_from_slice(&first);
        }
        let pieces = layout_with_arrays(&obs, &arrays, 512).unwrap();
        assert!(matches!(&pieces[0], Piece::Fixed(b) if b.len() == 4 + 800));
        for (k, original) in obs.iter().enumerate() {
            assert_eq!(&rebuild(&pieces, k), original, "observation {k}");
        }
    }

    #[test]
    fn arrays_of_varying_length_keep_every_byte() {
        let mut obs = Vec::new();
        let mut arrays = Vec::new();
        for len in [600usize, 700, 650] {
            let mut m = vec![0x01, 0x02];
            m.extend_from_slice(&(len as u16).to_le_bytes());
            let start = m.len();
            m.extend((0..len).map(|i| (i * len) as u8));
            // The one array this message carries.
            let array = start..m.len();
            arrays.push(vec![array]);
            m.extend_from_slice(&[9, 9, 0]);
            obs.push(m);
        }
        let pieces = layout_with_arrays(&obs, &arrays, 512).unwrap();
        for (k, original) in obs.iter().enumerate() {
            assert_eq!(&rebuild(&pieces, k), original, "observation {k}");
        }
        assert!(
            pieces
                .iter()
                .any(|p| p.as_slot().is_some_and(|s| s.kind == Kind::Array))
        );
    }

    #[test]
    fn observations_without_a_common_structure_are_laid_out_byte_by_byte() {
        let (obs, mut arrays) = waveform_channel(100);
        arrays[2].clear();
        assert_eq!(
            layout_with_arrays(&obs, &arrays, 512).unwrap(),
            layout(&obs).unwrap()
        );
    }

    #[test]
    fn one_observation_is_refused() {
        assert_eq!(align(&[vec![1, 2, 3]]), Err(DiffError::TooFew { given: 1 }));
    }

    #[test]
    fn analysis_is_deterministic() {
        let obs = string_channel();
        assert_eq!(layout(&obs).unwrap(), layout(&obs).unwrap());
    }
}
