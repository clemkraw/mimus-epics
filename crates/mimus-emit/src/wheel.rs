//! Scheduling a million channels at multiple rates.
//!
//! The naive approach, with one timer per PV, does not scale. A timing
//! wheel replaces it with a single timer: time is divided into ticks,
//! each channel is placed in the bucket for its next scheduled tick,
//! and each tick processes one bucket.
//!
//! The wheel supports two key capabilities:
//!
//! **Multiple updates per tick.** A channel is no longer limited to one
//! update per tick. It can emit several updates per firing, with timestamps
//! spread across the tick. This also reduces overhead: many updates can be
//! rendered into one buffer and written in a single operation.
//!
//! **Multiple rates in one run.** Different facilities run at different
//! rates, and a uniform rate changes the shape of the offered load,
//! not just its volume. A [`Schedule`] maintains one wheel per distinct rate.
//!
//! Channels are distributed across buckets by index to avoid synchronizing
//! all emissions on the same tick. A consumer receiving one large burst
//! per second would face an unrealistic load.

/// A channel instance.
///
/// Kept compact because the wheel walks these linearly and may hold
/// millions of instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instance {
    /// Index into the emitter's compiled-template table.
    pub template: u16,
    /// Virtual IOC serving this channel.
    pub ioc: u16,
    /// Rate group index into the schedule's wheels.
    pub rate_group: u16,
    /// Client request ID, assigned when the monitor is subscribed.
    pub request_id: i32,
    /// Update counter used for signal generation and sample replay.
    pub step: u64,
    /// Stable identity for reproducible signal generation.
    pub seed: u64,
}

impl Instance {
    pub fn new(template: u16, ioc: u16, seed: u64) -> Self {
        Self {
            template,
            ioc,
            rate_group: 0,
            request_id: 0,
            step: 0,
            seed,
        }
    }

    pub fn with_rate_group(mut self, group: u16) -> Self {
        self.rate_group = group;
        self
    }
}

/// Why a timing wheel could not be built.
///
/// Does not implement `Eq` because errors may contain floating-point rates.
#[derive(Debug, Clone, PartialEq)]
pub enum WheelError {
    /// Rate is zero, negative, or not finite.
    BadRate(String),
    /// Tick resolution is zero or longer than [`MAX_TICK_US`].
    BadTick(u64),
    /// One period of the rate spans more than [`MAX_BUCKETS`] ticks.
    TooSlow { rate_hz: f64, tick_us: u64 },
    /// The rate asks for more than [`MAX_REPEATS`] updates per channel in one tick.
    TooFast { rate_hz: f64, tick_us: u64 },
    /// Invalid rate distribution for the channel population.
    BadMix(String),
}

impl std::fmt::Display for WheelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRate(r) => write!(f, "invalid rate {r}"),
            Self::BadTick(t) => write!(
                f,
                "a tick resolution of {t} µs is outside 1 µs to {MAX_TICK_US} µs"
            ),
            Self::TooSlow { rate_hz, tick_us } => write!(
                f,
                "a rate of {rate_hz} Hz spans more than {MAX_BUCKETS} ticks of {tick_us} µs; \
                 use a coarser tick"
            ),
            Self::TooFast { rate_hz, tick_us } => write!(
                f,
                "a rate of {rate_hz} Hz is more than {MAX_REPEATS} updates per channel in \
                 one tick of {tick_us} µs; use a finer tick"
            ),
            Self::BadMix(m) => write!(f, "invalid rate mix: {m}"),
        }
    }
}

impl std::error::Error for WheelError {}

/// The coarsest tick accepted: one second. A tick is the scheduler's
/// resolution; a coarser one would only batch a second's updates together.
pub const MAX_TICK_US: u64 = 1_000_000;

/// The most buckets one wheel may have, one per tick of the slowest period.
///
/// Each bucket costs memory whether or not a channel uses it. Without a
/// bound, a very slow rate at a fine tick asks for the impossible: 1e-7 Hz
/// at 1 ms is ten billion buckets. At the default tick this allows one
/// update every 17 minutes; a coarser tick allows slower rates.
pub const MAX_BUCKETS: usize = 1 << 20;

/// The most updates one channel may be due in a single tick.
///
/// A tick renders all of them before anything is written, so a rate far
/// above the tick rate turns one tick into an unbounded burst. 65 536 per
/// millisecond tick is 65 MHz per channel.
pub const MAX_REPEATS: u32 = 1 << 16;

