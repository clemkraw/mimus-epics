//! Emission accounting.
//!
//! MIMUS tracks how many updates it emits and how many it drops.
//! These counts let consumers verify end-to-end update delivery.
//! Without them, a zero-loss claim covers only the network boundary.
//!
//! Counters are plain and local to each emitter. Each virtual IOC
//! maintains its own counts, which are merged at the end of the run.
//! No atomic operations are needed on the hot path.

/// Emission totals for one emitter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Updates successfully written to the wire.
    pub emitted: u64,
    /// Updates discarded because the consumer could not keep up.
    /// Always counted, never silently dropped.
    pub dropped_backpressure: u64,
    /// Bytes written, including headers.
    pub bytes: u64,
    /// Credit updates received from pipelined clients.
    pub pipeline_acks: u64,
    /// Updates withheld because a pipelined client had no available credit.
    /// Nonzero only under the strict policy. High values indicate that
    /// the consumer, rather than the emitter, is the bottleneck.
    pub flow_control_holds: u64,
    /// Times a session fell more than 16 ticks behind and skipped the rest
    /// instead of sending it late. What was skipped is never sent, so a
    /// nonzero value means the configured rate was not delivered in full.
    pub late_ticks: u64,
    /// Number of wheel steps taken, used as the denominator for reported rates.
    ///
    /// Needed to distinguish an incorrect wheel speed from an incorrect
    /// population. Both can produce the same update count but require
    /// different fixes.
    pub wheel_steps: u64,
    /// Updates a scenario held back: due while their virtual IOC was frozen,
    /// and never sent. An IOC that stops processing does not send late what
    /// it did not process, so these are not losses of the consumer's making.
    pub held: u64,
    /// Updates sent by a scenario's bursts, and by frozen IOCs resuming:
    /// a part of `emitted`, counted apart so a report can say which phase
    /// asked for them.
    pub burst: u64,
}

impl Counters {
    /// Returns the total number of updates offered, including dropped updates.
    #[inline]
    pub fn offered(&self) -> u64 {
        self.emitted + self.dropped_backpressure
    }

    /// Returns the fraction of offered updates that reached the wire.
    ///
    /// Returns `None` if no updates were offered.
    pub fn delivery_ratio(&self) -> Option<f64> {
        let offered = self.offered();
        (offered > 0).then(|| self.emitted as f64 / offered as f64)
    }

    /// Returns the average number of bytes per emitted update.
    ///
    /// Returns `None` if no updates were emitted.
    pub fn bytes_per_event(&self) -> Option<f64> {
        (self.emitted > 0).then(|| self.bytes as f64 / self.emitted as f64)
    }

    /// Adds another emitter's counters to these totals.
    pub fn merge(&mut self, other: &Self) {
        self.emitted += other.emitted;
        self.dropped_backpressure += other.dropped_backpressure;
        self.bytes += other.bytes;
        self.late_ticks += other.late_ticks;
        self.wheel_steps += other.wheel_steps;
        self.pipeline_acks += other.pipeline_acks;
        self.flow_control_holds += other.flow_control_holds;
        self.held += other.held;
        self.burst += other.burst;
    }

    /// Returns what was counted between `earlier` and these totals.
    ///
    /// Both must come from the same counters, `earlier` first, as when a
    /// scenario takes a snapshot at each phase boundary. A count that went
    /// down, which those cannot, gives zero rather than wrapping around.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            emitted: self.emitted.saturating_sub(earlier.emitted),
            dropped_backpressure: self
                .dropped_backpressure
                .saturating_sub(earlier.dropped_backpressure),
            bytes: self.bytes.saturating_sub(earlier.bytes),
            pipeline_acks: self.pipeline_acks.saturating_sub(earlier.pipeline_acks),
            flow_control_holds: self
                .flow_control_holds
                .saturating_sub(earlier.flow_control_holds),
            late_ticks: self.late_ticks.saturating_sub(earlier.late_ticks),
            wheel_steps: self.wheel_steps.saturating_sub(earlier.wheel_steps),
            held: self.held.saturating_sub(earlier.held),
            burst: self.burst.saturating_sub(earlier.burst),
        }
    }
}

/// What to do when the consumer stops reading.
///
/// Each policy measures a different aspect of performance, so published
/// results must identify which one was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backpressure {
    /// Let TCP throttle the emitter. The achieved rate measures the
    /// consumer's actual absorption capacity.
    #[default]
    Block,
    /// Maintain the nominal rate and count updates that cannot be sent
    /// as dropped. Models an IOC monitor queue overflowing under load.
    Drop,
}

impl std::fmt::Display for Backpressure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Block => "block",
            Self::Drop => "drop",
        })
    }
}

impl std::str::FromStr for Backpressure {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "block" => Ok(Self::Block),
            "drop" => Ok(Self::Drop),
            other => Err(format!("unknown backpressure policy {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phase_counts_what_happened_since_its_start() {
        let start = Counters {
            emitted: 100,
            bytes: 4000,
            held: 7,
            ..Default::default()
        };
        let end = Counters {
            emitted: 250,
            bytes: 10_000,
            held: 7,
            late_ticks: 2,
            ..Default::default()
        };
        let phase = end.since(&start);
        assert_eq!(phase.emitted, 150);
        assert_eq!(phase.bytes, 6000);
        assert_eq!(phase.held, 0);
        assert_eq!(phase.late_ticks, 2);
        assert_eq!(
            start.since(&end).emitted,
            0,
            "saturates instead of wrapping"
        );
    }

    #[test]
    fn offered_is_emitted_plus_dropped() {
        let c = Counters {
            emitted: 990,
            dropped_backpressure: 10,
            ..Default::default()
        };
        assert_eq!(c.offered(), 1000);
        assert!((c.delivery_ratio().unwrap() - 0.99).abs() < 1e-9);
    }

    #[test]
    fn ratios_are_absent_rather_than_zero_when_nothing_ran() {
        let c = Counters::default();
        assert!(c.delivery_ratio().is_none());
        assert!(c.bytes_per_event().is_none());
    }

    #[test]
    fn bytes_per_event_reflects_the_wire() {
        let c = Counters {
            emitted: 100,
            bytes: 3600,
            ..Default::default()
        };
        assert_eq!(c.bytes_per_event().unwrap(), 36.0);
    }

    #[test]
    fn merging_sums_every_field() {
        let mut a = Counters {
            emitted: 1,
            dropped_backpressure: 2,
            bytes: 3,
            late_ticks: 4,
            wheel_steps: 40,
            pipeline_acks: 5,
            flow_control_holds: 6,
            held: 7,
            burst: 8,
        };
        a.merge(&Counters {
            emitted: 10,
            dropped_backpressure: 20,
            bytes: 30,
            late_ticks: 40,
            wheel_steps: 400,
            pipeline_acks: 50,
            flow_control_holds: 60,
            held: 70,
            burst: 80,
        });
        assert_eq!(
            a,
            Counters {
                emitted: 11,
                dropped_backpressure: 22,
                bytes: 33,
                late_ticks: 44,
                wheel_steps: 440,
                pipeline_acks: 55,
                flow_control_holds: 66,
                held: 77,
                burst: 88,
            }
        );
    }

    #[test]
    fn backpressure_parses_both_ways() {
        assert_eq!("block".parse(), Ok(Backpressure::Block));
        assert_eq!("drop".parse(), Ok(Backpressure::Drop));
        assert!("wait".parse::<Backpressure>().is_err());
        assert_eq!(Backpressure::Drop.to_string(), "drop");
    }
}
