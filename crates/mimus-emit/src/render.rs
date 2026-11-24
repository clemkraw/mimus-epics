//! The hot path: rendering a learned template as bytes.
//!
//! A [`Template`] is the portable description. A [`Compiled`] is its
//! startup-compiled representation, arranged for fast rendering.
//! Rendering performs no per-event allocation, parsing, or socket I/O.
//!
//! Two rendering paths handle the two possible template shapes:
//!
//! - **Flat**: every slot has a fixed width, so the message length never
//!   changes. Copy the template once, then write each slot. This is the
//!   overwhelmingly common case.
//! - **Chunked**: at least one slot has a variable width, so later bytes
//!   shift. Render each piece in order.
//!
//! Both paths use the same [`Template`], ensuring identical message
//! contents while differing only in how they are produced.

use mimus_profile::{Chunk, ScalarType, SlotRole, Template};

use crate::signal::{EpicsTime, Signal, write_scalar};

/// How a slot is filled at emission time.
#[derive(Debug, Clone)]
pub enum Fill {
    /// Current wall-clock seconds.
    TimestampSeconds { width: usize },
    /// Current wall-clock nanoseconds.
    TimestampNanos { width: usize },
    /// Generated from a signal function.
    Generated {
        signal: Signal,
        scalar: ScalarType,
        width: usize,
    },
    /// Replayed cyclically from observed samples.
    ///
    /// Supports unclassified and variable-length slots without requiring
    /// knowledge of their contents.
    Replay { samples: Vec<Vec<u8>> },
}

impl Fill {
    /// Returns the maximum output width, for buffer sizing.
    fn max_width(&self) -> usize {
        match self {
            Self::TimestampSeconds { width }
            | Self::TimestampNanos { width }
            | Self::Generated { width, .. } => *width,
            Self::Replay { samples } => samples.iter().map(|s| s.len()).max().unwrap_or(0),
        }
    }

    /// Returns whether the fill always produces the same number of bytes.
    ///
    /// Replay is fixed-width only if it has at least one sample and
    /// all samples have the same length.
    fn is_fixed(&self) -> bool {
        match self {
            Self::Replay { samples } => {
                let mut it = samples.iter().map(|s| s.len());
                match it.next() {
                    None => false,
                    Some(first) => it.all(|l| l == first),
                }
            }
            _ => true,
        }
    }
}

/// A segment of a chunked template.
#[derive(Debug, Clone)]
enum Piece {
    /// Range of constant bytes in the compiled blob.
    Fixed { start: usize, end: usize },
    /// Index into `fills`.
    Slot(usize),
}

/// Position of a fixed-width slot in the flat buffer.
#[derive(Debug, Clone, Copy)]
struct Placed {
    offset: usize,
    len: usize,
    fill_idx: usize,
}

#[derive(Debug, Clone)]
enum Layout {
    Flat {
        /// Complete message with slot bytes left as placeholders.
        bytes: Vec<u8>,
        placed: Vec<Placed>,
    },
    Chunked {
        blob: Vec<u8>,
        pieces: Vec<Piece>,
    },
}

/// A template compiled for emission.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub name: String,
    /// Complete value sent when a monitor starts.
    ///
    /// A subscription requires both the initial value and steady-state
    /// updates. Without the initial value, fields that never change remain
    /// undefined for the lifetime of the connection.
    pub initial: Option<Box<Compiled>>,
    /// Monitor INIT bytes for the first channel of this type on a
    /// connection. Replayed verbatim without parsing.
    pub intro_full: Vec<u8>,
    /// Monitor INIT bytes using the cached type identifier, for subsequent
    /// channels. Absent if the study observed no second channel.
    pub intro_id_only: Option<Vec<u8>>,
    layout: Layout,
    fills: Vec<Fill>,
    max_len: usize,
}

/// Why a template could not be compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// A slot has neither a usable classification nor any samples.
    /// Profile validation checks this too, but compilation must also
    /// reject unfillable templates from any other source.
    UnfillableSlot { template: String, slot: u16 },
    /// A chunk refers to a slot not defined by the template.
    UnknownSlot { template: String, slot: u16 },
    /// A classified scalar slot has the wrong width.
    ///
    /// Profile validation checks this too, but compilation cannot assume
    /// the profile was validated. Refuse to prevent writes from overrunning
    /// the slot and corrupting neighboring fields.
    SlotWidthMismatch {
        template: String,
        slot: u16,
        scalar_bytes: usize,
        slot_bytes: usize,
    },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnfillableSlot { template, slot } => write!(
                f,
                "template {template}: slot {slot} has no classification and no samples"
            ),
            Self::UnknownSlot { template, slot } => {
                write!(f, "template {template}: unknown slot {slot}")
            }
            Self::SlotWidthMismatch {
                template,
                slot,
                scalar_bytes,
                slot_bytes,
            } => write!(
                f,
                "template {template}: slot {slot} is classified as a {scalar_bytes} B \
                 scalar but is {slot_bytes} B wide"
            ),
        }
    }
}