/// Timing wheel for channels sharing the same update rate.
#[derive(Debug)]
pub struct Wheel {
    buckets: Vec<Vec<u32>>,
    tick_us: u64,
    cursor: usize,
    period_ticks: usize,
    repeats: u32,
    rate_hz: f64,
}

impl Wheel {
    /// Creates a wheel for `rate_hz` updates per channel at `tick_us`
    /// resolution.
    ///
    /// Below one update per tick, channels are distributed across
    /// multiple buckets. Above one update per tick, each channel
    /// occupies a single bucket and emits `repeats` times per tick.
    /// No upper rate limit is imposed, allowing the benchmark to
    /// explore loads beyond typical IOC capabilities.
    pub fn new(rate_hz: f64, tick_us: u64) -> Result<Self, WheelError> {
        if !rate_hz.is_finite() || rate_hz <= 0.0 {
            return Err(WheelError::BadRate(rate_hz.to_string()));
        }
        if tick_us == 0 || tick_us > MAX_TICK_US {
            return Err(WheelError::BadTick(tick_us));
        }
        let period_us = 1_000_000.0 / rate_hz;
        let (period_ticks, repeats) = if period_us >= tick_us as f64 {
            let ticks = (period_us / tick_us as f64).round();
            if ticks > MAX_BUCKETS as f64 {
                return Err(WheelError::TooSlow { rate_hz, tick_us });
            }
            ((ticks as usize).max(1), 1)
        } else {
            let repeats = (tick_us as f64 / period_us).round();
            if repeats > f64::from(MAX_REPEATS) {
                return Err(WheelError::TooFast { rate_hz, tick_us });
            }
            (1, (repeats as u32).max(1))
        };
        Ok(Self {
            buckets: vec![Vec::new(); period_ticks],
            tick_us,
            cursor: 0,
            period_ticks,
            repeats,
            rate_hz,
        })
    }

    /// Inserts a channel, distributing channels evenly across the buckets.
    pub fn insert(&mut self, index: u32) {
        let bucket = (index as usize) % self.period_ticks;
        self.buckets[bucket].push(index);
    }

    /// Returns the channels due on the current tick.
    #[inline]
    pub fn current(&self) -> &[u32] {
        &self.buckets[self.cursor]
    }

    /// Returns the number of updates each due channel emits per tick.
    #[inline]
    pub fn repeats(&self) -> u32 {
        self.repeats
    }

    /// Returns the spacing between updates within a burst, in microseconds.
    ///
    /// Multiple updates within one tick need distinct timestamps to preserve
    /// realistic ordering and latency measurements.
    #[inline]
    pub fn burst_spacing_us(&self) -> u64 {
        if self.repeats <= 1 {
            0
        } else {
            self.tick_us / self.repeats as u64
        }
    }

    /// Returns the spacing between updates within a burst, in nanoseconds.
    ///
    /// Microsecond precision cannot distinguish updates above 1 MHz
    /// per channel with the default tick. Nanosecond precision keeps
    /// timestamps distinct up to 1 GHz.
    #[inline]
    pub fn burst_spacing_ns(&self) -> u64 {
        if self.repeats <= 1 {
            0
        } else {
            self.tick_us.saturating_mul(1_000) / self.repeats as u64
        }
    }

    /// Advances the wheel to the next tick.
    #[inline]
    pub fn advance(&mut self) {
        self.cursor = (self.cursor + 1) % self.period_ticks;
    }

    #[inline]
    pub fn rate_hz(&self) -> f64 {
        self.rate_hz
    }

    #[inline]
    pub fn tick_us(&self) -> u64 {
        self.tick_us
    }

    #[inline]
    pub fn ticks_per_period(&self) -> usize {
        self.period_ticks
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buckets.iter().all(Vec::is_empty)
    }

    /// Returns the size of the largest bucket, the worst case per tick.
    pub fn max_bucket(&self) -> usize {
        self.buckets.iter().map(Vec::len).max().unwrap_or(0)
    }

    /// Returns the difference between the largest and smallest buckets.
    ///
    /// An evenly distributed population has an imbalance of at most one.
    /// Larger values indicate uneven load distribution across ticks.
    pub fn imbalance(&self) -> usize {
        let max = self.buckets.iter().map(Vec::len).max().unwrap_or(0);
        let min = self.buckets.iter().map(Vec::len).min().unwrap_or(0);
        max - min
    }
}

