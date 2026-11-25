//! Templates: the learned structure of a message.
//!
//! A template is a sequence of fixed byte chunks and variable slots:
//!
//! ```text
//! [ fixed ][ slot 0 ][ fixed ][ slot 1 ][ slot 2 ][ fixed ]
//! ```
//!
//! A flat buffer with offsets would be faster, but cannot represent
//! variable-length slots: every byte after them shifts. Strings and
//! variable-length arrays are common examples, deliberately included
//! in the reference catalogue.
//!
//! The chunk sequence is the source of truth. A [`FlatTemplate`] is
//! derived from it when all slots have a fixed width, which covers
//! most cases. Deriving the fast path avoids keeping two representations
//! in sync.

use serde::{Deserialize, Serialize};

use crate::slot::{Slot, SlotError, SlotWidth};

/// A fixed or variable part of a template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "chunk", rename_all = "snake_case")]
pub enum Chunk {
    /// Bytes unchanged across all observed updates, copied verbatim.
    Fixed {
        #[serde(with = "crate::hex::bytes")]
        bytes: Vec<u8>,
    },
    /// A changing region, identified by its [`Slot`] ID.
    Slot { id: u16 },
}

/// The introspection payload sent by PVXS during monitor initialization.
///
/// MIMUS replays these bytes unchanged without interpreting the type
/// descriptor, keeping it independent of PVXS type definitions.
///
/// PVA caches type descriptors per connection: the first channel of
/// a given type carries the full descriptor; subsequent channels refer
/// to its cached ID. Replaying the full descriptor every time would
/// make connection setup far heavier than in reality, especially
/// with many channels of the same type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Introspection {
    /// Monitor INIT payload with the full type descriptor.
    #[serde(with = "crate::hex::bytes")]
    pub full: Vec<u8>,
    /// Monitor INIT payload referencing the cached type ID.
    /// Absent if calibration never observed a second channel of this type.
    /// The catalogue therefore requires at least two instances per type.
    #[serde(
        with = "crate::hex::opt_bytes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub id_only: Option<Vec<u8>>,
}

/// The learned structure of a message type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    /// Stable name, derived from the introspection type ID when available, otherwise generated.
    pub name: String,
    /// PVA command produced by this template.
    pub command: u8,
    /// Monitor sub-command byte, if applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_command: Option<u8>,
    /// Payload sent when a client initializes a monitor.
    /// Required for monitor templates so clients can interpret updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introspection: Option<Introspection>,
    /// Full value sent when a monitor starts.
    ///
    /// A subscription starts with a complete value, followed by updates
    /// containing only changed fields. Clients retain the other fields
    /// from the initial value. Without it, fields such as `display`,
    /// `control` and `valueAlarm` remain undefined. In tests against
    /// `pvxmonitor`, this resulted in only four fields being displayed
    /// instead of the thirty-four provided by a real IOC.
    ///
    /// This template always has `initial: None` to stop recursion,
    /// as enforced by validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<Box<Template>>,
    pub chunks: Vec<Chunk>,
    pub slots: Vec<Slot>,
    /// Number of distinct updates used to build this template.
    /// Higher counts provide stronger evidence of the learned structure
    /// and allow coverage to be reported.
    pub observations: u32,
    /// The top-level fields this message changes, as the type descriptor
    /// names them: `value` and `timeStamp` for an ordinary update, with
    /// `alarm` too for one that crosses an alarm limit. Empty when
    /// calibration could not read the descriptor, or predates this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    /// Other updates of the same type, which change other fields: the update
    /// that carries an alarm transition, above all. Each was learned and
    /// checked against the bytes it came from as this template was, from the
    /// channels of the type that sent it.
    ///
    /// A variant has neither an opening message nor variants of its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<Template>,
}