impl std::error::Error for CompileError {}

impl Compiled {
    /// Compile a template into its emission-ready representation.
    ///
    /// Resolves slot fills, validates widths and builds either a flat
    /// or chunked layout. Also compiles the initial value and copies
    /// introspection bytes. Called once per template at startup.
    pub fn new(template: &Template) -> Result<Self, CompileError> {
        let mut fills = Vec::with_capacity(template.slots.len());
        let mut fill_of_slot: Vec<(u16, usize)> = Vec::with_capacity(template.slots.len());

        for slot in &template.slots {
            let width = match slot.width {
                mimus_profile::SlotWidth::Fixed { bytes } => bytes,
                mimus_profile::SlotWidth::Variable { max, .. } => max,
            };
            let fill = match slot.role {
                SlotRole::TimestampSeconds => Fill::TimestampSeconds { width },
                SlotRole::TimestampNanos => Fill::TimestampNanos { width },

                // Prefer observed bytes whenever available.
                //
                // Generated values overwrite the entire slot, but slot
                // boundaries are inferred rather than known. A classified
                // double once overlapped a string length prefix, corrupting
                // it and causing PVXS to read beyond the message and close
                // the connection. The shadow check missed this because it
                // ignores slot bytes.
                //
                // Timestamps are the exception: they must be live, and
                // their classification is supported by calendar-range
                // checks across all observations.
                SlotRole::Value { .. } if !slot.samples.is_empty() => Fill::Replay {
                    samples: slot.samples.clone(),
                },

                SlotRole::Value { scalar } if scalar.fixed_width().is_some() => {
                    let expected = scalar.fixed_width().expect("checked by the guard");
                    if expected != width {
                        return Err(CompileError::SlotWidthMismatch {
                            template: template.name.clone(),
                            slot: slot.id,
                            scalar_bytes: expected,
                            slot_bytes: width,
                        });
                    }
                    Fill::Generated {
                        signal: Signal::default_for(scalar),
                        scalar,
                        width,
                    }
                }
                // Unclassified slots and unsupported value types, such as
                // alarm codes and strings, replay observed bytes.
                _ => {
                    if slot.samples.is_empty() {
                        return Err(CompileError::UnfillableSlot {
                            template: template.name.clone(),
                            slot: slot.id,
                        });
                    }
                    Fill::Replay {
                        samples: slot.samples.clone(),
                    }
                }
            };
            fill_of_slot.push((slot.id, fills.len()));
            fills.push(fill);
        }

        let all_fixed = fills.iter().all(Fill::is_fixed);

        let mut blob = Vec::new();
        let mut pieces = Vec::with_capacity(template.chunks.len());
        for chunk in &template.chunks {
            match chunk {
                Chunk::Fixed { bytes } => {
                    let start = blob.len();
                    blob.extend_from_slice(bytes);
                    pieces.push(Piece::Fixed {
                        start,
                        end: blob.len(),
                    });
                }
                Chunk::Slot { id } => {
                    let idx = fill_of_slot
                        .iter()
                        .find(|(sid, _)| sid == id)
                        .map(|(_, i)| *i)
                        .ok_or(CompileError::UnknownSlot {
                            template: template.name.clone(),
                            slot: *id,
                        })?;
                    pieces.push(Piece::Slot(idx));
                }
            }
        }

        let max_len: usize = pieces
            .iter()
            .map(|p| match p {
                Piece::Fixed { start, end } => end - start,
                Piece::Slot(i) => fills[*i].max_width(),
            })
            .sum();

        let layout = if all_fixed {
            let mut bytes = Vec::with_capacity(max_len);
            let mut placed = Vec::with_capacity(fills.len());
            for piece in &pieces {
                match piece {
                    Piece::Fixed { start, end } => bytes.extend_from_slice(&blob[*start..*end]),
                    Piece::Slot(i) => {
                        let len = fills[*i].max_width();
                        placed.push(Placed {
                            offset: bytes.len(),
                            len,
                            fill_idx: *i,
                        });
                        bytes.resize(bytes.len() + len, 0);
                    }
                }
            }
            Layout::Flat { bytes, placed }
        } else {
            Layout::Chunked { blob, pieces }
        };

        let initial = match &template.initial {
            Some(inner) => Some(Box::new(Compiled::new(inner)?)),
            None => None,
        };

        let (intro_full, intro_id_only) = match &template.introspection {
            Some(i) => (i.full.clone(), i.id_only.clone()),
            None => (Vec::new(), None),
        };

        Ok(Self {
            name: template.name.clone(),
            initial,
            intro_full,
            intro_id_only,
            layout,
            fills,
            max_len,
        })
    }