/// Distribution of the population across update rates.
///
/// For example, `"10:2,1:60,0.1:38"` assigns relative shares of
/// 2 to 10 Hz, 60 to 1 Hz and 38 to 0.1 Hz. Shares need not sum
/// to 100. The resulting aggregate update rate describes the total
/// load and is worth reporting.
#[derive(Debug, Clone, PartialEq)]
pub struct RateMix {
    /// Rates and their relative shares, in input order.
    pub groups: Vec<(f64, u32)>,
}

impl RateMix {
    /// Creates a mix with one rate shared by all channels.
    pub fn uniform(rate_hz: f64) -> Self {
        Self {
            groups: vec![(rate_hz, 1)],
        }
    }

    /// Parses comma-separated `rate:share` pairs.
    ///
    /// Rejects invalid or non-positive rates and requires at least one
    /// group with a nonzero share.
    pub fn parse(spec: &str) -> Result<Self, WheelError> {
        let mut groups = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (rate, share) = part
                .split_once(':')
                .ok_or_else(|| WheelError::BadMix(format!("{part:?} is not rate:share")))?;
            let rate: f64 = rate
                .trim()
                .parse()
                .map_err(|_| WheelError::BadMix(format!("{rate:?} is not a rate")))?;
            let share: u32 = share
                .trim()
                .parse()
                .map_err(|_| WheelError::BadMix(format!("{share:?} is not a share")))?;
            if !rate.is_finite() || rate <= 0.0 {
                return Err(WheelError::BadMix(format!("rate {rate} must be positive")));
            }
            if share > 0 {
                groups.push((rate, share));
            }
        }
        if groups.is_empty() {
            return Err(WheelError::BadMix("no groups with a nonzero share".into()));
        }
        Ok(Self { groups })
    }

    /// Returns the rate group assigned to the channel at `index`.
    ///
    /// Repeats the group shares in a cycle rather than assigning contiguous
    /// blocks. This keeps the proportions roughly balanced even for small
    /// channel populations.
    pub fn group_of(&self, index: u32) -> u16 {
        let total: u32 = self.groups.iter().map(|(_, s)| *s).sum();
        let mut pos = index % total;
        for (i, (_, share)) in self.groups.iter().enumerate() {
            if pos < *share {
                return i as u16;
            }
            pos -= share;
        }
        0
    }

    /// Returns the aggregate update rate the mix's proportions imply for
    /// `channels` channels.
    ///
    /// Channels are assigned to groups in cycles of the shares, so a
    /// population that is not a whole number of cycles carries a somewhat
    /// different load, and a small one a very different load. `mimus serve`
    /// therefore reports the sum of the rates its channels were given.
    pub fn aggregate_rate(&self, channels: u32) -> f64 {
        let total: u32 = self.groups.iter().map(|(_, s)| *s).sum();
        self.groups
            .iter()
            .map(|(rate, share)| rate * channels as f64 * *share as f64 / total as f64)
            .sum()
    }
}

impl std::fmt::Display for RateMix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self
            .groups
            .iter()
            .map(|(r, s)| format!("{r}:{s}"))
            .collect();
        f.write_str(&parts.join(","))
    }
}

/// One timing wheel per rate group, advanced together.
#[derive(Debug)]
pub struct Schedule {
    wheels: Vec<Wheel>,
    /// Request IDs in slot order, one vector per wheel.
    slots: Vec<Vec<i32>>,
    /// Maps each live monitor to its position for constant-time removal.
    ///
    /// Without this index, removing a monitor required scanning every
    /// slot in every wheel. Disconnecting a client with many subscriptions
    /// therefore became quadratic in the number of monitors.
    index: std::collections::HashMap<i32, (usize, usize)>,
    /// Number of live monitors.
    ///
    /// Cached because `is_empty` is checked on every session-loop pass
    /// to decide whether to arm the tick. Recounting would scan every
    /// slot on every pass.
    live: usize,
}

/// A channel due for emission and the updates it must produce this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Due {
    pub request_id: i32,
    pub repeats: u32,
    /// Spacing between consecutive updates, in microseconds.
    pub spacing_us: u64,
    /// Spacing between consecutive updates, in nanoseconds.
    ///
    /// Used for timestamps when microsecond precision is insufficient.
    pub spacing_ns: u64,
}