/// Errors indicating an invalid or inconsistent template.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateError {
    /// A chunk references a non-existent slot.
    UnknownSlot {
        template: String,
        id: u16,
    },
    /// Two slots have the same ID.
    DuplicateSlot {
        template: String,
        id: u16,
    },
    /// A slot is defined but not referenced by any chunk.
    OrphanSlot {
        template: String,
        id: u16,
    },
    /// A slot is referenced more than once.
    RepeatedSlot {
        template: String,
        id: u16,
    },
    Slot {
        template: String,
        source: SlotError,
    },
    /// A monitor template has no introspection data.
    MissingIntrospection {
        template: String,
    },
    /// An initial-update template contains another initial update.
    NestedInitial {
        template: String,
    },
    /// A variant carries an opening message or variants of its own.
    NestedVariant {
        template: String,
    },
    /// Fewer observations than required.
    TooFewObservations {
        template: String,
        observations: u32,
        required: u32,
    },
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSlot { template, id } => {
                write!(f, "template {template}: chunk refers to unknown slot {id}")
            }
            Self::DuplicateSlot { template, id } => {
                write!(f, "template {template}: slot {id} is defined twice")
            }
            Self::OrphanSlot { template, id } => {
                write!(f, "template {template}: slot {id} is never placed")
            }
            Self::RepeatedSlot { template, id } => {
                write!(f, "template {template}: slot {id} is placed twice")
            }
            Self::Slot { template, source } => write!(f, "template {template}: {source}"),
            Self::MissingIntrospection { template } => write!(
                f,
                "template {template}: monitor template has no introspection bytes"
            ),
            Self::NestedInitial { template } => write!(
                f,
                "template {template}: an initial update cannot itself have one"
            ),
            Self::NestedVariant { template } => write!(
                f,
                "template {template}: a variant has no opening message and no variants"
            ),
            Self::TooFewObservations {
                template,
                observations,
                required,
            } => write!(
                f,
                "template {template}: built from {observations} updates, {required} required"
            ),
        }
    }
}

impl std::error::Error for TemplateError {}

impl Template {
    pub fn new(name: impl Into<String>, command: u8) -> Self {
        Self {
            name: name.into(),
            command,
            sub_command: None,
            introspection: None,
            initial: None,
            chunks: Vec::new(),
            slots: Vec::new(),
            observations: 0,
            changes: Vec::new(),
            variants: Vec::new(),
        }
    }

    /// Adds another update of the same type, changing other fields.
    pub fn with_variant(&mut self, mut variant: Template) -> &mut Self {
        variant.initial = None;
        variant.variants.clear();
        self.variants.push(variant);
        self
    }

    /// Returns whether this message changes the `alarm` field.
    pub fn changes_alarm(&self) -> bool {
        self.changes.iter().any(|f| f == "alarm")
    }

    /// Returns the variant that carries alarm transitions, if calibration
    /// saw one: of the variants that change `alarm` where the ordinary
    /// update does not, the one learned from the most updates.
    ///
    /// A type whose ordinary update already changes `alarm`, as some group
    /// structures do on every update, has no transition to add.
    pub fn alarm_variant(&self) -> Option<&Template> {
        if self.changes_alarm() {
            return None;
        }
        self.variants
            .iter()
            .filter(|v| v.changes_alarm())
            .max_by_key(|v| v.observations)
    }

    pub fn push_fixed(&mut self, bytes: impl Into<Vec<u8>>) -> &mut Self {
        self.chunks.push(Chunk::Fixed {
            bytes: bytes.into(),
        });
        self
    }

    pub fn with_introspection(&mut self, intro: Introspection) -> &mut Self {
        self.introspection = Some(intro);
        self
    }

    /// Sets the complete value sent when a monitor starts.
    /// Any nested initial value is discarded.
    pub fn with_initial(&mut self, mut initial: Template) -> &mut Self {
        initial.initial = None;
        self.initial = Some(Box::new(initial));
        self
    }

    pub fn push_slot(&mut self, slot: Slot) -> &mut Self {
        self.chunks.push(Chunk::Slot { id: slot.id });
        self.slots.push(slot);
        self
    }

    pub fn slot(&self, id: u16) -> Option<&Slot> {
        self.slots.iter().find(|s| s.id == id)
    }

    /// Returns true if all slots have a fixed width.
    pub fn is_fixed_width(&self) -> bool {
        self.slots.iter().all(|s| s.width.is_fixed())
    }

    /// Minimum wire length, with every variable slot at its minimum width.
    pub fn min_len(&self) -> usize {
        self.chunks
            .iter()
            .map(|c| match c {
                Chunk::Fixed { bytes } => bytes.len(),
                Chunk::Slot { id } => match self.slot(*id).map(|s| s.width) {
                    Some(SlotWidth::Fixed { bytes }) => bytes,
                    Some(SlotWidth::Variable { min, .. }) => min,
                    None => 0,
                },
            })
            .sum()
    }