    /// Returns the maximum message length.
    ///
    /// Use this to size an emission buffer and avoid reallocations on the hot path.
    #[inline]
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Whether every update would be the same bytes: a template with nothing
    /// to fill, built from a channel that never moved during calibration.
    ///
    /// Such a channel sends its opening message and nothing after it, as on
    /// the IOC. Repeating its one recorded body at the serving rate would
    /// send the same value with the same frozen timestamp, minutes older
    /// than the opening message, over and over.
    #[inline]
    pub fn is_static(&self) -> bool {
        self.fills.is_empty()
    }

    /// Returns whether the flat rendering path is used.
    #[inline]
    pub fn is_flat(&self) -> bool {
        matches!(self.layout, Layout::Flat { .. })
    }

    /// Appends one rendered message to `out`.
    ///
    /// `step` is the instance's update counter and `seed` its identity.
    /// Together they make generated values reproducible.
    #[inline]
    pub fn render(&self, step: u64, seed: u64, now: EpicsTime, out: &mut Vec<u8>) {
        match &self.layout {
            Layout::Flat { bytes, placed } => {
                let base = out.len();
                out.extend_from_slice(bytes);
                for p in placed {
                    let dst = &mut out[base + p.offset..base + p.offset + p.len];
                    write_fill(&self.fills[p.fill_idx], step, seed, now, dst);
                }
            }
            Layout::Chunked { blob, pieces } => {
                for piece in pieces {
                    match piece {
                        Piece::Fixed { start, end } => out.extend_from_slice(&blob[*start..*end]),
                        Piece::Slot(i) => append_fill(&self.fills[*i], step, seed, now, out),
                    }
                }
            }
        }
    }
}

#[inline]
fn write_fill(fill: &Fill, step: u64, seed: u64, now: EpicsTime, dst: &mut [u8]) {
    match fill {
        Fill::TimestampSeconds { .. } => write_int(now.seconds, dst),
        Fill::TimestampNanos { .. } => write_int(now.nanos as i64, dst),
        Fill::Generated { signal, scalar, .. } => {
            write_scalar(signal.sample(step, seed), *scalar, dst)
        }
        Fill::Replay { samples } => {
            let s = &samples[(step as usize) % samples.len()];
            let n = s.len().min(dst.len());
            dst[..n].copy_from_slice(&s[..n]);
        }
    }
}

#[inline]
fn append_fill(fill: &Fill, step: u64, seed: u64, now: EpicsTime, out: &mut Vec<u8>) {
    match fill {
        Fill::Replay { samples } => {
            out.extend_from_slice(&samples[(step as usize) % samples.len()])
        }
        other => {
            let w = other.max_width();
            let base = out.len();
            out.resize(base + w, 0);
            write_fill(other, step, seed, now, &mut out[base..base + w]);
        }
    }
}