impl Schedule {
    pub fn new(mix: &RateMix, tick_us: u64) -> Result<Self, WheelError> {
        let mut wheels = Vec::with_capacity(mix.groups.len());
        for (rate, _) in &mix.groups {
            wheels.push(Wheel::new(*rate, tick_us)?);
        }
        let slots = vec![Vec::new(); wheels.len()];
        Ok(Self {
            wheels,
            slots,
            index: std::collections::HashMap::new(),
            live: 0,
        })
    }

    /// Registers a monitor in its rate group.
    pub fn insert(&mut self, group: u16, request_id: i32) {
        let g = (group as usize).min(self.wheels.len() - 1);
        let slot = self.slots[g].len();
        self.slots[g].push(request_id);
        self.wheels[g].insert(slot as u32);
        self.index.insert(request_id, (g, slot));
        self.live += 1;
    }

    /// Removes a monitor from the schedule.
    ///
    /// Leaves a sentinel in its slot so that all other slot indices remain valid.
    pub fn remove(&mut self, request_id: i32) {
        let Some((g, slot)) = self.index.remove(&request_id) else {
            return;
        };
        if let Some(s) = self.slots.get_mut(g).and_then(|v| v.get_mut(slot)) {
            *s = i32::MIN;
        }
        self.live -= 1;
    }

    /// Collects all monitors due on the current tick, then advances every wheel.
    pub fn tick(&mut self) -> Vec<Due> {
        let mut out = Vec::new();
        for (g, wheel) in self.wheels.iter_mut().enumerate() {
            let repeats = wheel.repeats();
            let spacing_us = wheel.burst_spacing_us();
            let spacing_ns = wheel.burst_spacing_ns();
            for &slot in wheel.current() {
                let Some(&request_id) = self.slots[g].get(slot as usize) else {
                    continue;
                };
                if request_id == i32::MIN {
                    continue;
                }
                out.push(Due {
                    request_id,
                    repeats,
                    spacing_us,
                    spacing_ns,
                });
            }
            wheel.advance();
        }
        out
    }

