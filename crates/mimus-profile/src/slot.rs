//! Slots: the parts of a message that change between updates.
//!
//! Slots are discovered through differential analysis: MIMUS compares
//! successive updates of the same PV and identifies the bytes that change.
//! This reveals **which bytes change**, not what they mean.
//!
//! The type system reflects this distinction. Every slot starts as
//! [`SlotRole::Unclassified`], a fully functional state: unclassified slots
//! are replayed from observed samples. Classification is an optional
//! refinement that enables nicer synthetic signals, never a requirement
//! for correctness.
//!
//! This avoids a common failure of template-based generators: assuming,
//! for example, that eight bytes at offset 12 represent an IEEE double.
//! If that assumption is wrong, the generator may emit plausible-looking
//! but incorrect data that tests cannot detect.

use serde::{Deserialize, Serialize};

/// Width of a slot in the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SlotWidth {
    /// Fixed byte length, used for scalars, timestamps, alarm codes, and fixed-size arrays.
    Fixed { bytes: usize },
    /// Variable byte length, used for strings and arrays with a
    /// variable element count. Changes in length shift subsequent
    /// data, so templates must be represented as sequences of chunks.
    Variable {
        /// Smallest length observed during learning.
        min: usize,
        /// Largest length observed during learning.
        max: usize,
    },
}

impl SlotWidth {
    #[inline]
    pub fn is_fixed(&self) -> bool {
        matches!(self, Self::Fixed { .. })
    }

    /// Bytes to reserve when sizing a write buffer.
    #[inline]
    pub fn max_bytes(&self) -> usize {
        match self {
            Self::Fixed { bytes } => *bytes,
            Self::Variable { max, .. } => *max,
        }
    }
}

/// MIMUS's best-effort interpretation of a slot's contents.
///
/// Classification is optional and can be revised at any time.
/// The emission path works without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum SlotRole {
    /// The bytes change, but their meaning is unknown.
    /// Filled by replaying observed samples. This is the default,
    /// fully supported state, not an error.
    #[default]
    Unclassified,
    /// Measurement value. Enables smooth synthetic signals instead
    /// of replaying a loop, which matters for clients that compress
    /// or deduplicate data.
    Value { scalar: ScalarType },
    /// Seconds since the EPICS epoch. Must use wall-clock time
    /// at emission so consumers can measure end-to-end latency.
    TimestampSeconds,
    /// Nanoseconds within the second.
    TimestampNanos,
    /// Alarm severity, status, or another small enumerated code.
    AlarmCode,
}

impl SlotRole {
    /// Whether the emitter must generate a live value instead of replaying one.
    #[inline]
    pub fn needs_live_fill(&self) -> bool {
        matches!(self, Self::TimestampSeconds | Self::TimestampNanos)
    }
}

/// Scalar types supported by the synthetic signal generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScalarType {
    F64,
    F32,
    I64,
    I32,
    I16,
    I8,
    U64,
    U32,
    U16,
    U8,
    String,
}

impl ScalarType {
    /// Returns the wire width, or `None` for variable-length types.
    pub fn fixed_width(&self) -> Option<usize> {
        Some(match self {
            Self::F64 | Self::I64 | Self::U64 => 8,
            Self::F32 | Self::I32 | Self::U32 => 4,
            Self::I16 | Self::U16 => 2,
            Self::I8 | Self::U8 => 1,
            Self::String => return None,
        })
    }
}

/// A variable region within a template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    /// Stable identifier, unique within the template.
    /// Used by the emitter and to compare profiles.
    pub id: u16,
    pub width: SlotWidth,
    #[serde(flatten)]
    pub role: SlotRole,
    /// Observed byte sequences, replayed cyclically when the emitter
    /// cannot generate live values. Kept small to remain representative
    /// without storing the full calibration run.
    #[serde(
        with = "crate::hex::vec_of_bytes",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub samples: Vec<Vec<u8>>,
}

impl Slot {
    pub fn new(id: u16, width: SlotWidth) -> Self {
        Self {
            id,
            width,
            role: SlotRole::Unclassified,
            samples: Vec::new(),
        }
    }

    pub fn with_role(mut self, role: SlotRole) -> Self {
        self.role = role;
        self
    }

    pub fn with_samples(mut self, samples: Vec<Vec<u8>>) -> Self {
        self.samples = samples;
        self
    }

    /// Whether the slot can be generated without replaying samples.
    #[inline]
    pub fn is_synthesisable(&self) -> bool {
        self.role.needs_live_fill()
            || matches!(self.role, SlotRole::Value { scalar } if scalar.fixed_width().is_some())
    }

    /// Whether the slot can be emitted.
    ///
    /// A slot that cannot be synthesized and has no samples is a hole
    /// MIMUS cannot fill. Profiles containing such slots are rejected:
    /// emitting zeroes would silently produce invalid data.
    #[inline]
    pub fn is_emittable(&self) -> bool {
        self.is_synthesisable() || !self.samples.is_empty()
    }

