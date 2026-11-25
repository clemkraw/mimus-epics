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
    ///
    /// While a scenario holds the IOC down, its listening socket is closed,
    /// so a client trying to connect is refused, as it is by a host whose IOC
    /// process has exited, rather than left waiting in the accept queue. The
    /// socket is bound again, on the same address, when the IOC comes back.
    pub async fn serve(self) {
        let Self {
            index,
            addr,
            listener,
            cfg,
            fleet,
            totals,
        } = self;
        let mut listener = Some(listener);
        let mut stage = cfg.stages.as_ref().and_then(|s| s.watch(index));
        loop {
            if stage.as_ref().is_some_and(|r| r.borrow().down) {
                listener = None;
                if let Some(r) = stage.as_mut()
                    && r.changed().await.is_err()
                {
                    // The scenario is over and left the IOC down.
                    return;
                }
                continue;
            }
            let Some(open) = &listener else {
                match rebind(addr).await {
                    Ok(l) => listener = Some(l),
                    Err(e) => {
                        eprintln!(
                            "warning: IOC {index} cannot listen on {addr} again ({e}); retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                continue;
            };
            tokio::select! {
                accepted = open.accept() => {
                    let Ok((stream, _peer)) = accepted else {
                        continue;
                    };
                    let cfg = cfg.clone();
                    let fleet = Arc::clone(&fleet);
                    let totals = Arc::clone(&totals);
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
                changed = changed(&mut stage) => {
                    if !changed {
                        // The scenario is over: follow nothing from now on.
                        stage = None;
                    }
                }
            }
        }
    }
}

/// Completes with `true` when the stage changes, with `false` when the
/// scenario has ended, and never when there is none.
async fn changed(stage: &mut Option<tokio::sync::watch::Receiver<crate::stage::Stage>>) -> bool {
    match stage {
        Some(r) => r.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Binds a listener again on the address the IOC had.
///
/// The address is reused at once, as a restarted IOC does: the connections
/// closed when it went down may still be in TIME_WAIT on this side.
async fn rebind(addr: std::net::SocketAddr) -> std::io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
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