    #[inline]
    pub fn groups(&self) -> usize {
        self.wheels.len()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.live
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn wheel(&self, group: usize) -> Option<&Wheel> {
        self.wheels.get(group)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_outside_one_microsecond_to_one_second_is_refused() {
        assert_eq!(Wheel::new(1.0, 0).unwrap_err(), WheelError::BadTick(0));
        assert!(Wheel::new(1.0, MAX_TICK_US).is_ok());
        assert_eq!(
            Wheel::new(1.0, MAX_TICK_US + 1).unwrap_err(),
            WheelError::BadTick(MAX_TICK_US + 1)
        );
    }

    #[test]
    fn a_rate_too_slow_for_the_tick_is_refused_not_allocated() {
        // 1e-7 Hz at 1 ms would be ten billion buckets: 240 GB requested
        // before a single channel is served.
        assert!(matches!(
            Wheel::new(1e-7, 1_000),
            Err(WheelError::TooSlow { .. })
        ));
        // The same rate fits once the tick is coarse enough, and the
        // slowest rate the default tick takes still works.
        assert!(Wheel::new(0.001, 1_000).is_ok());
        assert!(Wheel::new(1e-4, 1_000_000).is_ok());
    }

    #[test]
    fn a_rate_too_fast_for_the_tick_is_refused() {
        assert!(matches!(
            Wheel::new(1e9, 1_000),
            Err(WheelError::TooFast { .. })
        ));
        // The same rate at a finer tick is a thousand per tick.
        assert!(Wheel::new(1e9, 1).is_ok());
        assert!(Wheel::new(65_536_000.0, 1_000).is_ok());
    }

    #[test]
    fn one_hertz_with_millisecond_ticks_is_a_thousand_buckets() {
        let w = Wheel::new(1.0, 1000).unwrap();
        assert_eq!(w.ticks_per_period(), 1000);
        assert_eq!(w.repeats(), 1);
    }

    #[test]
    fn seven_hertz_rounds_to_the_nearest_tick() {
        let w = Wheel::new(7.0, 1000).unwrap();
        // 1/7 second is about 142.857 ms, rounded to 143 ticks.
        assert_eq!(w.ticks_per_period(), 143);
    }

    #[test]
    fn rates_above_one_per_tick_burst_instead_of_being_refused() {
        // Rates above the tick frequency are supported so the benchmark
        // can explore loads beyond what a soft IOC can produce.
        let w = Wheel::new(10_000.0, 1000).unwrap();
        assert_eq!(w.ticks_per_period(), 1);
        assert_eq!(w.repeats(), 10, "10 updates per millisecond tick");
    }

    #[test]
    fn a_megahertz_channel_is_a_burst_of_a_thousand() {
        let w = Wheel::new(1_000_000.0, 1000).unwrap();
        assert_eq!(w.repeats(), 1000);
        assert_eq!(w.burst_spacing_us(), 1, "one update per microsecond");
    }

    #[test]
    fn a_burst_spreads_its_timestamps_across_the_tick() {
        // Giving every update the same timestamp would produce unrealistic
        // traffic and invalidate latency measurements.
        let w = Wheel::new(4000.0, 1000).unwrap();
        assert_eq!(w.repeats(), 4);
        assert_eq!(w.burst_spacing_us(), 250);
    }

    #[test]
    fn a_single_update_per_tick_needs_no_spacing() {
        assert_eq!(Wheel::new(1.0, 1000).unwrap().burst_spacing_us(), 0);
        assert_eq!(Wheel::new(1.0, 1000).unwrap().burst_spacing_ns(), 0);
    }

    #[test]
    fn a_burst_above_a_megahertz_keeps_distinct_timestamps() {
        // At 10 MHz, a 1 ms tick contains 10,000 updates spaced 100 ns apart.
        // Microsecond precision would give every update the same timestamp.
        let w = Wheel::new(10_000_000.0, 1000).unwrap();
        assert_eq!(w.repeats(), 10_000);
        assert_eq!(w.burst_spacing_us(), 0);
        assert_eq!(w.burst_spacing_ns(), 100);
        // All updates fit within the tick.
        assert!(w.burst_spacing_ns() * w.repeats() as u64 <= 1_000_000);
    }

    #[test]
    fn nanosecond_spacing_agrees_with_microsecond_spacing_where_both_work() {
        for rate in [2_000.0, 4_000.0, 10_000.0, 100_000.0, 1_000_000.0] {
            let w = Wheel::new(rate, 1000).unwrap();
            assert_eq!(
                w.burst_spacing_ns(),
                w.burst_spacing_us() * 1_000,
                "{rate} Hz"
            );
        }
    }

    #[test]
    fn a_due_channel_carries_the_nanosecond_spacing() {
        let mix = RateMix::uniform(3_000_000.0);
        let mut s = Schedule::new(&mix, 1000).unwrap();
        s.insert(0, 7);
        let due = s.tick();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].repeats, 3_000);
        assert_eq!(due[0].spacing_ns, 333);
    }

    #[test]
    fn a_million_instances_spread_evenly() {
        let mut w = Wheel::new(1.0, 1000).unwrap();
        for i in 0..1_000_000u32 {
            w.insert(i);
        }
        assert_eq!(w.len(), 1_000_000);
        assert_eq!(w.max_bucket(), 1000);
        assert_eq!(w.imbalance(), 0, "load must not arrive in bursts");
    }

    #[test]
    fn an_uneven_population_is_off_by_at_most_one() {
        let mut w = Wheel::new(1.0, 1000).unwrap();
        for i in 0..1_000_007u32 {
            w.insert(i);
        }
        assert_eq!(w.imbalance(), 1);
    }

    #[test]
    fn every_instance_fires_exactly_once_per_period() {
        let mut w = Wheel::new(1.0, 1000).unwrap();
        for i in 0..5000u32 {
            w.insert(i);
        }
        let mut seen = vec![0u32; 5000];
        for _ in 0..w.ticks_per_period() {
            for &idx in w.current() {
                seen[idx as usize] += 1;
            }
            w.advance();
        }
        assert!(seen.iter().all(|&n| n == 1));
    }

    #[test]
    fn rejects_nonsense_rates() {
        assert!(Wheel::new(0.0, 1000).is_err());
        assert!(Wheel::new(-1.0, 1000).is_err());
        assert!(Wheel::new(f64::NAN, 1000).is_err());
        assert!(Wheel::new(1.0, 0).is_err());
    }

