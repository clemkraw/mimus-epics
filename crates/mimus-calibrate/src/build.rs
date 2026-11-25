//! Turns observations into a template that the emitter can serve.
//!
//! [`crate::layout::layout`] produces an ordered list of pieces: bytes that remain
//! constant and slots that change. This module serializes them into the
//! profile format, attaches the type descriptor captured at monitor INIT,
//! and assigns the resulting profile a name.
//!
//! Two design decisions are worth explaining.
//!
//! **Every slot retains its samples, including classified slots.** A slot
//! recognized as a double is populated by a signal function during emission,
//! so its samples are not replayed. However, they allow the shadow check to
//! reproduce observations exactly and let `mimus inspect` show the original
//! field contents. Discarding them would save a few kilobytes but remove the
//! ability to verify fidelity.
//!
//! **Names come from the type descriptor, not the record.** An `ai` and a
//! `calc` produce identical bytes, so distinguishing them would add noise.
//! Names derived from the wire format also remain stable across
//! recalibrations. A `--mix ntscalar_f64:70` written today therefore keeps
//! the same meaning after a PVXS upgrade.

use mimus_profile::{
    Introspection, ScalarType, Slot, SlotRole, SlotWidth, Template, TemplateError,
};

use crate::diff::Kind;
use crate::layout::Piece;

/// PVA command code for monitor messages.
const MONITOR: u8 = 0x0D;

/// Explains why a template could not be built from the observations.
///
/// `Eq` is not derived because `Invalid` contains a `TemplateError` that
/// includes floating-point values through its reported slot widths.
#[derive(Debug, Clone, PartialEq)]
pub enum BuildError {
    /// No monitor INIT was captured, so the message bodies have no descriptor.
    ///
    /// Without a descriptor, consumers cannot interpret updates. Returning
    /// nothing is safer than sending messages they cannot understand.
    NoIntrospection { pv: String },
    /// A slot has neither a recognized type nor samples to replay.
    ///
    /// This should never occur with [`crate::layout::layout`], which always preserves
    /// samples. The check prevents unfillable slots from being silently
    /// populated with zeroes.
    UnfillableSlot { pv: String },
    /// The completed template failed the profile format's validation.
    Invalid { pv: String, source: TemplateError },
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoIntrospection { pv } => write!(
                f,
                "{pv}: no type descriptor was captured; updates without one \
                 cannot be interpreted by any consumer"
            ),
            Self::UnfillableSlot { pv } => {
                write!(f, "{pv}: a slot has neither a classification nor samples")
            }
            Self::Invalid { pv, source } => write!(f, "{pv}: {source}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// Data contributed by a channel, ready to be written to a profile.
#[derive(Debug, Clone)]
pub struct Observed<'a> {
    pub pv: &'a str,
    pub pieces: &'a [Piece],
    /// Monitor INIT message containing the complete type descriptor.
    pub introspection_full: Option<&'a [u8]>,
    /// Monitor INIT message containing only a cached type identifier.
    pub introspection_id_only: Option<&'a [u8]>,
    /// Number of distinct message bodies used to build the pieces.
    pub observations: u32,
}

fn role_for(kind: Kind, width: &SlotWidth) -> SlotRole {
    match kind {
        Kind::TimestampSeconds => SlotRole::TimestampSeconds,
        Kind::TimestampNanos => SlotRole::TimestampNanos,
        // Only treat an F64 slot as a value when its width is exactly
        // eight bytes. Otherwise, keep it unclassified so it can be
        // replayed from samples instead of risking an out-of-bounds write.
        Kind::F64 if matches!(width, SlotWidth::Fixed { bytes: 8 }) => SlotRole::Value {
            scalar: ScalarType::F64,
        },
        _ => SlotRole::Unclassified,
    }
}

fn width_for(samples: &[Vec<u8>]) -> SlotWidth {
    let min = samples.iter().map(Vec::len).min().unwrap_or(0);
    let max = samples.iter().map(Vec::len).max().unwrap_or(0);
    if min == max {
        SlotWidth::Fixed { bytes: max }
    } else {
        SlotWidth::Variable { min, max }
    }
}

/// Builds a template from one channel's observations.
pub fn template(obs: &Observed<'_>, name: impl Into<String>) -> Result<Template, BuildError> {
    let full = obs
        .introspection_full
        .ok_or_else(|| BuildError::NoIntrospection {
            pv: obs.pv.to_string(),
        })?;

    let mut t = Template::new(name, MONITOR);
    t.with_introspection(Introspection {
        full: full.to_vec(),
        id_only: obs.introspection_id_only.map(<[u8]>::to_vec),
    });

    let mut next_id: u16 = 1;
    for piece in obs.pieces {
        match piece {
            Piece::Fixed(bytes) => {
                t.push_fixed(bytes.clone());
            }
            Piece::Slot(slot) => {
                let width = width_for(&slot.samples);
                let role = role_for(slot.kind, &width);
                if slot.samples.is_empty() && matches!(role, SlotRole::Unclassified) {
                    return Err(BuildError::UnfillableSlot {
                        pv: obs.pv.to_string(),
                    });
                }
                // Retain samples for every slot, including classified ones.
                // The shadow check uses them for exact comparison, and
                // `mimus inspect` uses them to display the original values.
                t.push_slot(
                    Slot::new(next_id, width)
                        .with_role(role)
                        .with_samples(slot.samples.clone()),
                );
                next_id += 1;
            }
        }
    }
    t.observations = obs.observations;
    Ok(t)
}

/// What [`keep_samples`] kept of a template's samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kept {
    /// Observations whose samples are kept: the first ones.
    pub kept: usize,
    /// Observations the samples came from.
    pub of: usize,
    /// Bytes of samples each observation takes.
    pub per_observation: usize,
}

