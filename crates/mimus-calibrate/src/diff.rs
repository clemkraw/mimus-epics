//! Finding the parts of a message that change.
//!
//! Calibration watches one channel and compares successive updates byte
//! by byte. Bytes that remain constant become part of the template;
//! bytes that change become slots. This requires no knowledge of EPICS
//! normative types or the meaning of individual fields, keeping MIMUS
//! independent of the PVXS version used during calibration.
//!
//! Two challenges make this more than a simple diff.
//!
//! **A field can appear constant.** The high byte of a double may not
//! change while its value stays within one decade. The high bytes of a
//! POSIX timestamp may remain unchanged for months. Freezing these bytes
//! into the template would produce correct output only within the
//! observation window. Rather than guessing, we propose and validate:
//! identify a run of changing bytes, propose that it belongs to an
//! 8-byte double or timestamp, and accept the proposal only if every
//! observation decodes plausibly under that interpretation.
//!
//! **Samples must stay aligned.** Each slot retains its observed bytes
//! in order, and sample **k** from every slot comes from observation
//! **k**. Replaying sample **k** therefore reproduces observation **k**
//! exactly, byte for byte, even when slot boundaries are imperfect.
//! Fidelity does not depend on identifying the correct boundaries;
//! only signal generation does.

use std::ops::Range;

/// The inferred meaning of a variable slot.
///
/// Every classification is validated against the observations, never
/// assumed. If no proposal fits, the slot remains [`Kind::Opaque`],
/// which is a valid state: opaque slots replay their observed samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The bytes change, but their meaning is unknown.
    /// This is the default, not a failure.
    Opaque,
    /// An IEEE double whose decoded values are finite and within
    /// a plausible magnitude range in every observation.
    F64,
    /// POSIX epoch seconds, stored as an `i64`, within a valid calendar
    /// range and never decreasing across the observation window.
    TimestampSeconds,
    /// Nanoseconds within a second, stored as a `u32` below one billion in every observation.
    TimestampNanos,
}

/// A region of the message that changed, with its inferred classification.
#[derive(Debug, Clone, PartialEq)]
pub struct InferredSlot {
    /// Half-open byte range within the message.
    pub range: Range<usize>,
    pub kind: Kind,
    /// Observed bytes for this range, one entry per observation.
    ///
    /// Samples remain index-aligned across slots, so replaying sample
    /// **k** from every slot reproduces observation **k** exactly.
    pub samples: Vec<Vec<u8>>,
}

impl InferredSlot {
    /// Returns whether every observation contains the same bytes here.
    ///
    /// Slots produced by [`infer`] cannot be constant because inference
    /// only selects ranges where bytes change. This is useful for
    /// validating slots constructed by other means.
    pub fn is_constant(&self) -> bool {
        self.samples.windows(2).all(|w| w[0] == w[1])
    }
}

/// Why a set of observations cannot be analysed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffError {
    /// Fewer than two observations were provided, leaving nothing to compare.
    TooFew { given: usize },
    /// The observations do not all have the same length.
    ///
    /// Variable-length fields shift all subsequent bytes, making
    /// byte-position analysis unreliable. Treating such messages as
    /// fixed-length would produce a template valid for only one length.
    LengthVaries { first: usize, other: usize },
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooFew { given } => {
                write!(f, "{given} observation(s); at least two are needed")
            }
            Self::LengthVaries { first, other } => write!(
                f,
                "message length varies ({first} then {other}): a variable-length \
                 field shifts every byte after it, so fixed positions cannot \
                 describe this message"
            ),
        }
    }
}

impl std::error::Error for DiffError {}

/// Returns a mask marking byte positions that differ across observations.
///
/// A position is marked `true` if at least one observation differs
/// from the first observation at that position.
pub fn changed_mask(observations: &[Vec<u8>]) -> Result<Vec<bool>, DiffError> {
    if observations.len() < 2 {
        return Err(DiffError::TooFew {
            given: observations.len(),
        });
    }
    let len = observations[0].len();
    if let Some(other) = observations.iter().find(|o| o.len() != len) {
        return Err(DiffError::LengthVaries {
            first: len,
            other: other.len(),
        });
    }

    let mut mask = vec![false; len];
    for obs in &observations[1..] {
        for (i, (a, b)) in observations[0].iter().zip(obs.iter()).enumerate() {
            if a != b {
                mask[i] = true;
            }
        }
    }
    Ok(mask)
}