    #[test]
    fn an_instance_is_small() {
        // At one million channels, each additional byte costs one megabyte.
        assert!(
            std::mem::size_of::<Instance>() <= 32,
            "Instance grew to {} bytes",
            std::mem::size_of::<Instance>()
        );
    }

    // ------------------------------------------------------------ rate mix

    #[test]
    fn a_uniform_mix_is_one_group() {
        let m = RateMix::uniform(5.0);
        assert_eq!(m.groups, vec![(5.0, 1)]);
        assert_eq!(m.group_of(12345), 0);
    }

    #[test]
    fn a_mix_parses_and_keeps_its_order() {
        let m = RateMix::parse("10:2,1:60,0.1:38").unwrap();
        assert_eq!(m.groups, vec![(10.0, 2), (1.0, 60), (0.1, 38)]);
        assert_eq!(m.to_string(), "10:2,1:60,0.1:38");
    }

    #[test]
    fn groups_are_assigned_in_the_declared_proportions() {
        let m = RateMix::parse("10:2,1:60,0.1:38").unwrap();
        let mut counts = [0usize; 3];
        for i in 0..100_000u32 {
            counts[m.group_of(i) as usize] += 1;
        }
        assert_eq!(counts, [2000, 60_000, 38_000]);
    }

    #[test]
    fn the_proportions_hold_for_an_awkward_population() {
        // The repeating pattern preserves group proportions across small
        // populations, unlike contiguous blocks that may omit later groups.
        let m = RateMix::parse("10:2,1:60,0.1:38").unwrap();
        let mut counts = [0usize; 3];
        for i in 0..7u32 {
            counts[m.group_of(i) as usize] += 1;
        }
        assert_eq!(counts.iter().sum::<usize>(), 7);
        assert!(counts[1] > 0, "the majority group must appear");
    }

    #[test]
    fn aggregate_rate_is_the_number_that_describes_the_run() {
        let m = RateMix::parse("10:2,1:60,0.1:38").unwrap();
        let agg = m.aggregate_rate(1_000_000);
        // Weighted average: 2% at 10 Hz, 60% at 1 Hz, and 38% at 0.1 Hz.
        assert!((agg - 838_000.0).abs() < 1.0, "got {agg}");
    }

    #[test]
    fn a_uniform_mix_aggregates_to_the_obvious_thing() {
        assert_eq!(RateMix::uniform(10.0).aggregate_rate(1000), 10_000.0);
    }

    #[test]
    fn a_malformed_mix_is_refused_rather_than_guessed() {
        // Reject invalid input rather than silently changing the requested
        // rate distribution and invalidating the benchmark.
        assert!(RateMix::parse("").is_err());
        assert!(RateMix::parse("10").is_err());
        assert!(RateMix::parse("abc:1").is_err());
        assert!(RateMix::parse("10:x").is_err());
        assert!(RateMix::parse("0:50").is_err());
        assert!(RateMix::parse("-1:50").is_err());
        assert!(RateMix::parse("10:0").is_err());
    }

    // ------------------------------------------------------------ schedule

    fn drain(sched: &mut Schedule, ticks: usize) -> Vec<Due> {
        let mut all = Vec::new();
        for _ in 0..ticks {
            all.extend(sched.tick());
        }
        all
    }

    #[test]
    fn a_schedule_has_one_wheel_per_rate() {
        let m = RateMix::parse("10:1,1:1").unwrap();
        let s = Schedule::new(&m, 1000).unwrap();
        assert_eq!(s.groups(), 2);
    }

    #[test]
    fn each_group_fires_at_its_own_rate() {
        let m = RateMix::parse("10:1,1:1").unwrap();
        let mut s = Schedule::new(&m, 1000).unwrap();
        s.insert(0, 100); // 10 Hz
        s.insert(1, 200); // 1 Hz

        let all = drain(&mut s, 1000); // one second of ticks
        let fast = all.iter().filter(|d| d.request_id == 100).count();
        let slow = all.iter().filter(|d| d.request_id == 200).count();
        assert_eq!(fast, 10);
        assert_eq!(slow, 1);
    }