/// Writes an integer in little-endian order, using the slot's width.
#[inline]
fn write_int(v: i64, dst: &mut [u8]) {
    match dst.len() {
        8 => dst.copy_from_slice(&v.to_le_bytes()),
        4 => dst.copy_from_slice(&(v as i32).to_le_bytes()),
        2 => dst.copy_from_slice(&(v as i16).to_le_bytes()),
        1 => dst[0] = v as u8,
        n => {
            let b = v.to_le_bytes();
            dst.copy_from_slice(&b[..n.min(8)]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimus_profile::{Slot, SlotWidth};

    const MONITOR: u8 = 0x0D;

    fn scalar_template() -> Template {
        let mut t = Template::new("ntscalar_double", MONITOR);
        t.push_fixed(vec![0xAA, 0xBB])
            .push_slot(
                Slot::new(1, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::Value {
                    scalar: ScalarType::F64,
                }),
            )
            .push_slot(
                Slot::new(2, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::TimestampSeconds),
            )
            .push_slot(
                Slot::new(3, SlotWidth::Fixed { bytes: 4 }).with_role(SlotRole::TimestampNanos),
            );
        t.observations = 100;
        t
    }

    fn string_template() -> Template {
        let mut t = Template::new("ntscalar_string", MONITOR);
        t.push_fixed(vec![0xAA])
            .push_slot(
                Slot::new(1, SlotWidth::Variable { min: 3, max: 5 })
                    .with_samples(vec![b"abc".to_vec(), b"defgh".to_vec()]),
            )
            .push_fixed(vec![0xBB]);
        t.observations = 100;
        t
    }

    #[test]
    fn a_fixed_template_uses_the_flat_path() {
        let c = Compiled::new(&scalar_template()).unwrap();
        assert!(c.is_flat());
        assert_eq!(c.max_len(), 2 + 8 + 8 + 4);
    }

    #[test]
    fn rendering_places_every_field_where_it_belongs() {
        let c = Compiled::new(&scalar_template()).unwrap();
        let now = EpicsTime::from_parts(1_000_000, 123_456_789);
        let mut out = Vec::new();
        c.render(0, 7, now, &mut out);

        assert_eq!(out.len(), 22);
        assert_eq!(&out[0..2], &[0xAA, 0xBB]);
        assert_eq!(
            i64::from_le_bytes(out[10..18].try_into().unwrap()),
            1_000_000
        );
        assert_eq!(
            i32::from_le_bytes(out[18..22].try_into().unwrap()),
            123_456_789
        );
        // The value slot must contain generated data, not its zero placeholder.
        assert_ne!(&out[2..10], &[0u8; 8]);
    }

    #[test]
    fn rendering_appends_and_never_clobbers() {
        let c = Compiled::new(&scalar_template()).unwrap();
        let mut out = vec![0x11, 0x22];
        c.render(0, 1, EpicsTime::default(), &mut out);
        c.render(1, 1, EpicsTime::default(), &mut out);
        assert_eq!(&out[..2], &[0x11, 0x22]);
        assert_eq!(out.len(), 2 + 22 * 2);
    }

    #[test]
    fn successive_updates_differ_in_value() {
        let c = Compiled::new(&scalar_template()).unwrap();
        let now = EpicsTime::from_parts(1, 0);
        let mut a = Vec::new();
        let mut b = Vec::new();
        c.render(0, 1, now, &mut a);
        c.render(1, 1, now, &mut b);
        assert_ne!(a, b);
    }

    #[test]
    fn different_instances_produce_different_streams() {
        // Seeds give each channel a distinct signal phase, even when
        // channels share a template and update at the same step.
        let c = Compiled::new(&scalar_template()).unwrap();
        let now = EpicsTime::from_parts(1, 0);
        let mut a = Vec::new();
        let mut b = Vec::new();
        c.render(3, 1, now, &mut a);
        c.render(3, 2, now, &mut b);
        assert_ne!(a, b);
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn a_variable_slot_forces_the_chunked_path() {
        let c = Compiled::new(&string_template()).unwrap();
        assert!(!c.is_flat());

        let mut out = Vec::new();
        c.render(0, 0, EpicsTime::default(), &mut out);
        assert_eq!(out, b"\xAAabc\xBB");

        let mut out = Vec::new();
        c.render(1, 0, EpicsTime::default(), &mut out);
        assert_eq!(out, b"\xAAdefgh\xBB");
    }

    #[test]
    fn replay_cycles_through_samples() {
        let c = Compiled::new(&string_template()).unwrap();
        let mut first = Vec::new();
        let mut third = Vec::new();
        c.render(0, 0, EpicsTime::default(), &mut first);
        c.render(2, 0, EpicsTime::default(), &mut third);
        assert_eq!(first, third);
    }

    #[test]
    fn an_unfillable_slot_is_refused() {
        let mut t = Template::new("broken", MONITOR);
        t.push_slot(Slot::new(1, SlotWidth::Fixed { bytes: 8 }));
        t.observations = 100;
        assert_eq!(
            Compiled::new(&t).unwrap_err(),
            CompileError::UnfillableSlot {
                template: "broken".into(),
                slot: 1
            }
        );
    }

    #[test]
    fn unclassified_slots_with_samples_compile_and_render() {
        let mut t = Template::new("opaque", MONITOR);
        t.push_fixed(vec![0x01]).push_slot(
            Slot::new(1, SlotWidth::Fixed { bytes: 4 })
                .with_samples(vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]]),
        );
        t.observations = 100;
        let c = Compiled::new(&t).unwrap();
        assert!(c.is_flat(), "same-width samples should still flatten");
        let mut out = Vec::new();
        c.render(1, 0, EpicsTime::default(), &mut out);
        assert_eq!(out, vec![0x01, 5, 6, 7, 8]);
    }

    #[test]
    fn rendering_is_pure() {
        let c = Compiled::new(&scalar_template()).unwrap();
        let now = EpicsTime::from_parts(42, 43);
        let mut a = Vec::new();
        let mut b = Vec::new();
        c.render(9, 9, now, &mut a);
        c.render(9, 9, now, &mut b);
        assert_eq!(a, b);
    }
}