/// Returns the maximal contiguous ranges of `true` values in a mask.
fn runs(mask: &[bool]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &changed) in mask.iter().enumerate() {
        match (changed, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(s..mask.len());
    }
    out
}

/// A candidate interpretation for a byte range.
#[derive(Debug, Clone)]
struct Proposal {
    range: Range<usize>,
    kind: Kind,
}

fn le_i64(bytes: &[u8]) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(bytes);
    i64::from_le_bytes(b)
}

fn le_u32(bytes: &[u8]) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(bytes);
    u32::from_le_bytes(b)
}

fn le_f64(bytes: &[u8]) -> f64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(bytes);
    f64::from_le_bytes(b)
}

/// POSIX timestamps between 2020-01-01 and 2100-01-01.
const PLAUSIBLE_EPOCH: Range<i64> = 1_577_836_800..4_102_444_800;

/// Longest span of seconds a calibration can observe, with a wide margin:
/// the default deadline is ten minutes.
const MAX_SECONDS_SPAN: i64 = 86_400;

/// Maximum plausible magnitude for a decoded double.
///
/// Values above this threshold are more likely to result from a
/// misaligned byte range than from a real measurement. A control
/// system value of 10^12 is possible but extremely rare, while
/// misaligned ranges routinely produce values around 10^50.
const PLAUSIBLE_F64_MAX: f64 = 1e12;

/// Returns all `width`-byte windows that overlap the run and fit the message.
///
/// A window need not contain the entire run. Consider a variable-length
/// string immediately followed by a timestamp, as in the reference
/// catalogue. The changing string bytes and the timestamp's changing
/// low byte form one continuous run, potentially wider than either field.
/// Requiring full containment would reject every candidate and leave
/// the timestamp hidden in an opaque slot.
///
/// Instead, propose overlapping windows and let validation distinguish
/// them using plausible values, monotonic timestamps, adjacency to
/// accepted seconds, and other evidence. Windows containing the entire
/// run are tried first so the common case remains unchanged.
fn windows_around(run: &Range<usize>, width: usize, len: usize) -> Vec<Range<usize>> {
    let lowest = run.start.saturating_sub(width.saturating_sub(1));
    let highest = run.end.saturating_sub(1);
    let mut out: Vec<Range<usize>> = (lowest..=highest)
        .filter(|s| s + width <= len)
        .map(|s| s..s + width)
        .collect();
    // Try containing windows first, then sort by position for deterministic results.
    out.sort_by_key(|w| {
        let contains = w.start <= run.start && w.end >= run.end;
        (!contains, w.start)
    });
    out
}

fn moves<T: PartialEq>(values: &[T]) -> bool {
    values.windows(2).any(|p| p[1] != p[0])
}

/// Windows that decode as POSIX seconds in every observation.
///
/// Every condition must hold. A plausible calendar range can be met by
/// chance, and so can monotonicity: an int64 counter placed in the high
/// byte of a window shifted three bytes to its left reads as a date that
/// climbs by 194 days per update. A clock does not. It advances by seconds,
/// so its low byte changes whenever it moves, and over a calibration it
/// covers minutes, not months.
fn seconds_proposals(run: &Range<usize>, obs: &[Vec<u8>], len: usize) -> Vec<Proposal> {
    windows_around(run, 8, len)
        .into_iter()
        .filter(|w| {
            let v: Vec<i64> = obs.iter().map(|o| le_i64(&o[w.clone()])).collect();
            let low: Vec<u8> = obs.iter().map(|o| o[w.start]).collect();
            v.iter().all(|x| PLAUSIBLE_EPOCH.contains(x))
                && v.windows(2).all(|p| p[1] >= p[0])
                && moves(&v)
                && moves(&low)
                && v.last()
                    .zip(v.first())
                    .is_some_and(|(l, f)| l - f <= MAX_SECONDS_SPAN)
        })
        .map(|range| Proposal {
            range,
            kind: Kind::TimestampSeconds,
        })
        .collect()
}

/// Windows that decode as sub-second nanoseconds immediately after an
/// accepted seconds field.
///
/// Adjacency is essential to distinguish timestamps from arbitrary data.
/// Any three or four changing bytes may decode to a value below one billion,
/// so the range check alone produces false positives. In a `time_t`
/// representation, nanoseconds immediately follow seconds. Requiring this
/// adjacency rejects unrelated fields without weakening valid matches.
fn nanos_proposals(
    run: &Range<usize>,
    obs: &[Vec<u8>],
    len: usize,
    seconds_ends: &[usize],
) -> Vec<Proposal> {
    windows_around(run, 4, len)
        .into_iter()
        .filter(|w| seconds_ends.contains(&w.start))
        .filter(|w| {
            let v: Vec<u32> = obs.iter().map(|o| le_u32(&o[w.clone()])).collect();
            v.iter().all(|x| *x < 1_000_000_000) && moves(&v)
        })
        .map(|range| Proposal {
            range,
            kind: Kind::TimestampNanos,
        })
        .collect()
}

