//! Filling template slots with timestamps and values.

use mimus_profile::ScalarType;

/// Seconds between the POSIX epoch and the EPICS epoch (1990-01-01).
///
/// Kept for reference and for converting record timestamps.
/// Not applied to wire timestamps; see [`EpicsTime::now`].
pub const EPICS_EPOCH_OFFSET: i64 = 631_152_000;

/// A timestamp in the format used on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EpicsTime {
    pub seconds: i64,
    pub nanos: i32,
}

impl EpicsTime {
    /// Returns the current system time.
    ///
    /// Despite its name, `secondsPastEpoch` on the wire counts from the
    /// POSIX epoch, not the EPICS epoch. QSRV2 converts the IOC's internal
    /// timestamp before sending it. This was confirmed against a real
    /// `softIocPVX`, which reported an unshifted POSIX timestamp.
    ///
    /// Applying the EPICS epoch offset here would produce timestamps
    /// twenty years too early. They would still decode correctly, but
    /// any latency measurements based on them would be invalid.
    ///
    /// Use wall-clock time rather than a counter: clients may compare
    /// this timestamp with the time an event reaches storage.
    pub fn now() -> Self {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            seconds: d.as_secs() as i64,
            nanos: d.subsec_nanos() as i32,
        }
    }

    /// Creates a timestamp from explicit components.
    ///
    /// Useful for reproducible tests and benchmarks.
    pub const fn from_parts(seconds: i64, nanos: i32) -> Self {
        Self { seconds, nanos }
    }

    /// Adds microseconds, normalizing the nanosecond field.
    ///
    /// Used to spread updates within a sub-tick burst. Giving every update
    /// the same timestamp would misrepresent IOC behavior and invalidate
    /// latency measurements based on this field.
    pub fn plus_micros(self, us: u64) -> Self {
        self.plus_nanos(us.saturating_mul(1_000))
    }

    /// Adds nanoseconds, normalizing the nanosecond field.
    ///
    /// Required for bursts faster than one update per microsecond. At 10 MHz
    /// on a 1 ms tick, updates are 100 ns apart. Microsecond precision would
    /// round this spacing to zero, giving all 10,000 updates the same timestamp.
    pub fn plus_nanos(self, ns: u64) -> Self {
        let total = self.nanos as i128 + ns as i128;
        Self {
            seconds: self.seconds + (total / 1_000_000_000) as i64,
            nanos: (total % 1_000_000_000) as i32,
        }
    }

    /// Returns the latest instant at or before `self` aligned to `period_ns`,
    /// with boundaries phased from the POSIX epoch.
    ///
    /// Periodically processed records retain the timestamp of their last scan
    /// when read between scans, rather than the time of the read. A zero
    /// period has no boundaries, so `self` is returned unchanged.
    pub fn last_boundary(self, period_ns: u64, phase_ns: u64) -> Self {
        if period_ns == 0 {
            return self;
        }
        let t = self.seconds as i128 * 1_000_000_000 + self.nanos as i128;
        let period = period_ns as i128;
        let phase = (phase_ns % period_ns) as i128;
        let b = (t - phase).div_euclid(period) * period + phase;
        Self {
            seconds: b.div_euclid(1_000_000_000) as i64,
            nanos: b.rem_euclid(1_000_000_000) as i32,
        }
    }
}

/// How a classified value slot is generated.
///
/// Only classified slots use a signal generator. All others replay
/// observed samples and need no generator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    /// Sawtooth between `min` and `max`, advancing by `step` per update.
    Ramp { min: f64, max: f64, step: f64 },
    /// Sine wave with a period measured in updates.
    Sine {
        min: f64,
        max: f64,
        period_updates: u32,
    },
    /// Uniform noise, deterministic for a given instance seed.
    ///
    /// This makes benchmark runs reproducible.
    Noise { min: f64, max: f64 },
    /// Constant value, useful for testing consumer deduplication.
    Constant { value: f64 },
}

impl Signal {
    /// Returns the value for update `step` of an instance seeded with `seed`.
    ///
    /// This function is pure: the same inputs always produce the same
    /// output. This makes runs reproducible and benchmarks comparable.
    pub fn sample(&self, step: u64, seed: u64) -> f64 {
        match *self {
            Self::Constant { value } => value,
            Self::Ramp { min, max, step: d } => {
                let span = max - min;
                if span <= 0.0 || d == 0.0 {
                    return min;
                }
                let travelled = (step as f64) * d;
                min + travelled % span
            }
            Self::Sine {
                min,
                max,
                period_updates,
            } => {
                if period_updates == 0 {
                    return min;
                }
                // Offset each instance to avoid synchronized scans.
                // Otherwise, aggregate load would fluctuate instead of
                // remaining steady. The offset also avoids a midpoint
                // value at step 0, which could produce all-zero bytes.
                let offset = seed % period_updates as u64;
                let phase = (step.wrapping_add(offset) % period_updates as u64) as f64
                    / period_updates as f64;
                let unit = (phase * std::f64::consts::TAU).sin() * 0.5 + 0.5;
                min + unit * (max - min)
            }
            Self::Noise { min, max } => {
                let r = xorshift(seed ^ step.wrapping_mul(0x9E37_79B9_7F4A_7C15));
                min + (r as f64 / u64::MAX as f64) * (max - min)
            }
        }
    }