/// Keeps no more than `budget` bytes of a template's samples: the samples
/// of its first observations, at least two.
///
/// A template replays the updates it was learned from, so it carries them:
/// thirty updates of a scalar are a few hundred bytes, thirty frames of an
/// image thirty images. Past the budget, every slot keeps the samples of the
/// same first observations. Sample `k` of every slot still comes from
/// observation `k`, so each update kept is still reproduced byte for byte:
/// what is replayed stays real, there is only less of it.
///
/// Returns what was kept, or `None` when everything fits.
pub fn keep_samples(t: &mut Template, budget: usize) -> Option<Kept> {
    let of = t.slots.iter().map(|s| s.samples.len()).max().unwrap_or(0);
    let per_observation: usize = t
        .slots
        .iter()
        .map(|s| s.samples.iter().map(Vec::len).max().unwrap_or(0))
        .sum();
    if per_observation == 0 {
        return None;
    }
    let kept = (budget / per_observation).max(2);
    if kept >= of {
        return None;
    }
    for s in &mut t.slots {
        s.samples.truncate(kept);
    }
    Some(Kept {
        kept,
        of,
        per_observation,
    })
}

/// Extracts the type identifier from a full introspection descriptor, if present.
///
/// A full descriptor starts with an OK status, followed by an optional
/// registry marker and ID, a structure code, and a size-prefixed type name:
///
/// ```text
///   FF FD <id:u16> 80 <size> "epics:nt/NTScalar:1.0" ...
/// ```
///
/// Only the type name is read, to name the template. The rest of the
/// descriptor is preserved verbatim and never interpreted. MIMUS does
/// not need to understand types such as `NTScalar`.
fn type_id_of(full: &[u8]) -> Option<&str> {
    const STRUCTURE: u8 = 0x80;
    const FULL_WITH_ID: u8 = 0xFD;

    // Skip the status. A bare OK uses one byte. Other statuses contain a type byte followed by
    // two size-prefixed fields: a message and a call tree. Either field may be empty.
    let mut i = match full.first()? {
        0xFF => 1,
        _ => {
            let mut j = 1usize;
            for _ in 0..2 {
                let size = *full.get(j)? as usize;
                j += 1;
                if size < 254 {
                    j += size;
                } else {
                    return None;
                }
            }
            j
        }
    };

    // The registry ID is optional. PVXS can encode the structure
    // directly, for example `ff 80 15 "epics:nt/NTScalar:1.0"`.
    // Skipping this form would leave every template with the generic
    // name `type`, causing unrelated templates to collide.
    if *full.get(i)? == FULL_WITH_ID {
        i += 3;
    }
    if *full.get(i)? != STRUCTURE {
        return None;
    }
    i += 1;

    let size = *full.get(i)? as usize;
    if size >= 254 {
        return None;
    }
    i += 1;
    std::str::from_utf8(full.get(i..i + size)?).ok()
}

