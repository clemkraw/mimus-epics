//! The contract between the two halves of MIMUS.
//!
//! `mimus calibrate` produces a [`Profile`]; `mimus serve` consumes one.
//! Neither depends on the other. They communicate only through this crate.
//!
//! The three concepts to keep in mind:
//!
//! - A [`Template`] is a sequence of chunks, not a flat buffer with holes.
//!   This allows slots to vary in length, as strings and variable-length
//!   arrays do. A flat representation is derived as a fast path.
//! - A [`Slot`] starts as [`SlotRole::Unclassified`], which is a valid,
//!   fully supported state. Differential analysis identifies changing
//!   bytes, but does not reveal their meaning.
//! - [`SessionRules`] captures connection-level behavior that cannot be
//!   inferred from individual messages, especially whether the server sends
//!   a full type descriptor every time or only a cached identifier after the
//!   first. PVXS sends the full descriptor every time.
//!
//! MIMUS targets PVXS (QSRV2). See `catalogue/README.md`.

pub mod hex;
pub mod profile;
pub mod session;
pub mod slot;
pub mod template;

pub use profile::{Assignment, DEFAULT_MIN_OBSERVATIONS, Profile, ProfileError, SCHEMA_VERSION};
pub use session::{ChangedSetPattern, DescriptorPolicy, Manifest, SessionRules};
pub use slot::{ScalarType, Slot, SlotError, SlotRole, SlotWidth};
pub use template::{Chunk, FlatTemplate, Introspection, SlotPosition, Template, TemplateError};