    /// Maximum wire length, with every variable slot at its largest width.
    /// Use this to size emission buffers and avoid reallocations.
    pub fn max_len(&self) -> usize {
        self.chunks
            .iter()
            .map(|c| match c {
                Chunk::Fixed { bytes } => bytes.len(),
                Chunk::Slot { id } => self.slot(*id).map(|s| s.width.max_bytes()).unwrap_or(0),
            })
            .sum()
    }

    /// Checks template structure and validates each slot.
    pub fn validate(&self, min_observations: u32) -> Result<(), TemplateError> {
        let mut seen_defs: Vec<u16> = Vec::with_capacity(self.slots.len());
        for s in &self.slots {
            if seen_defs.contains(&s.id) {
                return Err(TemplateError::DuplicateSlot {
                    template: self.name.clone(),
                    id: s.id,
                });
            }
            seen_defs.push(s.id);
            s.validate().map_err(|source| TemplateError::Slot {
                template: self.name.clone(),
                source,
            })?;
        }

        let mut placed: Vec<u16> = Vec::with_capacity(self.slots.len());
        for c in &self.chunks {
            if let Chunk::Slot { id } = c {
                if !seen_defs.contains(id) {
                    return Err(TemplateError::UnknownSlot {
                        template: self.name.clone(),
                        id: *id,
                    });
                }
                if placed.contains(id) {
                    return Err(TemplateError::RepeatedSlot {
                        template: self.name.clone(),
                        id: *id,
                    });
                }
                placed.push(*id);
            }
        }

        if let Some(orphan) = seen_defs.iter().find(|id| !placed.contains(id)) {
            return Err(TemplateError::OrphanSlot {
                template: self.name.clone(),
                id: *orphan,
            });
        }

        // 0x0D is CMD_MONITOR. Keep the value local to avoid depending
        // on the wire crate from the profile format crate.
        if self.command == 0x0D && self.introspection.is_none() {
            return Err(TemplateError::MissingIntrospection {
                template: self.name.clone(),
            });
        }

        for v in &self.variants {
            if v.initial.is_some() || !v.variants.is_empty() {
                return Err(TemplateError::NestedVariant {
                    template: v.name.clone(),
                });
            }
            v.validate(min_observations)?;
        }

        // Always enforce a structural minimum, then apply the caller's threshold.
        //
        // Templates with slots are built from a diff, which requires at least
        // two observations. Templates without slots are static: one observation
        // fully describes the message. Applying the same minimum to both would
        // reject valid static templates.
        let required = if self.slots.is_empty() {
            1
        } else {
            min_observations.max(2)
        };
        if self.observations < required {
            return Err(TemplateError::TooFewObservations {
                template: self.name.clone(),
                observations: self.observations,
                required,
            });
        }

        Ok(())
    }

    /// Builds a flat representation when all slots have a fixed width.
    ///
    /// Returns `None` if any slot is variable-width. The emitter must then
    /// walk the chunk sequence.
    pub fn flatten(&self) -> Option<FlatTemplate> {
        if !self.is_fixed_width() {
            return None;
        }
        let mut bytes = Vec::with_capacity(self.max_len());
        let mut positions = Vec::with_capacity(self.slots.len());
        for c in &self.chunks {
            match c {
                Chunk::Fixed { bytes: b } => bytes.extend_from_slice(b),
                Chunk::Slot { id } => {
                    let slot = self.slot(*id)?;
                    let SlotWidth::Fixed { bytes: w } = slot.width else {
                        return None;
                    };
                    positions.push(SlotPosition {
                        id: *id,
                        offset: bytes.len(),
                        len: w,
                    });
                    // Placeholder bytes, overwritten before emission.
                    // Validation ensures every slot can be filled.
                    bytes.resize(bytes.len() + w, 0);
                }
            }
        }
        Some(FlatTemplate { bytes, positions })
    }
}

/// Position of a slot within a [`FlatTemplate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotPosition {
    pub id: u16,
    pub offset: usize,
    pub len: usize,
}

/// A template represented as a single buffer with slot offsets.
///
/// Designed for fast emission: copy the buffer, then overwrite each
/// slot at its known offset. Derived from a [`Template`], never authored
/// directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatTemplate {
    pub bytes: Vec<u8>,
    pub positions: Vec<SlotPosition>,
}