/// Windows that decode as plausible doubles in every observation.
///
/// The window starting at the run's first byte is tried first. A double's
/// low-order bytes usually change before its high-order bytes, so a run
/// often starts at the field but ends before its exponent bytes.
fn f64_proposals(run: &Range<usize>, obs: &[Vec<u8>], len: usize) -> Vec<Proposal> {
    let mut out: Vec<Range<usize>> = windows_around(run, 8, len)
        .into_iter()
        .filter(|w| {
            let v: Vec<f64> = obs.iter().map(|o| le_f64(&o[w.clone()])).collect();
            v.iter()
                .all(|x| x.is_finite() && (*x == 0.0 || x.abs() < PLAUSIBLE_F64_MAX))
                && moves(&v)
        })
        .collect();
    out.sort_by_key(|w| (w.start != run.start, w.start));
    out.into_iter()
        .map(|range| Proposal {
            range,
            kind: Kind::F64,
        })
        .collect()
}

/// Accept proposals that overlap a changing byte and do not overlap
/// any previously accepted proposal.
fn accept(proposals: Vec<Proposal>, mask: &[bool], taken: &mut [bool], chosen: &mut Vec<Proposal>) {
    for p in proposals {
        if p.range.clone().any(|i| taken[i]) {
            continue;
        }
        // Reject expansions that cover only constant bytes, leaving them
        // available for proposals associated with other runs.
        if !p.range.clone().any(|i| mask[i]) {
            continue;
        }
        for i in p.range.clone() {
            taken[i] = true;
        }
        chosen.push(p);
    }
}

