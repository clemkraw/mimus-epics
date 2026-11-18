//! Producing bytes through templates, scheduling and accounting. No sockets.
//!
//! Keeping this crate free of I/O makes emission cost measurable in
//! nanoseconds per event, without kernel overhead. Otherwise, socket writes
//! would contribute to the benchmark alongside MIMUS itself.
//!
//! The main components:
//!
//! - [`Compiled`]: a learned template compiled once for fast rendering.
//! - [`Wheel`]: one timing wheel for the process instead of one timer per PV.
//! - [`Counters`]: emission statistics that let consumers verify the
//!   conservation of updates.
//!
//! The crate is split into three concerns: rendering, scheduling and
//! accounting.

pub mod counters;
pub mod render;
pub mod signal;
pub mod wheel;

pub use counters::{Backpressure, Counters};
pub use render::{CompileError, Compiled, Fill};
pub use signal::{EPICS_EPOCH_OFFSET, EpicsTime, Signal};
pub use wheel::{Due, Instance, RateMix, Schedule, Wheel, WheelError};
