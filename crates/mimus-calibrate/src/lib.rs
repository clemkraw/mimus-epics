//! Extract message templates by observing a reference IOC.
//!
//! MIMUS does not implement PVAccess semantics. It observes a running
//! `softIocPVX`, compares successive updates for each channel byte by byte,
//! and records the result: unchanged regions become fixed template data,
//! while changing regions become slots.
//!
//! This is comparison, not inference. It is deterministic and reproducible
//! at the byte level: calibrating twice against the same IOC produces
//! identical profiles. That is why the vocabulary here is **observe**,
//! **extract**, and **calibrate**, never **learn** or **train**.
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

pub use build::{BuildError, Namer, Observed, base_name, template};
pub use client::{ClientError, Descriptor, Settings, descriptor_key, descriptor_shape, observe};
pub use diff::{DiffError, InferredSlot, Kind, changed_mask, infer};
pub use layout::{Alignment, Piece, align, layout, rebuild};
pub use observe::{ChannelObserver, Criterion, Progress, Status, progress, status};
pub use shadow::{Divergence, Report, verify};
