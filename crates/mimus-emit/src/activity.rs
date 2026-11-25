//! Irregular updates: channels that alternate between bursts and silences.
//!
//! A real IOC does not send every channel at its scan rate. A monitor with a
//! deadband sends only when the value has moved far enough, so a channel
//! posts at the scan rate while its process moves, and not at all while it
//! is still. A consumer that sees every channel post like a clock is spared
//! the irregular arrivals, gaps and runs a facility puts on it, and is
//! flattered by its figures.
//!
//! [`Activity`] gives each channel that behavior. A channel is either active,
//! and sends each update it is due, or quiet, and sends none. After each
//! update it is due, an active channel goes quiet with probability `1 / L`,
//! and a quiet one wakes with the probability that keeps it active a
//! fraction `p` of the time. Its bursts then last `L` updates on average,
//! its silences `L (1 - p) / p`, and it sends `p` of what its rate asks.
//!
//! The draws come from the channel's seed and the count of updates it has
//! been due, so a channel goes through the same bursts and silences on
//! every run and every connection.

/// How often channels post, and in what runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Activity {
    /// Fraction of the time a channel is active, above 0 and up to 1.
    pub fraction: f64,
    /// Updates in a burst, on average; at least 1.
    pub mean_burst: f64,
}

impl Activity {
    /// Parses `FRACTION:BURST`, such as `0.3:20`: active 30% of the time, in
    /// bursts of 20 updates on average.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (fraction, burst) = spec
            .split_once(':')
            .ok_or_else(|| format!("{spec:?} is not FRACTION:BURST, such as 0.3:20"))?;
        let fraction: f64 = fraction
            .trim()
            .parse()
            .map_err(|_| format!("{fraction:?} is not a fraction"))?;
        let mean_burst: f64 = burst
            .trim()
            .parse()
            .map_err(|_| format!("{burst:?} is not a number of updates"))?;
        Self::new(fraction, mean_burst)
    }

    /// Checks that a channel can be active `fraction` of the time in bursts
    /// of `mean_burst` updates.
    pub fn new(fraction: f64, mean_burst: f64) -> Result<Self, String> {
        if !(fraction.is_finite() && fraction > 0.0 && fraction <= 1.0) {
            return Err(format!(
                "the fraction {fraction} is not above 0 and up to 1"
            ));
        }
        if !(mean_burst.is_finite() && mean_burst >= 1.0) {
            return Err(format!(
                "a burst of {mean_burst} updates is shorter than one update"
            ));
        }
        // A silence lasts at least one update, so a channel active most of
        // the time needs long bursts.
        if fraction < 1.0 && mean_burst * (1.0 - fraction) / fraction < 1.0 {
            return Err(format!(
                "a channel active {}% of the time has silences of at least one update, \
                 so its bursts must last {:.1} updates or more on average, not {mean_burst}",
                fraction * 100.0,
                fraction / (1.0 - fraction)
            ));
        }
        Ok(Self {
            fraction,
            mean_burst,
        })
    }

    /// Whether every channel is always active: no model to apply.
    pub fn is_constant(&self) -> bool {
        self.fraction >= 1.0
    }

    /// Whether the channel seeded with `seed` starts active.
    pub fn starts_active(&self, seed: u64) -> bool {
        unit(mix(seed ^ 0x5851_F42D_4C95_7F2D)) < self.fraction
    }

    /// Returns whether the channel is active for the update after its
    /// `n`th, given whether it was active for the `n`th.
    pub fn next(&self, active: bool, seed: u64, n: u64) -> bool {
        if self.is_constant() {
            return true;
        }
        let draw = unit(mix(seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)));
        if active {
            draw >= 1.0 / self.mean_burst
        } else {
            draw < self.fraction / (self.mean_burst * (1.0 - self.fraction))
        }
    }
}

impl std::fmt::Display for Activity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.fraction, self.mean_burst)
    }
}

/// The finalizer of SplitMix64: spreads nearby inputs over the whole range.
fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Maps 64 random bits to a fraction in [0, 1).
fn unit(x: u64) -> f64 {
    (x >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays `n` updates of the channel seeded with `seed`, and returns the
    /// fraction it was active and the mean length of its bursts.
    fn run(a: &Activity, seed: u64, n: u64) -> (f64, f64) {
        let mut active = a.starts_active(seed);
        let (mut on, mut bursts) = (0u64, 0u64);
        let mut was = false;
        for k in 0..n {
            if active {
                on += 1;
                if !was {
                    bursts += 1;
                }
            }
            was = active;
            active = a.next(active, seed, k);
        }
        (on as f64 / n as f64, on as f64 / bursts.max(1) as f64)
    }

    #[test]
    fn a_channel_is_active_the_fraction_asked_in_bursts_of_the_length_asked() {
        for (fraction, burst) in [(0.3, 20.0), (0.05, 3.0), (0.8, 50.0)] {
            let a = Activity::new(fraction, burst).unwrap();
            for seed in [1u64, 42, 0xDEAD_BEEF] {
                let (f, b) = run(&a, seed, 2_000_000);
                assert!(
                    (f - fraction).abs() < fraction * 0.1,
                    "{fraction}:{burst} seed {seed}: active {f}"
                );
                assert!(
                    (b - burst).abs() < burst * 0.1,
                    "{fraction}:{burst} seed {seed}: bursts of {b}"
                );
            }
        }
    }

    #[test]
    fn the_same_channel_goes_through_the_same_bursts() {
        let a = Activity::new(0.3, 20.0).unwrap();
        let trace = |seed| {
            let mut active = a.starts_active(seed);
            (0..500)
                .map(|k| {
                    let now = active;
                    active = a.next(active, seed, k);
                    now
                })
                .collect::<Vec<bool>>()
        };
        assert_eq!(trace(7), trace(7));
        assert_ne!(trace(7), trace(8), "different channels, different runs");
    }

    #[test]
    fn channels_start_in_both_states_in_proportion() {
        let a = Activity::new(0.25, 10.0).unwrap();
        let active = (0..100_000u64).filter(|&s| a.starts_active(s)).count();
        assert!((23_000..27_000).contains(&active), "{active}");
    }

    #[test]
    fn what_cannot_be_modeled_is_refused() {
        assert!(Activity::parse("0.3:20").is_ok());
        assert!(Activity::parse("1:1").unwrap().is_constant());
        for bad in ["0.3", "0:5", "1.5:5", "0.3:0.5", "x:5", "0.3:y", "0.9:2"] {
            assert!(Activity::parse(bad).is_err(), "{bad} was accepted");
        }
        let e = Activity::parse("0.9:2").unwrap_err();
        assert!(e.contains("9.0 updates or more"), "{e}");
    }
}
