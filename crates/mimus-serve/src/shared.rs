//! Counters published by active sessions.
//!
//! If each session kept its own totals and reported them only when it ended,
//! running sessions would contribute nothing to the report. That is a problem
//! for benchmarks, which typically measure sessions while they are active.
//!
//! Instead, sessions publish their counters through shared atomics. To avoid
//! atomic operations on every event, each session accumulates counts locally
//! and adds them in batches when it flushes. This keeps the report up to date
//! without adding synchronization overhead to the hot path.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mimus_emit::Counters;

/// Aggregate counters for one virtual IOC, readable while sessions are active.
#[derive(Debug, Default)]
pub struct SharedCounters {
    emitted: AtomicU64,
    dropped: AtomicU64,
    bytes: AtomicU64,
    late_ticks: AtomicU64,
    wheel_steps: AtomicU64,
    pipeline_acks: AtomicU64,
    flow_control_holds: AtomicU64,
    sessions_open: AtomicU64,
    sessions_total: AtomicU64,
    sessions_dropped: AtomicU64,
    /// Number of monitors currently started across all sessions.
    ///
    /// This is a gauge, not a cumulative count. It distinguishes an offered
    /// load that produces too few updates from a scheduler that cannot keep
    /// up. These cases require different remedies.
    subscriptions: AtomicU64,
}

impl SharedCounters {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Adds a batch of session-local counters to the shared totals.
    ///
    /// Relaxed ordering is sufficient because these atomics collect
    /// statistics only. They do not provide synchronization or control
    /// execution flow.
    pub fn add(&self, batch: &Counters) {
        if batch.emitted > 0 {
            self.emitted.fetch_add(batch.emitted, Ordering::Relaxed);
        }
        if batch.dropped_backpressure > 0 {
            self.dropped
                .fetch_add(batch.dropped_backpressure, Ordering::Relaxed);
        }
        if batch.bytes > 0 {
            self.bytes.fetch_add(batch.bytes, Ordering::Relaxed);
        }
        if batch.wheel_steps > 0 {
            self.wheel_steps
                .fetch_add(batch.wheel_steps, Ordering::Relaxed);
        }
        if batch.late_ticks > 0 {
            self.late_ticks
                .fetch_add(batch.late_ticks, Ordering::Relaxed);
        }
        if batch.pipeline_acks > 0 {
            self.pipeline_acks
                .fetch_add(batch.pipeline_acks, Ordering::Relaxed);
        }
        if batch.flow_control_holds > 0 {
            self.flow_control_holds
                .fetch_add(batch.flow_control_holds, Ordering::Relaxed);
        }
    }

    pub fn session_opened(&self) {
        self.sessions_open.fetch_add(1, Ordering::Relaxed);
        self.sessions_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn session_closed(&self) {
        self.sessions_open.fetch_sub(1, Ordering::Relaxed);
    }

    /// Records a subscription terminated by MIMUS rather than closed by the client.
    ///
    /// Kept separate from normal closures so this count can be compared
    /// with the consumer's reconnection count.
    pub fn subscription_started(&self) {
        self.subscriptions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn subscription_stopped(&self) {
        self.subscriptions.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn subscriptions(&self) -> u64 {
        self.subscriptions.load(Ordering::Relaxed)
    }

    pub fn session_dropped(&self) {
        self.sessions_dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn sessions_dropped(&self) -> u64 {
        self.sessions_dropped.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Counters {
        Counters {
            emitted: self.emitted.load(Ordering::Relaxed),
            dropped_backpressure: self.dropped.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            late_ticks: self.late_ticks.load(Ordering::Relaxed),
            wheel_steps: self.wheel_steps.load(Ordering::Relaxed),
            pipeline_acks: self.pipeline_acks.load(Ordering::Relaxed),
            flow_control_holds: self.flow_control_holds.load(Ordering::Relaxed),
        }
    }

    pub fn sessions_open(&self) -> u64 {
        self.sessions_open.load(Ordering::Relaxed)
    }

    pub fn sessions_total(&self) -> u64 {
        self.sessions_total.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_are_visible_before_a_session_ends() {
        // Shared counters must include active sessions, not just sessions that have already closed.
        let c = SharedCounters::new();
        c.session_opened();
        c.add(&Counters {
            emitted: 100,
            bytes: 3600,
            ..Default::default()
        });
        let snap = c.snapshot();
        assert_eq!(snap.emitted, 100);
        assert_eq!(c.sessions_open(), 1, "still running");
    }

    #[test]
    fn batches_accumulate() {
        let c = SharedCounters::new();
        for _ in 0..10 {
            c.add(&Counters {
                emitted: 7,
                dropped_backpressure: 1,
                bytes: 70,
                ..Default::default()
            });
        }
        let s = c.snapshot();
        assert_eq!(s.emitted, 70);
        assert_eq!(s.dropped_backpressure, 10);
        assert_eq!(s.offered(), 80);
    }

    #[test]
    fn session_counts_track_open_and_total() {
        let c = SharedCounters::new();
        c.session_opened();
        c.session_opened();
        c.session_closed();
        assert_eq!(c.sessions_open(), 1);
        assert_eq!(c.sessions_total(), 2);
    }

    #[test]
    fn concurrent_writers_do_not_lose_updates() {
        let c = SharedCounters::new();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&c);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    c.add(&Counters {
                        emitted: 1,
                        ..Default::default()
                    });
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(c.snapshot().emitted, 8000);
    }
}