/// Returns the type code of the `value` field, if present.
///
/// Descriptor members are encoded as a size-prefixed name followed by
/// a type code. The byte immediately after `value` identifies the field
/// type, for example distinguishing an NTScalar of doubles from one
/// of strings. No other descriptor fields are interpreted.
fn value_type_code(full: &[u8]) -> Option<u8> {
    // Find the length-prefixed field name `value` and read its type code.
    // This deliberately avoids parsing the descriptor: doing so would
    // require knowledge of normative types, which MIMUS does not need.
    full.windows(7)
        .find(|w| w[0] == 5 && &w[1..6] == b"value")
        .map(|w| w[6])
}

/// Returns a short, stable name for a PVA type code.
fn code_name(code: u8) -> String {
    const ARRAY: u8 = 0x08;
    let base = match code & !ARRAY {
        0x00 => "bool",
        0x20 => "i8",
        0x21 => "i16",
        0x22 => "i32",
        0x23 => "i64",
        0x24 => "u8",
        0x25 => "u16",
        0x26 => "u32",
        0x27 => "u64",
        0x42 => "f32",
        0x43 => "f64",
        0x60 => "string",
        0x80 => "struct",
        0x81 => "union",
        0x82 => "any",
        _ => return format!("t{code:02x}"),
    };
    if code & ARRAY != 0 && code & 0xE0 != 0x80 {
        format!("{base}a")
    } else {
        base.to_string()
    }
}

