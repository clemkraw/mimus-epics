//! Server-side components: socket listeners, the PVA state machine,
//! channel discovery, and the emission loop.

pub mod fleet;
pub mod listener;
pub mod naming;
pub mod registry;
pub mod scenario;
pub mod session;
pub mod shared;
pub mod stage;
pub mod udp;
pub mod written;

pub use fleet::{Fleet, FleetError, FleetSpec, Layout};
pub use listener::Ioc;
pub use naming::{NameContext, NameError, NameTemplate};
pub use registry::Registry;
pub use scenario::{Scenario, Target};
pub use session::{Put, Rpc, Session, SessionConfig, SessionEnd};
pub use shared::{Dropper, SharedCounters};
pub use stage::{Stage, Stages};
pub use udp::{DEFAULT_SEARCH_PORT, IocEndpoint, SearchResponder};
pub use written::Written;