/// Infer variable slots from successive observations of a message.
///
/// Observations must contain the complete message body covered by the
/// template, in order, and all must have the same length. Different
/// lengths indicate variable-length fields, which cannot be analysed
/// reliably and are rejected.
pub fn infer(observations: &[Vec<u8>]) -> Result<Vec<InferredSlot>, DiffError> {
    let mask = changed_mask(observations)?;
    let len = mask.len();
    let runs = runs(&mask);

    let mut taken = vec![false; len];
    let mut chosen: Vec<Proposal> = Vec::new();

    // Apply classifications in order of evidence strength. Each stage
    // respects ranges already accepted. This lets nanoseconds require an
    // adjacent seconds field and prevents doubles from swallowing timestamps.
    let seconds: Vec<Proposal> = runs
        .iter()
        .flat_map(|r| seconds_proposals(r, observations, len))
        .collect();
    accept(seconds, &mask, &mut taken, &mut chosen);
    let seconds_ends: Vec<usize> = chosen.iter().map(|p| p.range.end).collect();

    let nanos: Vec<Proposal> = runs
        .iter()
        .flat_map(|r| nanos_proposals(r, observations, len, &seconds_ends))
        .collect();
    accept(nanos, &mask, &mut taken, &mut chosen);

    let doubles: Vec<Proposal> = runs
        .iter()
        .flat_map(|r| f64_proposals(r, observations, len))
        .collect();
    accept(doubles, &mask, &mut taken, &mut chosen);

    // Keep all remaining changing bytes as opaque slots. They make no
    // assumptions about the data and can always replay the observed bytes.
    let leftovers: Vec<Proposal> = runs
        .iter()
        .flat_map(|r| {
            let mut out = Vec::new();
            let mut start = None;
            for i in r.clone() {
                match (taken[i], start) {
                    (false, None) => start = Some(i),
                    (true, Some(s)) => {
                        out.push(s..i);
                        start = None;
                    }
                    _ => {}
                }
            }
            if let Some(s) = start {
                out.push(s..r.end);
            }
            out.into_iter().map(|range| Proposal {
                range,
                kind: Kind::Opaque,
            })
        })
        .collect();
    accept(leftovers, &mask, &mut taken, &mut chosen);

    chosen.sort_by_key(|p| p.range.start);
    Ok(chosen
        .into_iter()
        .map(|p| InferredSlot {
            samples: observations
                .iter()
                .map(|o| o[p.range.clone()].to_vec())
                .collect(),
            range: p.range,
            kind: p.kind,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seven consecutive monitor updates for `MIM:F64:0`, captured from
    /// softIocPVX (PVXS, EPICS 7.0.8.1) using tcpdump.
    ///
    /// These are the message bodies covered by the template, starting
    /// after the request ID and sub-command byte. The layout below is
    /// provided for reference only; the inference algorithm knows nothing
    /// about these fields.
    ///
    /// ```text
    ///  0..3   changed-set bitset: size 02, then 82 03 (bits 1, 7, 8, 9)
    ///  3..11  value, double
    /// 11..19  timeStamp.secondsPastEpoch, int64, POSIX epoch
    /// 19..23  timeStamp.nanoseconds, int32
    /// 23..27  timeStamp.userTag, int32, always zero
    /// 27..28  overrun bitset, empty
    /// ```
    fn real_capture() -> Vec<Vec<u8>> {
        [
            "028203 4e2f1da0a7ca5840 e91f866a00000000 d3236009 0000000000",
            "028203 24b95bf59f585840 ea1f866a00000000 9bb86009 0000000000",
            "028203 047c4c1052a85740 eb1f866a00000000 20596309 0000000000",
            "028203 d7d473e780bb5640 ec1f866a00000000 476e5f09 0000000000",
            "028203 d4f1ef398a945540 ed1f866a00000000 15c75f09 0000000000",
            "028203 fe770d8260365440 ee1f866a00000000 6c4f6009 0000000000",
            "028203 d4e46c6b83a45240 ef1f866a00000000 a3166109 0000000000",
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
    fn the_capture_fixture_is_what_the_capture_showed() {
        let obs = real_capture();
        assert_eq!(obs.len(), 7);
        assert!(obs.iter().all(|o| o.len() == 28));
        // The full frame was 41 bytes: 8-byte header, 4-byte request ID,
        // 1-byte sub-command, and 28-byte body.
        assert_eq!(8 + 4 + 1 + 28, 41);
    }

    // ----------------------------------------------------------- masking

    #[test]
    fn a_constant_message_has_no_changed_bytes() {
        let obs = vec![vec![1, 2, 3], vec![1, 2, 3]];
        assert_eq!(changed_mask(&obs).unwrap(), vec![false, false, false]);
    }

    #[test]
    fn one_observation_is_not_enough_to_compare() {
        assert_eq!(
            changed_mask(&[vec![1, 2, 3]]),
            Err(DiffError::TooFew { given: 1 })
        );
    }

    #[test]
    fn a_length_change_is_refused_rather_than_approximated() {
        // A variable-length field shifts all subsequent bytes. Byte-position
        // analysis would produce a template valid for only one message length.
        let obs = vec![vec![1, 2, 3], vec![1, 2, 3, 4]];
        assert_eq!(
            changed_mask(&obs),
            Err(DiffError::LengthVaries { first: 3, other: 4 })
        );
    }

    #[test]
    fn a_byte_that_changes_in_any_pair_is_marked() {
        // Compare against the first observation so changes occurring later
        // in the observation window are still detected.
        let obs = vec![vec![0, 0], vec![0, 0], vec![0, 9]];
        assert_eq!(changed_mask(&obs).unwrap(), vec![false, true]);
    }

    // ---------------------------------------------------- real bytes

    #[test]
    fn the_bitset_and_the_tail_stay_in_the_template() {
        // The changed-set bitset is identical across all updates, as are
        // userTag and the overrun bitset. Constant bytes remain in the
        // fixed template rather than becoming variable slots.
        let mask = changed_mask(&real_capture()).unwrap();
        assert_eq!(&mask[0..3], &[false, false, false], "changed-set bitset");
        assert_eq!(&mask[23..28], &[false; 5], "userTag and overrun bitset");
    }

    #[test]
    fn the_value_is_recognised_as_a_double_despite_its_constant_high_byte() {
        // Byte 10 is 0x40 in all seven observations because the value stays between 64 and 128.
        // A plain diff stops the run at byte 9, leaving the sign and exponent in the template.
        // That works for this window but fails as soon as the value becomes negative.
        let obs = real_capture();
        let mask = changed_mask(&obs).unwrap();
        assert!(!mask[10], "byte 10 really is constant in this window");

        let slots = infer(&obs).unwrap();
        let value = slots
            .iter()
            .find(|s| s.kind == Kind::F64)
            .expect("no double found");
        assert_eq!(
            value.range,
            3..11,
            "the whole double, not just its low bytes"
        );
    }

    #[test]
    fn the_seconds_field_is_recognised_from_a_single_moving_byte() {
        // Only byte 11 changes because the timestamp advances by one second
        // per update. The upper seven bytes remain constant. The proposal is
        // accepted because every observation decodes to a plausible date
        // and the timestamps never decrease.
        let obs = real_capture();
        let mask = changed_mask(&obs).unwrap();
        assert_eq!(
            mask[11..19],
            [true, false, false, false, false, false, false, false]
        );

        let slots = infer(&obs).unwrap();
        let secs = slots
            .iter()
            .find(|s| s.kind == Kind::TimestampSeconds)
            .expect("no timestamp found");
        assert_eq!(secs.range, 11..19);

        let first = le_i64(&secs.samples[0]);
        assert_eq!(first, 1_787_174_889, "POSIX seconds, not EPICS seconds");
        for (k, s) in secs.samples.iter().enumerate() {
            assert_eq!(le_i64(s), first + k as i64, "one second per update");
        }
    }

    #[test]
    fn the_nanoseconds_field_is_recognised_and_not_shifted() {
        // The changing run covers bytes 19..22, while byte 22 remains 0x09.
        // Two 4-byte windows overlap the run, but only one decodes to a value
        // below one billion in every observation.
        let obs = real_capture();
        let slots = infer(&obs).unwrap();
        let nanos = slots
            .iter()
            .find(|s| s.kind == Kind::TimestampNanos)
            .expect("no nanoseconds found");
        assert_eq!(nanos.range, 19..23);
        assert!(nanos.samples.iter().all(|s| le_u32(s) < 1_000_000_000));
    }

    /// Monitor updates of an NTScalar int64 whose value counts up from 101,
    /// laid out as softIocPVX sends them: the changed-set, the value, the
    /// seconds and nanoseconds of the time stamp, then the user tag and the
    /// overrun bitset.
    fn int64_counter(first: u8, count: u8) -> Vec<Vec<u8>> {
        (0..count)
            .map(|k| {
                let mut b = vec![0x02, 0x82, 0x03];
                b.extend_from_slice(&i64::from(first + k).to_le_bytes());
                b.extend_from_slice(&(1_795_353_484 + i64::from(k)).to_le_bytes());
                b.extend_from_slice(&(100_000_000 + u32::from(k) * 7).to_le_bytes());
                b.extend_from_slice(&[0; 5]);
                b
            })
            .collect()
    }

    #[test]
    fn a_counter_beside_the_changed_set_is_not_a_clock() {
        let slots = infer(&int64_counter(101, 36)).unwrap();
        assert!(
            slots
                .iter()
                .all(|s| s.kind != Kind::TimestampSeconds || s.range == (11..19)),
            "{:?}",
            slots
                .iter()
                .map(|s| (s.range.clone(), s.kind))
                .collect::<Vec<_>>()
        );
        assert!(
            slots
                .iter()
                .any(|s| s.kind == Kind::TimestampSeconds && s.range == (11..19))
        );
        assert!(
            slots.iter().all(|s| s.range.start >= 3),
            "the changed-set never moves"
        );
    }

    #[test]
    fn the_real_capture_yields_exactly_three_slots() {
        let slots = infer(&real_capture()).unwrap();
        assert_eq!(
            slots
                .iter()
                .map(|s| (s.range.clone(), s.kind))
                .collect::<Vec<_>>(),
            vec![
                (3..11, Kind::F64),
                (11..19, Kind::TimestampSeconds),
                (19..23, Kind::TimestampNanos),
            ]
        );
    }

    #[test]
    fn slots_do_not_overlap_and_cover_every_moving_byte() {
        // Every changing byte must belong to a slot, and no byte may belong
        // to more than one slot. Otherwise, the template could lose data or
        // contain overlapping variable regions.
        let obs = real_capture();
        let mask = changed_mask(&obs).unwrap();
        let slots = infer(&obs).unwrap();

        let mut covered = vec![0u8; mask.len()];
        for s in &slots {
            for i in s.range.clone() {
                covered[i] += 1;
            }
        }
        assert!(covered.iter().all(|c| *c <= 1), "slots overlap");
        for (i, changed) in mask.iter().enumerate() {
            if *changed {
                assert_eq!(covered[i], 1, "moving byte {i} left in the template");
            }
        }
    }

    #[test]
    fn samples_stay_aligned_with_the_observations() {
        // Replaying sample k from every slot must reproduce observation k
        // exactly. This guarantees byte-for-byte fidelity even when slot
        // boundaries are not inferred perfectly.
        let obs = real_capture();
        let slots = infer(&obs).unwrap();
        for (k, original) in obs.iter().enumerate() {
            let mut rebuilt = original.clone();
            // Clear the slot bytes, then restore them from sample k.
            for s in &slots {
                for i in s.range.clone() {
                    rebuilt[i] = 0xAA;
                }
            }
            for s in &slots {
                rebuilt[s.range.clone()].copy_from_slice(&s.samples[k]);
            }
            assert_eq!(&rebuilt, original, "observation {k} did not round-trip");
        }
    }

    #[test]
    fn inference_is_deterministic() {
        // Repeated inference on the same observations must produce identical
        // slots so that downstream template generation remains reproducible.
        let obs = real_capture();
        assert_eq!(infer(&obs).unwrap(), infer(&obs).unwrap());
    }

    // ------------------------------------------------- synthetic cases

    #[test]
    fn a_run_no_proposal_fits_stays_opaque() {
        // Three changing bytes with no recognizable interpretation.
        //
        // This case exposed a false positive: any three or four changing
        // bytes with a small high byte can decode to a value below one billion.
        // The range check alone therefore misclassified arbitrary data as
        // nanoseconds. Requiring an adjacent, accepted seconds field fixes
        // this using the actual layout of `time_t`, rather than an arbitrary
        // threshold.
        let obs = vec![
            vec![0x01, 0xDE, 0xAD, 0xBE, 0x01],
            vec![0x01, 0xCA, 0xFE, 0xBA, 0x01],
            vec![0x01, 0xF0, 0x0D, 0xAA, 0x01],
        ];
        let slots = infer(&obs).unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].kind, Kind::Opaque);
        assert_eq!(slots[0].range, 1..4);
    }

    #[test]
    fn an_absurd_magnitude_is_not_accepted_as_a_double() {
        // A misaligned 8-byte window decodes to a finite but enormous value.
        // Accepting it would identify the wrong bytes as a double and allow
        // --noise to overwrite a neighboring field.
        let mut obs = Vec::new();
        for k in 0..4u8 {
            let mut m = vec![0u8; 16];
            // Two changing bytes surrounded by a pattern that cannot represent
            // a plausible double in any overlapping 8-byte window.
            m[4] = k;
            m[5] = 0xCA;
            m[6] = 0xFF;
            m[7] = 0xFF;
            m[8] = 0xFF;
            m[9] = 0xFF;
            m[10] = 0xFF;
            m[11] = 0x7F;
            obs.push(m);
        }
        let slots = infer(&obs).unwrap();
        assert!(
            slots.iter().all(|s| s.kind != Kind::F64),
            "an implausible magnitude was accepted as a value: {slots:?}"
        );
    }

    #[test]
    fn a_timestamp_that_goes_backwards_is_not_a_timestamp() {
        // Monotonicity distinguishes a clock from an arbitrary integer
        // that happens to fall within the plausible timestamp range.
        let mut obs = Vec::new();
        for secs in [1_786_993_129i64, 1_786_993_130, 1_700_000_000] {
            let mut m = vec![0u8; 8];
            m.copy_from_slice(&secs.to_le_bytes());
            obs.push(m);
        }
        let slots = infer(&obs).unwrap();
        assert!(slots.iter().all(|s| s.kind != Kind::TimestampSeconds));
    }

    #[test]
    fn a_constant_message_produces_no_slots() {
        // A static channel has no changing bytes, so the entire message
        // remains in the template. This is a valid result, not an error.
        let obs = vec![vec![1, 2, 3, 4]; 5];
        assert!(infer(&obs).unwrap().is_empty());
    }

    #[test]
    fn every_moving_byte_is_covered_even_in_a_pathological_message() {
        // Changes are scattered across individual bytes. Regardless of
        // which proposals are accepted, every changing byte must belong
        // to a slot rather than remain in the fixed template.
        let mut obs = Vec::new();
        for k in 0..5u8 {
            let mut m = vec![0u8; 32];
            for (i, b) in m.iter_mut().enumerate() {
                if i % 5 == 0 {
                    *b = k.wrapping_mul(i as u8).wrapping_add(1);
                }
            }
            obs.push(m);
        }
        let mask = changed_mask(&obs).unwrap();
        let slots = infer(&obs).unwrap();
        for (i, changed) in mask.iter().enumerate() {
            if *changed {
                assert!(
                    slots.iter().any(|s| s.range.contains(&i)),
                    "moving byte {i} was left in the template"
                );
            }
        }
    }
}
