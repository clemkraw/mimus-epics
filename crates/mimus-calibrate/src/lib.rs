//! Extract message templates by observing a reference IOC.
//!
//! MIMUS does not decode the structures it carries. It observes a running
//! `softIocPVX`, compares successive updates for each channel byte by byte,
//! and records the result: unchanged regions become fixed template data,
//! while changing regions become slots. The only reading of a slot's bytes
//! is to recognize a timestamp, which serving fills with the current time,
//! or a number; every other slot, the value included, replays what was
//! observed.
//!
//! This is comparison, not statistical learning: nothing is trained or
//! fitted, and the same observations always produce the same templates.
//! Two calibrations of one IOC observe different updates, so a byte that
//! held still in one window and moved in the other can be fixed in one
//! profile and a slot in the other; the types and their descriptors do not
//! change. Where the documentation says MIMUS learns a template, it means
//! this: observed, extracted, and checked against what was observed.
//!
//! MIMUS is independent of the PVXS version because it does not depend on
//! PVXS internals or version-specific semantics. If PVXS changes the bytes
//! it sends, MIMUS observes the difference and generates a corresponding
//! template without requiring code changes.

pub mod build;
pub mod client;
pub mod diff;
pub mod layout;
pub mod observe;
pub mod shadow;

pub use build::{BuildError, Kept, Namer, Observed, base_name, keep_samples, template};
pub use client::{ClientError, Descriptor, Settings, descriptor_key, descriptor_shape, observe};
pub use diff::{DiffError, InferredSlot, Kind, changed_mask, infer};
pub use layout::{Alignment, Piece, align, layout, layout_with_arrays, rebuild};
pub use observe::{ChannelObserver, Criterion, Progress, Status, progress, status};
pub use shadow::{Divergence, Report, verify};