    #[test]
    fn a_high_rate_group_reports_repeats_rather_than_firing_more_often() {
        let m = RateMix::parse("5000:1").unwrap();
        let mut s = Schedule::new(&m, 1000).unwrap();
        s.insert(0, 7);
        let all = drain(&mut s, 100);
        assert_eq!(all.len(), 100, "once per tick");
        assert!(all.iter().all(|d| d.repeats == 5));
        // 100 ticks × 5 updates = 500 updates in 100 ms, or 5000 Hz.
        let total: u32 = all.iter().map(|d| d.repeats).sum();
        assert_eq!(total, 500);
    }

    #[test]
    fn a_removed_monitor_stops_firing_without_shifting_the_others() {
        // Slots retain their positions after removal, so other monitors
        // remain associated with the correct request IDs.
        let m = RateMix::uniform(1000.0);
        let mut s = Schedule::new(&m, 1000).unwrap();
        for id in [1, 2, 3] {
            s.insert(0, id);
        }
        s.remove(2);
        let all = drain(&mut s, 1);
        let ids: Vec<i32> = all.iter().map(|d| d.request_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&3));
        assert!(!ids.contains(&2));
    }

    #[test]
    fn an_out_of_range_group_does_not_panic() {
        let m = RateMix::uniform(1.0);
        let mut s = Schedule::new(&m, 1000).unwrap();
        s.insert(99, 5);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn the_schedule_counts_live_monitors_only() {
        let m = RateMix::parse("1:1,2:1").unwrap();
        let mut s = Schedule::new(&m, 1000).unwrap();
        s.insert(0, 1);
        s.insert(1, 2);
        assert_eq!(s.len(), 2);
        s.remove(1);
        assert_eq!(s.len(), 1);
    }
}
#[cfg(test)]
mod schedule_cost_tests {
    use super::*;
    use std::time::Instant;

    fn filled(n: usize) -> Schedule {
        let mut s = Schedule::new(&RateMix::uniform(1.0), 1000).unwrap();
        for i in 0..n {
            s.insert(0, i as i32);
        }
        s
    }

    #[test]
    fn asking_whether_anything_is_scheduled_does_not_walk_it() {
        // The session loop checks this on every pass to decide whether
        // to arm the tick. Recounting slots would make the check scale
        // with the number of subscriptions.
        let small = filled(100);
        let large = filled(200_000);

        let t0 = Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(small.is_empty());
        }
        let small_ns = t0.elapsed().as_nanos();

        let t0 = Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(large.is_empty());
        }
        let large_ns = t0.elapsed().as_nanos();

        // A 2,000-fold increase in population should have little impact.
        // The generous bound tests the scaling behavior, not absolute speed.
        assert!(
            large_ns < small_ns * 10 + 1_000_000,
            "is_empty scales with the population: {small_ns} ns vs {large_ns} ns"
        );
    }

    #[test]
    fn removing_a_monitor_does_not_search_for_it() {
        // Disconnecting a client removes its monitors one by one.
        // Scanning all slots for each removal would make teardown quadratic.
        let mut s = filled(50_000);
        let t0 = Instant::now();
        for i in 0..50_000 {
            s.remove(i);
        }
        let elapsed = t0.elapsed();
        assert!(s.is_empty());
        assert!(
            elapsed.as_millis() < 200,
            "tearing down 50k monitors took {elapsed:?}"
        );
    }

    #[test]
    fn the_count_survives_removal_and_reinsertion() {
        let mut s = filled(10);
        assert_eq!(s.len(), 10);
        s.remove(3);
        assert_eq!(s.len(), 9);
        // Removing the same monitor twice must not decrement the count twice.
        s.remove(3);
        assert_eq!(s.len(), 9);
        // Removing an unknown monitor must not change the count.
        s.remove(999);
        assert_eq!(s.len(), 9);
        s.insert(0, 42);
        assert_eq!(s.len(), 10);
    }

    #[test]
    fn a_removed_monitor_still_stops_firing() {
        // The index locates the slot but does not replace it.
        // Marking the slot with a sentinel prevents the wheel from
        // emitting updates for a removed monitor.
        let mut s = Schedule::new(&RateMix::uniform(1000.0), 1000).unwrap();
        for id in [1, 2, 3] {
            s.insert(0, id);
        }
        s.remove(2);
        let due: Vec<i32> = s.tick().into_iter().map(|d| d.request_id).collect();
        assert!(due.contains(&1));
        assert!(due.contains(&3));
        assert!(!due.contains(&2));
    }
}