impl FlatTemplate {
    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn position(&self, id: u16) -> Option<SlotPosition> {
        self.positions.iter().copied().find(|p| p.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::{ScalarType, SlotRole, SlotWidth};

    const MONITOR: u8 = 0x0D;

    fn intro() -> Introspection {
        Introspection {
            full: vec![0xFD, 0x01, 0x00, 0x80],
            id_only: Some(vec![0xFE, 0x01, 0x00]),
        }
    }

    fn value_slot(id: u16) -> Slot {
        Slot::new(id, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::Value {
            scalar: ScalarType::F64,
        })
    }

    fn scalar_template() -> Template {
        let mut t = Template::new("ntscalar_double", MONITOR);
        t.with_introspection(intro())
            .push_fixed(vec![0xAA; 5])
            .push_slot(value_slot(1))
            .push_slot(
                Slot::new(2, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::TimestampSeconds),
            )
            .push_slot(
                Slot::new(3, SlotWidth::Fixed { bytes: 4 }).with_role(SlotRole::TimestampNanos),
            );
        t.observations = 100;
        t
    }

    #[test]
    fn a_scalar_template_validates_and_measures() {
        let t = scalar_template();
        t.validate(10).unwrap();
        assert!(t.is_fixed_width());
        assert_eq!(t.min_len(), 5 + 8 + 8 + 4);
        assert_eq!(t.max_len(), t.min_len());
    }

    #[test]
    fn flattening_places_slots_at_the_right_offsets() {
        let flat = scalar_template().flatten().unwrap();
        assert_eq!(flat.len(), 25);
        assert_eq!(
            flat.position(1),
            Some(SlotPosition {
                id: 1,
                offset: 5,
                len: 8
            })
        );
        assert_eq!(
            flat.position(3),
            Some(SlotPosition {
                id: 3,
                offset: 21,
                len: 4
            })
        );
        assert_eq!(&flat.bytes[..5], &[0xAA; 5]);
    }

    #[test]
    fn a_variable_slot_blocks_flattening() {
        // A variable-length string requires chunk-based emission.
        // Flattening it would produce a fixed-length message.
        let mut t = Template::new("ntscalar_string", MONITOR);
        t.with_introspection(intro())
            .push_fixed(vec![0xAA; 4])
            .push_slot(
                Slot::new(1, SlotWidth::Variable { min: 8, max: 26 })
                    .with_samples(vec![b"12:00:00".to_vec()]),
            )
            .push_fixed(vec![0xBB; 4]);
        t.observations = 50;

        t.validate(10).unwrap();
        assert!(!t.is_fixed_width());
        assert!(t.flatten().is_none());
        assert_eq!(t.min_len(), 16);
        assert_eq!(t.max_len(), 34);
    }

    #[test]
    fn rejects_a_chunk_pointing_at_no_slot() {
        let mut t = scalar_template();
        t.chunks.push(Chunk::Slot { id: 99 });
        assert_eq!(
            t.validate(0),
            Err(TemplateError::UnknownSlot {
                template: "ntscalar_double".into(),
                id: 99
            })
        );
    }

    #[test]
    fn rejects_a_slot_that_is_never_placed() {
        let mut t = scalar_template();
        t.slots.push(value_slot(42));
        assert_eq!(
            t.validate(0),
            Err(TemplateError::OrphanSlot {
                template: "ntscalar_double".into(),
                id: 42
            })
        );
    }

    #[test]
    fn rejects_duplicate_and_repeated_slots() {
        let mut dup = scalar_template();
        dup.slots.push(value_slot(1));
        assert!(matches!(
            dup.validate(0),
            Err(TemplateError::DuplicateSlot { id: 1, .. })
        ));

        let mut rep = scalar_template();
        rep.chunks.push(Chunk::Slot { id: 1 });
        assert!(matches!(
            rep.validate(0),
            Err(TemplateError::RepeatedSlot { id: 1, .. })
        ));
    }

    #[test]
    fn rejects_a_template_built_from_too_few_updates() {
        let mut t = scalar_template();
        t.observations = 3;
        assert_eq!(
            t.validate(10),
            Err(TemplateError::TooFewObservations {
                template: "ntscalar_double".into(),
                observations: 3,
                required: 10
            })
        );
    }

    #[test]
    fn round_trips_through_json() {
        let t = scalar_template();
        let json = serde_json::to_string_pretty(&t).unwrap();
        assert_eq!(serde_json::from_str::<Template>(&json).unwrap(), t);
        assert!(json.contains("aaaaaaaaaa"), "fixed bytes should be hex");
    }
}

#[cfg(test)]
mod introspection_tests {
    use super::*;

    const MONITOR: u8 = 0x0D;

    #[test]
    fn a_monitor_template_without_introspection_is_refused() {
        // Without introspection, clients cannot decode monitor updates.
        let mut t = Template::new("no_intro", MONITOR);
        t.push_fixed(vec![0x01]);
        t.observations = 100;
        assert_eq!(
            t.validate(0),
            Err(TemplateError::MissingIntrospection {
                template: "no_intro".into()
            })
        );
    }

    #[test]
    fn a_non_monitor_template_needs_none() {
        let mut t = Template::new("get_response", 0x0A);
        t.push_fixed(vec![0x01]);
        t.observations = 100;
        t.validate(0).unwrap();
    }

    #[test]
    fn introspection_round_trips_as_hex() {
        let i = Introspection {
            full: vec![0xFD, 0x01, 0x00, 0x80],
            id_only: Some(vec![0xFE, 0x01, 0x00]),
        };
        let json = serde_json::to_string(&i).unwrap();
        assert!(json.contains("fd010080"), "{json}");
        assert_eq!(serde_json::from_str::<Introspection>(&json).unwrap(), i);
    }

    #[test]
    fn a_missing_id_only_variant_survives_a_round_trip() {
        // Absence means calibration never observed a second channel
        // of this type. Preserve it rather than encoding an empty blob.
        let i = Introspection {
            full: vec![0xFD],
            id_only: None,
        };
        let json = serde_json::to_string(&i).unwrap();
        assert!(!json.contains("id_only"), "{json}");
        assert_eq!(serde_json::from_str::<Introspection>(&json).unwrap(), i);
    }
}

#[cfg(test)]
mod variant_tests {
    use super::*;

    const MONITOR: u8 = 0x0D;

    fn message(name: &str, changes: &[&str], observations: u32) -> Template {
        let mut t = Template::new(name, MONITOR);
        t.with_introspection(Introspection {
            full: vec![0xFD, 0x01, 0x00, 0x80],
            id_only: None,
        })
        .push_fixed(vec![0x01]);
        t.observations = observations;
        t.changes = changes.iter().map(|c| c.to_string()).collect();
        t
    }

    #[test]
    fn the_alarm_variant_is_the_best_observed_one_that_changes_alarm() {
        let mut t = message("dbl", &["value", "timeStamp"], 30);
        assert!(t.alarm_variant().is_none());
        t.with_variant(message("dbl+stamp", &["timeStamp"], 50))
            .with_variant(message("dbl+alarm", &["value", "alarm", "timeStamp"], 4))
            .with_variant(message("dbl+alarm2", &["alarm", "timeStamp"], 9));
        assert_eq!(
            t.alarm_variant().map(|v| v.name.as_str()),
            Some("dbl+alarm2")
        );
        assert!(!t.changes_alarm());
        assert!(t.validate(0).is_ok());

        // A type whose every update changes `alarm` has no transition to add.
        let mut group = message("grp", &["alarm", "timeStamp", "x"], 30);
        group.with_variant(message("grp+other", &["alarm", "timeStamp", "x"], 60));
        assert!(group.alarm_variant().is_none());
    }

    #[test]
    fn a_variant_is_checked_and_cannot_nest() {
        let mut t = message("dbl", &[], 30);
        let mut bad = message("dbl+alarm", &["alarm"], 0);
        bad.push_slot(Slot::new(1, SlotWidth::Fixed { bytes: 8 }).with_samples(vec![vec![0; 8]]));
        t.with_variant(bad);
        assert!(matches!(
            t.validate(0),
            Err(TemplateError::TooFewObservations { ref template, .. }) if template == "dbl+alarm"
        ));

        let mut t = message("dbl", &[], 30);
        t.with_variant(message("v", &["alarm"], 3));
        t.variants[0]
            .variants
            .push(message("deeper", &["alarm"], 3));
        assert_eq!(
            t.validate(0),
            Err(TemplateError::NestedVariant {
                template: "v".into()
            })
        );
    }

    #[test]
    fn a_template_without_variants_reads_and_writes_as_before() {
        let t = message("dbl", &[], 3);
        let json = serde_json::to_string(&Template {
            changes: Vec::new(),
            ..t.clone()
        })
        .unwrap();
        assert!(!json.contains("variants") && !json.contains("changes"));
        let back: Template = serde_json::from_str(&json).unwrap();
        assert!(back.variants.is_empty() && back.changes.is_empty());
    }
}
