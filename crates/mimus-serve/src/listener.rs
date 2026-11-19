//! Binds N ports and accepts client connections.
//!
//! Each virtual IOC has its own listener. To a client, N ports on the same
//! host represent N independent IOCs: they handle searches separately,
//! assign independent channel IDs, and maintain separate type registries.
//! The operating system sees a single process, allowing MIMUS to scale
//! beyond what a fleet of separate soft IOCs can achieve.

use std::sync::Arc;

use tokio::net::TcpListener;

use crate::fleet::Fleet;
use crate::session::{Session, SessionConfig};
use crate::shared::SharedCounters;

/// A bound virtual IOC.
pub struct Ioc {
    pub index: u16,
    pub addr: std::net::SocketAddr,
    listener: TcpListener,
    cfg: SessionConfig,
    fleet: Arc<Fleet>,
    totals: Arc<SharedCounters>,
}

impl Ioc {
    /// Binds a virtual IOC to the requested address.
    ///
    /// A port of `0` lets the OS select an available port, avoiding fixed-port conflicts in tests.
    pub async fn bind(
        index: u16,
        bind_addr: &str,
        mut cfg: SessionConfig,
        fleet: Arc<Fleet>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind_addr).await?;
        let addr = listener.local_addr()?;
        cfg.ioc = index;
        cfg.port = addr.port();
        cfg.guid = guid_for(index);
        Ok(Self {
            index,
            addr,
            listener,
            cfg,
            fleet,
            totals: SharedCounters::new(),
        })
    }

    /// Returns the shared counters for this IOC.
    ///
    /// The counters remain readable while sessions are running.
    pub fn totals(&self) -> Arc<SharedCounters> {
        Arc::clone(&self.totals)
    }

    /// Accepts connections indefinitely, spawning one task per connection.
    pub async fn serve(self) {
        loop {
            let Ok((stream, _peer)) = self.listener.accept().await else {
                continue;
            };
            let cfg = self.cfg.clone();
            let fleet = Arc::clone(&self.fleet);
            let totals = Arc::clone(&self.totals);
            tokio::spawn(async move {
                let Ok(session) = Session::new(cfg, fleet) else {
                    return;
                };
                totals.session_opened();
                let session = session.with_shared(Arc::clone(&totals));
                let _ = session.run(stream).await;
                totals.session_closed();
            });
        }
    }
}

/// Returns a stable GUID for a virtual IOC.
///
/// Each IOC must have a distinct GUID. If two addresses advertise the same
/// GUID, clients may treat them as alternate routes to one server and use
/// only one, reducing the visible fleet.
///
/// This function is public because the search responder and TCP connection
/// must advertise the same GUID. Different GUIDs across UDP and TCP would
/// make a client interpret the connection as a different server.
pub fn guid_for(index: u16) -> [u8; mimus_wire::GUID_LEN] {
    let mut g = *b"MIMUS-------";
    g[6..8].copy_from_slice(&index.to_be_bytes());
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_are_distinct_per_ioc() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..1000u16 {
            assert!(seen.insert(guid_for(i)), "GUID collision at IOC {i}");
        }
    }

    #[test]
    fn a_guid_is_the_protocol_length() {
        assert_eq!(guid_for(7).len(), mimus_wire::GUID_LEN);
    }
}