/// Returns a readable, stable label derived from the type descriptor.
///
/// For example, `epics:nt/NTScalar:1.0` with a `value` field of type
/// double becomes `ntscalar_f64`.
///
/// Both parts come from the descriptor, which defines the type. Deriving
/// the suffix from inferred slots made names depend on channel activity
/// during calibration. The same IOC could then produce `ntscalar_f64_2`
/// or `ntscalar` across runs, breaking references such as
/// `--mix ntscalar_f64:70`. Descriptor-based naming keeps them stable.
pub fn base_name(full: &[u8], _pieces: &[Piece]) -> String {
    let base = type_id_of(full)
        .map(|id| {
            id.rsplit('/')
                .next()
                .unwrap_or(id)
                .split(':')
                .next()
                .unwrap_or(id)
                .to_ascii_lowercase()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "type".to_string());

    match value_type_code(full) {
        Some(code) => format!("{base}_{}", code_name(code)),
        None => base,
    }
}

/// Assigns unique names to types.
///
/// Different types can produce the same label, such as `NTScalarArray`
/// types with different element types. Rather than guessing, the second
/// occurrence gets a numeric suffix. `mimus inspect` provides the details
/// needed to distinguish them. Names are assigned in first-seen order,
/// making the result deterministic for a given channel list.
#[derive(Debug, Default)]
pub struct Namer {
    taken: Vec<String>,
}

impl Namer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn assign(&mut self, base: &str) -> String {
        if !self.taken.iter().any(|t| t == base) {
            self.taken.push(base.to_string());
            return base.to_string();
        }
        for n in 2.. {
            let candidate = format!("{base}_{n}");
            if !self.taken.contains(&candidate) {
                self.taken.push(candidate.clone());
                return candidate;
            }
        }
        unreachable!("an unbounded search cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout;
    use mimus_profile::Chunk;

    /// Builds a monitor INIT descriptor for an NTScalar double, using
    /// PVXS's inline format: OK status, structure code, then type name.
    fn inline_intro() -> Vec<u8> {
        let name = b"epics:nt/NTScalar:1.0";
        let mut v = vec![0xFF, 0x80, name.len() as u8];
        v.extend_from_slice(name);
        v.extend_from_slice(&[0x03, 0x05, b'v', b'a', b'l', b'u', b'e', 0x43]);
        v
    }

    fn ntscalar_intro(type_id: u16) -> Vec<u8> {
        let name = b"epics:nt/NTScalar:1.0";
        let mut v = vec![0xFF, 0xFD];
        v.extend_from_slice(&type_id.to_le_bytes());
        v.push(0x80);
        v.push(name.len() as u8);
        v.extend_from_slice(name);
        // The remaining descriptor bytes are preserved verbatim and are not interpreted.
        v.extend_from_slice(&[0x03, 0x05, b'v', b'a', b'l', b'u', b'e', 0x43]);
        v
    }

    fn cached_intro(type_id: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xFE];
        v.extend_from_slice(&type_id.to_le_bytes());
        v
    }

    /// Returns four consecutive monitor updates from the softIocPVX capture.
    fn capture() -> Vec<Vec<u8>> {
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

    fn built() -> (Vec<Piece>, Vec<u8>, Template) {
        let obs = capture();
        let pieces = layout::layout(&obs).unwrap();
        let full = ntscalar_intro(1);
        let cached = cached_intro(1);
        let t = template(
            &Observed {
                pv: "MIM:F64:0",
                pieces: &pieces,
                introspection_full: Some(&full),
                introspection_id_only: Some(&cached),
                observations: obs.len() as u32,
            },
            "ntscalar_f64",
        )
        .unwrap();
        (pieces, full, t)
    }

    #[test]
    fn the_capture_becomes_a_valid_template() {
        let (_, _, t) = built();
        t.validate(4).expect("the profile format rejected it");
        assert_eq!(t.command, MONITOR);
        assert_eq!(t.observations, 4);
    }

    #[test]
    fn the_roles_match_what_was_recognized() {
        let (_, _, t) = built();
        let roles: Vec<SlotRole> = t.slots.iter().map(|s| s.role).collect();
        assert_eq!(
            roles,
            vec![
                SlotRole::Value {
                    scalar: ScalarType::F64
                },
                SlotRole::TimestampSeconds,
                SlotRole::TimestampNanos,
            ]
        );
    }

    #[test]
    fn the_fixed_parts_are_the_bytes_that_never_moved() {
        // The changed-set at the front and the userTag with its overrun
        // bitset at the back are constant, so they belong in the template.
        let (_, _, t) = built();
        let fixed: Vec<Vec<u8>> = t
            .chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Fixed { bytes } => Some(bytes.clone()),
                Chunk::Slot { .. } => None,
            })
            .collect();
        assert_eq!(fixed[0], vec![0x02, 0x82, 0x03], "changed-set");
        assert_eq!(fixed[1], vec![0u8; 5], "userTag and overrun bitset");
    }

    #[test]
    fn classified_slots_keep_their_samples() {
        // Classified slots are filled from signals during emission, not
        // replayed. Their samples are still needed for shadow checks and
        // for displaying the original values in `mimus inspect`.
        let (_, _, t) = built();
        for s in &t.slots {
            assert_eq!(s.samples.len(), 4, "slot {} lost its samples", s.id);
        }
    }

    #[test]
    fn the_template_reproduces_the_observations() {
        // Rebuilding each observation with the corresponding sample from
        // every slot must reproduce the original bytes exactly.
        let (pieces, _, _) = built();
        let obs = capture();
        for (k, original) in obs.iter().enumerate() {
            assert_eq!(&layout::rebuild(&pieces, k), original, "observation {k}");
        }
    }

    #[test]
    fn both_descriptor_forms_are_carried() {
        let (_, _, t) = built();
        let intro = t.introspection.expect("no descriptor");
        assert_eq!(intro.full[1], 0xFD, "full descriptor");
        let cached = intro.id_only.expect("no cached form");
        assert_eq!(cached[1], 0xFE);
        assert_eq!(&cached[2..4], &intro.full[2..4], "same type id");
    }

    #[test]
    fn a_template_without_a_descriptor_is_refused() {
        // Without a descriptor, consumers cannot interpret the updates.
        // Sending them would create traffic without producing usable data.
        let obs = capture();
        let pieces = layout::layout(&obs).unwrap();
        assert_eq!(
            template(
                &Observed {
                    pv: "MIM:F64:0",
                    pieces: &pieces,
                    introspection_full: None,
                    introspection_id_only: None,
                    observations: 4,
                },
                "x"
            ),
            Err(BuildError::NoIntrospection {
                pv: "MIM:F64:0".into()
            })
        );
    }

    #[test]
    fn a_missing_cached_form_stays_missing_rather_than_becoming_empty() {
        // Absence means the study did not capture a cached descriptor for
        // this type. Preserve that information rather than substituting
        // an empty descriptor that consumers cannot resolve.
        let obs = capture();
        let pieces = layout::layout(&obs).unwrap();
        let full = ntscalar_intro(1);
        let t = template(
            &Observed {
                pv: "MIM:F64:0",
                pieces: &pieces,
                introspection_full: Some(&full),
                introspection_id_only: None,
                observations: 4,
            },
            "x",
        )
        .unwrap();
        assert!(t.introspection.unwrap().id_only.is_none());
    }

    #[test]
    fn a_variable_width_slot_is_recorded_as_variable() {
        let samples = vec![vec![1u8, 2, 3], vec![1, 2], vec![1, 2, 3, 4]];
        assert_eq!(width_for(&samples), SlotWidth::Variable { min: 2, max: 4 });
        assert_eq!(width_for(&[vec![1u8, 2]]), SlotWidth::Fixed { bytes: 2 });
    }

    #[test]
    fn a_double_of_the_wrong_width_is_demoted_rather_than_trusted() {
        // This case should not occur with `layout`, but the check prevents a signal function
        // from writing eight bytes into a four-byte slot and corrupting the next field.
        // Unclassified slots can still be emitted by replaying their samples.
        let width = SlotWidth::Fixed { bytes: 4 };
        assert_eq!(role_for(Kind::F64, &width), SlotRole::Unclassified);
        assert_eq!(
            role_for(Kind::F64, &SlotWidth::Fixed { bytes: 8 }),
            SlotRole::Value {
                scalar: ScalarType::F64
            }
        );
    }

    // -------------------------------------------------------------- names

    #[test]
    fn a_name_is_derived_from_the_type_descriptor() {
        let obs = capture();
        let pieces = layout::layout(&obs).unwrap();
        assert_eq!(base_name(&ntscalar_intro(1), &pieces), "ntscalar_f64");
    }

    #[test]
    fn the_name_does_not_depend_on_what_the_analysis_found() {
        // The same IOC type received different names across calibrations,
        // such as `ntscalar_f64_2` and `ntscalar`, because the suffix
        // depended on which slot was classified. This made mix definitions
        // refer to different populations across profiles.
        let full = ntscalar_intro(1);
        let rich = layout::layout(&capture()).unwrap();
        let bare: Vec<Piece> = Vec::new();
        assert_eq!(base_name(&full, &rich), base_name(&full, &bare));
    }

    #[test]
    fn the_value_type_code_distinguishes_types_sharing_a_normative_name() {
        // NTScalarArray types can share a normative name but have different
        // value element types. Derive the suffix from the value field, not
        // from a neighboring field such as the timestamp.
        let named = |code: u8| {
            let name = b"epics:nt/NTScalarArray:1.0";
            let mut v = vec![0xFF, 0x80, name.len() as u8];
            v.extend_from_slice(name);
            v.extend_from_slice(&[0x03, 0x05]);
            v.extend_from_slice(b"value");
            v.push(code);
            base_name(&v, &[])
        };
        assert_eq!(named(0x2A), "ntscalararray_i32a");
        assert_eq!(named(0x4B), "ntscalararray_f64a");
        assert_eq!(named(0x28), "ntscalararray_i8a");
        assert_ne!(named(0x2A), named(0x4B));
    }

    #[test]
    fn an_enum_names_its_structured_value() {
        let name = b"epics:nt/NTEnum:1.0";
        let mut v = vec![0xFF, 0x80, name.len() as u8];
        v.extend_from_slice(name);
        v.extend_from_slice(&[0x03, 0x05]);
        v.extend_from_slice(b"value");
        v.push(0x80);
        assert_eq!(base_name(&v, &[]), "ntenum_struct");
    }

    #[test]
    fn an_unreadable_descriptor_still_yields_a_name() {
        // An unreadable descriptor should not prevent calibration.
        // The generic name is sufficient as a fallback, while
        // `mimus inspect` exposes the remaining details.
        let pieces: Vec<Piece> = Vec::new();
        assert_eq!(base_name(&[0x00, 0x01], &pieces), "type");
    }

    #[test]
    fn colliding_names_are_numbered_in_order_of_appearance() {
        // Different types can produce the same label. Rather than guessing
        // which type each label represents, append a numeric suffix.
        // Assignment order is deterministic for a given channel list.
        let mut n = Namer::new();
        assert_eq!(n.assign("ntscalararray_b512"), "ntscalararray_b512");
        assert_eq!(n.assign("ntscalararray_b512"), "ntscalararray_b512_2");
        assert_eq!(n.assign("ntscalararray_b512"), "ntscalararray_b512_3");
        assert_eq!(n.assign("ntscalar_f64"), "ntscalar_f64");
    }

    #[test]
    fn an_inline_descriptor_is_named_too() {
        // PVXS can encode the type name directly, without a registry ID.
        // Supporting this form prevents unrelated templates from receiving the same generic name.
        assert_eq!(type_id_of(&inline_intro()), Some("epics:nt/NTScalar:1.0"));

        let obs = capture();
        let pieces = layout::layout(&obs).unwrap();
        assert_eq!(base_name(&inline_intro(), &pieces), "ntscalar_f64");
    }

    /// Updates of a waveform of 200 doubles beside a timestamp, with the
    /// array kept whole.
    fn waveform() -> (Vec<Vec<u8>>, Template) {
        let mut obs = Vec::new();
        let mut arrays = Vec::new();
        for k in 0..10u32 {
            let mut m = vec![0x02, 0x42, 0x00, 200];
            let start = m.len();
            for i in 0..200u32 {
                m.extend_from_slice(&f64::from(i * k).to_le_bytes());
            }
            // The one array this message carries.
            let array = start..m.len();
            arrays.push(vec![array]);
            m.extend_from_slice(&(1_787_174_889i64 + i64::from(k)).to_le_bytes());
            m.extend_from_slice(&(157_360_595u32 + k * 7_919).to_le_bytes());
            m.extend_from_slice(&[0u8; 5]);
            obs.push(m);
        }
        let pieces = layout::layout_with_arrays(&obs, &arrays, 1024).unwrap();
        let full = ntscalar_intro(1);
        let t = template(
            &Observed {
                pv: "MIM:WF:0",
                pieces: &pieces,
                introspection_full: Some(&full),
                introspection_id_only: None,
                observations: obs.len() as u32,
            },
            "ntscalararray_f64a",
        )
        .unwrap();
        (obs, t)
    }

    #[test]
    fn past_the_budget_the_first_updates_are_kept_and_still_exact() {
        let (obs, mut t) = waveform();
        // 1600 B of array and 12 B of timestamp per update.
        let kept = keep_samples(&mut t, 5_000).expect("past the budget");
        assert_eq!(
            kept,
            Kept {
                kept: 3,
                of: 10,
                per_observation: 1_612
            }
        );
        assert!(t.slots.iter().all(|s| s.samples.len() == 3));
        assert!(crate::shadow::verify(&t, &obs[..3]).passed());
        // What is gone is gone: the fourth update is no longer carried.
        assert!(!crate::shadow::verify(&t, &obs[..4]).passed());
        // The count of what the template was learned from is unchanged.
        assert_eq!(t.observations, 10);
    }

    #[test]
    fn two_updates_are_kept_whatever_the_budget() {
        let (_, mut t) = waveform();
        assert_eq!(keep_samples(&mut t, 0).map(|k| k.kept), Some(2));
    }

    #[test]
    fn a_template_within_the_budget_is_left_alone() {
        let (_, mut t) = waveform();
        let before = t.clone();
        assert_eq!(keep_samples(&mut t, 4 << 20), None);
        assert_eq!(t, before);

        // A scalar's samples are far within any budget.
        let (_, _, mut scalar) = built();
        assert_eq!(keep_samples(&mut scalar, 4 << 20), None);
    }

    #[test]
    fn a_type_id_is_read_only_when_the_shape_matches() {
        assert_eq!(
            type_id_of(&ntscalar_intro(7)),
            Some("epics:nt/NTScalar:1.0")
        );
        // A cached-form descriptor contains no type name.
        assert_eq!(type_id_of(&cached_intro(7)), None);
        assert_eq!(type_id_of(&[]), None);
        assert_eq!(type_id_of(&[0xFF, 0xFD, 1, 0, 0x60, 4, b'a']), None);
    }
}