    /// Returns a default signal for the given scalar type.
    ///
    /// Floating-point values use a sine wave; other types use a ramp.
    pub fn default_for(scalar: ScalarType) -> Self {
        match scalar {
            ScalarType::F64 | ScalarType::F32 => Self::Sine {
                min: -100.0,
                max: 100.0,
                period_updates: 360,
            },
            _ => Self::Ramp {
                min: 0.0,
                max: 65_536.0,
                step: 1.0,
            },
        }
    }
}

#[inline]
fn xorshift(mut x: u64) -> u64 {
    // Keep this lightweight: it runs on the hot path, and a full RNG
    // crate would add overhead without meaningful benefit.
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// Encodes a value into `out` using little-endian byte order.
///
/// A destination with the wrong width is left unchanged. Template
/// compilation should prevent this case, so reaching it indicates
/// an upstream error. On a hot path, a no-op is preferable to
/// terminating a long-running benchmark.
#[inline]
pub fn write_scalar(value: f64, scalar: ScalarType, out: &mut [u8]) {
    if let Some(width) = scalar.fixed_width()
        && out.len() != width
    {
        return;
    }
    match scalar {
        ScalarType::F64 => out.copy_from_slice(&value.to_le_bytes()),
        ScalarType::F32 => out.copy_from_slice(&(value as f32).to_le_bytes()),
        ScalarType::I64 => out.copy_from_slice(&(value as i64).to_le_bytes()),
        ScalarType::I32 => out.copy_from_slice(&(value as i32).to_le_bytes()),
        ScalarType::I16 => out.copy_from_slice(&(value as i16).to_le_bytes()),
        ScalarType::I8 => out.copy_from_slice(&(value as i8).to_le_bytes()),
        ScalarType::U64 => out.copy_from_slice(&(value as u64).to_le_bytes()),
        ScalarType::U32 => out.copy_from_slice(&(value as u32).to_le_bytes()),
        ScalarType::U16 => out.copy_from_slice(&(value as u16).to_le_bytes()),
        ScalarType::U8 => out.copy_from_slice(&(value as u8).to_le_bytes()),
        // Variable-length slots replay samples instead.
        ScalarType::String => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64, nanos: i32) -> EpicsTime {
        EpicsTime { seconds, nanos }
    }

    #[test]
    fn the_last_boundary_is_never_later_and_never_a_period_older() {
        // Whole-second boundaries.
        assert_eq!(at(100, 0).last_boundary(1_000_000_000, 0), at(100, 0));
        assert_eq!(
            at(100, 999_999_999).last_boundary(1_000_000_000, 0),
            at(100, 0)
        );
        // Boundaries shifted by a phase.
        assert_eq!(at(100, 100).last_boundary(1_000_000_000, 250), at(99, 250));
        assert_eq!(at(100, 250).last_boundary(1_000_000_000, 250), at(100, 250));
        // A period longer than one second.
        assert_eq!(
            at(1_005, 0).last_boundary(10_000_000_000, 3_500_000_000),
            at(1_003, 500_000_000)
        );
        // A phase larger than the period is reduced modulo the period.
        assert_eq!(
            at(100, 0).last_boundary(1_000_000_000, 1_000_000_250),
            at(99, 250)
        );
        // A zero period leaves the timestamp unchanged.
        assert_eq!(at(100, 7).last_boundary(0, 3), at(100, 7));
    }

    #[test]
    fn every_instant_in_one_period_maps_to_the_same_boundary() {
        let period = 20_000_000; // 50 Hz
        let phase = 7_654_321;
        let start = at(1_794_812_962, 7_654_321);
        let mut t = start;
        for _ in 0..1999 {
            t = t.plus_nanos(10_000);
            assert_eq!(t.last_boundary(period, phase), start);
        }
        assert_eq!(
            t.plus_nanos(10_000).last_boundary(period, phase),
            start.plus_nanos(period)
        );
    }

    #[test]
    fn the_wire_timestamp_counts_from_the_posix_epoch() {
        // Confirmed against softIocPVX: secondsPastEpoch contains an
        // unshifted POSIX timestamp. Applying the EPICS epoch offset
        // would put timestamps twenty years in the past, without any
        // error from the receiving client.
        let now = EpicsTime::now();
        let posix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (now.seconds - posix).abs() <= 1,
            "wire timestamp {} is not POSIX time {posix}",
            now.seconds
        );
    }

    #[test]
    fn now_is_plausible() {
        let t = EpicsTime::now();
        // After 2020 and before 2100, in POSIX seconds.
        assert!(t.seconds > 1_577_836_800, "{}", t.seconds);
        assert!(t.seconds < 4_102_444_800);
        assert!((0..1_000_000_000).contains(&t.nanos));
    }

    #[test]
    fn signals_are_deterministic() {
        for sig in [
            Signal::Ramp {
                min: 0.0,
                max: 10.0,
                step: 1.0,
            },
            Signal::Sine {
                min: -1.0,
                max: 1.0,
                period_updates: 100,
            },
            Signal::Noise { min: 0.0, max: 1.0 },
            Signal::Constant { value: 42.0 },
        ] {
            for step in [0u64, 1, 7, 1000] {
                assert_eq!(sig.sample(step, 99), sig.sample(step, 99));
            }
        }
    }

    #[test]
    fn signals_stay_in_range() {
        let cases = [
            Signal::Ramp {
                min: -5.0,
                max: 5.0,
                step: 0.7,
            },
            Signal::Sine {
                min: -5.0,
                max: 5.0,
                period_updates: 37,
            },
            Signal::Noise {
                min: -5.0,
                max: 5.0,
            },
        ];
        for sig in cases {
            for step in 0..5000u64 {
                let v = sig.sample(step, 12345);
                assert!((-5.0..=5.0).contains(&v), "{sig:?} produced {v}");
            }
        }
    }

    #[test]
    fn consecutive_values_differ() {
        // Repeated values could let consumer-side deduplication hide a throughput problem.
        let sig = Signal::Sine {
            min: 0.0,
            max: 1.0,
            period_updates: 360,
        };
        assert_ne!(sig.sample(0, 1), sig.sample(1, 1));
    }

    #[test]
    fn noise_differs_between_instances() {
        let sig = Signal::Noise { min: 0.0, max: 1.0 };
        assert_ne!(sig.sample(5, 1), sig.sample(5, 2));
    }

    #[test]
    fn degenerate_parameters_do_not_panic() {
        assert_eq!(
            Signal::Ramp {
                min: 3.0,
                max: 3.0,
                step: 1.0
            }
            .sample(10, 0),
            3.0
        );
        assert_eq!(
            Signal::Sine {
                min: 3.0,
                max: 9.0,
                period_updates: 0
            }
            .sample(10, 0),
            3.0
        );
    }

    #[test]
    fn micro_offsets_carry_into_seconds() {
        let t = EpicsTime::from_parts(100, 999_000_000);
        assert_eq!(t.plus_micros(500), EpicsTime::from_parts(100, 999_500_000));
        assert_eq!(t.plus_micros(2_000), EpicsTime::from_parts(101, 1_000_000));
        assert_eq!(t.plus_micros(0), t);
    }

    #[test]
    fn nano_offsets_carry_into_seconds_and_stay_ordered() {
        let t = EpicsTime::from_parts(100, 999_999_900);
        assert_eq!(t.plus_nanos(50), EpicsTime::from_parts(100, 999_999_950));
        assert_eq!(t.plus_nanos(100), EpicsTime::from_parts(101, 0));
        assert_eq!(t.plus_nanos(0), t);
        // A 10 MHz burst: 10,000 updates spaced 100 ns apart.
        // All timestamps must remain strictly increasing across a second boundary.
        let base = EpicsTime::from_parts(10, 999_500_000);
        let mut prev = base;
        for k in 1..10_000u64 {
            let t = base.plus_nanos(k * 100);
            assert!(
                (t.seconds, t.nanos) > (prev.seconds, prev.nanos),
                "offset {k} did not advance"
            );
            prev = t;
        }
    }

    #[test]
    fn a_burst_of_offsets_stays_ordered() {
        // Consumers may sort updates by timestamp, so timestamps must increase in emission order.
        let base = EpicsTime::from_parts(10, 999_990_000);
        let mut prev = base;
        for k in 1..2000u64 {
            let t = base.plus_micros(k);
            assert!(
                (t.seconds, t.nanos) > (prev.seconds, prev.nanos),
                "offset {k} went backwards"
            );
            prev = t;
        }
    }

    #[test]
    fn a_destination_of_the_wrong_width_is_left_alone() {
        // Template compilation should prevent this case. Keep the guard anyway.
        let mut b = [0xAAu8; 4];
        write_scalar(1.0, ScalarType::F64, &mut b);
        assert_eq!(b, [0xAA; 4]);
    }

    #[test]
    fn scalars_encode_little_endian() {
        let mut b = [0u8; 8];
        write_scalar(1.0, ScalarType::F64, &mut b);
        assert_eq!(b, 1.0f64.to_le_bytes());

        let mut b = [0u8; 4];
        write_scalar(-2.0, ScalarType::I32, &mut b);
        assert_eq!(b, (-2i32).to_le_bytes());

        let mut b = [0u8; 2];
        write_scalar(300.0, ScalarType::U16, &mut b);
        assert_eq!(b, 300u16.to_le_bytes());
    }
}