    /// Validates the declared width against the recorded samples.
    pub fn validate(&self) -> Result<(), SlotError> {
        match self.width {
            SlotWidth::Fixed { bytes } => {
                if bytes == 0 {
                    return Err(SlotError::ZeroWidth { slot: self.id });
                }
                if let Some(bad) = self.samples.iter().find(|s| s.len() != bytes) {
                    return Err(SlotError::SampleWidthMismatch {
                        slot: self.id,
                        expected: bytes,
                        found: bad.len(),
                    });
                }
                if let SlotRole::Value { scalar } = self.role
                    && let Some(w) = scalar.fixed_width()
                    && w != bytes
                {
                    return Err(SlotError::RoleWidthMismatch {
                        slot: self.id,
                        role_width: w,
                        slot_width: bytes,
                    });
                }
            }
            SlotWidth::Variable { min, max } => {
                if min > max {
                    return Err(SlotError::InvertedRange {
                        slot: self.id,
                        min,
                        max,
                    });
                }
                if let Some(bad) = self.samples.iter().find(|s| s.len() < min || s.len() > max) {
                    return Err(SlotError::SampleOutOfRange {
                        slot: self.id,
                        min,
                        max,
                        found: bad.len(),
                    });
                }
            }
        }
        if !self.is_emittable() {
            return Err(SlotError::NotEmittable { slot: self.id });
        }
        Ok(())
    }
}

/// Errors indicating an inconsistent slot definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotError {
    ZeroWidth {
        slot: u16,
    },
    InvertedRange {
        slot: u16,
        min: usize,
        max: usize,
    },
    SampleWidthMismatch {
        slot: u16,
        expected: usize,
        found: usize,
    },
    SampleOutOfRange {
        slot: u16,
        min: usize,
        max: usize,
        found: usize,
    },
    RoleWidthMismatch {
        slot: u16,
        role_width: usize,
        slot_width: usize,
    },
    NotEmittable {
        slot: u16,
    },
}

impl std::fmt::Display for SlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroWidth { slot } => write!(f, "slot {slot} has zero width"),
            Self::InvertedRange { slot, min, max } => {
                write!(f, "slot {slot} has min {min} above max {max}")
            }
            Self::SampleWidthMismatch {
                slot,
                expected,
                found,
            } => write!(
                f,
                "slot {slot} is {expected} B wide but a sample is {found} B"
            ),
            Self::SampleOutOfRange {
                slot,
                min,
                max,
                found,
            } => write!(
                f,
                "slot {slot} accepts {min}..={max} B but a sample is {found} B"
            ),
            Self::RoleWidthMismatch {
                slot,
                role_width,
                slot_width,
            } => write!(
                f,
                "slot {slot} is classified as a {role_width} B scalar but is {slot_width} B wide"
            ),
            Self::NotEmittable { slot } => write!(
                f,
                "slot {slot} is unclassified and has no samples: nothing to emit"
            ),
        }
    }
}

impl std::error::Error for SlotError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(id: u16, bytes: usize) -> Slot {
        Slot::new(id, SlotWidth::Fixed { bytes })
    }

    #[test]
    fn unclassified_with_samples_is_valid_and_emittable() {
        let s = fixed(1, 4).with_samples(vec![vec![0; 4], vec![1; 4]]);
        assert_eq!(s.role, SlotRole::Unclassified);
        assert!(s.is_emittable());
        assert!(!s.is_synthesisable());
        s.validate().unwrap();
    }

    #[test]
    fn unclassified_without_samples_is_rejected() {
        let s = fixed(7, 8);
        assert!(!s.is_emittable());
        assert_eq!(s.validate(), Err(SlotError::NotEmittable { slot: 7 }));
    }

    #[test]
    fn timestamps_need_no_samples() {
        let s = fixed(2, 8).with_role(SlotRole::TimestampSeconds);
        assert!(s.is_synthesisable());
        assert!(s.role.needs_live_fill());
        s.validate().unwrap();
    }

    #[test]
    fn sample_width_must_match_a_fixed_slot() {
        let s = fixed(3, 8).with_samples(vec![vec![0; 4]]);
        assert_eq!(
            s.validate(),
            Err(SlotError::SampleWidthMismatch {
                slot: 3,
                expected: 8,
                found: 4
            })
        );
    }

    #[test]
    fn a_classified_scalar_must_agree_with_the_slot_width() {
        // A double declared as four bytes would silently produce
        // plausible-looking but invalid data.
        let s = fixed(4, 4)
            .with_role(SlotRole::Value {
                scalar: ScalarType::F64,
            })
            .with_samples(vec![vec![0; 4]]);
        assert_eq!(
            s.validate(),
            Err(SlotError::RoleWidthMismatch {
                slot: 4,
                role_width: 8,
                slot_width: 4
            })
        );
    }

    #[test]
    fn variable_slots_accept_lengths_in_range_only() {
        let ok = Slot::new(5, SlotWidth::Variable { min: 2, max: 8 })
            .with_samples(vec![vec![0; 2], vec![0; 8]]);
        ok.validate().unwrap();

        let bad =
            Slot::new(5, SlotWidth::Variable { min: 2, max: 8 }).with_samples(vec![vec![0; 9]]);
        assert!(matches!(
            bad.validate(),
            Err(SlotError::SampleOutOfRange { found: 9, .. })
        ));
    }

    #[test]
    fn a_variable_string_value_is_not_synthesisable_but_is_emittable() {
        // Variable-length strings must be replayed, not synthesized.
        let s = Slot::new(6, SlotWidth::Variable { min: 1, max: 12 })
            .with_role(SlotRole::Value {
                scalar: ScalarType::String,
            })
            .with_samples(vec![b"12:00:00".to_vec()]);
        assert!(!s.is_synthesisable());
        assert!(s.is_emittable());
        s.validate().unwrap();
    }

    #[test]
    fn round_trips_through_json() {
        let s = fixed(9, 8)
            .with_role(SlotRole::Value {
                scalar: ScalarType::F64,
            })
            .with_samples(vec![1.0f64.to_le_bytes().to_vec()]);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Slot>(&json).unwrap(), s);
        // Samples are hex-encoded, not JSON arrays of bytes.
        assert!(json.contains("000000000000f03f"), "{json}");
    }
}
