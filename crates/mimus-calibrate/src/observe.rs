//! Watching one channel and deciding when observation is complete.
//!
//! Calibration ends when a completion criterion is met, not when a timer
//! expires. A fixed duration can stop too early or waste time waiting.
//! A criterion stops when there is no more useful information to collect
//! and identifies channels that still need more observations.
//!
//! Stopping too early can silently freeze a field into the template before
//! it has changed. The resulting template is correct for the observation
//! window but wrong for values outside it. The capture that motivated this
//! work illustrates the problem: over seven updates, the high byte of a
//! double never changed because the value remained within one decade.
//!
//! At least two channels of the same type are also needed, for a separate
//! protocol reason. PVA sends the full type descriptor for the first
//! channel of a type on a connection, then sends only its identifier for
//! subsequent channels. With a single channel, this behavior remains
//! invisible, and MIMUS would replay a full descriptor for every channel.
//! At a million channels, that would make subscription setup far heavier
//! than it is in reality.
//!
//! This module does not read the clock. The caller supplies timestamps,
//! allowing tests to simulate an observation period without waiting.

use std::collections::BTreeMap;

/// Observations collected for one channel.
#[derive(Debug, Clone)]
pub struct ChannelObserver {
    pub pv: String,
    /// Monitor INIT bytes containing the full type descriptor, if this
    /// was the first channel of its type on the connection.
    pub introspection_full: Option<Vec<u8>>,
    /// Monitor INIT bytes referencing a cached type identifier.
    pub introspection_id_only: Option<Vec<u8>>,
    /// Message bodies in arrival order, capped at `max_updates`.
    updates: Vec<Vec<u8>>,
    /// The first message body received by this channel.
    ///
    /// Kept separately because it is a distinct message: the complete
    /// value sent when monitoring starts, to which subsequent deltas
    /// are applied. Storing it separately protects it from eviction
    /// when the retained update limit is reached.
    first_update: Option<Vec<u8>>,
    /// Arrival time of each retained update, in seconds since calibration
    /// started. Has the same length as `updates`.
    arrivals: Vec<f64>,
    /// Each changed-set observed and its occurrence count.
    ///
    /// A channel that only produces one pattern exercises only one
    /// decoding path.
    changed_sets: BTreeMap<Vec<u8>, u32>,
    /// Total updates received, including those not retained after the
    /// update limit was reached.
    total_updates: u64,
    last_arrival: f64,
    /// Time when the retained set last gained a new, distinct body.
    ///
    /// Novelty matters more than traffic. A setpoint may send once and
    /// stop, while a deadbanded channel may repeat the same body
    /// indefinitely. Both may have finished teaching, and this timestamp
    /// tracks that independently of their traffic.
    last_new_distinct: f64,
    max_updates: usize,
    /// Most bytes of bodies retained: see [`ChannelObserver::with_byte_budget`].
    max_bytes: usize,
    /// Bytes of the bodies in `updates`.
    retained_bytes: usize,
}

impl ChannelObserver {
    pub fn new(pv: impl Into<String>, max_updates: usize) -> Self {
        Self {
            pv: pv.into(),
            introspection_full: None,
            introspection_id_only: None,
            updates: Vec::new(),
            first_update: None,
            arrivals: Vec::new(),
            changed_sets: BTreeMap::new(),
            total_updates: 0,
            last_arrival: 0.0,
            last_new_distinct: 0.0,
            max_updates: max_updates.max(2),
            max_bytes: usize::MAX,
            retained_bytes: 0,
        }
    }

    /// Caps the bytes of the bodies retained, as well as their number.
    ///
    /// Sixty-four updates of a scalar are a few kilobytes; of a 16 MB image,
    /// a gigabyte. Bodies that would pass the budget are not retained, or
    /// replace a duplicate, as past the count. Two are always retained:
    /// fewer leave nothing to compare.
    pub fn with_byte_budget(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// How many bodies of the size this channel sends can be retained: the
    /// count cap, or fewer when the bodies are large enough to reach the byte
    /// budget first. Never fewer than two.
    pub fn capacity(&self) -> usize {
        match self.updates.iter().map(Vec::len).max() {
            Some(len) if len > 0 => (self.max_bytes / len).clamp(2, self.max_updates),
            _ => self.max_updates,
        }
    }

    /// How many distinct bodies the criterion asks of this channel: its
    /// count, or the room the channel has for steady-state bodies if that is
    /// less. A channel of large frames that cannot keep thirty within the
    /// byte budget is complete when it holds all it can; otherwise it would
    /// wait for bodies it has no room for until the deadline.
    ///
    /// The room is what the channel can keep less what it keeps of other
    /// shapes, its opening message first: of fifteen frames of 8 MB that fit
    /// in 128 MB, one is the opening, and fourteen are all the steady state
    /// will ever have. Never fewer than two.
    pub fn wanted(&self, c: &Criterion) -> usize {
        let steady = self.steady_key().map_or(0, |(_, n)| n);
        let others = self.updates.len() - steady;
        let room = self.capacity().saturating_sub(others).max(2);
        c.min_distinct_updates.min(room)
    }

    /// Records the type descriptor received in the monitor INIT reply.
    ///
    /// The descriptor form is significant: an identifier-only reply proves
    /// that this channel was not the first of its type on the connection.
    /// MIMUS must reproduce this behavior.
    pub fn observe_introspection(&mut self, bytes: Vec<u8>, full: bool) {
        if full {
            self.introspection_full.get_or_insert(bytes);
        } else {
            self.introspection_id_only.get_or_insert(bytes);
        }
    }

    /// Records a monitor update body, excluding the sub-command byte.
    ///
    /// This is the payload covered by the learned template.
    ///
    /// `at` is the arrival time in seconds since calibration started.
    pub fn observe_update(&mut self, body: Vec<u8>, at: f64) {
        self.total_updates += 1;
        self.last_arrival = at;
        if self.first_update.is_none() {
            self.first_update = Some(body.clone());
        }

        if let Some(bitset) = leading_bitset(&body) {
            *self.changed_sets.entry(bitset).or_insert(0) += 1;
        }

        let novel = !self.updates.contains(&body);
        if novel {
            self.last_new_distinct = at;
        }

        let fits = self.updates.len() < 2
            || self.retained_bytes.saturating_add(body.len()) <= self.max_bytes;
        if self.updates.len() < self.max_updates && fits {
            self.retained_bytes += body.len();
            self.updates.push(body);
            self.arrivals.push(at);
            return;
        }

        // Once the cap is reached, replace a body that occurs more than
        // once. This preserves as much variety as possible instead of
        // retaining only the first N arrivals. Variety helps prevent
        // slowly changing fields from being frozen into the template.
        if let Some(dup) = self
            .updates
            .iter()
            .position(|u| self.updates.iter().filter(|o| *o == u).count() > 1)
        {
            self.retained_bytes = self.retained_bytes - self.updates[dup].len() + body.len();
            self.updates[dup] = body;
            self.arrivals[dup] = at;
        }
    }

    pub fn updates(&self) -> &[Vec<u8>] {
        &self.updates
    }

    /// Returns the complete value sent when this channel was first monitored.
    pub fn first_update(&self) -> Option<&Vec<u8>> {
        self.first_update.as_ref()
    }

    /// Returns the changed set of the opening message: every field, in the
    /// form the server marks a complete value with.
    pub fn opening_changed_set(&self) -> Option<Vec<u8>> {
        self.first_update.as_deref().and_then(leading_bitset)
    }

    pub fn total_updates(&self) -> u64 {
        self.total_updates
    }

    /// Returns the number of distinct retained message bodies.
    ///
    /// Repeated identical updates add no new information, regardless of
    /// how many times they were received.
    pub fn distinct_updates(&self) -> usize {
        let mut seen: Vec<&Vec<u8>> = self.updates.iter().collect();
        seen.sort();
        seen.dedup();
        seen.len()
    }

    /// Returns changed-sets ordered by descending frequency.
    pub fn changed_sets(&self) -> Vec<(Vec<u8>, u32)> {
        let mut v: Vec<(Vec<u8>, u32)> = self
            .changed_sets
            .iter()
            .map(|(k, n)| (k.clone(), *n))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    /// Groups updates by changed-set, largest group first.
    ///
    /// A monitor emits two distinct message shapes. The first update after
    /// subscription carries the complete value, with every leaf marked.
    /// Subsequent updates carry only the fields that changed. For example,
    /// a `calc` record produced a 174-byte initial update followed by
    /// 28-byte deltas, with bitsets `04 ba fb 36 1e` and `02 82 03`.
    ///
    /// These message types must be analyzed separately. Combining them
    /// produced a slot ranging from 0 to 146 bytes and two fields classified
    /// as doubles whose bytes decoded to NaN. The resulting template
    /// reproduced the observations but described no meaningful structure.
    pub fn by_changed_set(&self) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
        let mut groups: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
        for u in &self.updates {
            let key = leading_bitset(u).unwrap_or_default();
            groups.entry(key).or_default().push(u.clone());
        }
        let mut out: Vec<(Vec<u8>, Vec<Vec<u8>>)> = groups.into_iter().collect();
        // Largest first, ties broken by the bitset so a rebuilt calibration
        // picks the same group.
        out.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        out
    }

    /// The most common changed-set among the retained bodies and how many
    /// carry it, chosen as [`ChannelObserver::by_changed_set`] orders them,
    /// without copying a body: the criterion asks every second, and a
    /// channel of frames retains a hundred megabytes of them.
    fn steady_key(&self) -> Option<(Vec<u8>, usize)> {
        let mut counts: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
        for u in &self.updates {
            *counts
                .entry(leading_bitset(u).unwrap_or_default())
                .or_insert(0) += 1;
        }
        // Largest first, ties to the smallest bitset, as by_changed_set.
        counts
            .into_iter()
            .min_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)))
    }

    /// Returns the number of distinct bodies in the most common changed-set.
    ///
    /// This is the count used to determine whether a channel has provided
    /// enough observations, since it reflects the population used to build
    /// the template. Counting all distinct bodies mixes the initial full
    /// update with subsequent deltas. A run that appears to meet the
    /// threshold with five distinct bodies may therefore build a template
    /// from only four, causing the resulting profile to fail validation.
    pub fn steady_state_distinct(&self) -> usize {
        let Some((key, _)) = self.steady_key() else {
            return 0;
        };
        let mut bodies: Vec<&Vec<u8>> = self
            .updates
            .iter()
            .filter(|u| leading_bitset(u).unwrap_or_default() == key)
            .collect();
        bodies.sort();
        bodies.dedup();
        bodies.len()
    }

    /// Returns the updates belonging to the most common changed-set.
    ///
    /// These are steady-state messages: what a subscriber receives
    /// throughout its subscription and what a load generator emits most of the time.
    pub fn steady_state(&self) -> Vec<Vec<u8>> {
        self.by_changed_set()
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or_default()
    }

    /// Returns whether update lengths vary.
    ///
    /// Variable-length messages require two-sided layout analysis rather
    /// than analysis based on fixed byte positions.
    pub fn length_varies(&self) -> bool {
        self.updates
            .iter()
            .any(|u| u.len() != self.updates.first().map(Vec::len).unwrap_or(0))
    }

    /// Returns the time in seconds since the last update arrived.
    pub fn quiet_for(&self, now: f64) -> f64 {
        (now - self.last_arrival).max(0.0)
    }

    /// Returns the time in seconds since a new distinct body was retained.
    pub fn stale_for(&self, now: f64) -> f64 {
        (now - self.last_new_distinct).max(0.0)
    }
}

/// Extracts the leading changed-set from a message body, if available.
///
/// A PVA bitset consists of a length byte followed by that many bytes.
/// This reads the length and extracts the bytes without interpreting
/// which fields the bits represent, since that requires knowing the type.
/// It is used only to count distinct changed-set patterns per channel.
fn leading_bitset(body: &[u8]) -> Option<Vec<u8>> {
    let size = *body.first()? as usize;
    // Values 254 and 255 introduce escaped and null forms. These channels
    // do not produce changed-sets of that size, so reject them rather
    // than guessing how to decode the length.
    if size >= 254 || body.len() < 1 + size {
        return None;
    }
    Some(body[1..1 + size].to_vec())
}

/// When a channel counts as fully observed.
#[derive(Debug, Clone, Copy)]
pub struct Criterion {
    /// Minimum number of distinct message bodies required from a moving channel.
    pub min_distinct_updates: usize,
    /// Maximum time without a new distinct body before a channel is
    /// considered settled, regardless of its update rate.
    ///
    /// Measured from the last novel body, not the last update. A setpoint
    /// may send once, while a deadbanded channel may repeat the same body
    /// indefinitely. Tracking novelty allows both to be recognized as
    /// settled without waiting for the overall deadline.
    ///
    /// If this is too short, a channel scanned every ten seconds may be
    /// mistaken for a constant, freezing its variable fields into the
    /// template.
    pub settled_after: f64,
    /// Overall observation limit. When reached, calibration reports
    /// incomplete channels and refuses to write a profile.
    pub give_up_after: f64,
}

impl Default for Criterion {
    fn default() -> Self {
        Self {
            // Allows enough observations for a double's exponent byte
            // to change during a sine sweep, which seven updates missed.
            min_distinct_updates: 30,
            settled_after: 30.0,
            give_up_after: 600.0,
        }
    }
}

/// Describes a channel's progress against the observation criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No updates received.
    Silent,
    /// A descriptor was received, but no update yet, or too few updates
    /// have arrived and the channel is still active.
    Waiting { distinct: usize },
    /// No further distinct updates are expected. This includes setpoints
    /// that speak once and deadbanded channels that repeat one body.
    /// Both behaviors are valid and must be reproduced.
    Settled { distinct: usize },
    /// Enough distinct updates have been collected for analysis.
    Complete { distinct: usize },
    /// Updates arrived without a type descriptor. No usable template
    /// can be built because consumers would receive uninterpretable bodies.
    NoIntrospection,
}

impl Status {
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Settled { .. } | Self::Complete { .. })
    }
}

/// Evaluates one channel against the observation criterion.
pub fn status(obs: &ChannelObserver, c: &Criterion, now: f64) -> Status {
    let has_intro = obs.introspection_full.is_some() || obs.introspection_id_only.is_some();
    if obs.total_updates == 0 {
        return Status::Silent;
    }
    if !has_intro {
        return Status::NoIntrospection;
    }
    // Count only steady-state updates, since these are used to build the template.
    let distinct = obs.steady_state_distinct();
    if distinct >= obs.wanted(c) {
        return Status::Complete { distinct };
    }
    if obs.stale_for(now) >= c.settled_after {
        return Status::Settled { distinct };
    }
    Status::Waiting { distinct }
}

/// Summarizes the observation progress across all channels.
#[derive(Debug, Clone)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// Channels not yet complete, together with their current status.
    /// Excludes channels that have never responded, listed separately
    /// in [`Progress::missing`].
    pub outstanding: Vec<(String, Status)>,
    /// Channels for which the IOC never sent a response.
    ///
    /// Kept separate from `outstanding` because these cases require
    /// different handling. A missing channel contributes no template
    /// and cannot produce incorrect output; it represents a coverage
    /// gap visible in the coverage report. A partially observed channel
    /// can produce an incorrect template and must prevent the run
    /// from completing.
    pub missing: Vec<String>,
    /// Channels for which the server never used a cached descriptor.
    ///
    /// Informational only. A server that always sends full descriptors
    /// is behaving consistently, and MIMUS should reproduce that behavior.
    pub types_without_cached_form: Vec<String>,
}

impl Progress {
    /// Returns whether all channels that responded have finished observation.
    ///
    /// Missing channels do not block completion. Waiting for a channel
    /// that never responds would be pointless, and it contributes no
    /// template. Missing channels remain visible in the separate report.
    pub fn is_complete(&self) -> bool {
        self.outstanding.is_empty()
    }

    /// Returns whether no channel responded at all.
    pub fn heard_nothing(&self) -> bool {
        self.done == 0 && self.outstanding.is_empty() && !self.missing.is_empty()
    }

    /// Formats a one-line progress summary.
    pub fn summary(&self) -> String {
        format!("{}/{} channels observed", self.done, self.total)
    }
}

/// Evaluates every channel and checks the cross-channel descriptor rule.
pub fn progress(observers: &[ChannelObserver], c: &Criterion, now: f64) -> Progress {
    let mut outstanding = Vec::new();
    let mut missing = Vec::new();
    let mut done = 0;
    for o in observers {
        let s = status(o, c, now);
        if s.is_done() {
            done += 1;
        } else if s == Status::Silent {
            missing.push(o.pv.clone());
        } else {
            outstanding.push((o.pv.clone(), s));
        }
    }

    // Check whether the server ever used a cached descriptor.
    //
    // MIMUS initially assumed it would, based on the specification's
    // per-connection introspection registry. Reproducing this behavior
    // incorrectly would make subscription setup at a million channels
    // far heavier than necessary.
    //
    // Measurements against PVXS show that every subscription receives
    // a full descriptor with a fresh ID. Record this as server behavior,
    // not as a calibration failure. Requiring a descriptor form that the
    // reference implementation never sends would prevent every run
    // from completing.
    let saw_cached = observers.iter().any(|o| o.introspection_id_only.is_some());
    let distinct_types = {
        let mut keys: Vec<&Vec<u8>> = observers
            .iter()
            .filter_map(|o| o.introspection_full.as_ref())
            .collect();
        keys.sort();
        keys.dedup();
        keys.len()
    };
    let types_without_cached_form = if saw_cached || distinct_types == 0 {
        Vec::new()
    } else {
        vec![format!(
            "the server never used the cached descriptor form; \
             all {distinct_types} type(s) arrived in full"
        )]
    };

    Progress {
        done,
        total: observers.len(),
        outstanding,
        missing,
        types_without_cached_form,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moving(pv: &str, n: usize, cap: usize) -> ChannelObserver {
        let mut o = ChannelObserver::new(pv, cap);
        o.observe_introspection(vec![0xFF, 0xFD, 0x01, 0x00], true);
        for k in 0..n {
            let mut body = vec![0x02, 0x82, 0x03];
            body.extend_from_slice(&(k as f64).to_le_bytes());
            o.observe_update(body, k as f64);
        }
        o
    }

    #[test]
    fn a_silent_channel_is_not_static() {
        // No descriptor or message body has arrived. Treating this channel
        // as static would let calibration create a template without evidence.
        let o = ChannelObserver::new("MIM:X", 64);
        assert_eq!(status(&o, &Criterion::default(), 1000.0), Status::Silent);
    }

    #[test]
    fn a_channel_that_spoke_once_and_went_quiet_is_settled() {
        // A setpoint that speaks once is valid behavior, not a coverage gap.
        let o = moving("MIM:F64:S:0", 1, 64);
        let c = Criterion::default();
        assert!(matches!(status(&o, &c, 5.0), Status::Waiting { .. }));
        assert_eq!(status(&o, &c, 100.0), Status::Settled { distinct: 1 });
    }

    #[test]
    fn a_slow_channel_is_not_mistaken_for_a_static_one() {
        // The channel updates every ten seconds. Since only fifteen seconds
        // have passed since its last new body, it is still considered active.
        let mut o = moving("MIM:SLOW", 3, 64);
        o.observe_update(vec![0x02, 0x82, 0x03, 9, 9, 9, 9, 9, 9, 9, 9], 30.0);
        assert!(matches!(
            status(&o, &Criterion::default(), 45.0),
            Status::Waiting { .. }
        ));
    }

    #[test]
    fn enough_distinct_updates_completes_a_channel() {
        let o = moving("MIM:F64:0", 40, 64);
        assert_eq!(
            status(&o, &Criterion::default(), 40.0),
            Status::Complete { distinct: 40 }
        );
    }

    #[test]
    fn identical_updates_do_not_count_as_variety() {
        // Fifty copies of the same message provide no more information
        // than one. Counting arrivals instead of distinct bodies would
        // mark the channel complete and freeze its fields into the template.
        let mut o = ChannelObserver::new("MIM:DUP", 64);
        o.observe_introspection(vec![0xFF], true);
        for k in 0..50 {
            o.observe_update(vec![0x02, 0x82, 0x03, 7, 7, 7, 7], k as f64);
        }
        assert_eq!(o.total_updates(), 50);
        assert_eq!(o.distinct_updates(), 1);
        // Updates continue to arrive, but no new information has been learned.
        assert_eq!(
            status(&o, &Criterion::default(), 50.0),
            Status::Settled { distinct: 1 }
        );
    }

    #[test]
    fn updates_without_a_descriptor_are_unusable() {
        // Without a descriptor, the message bodies cannot be interpreted.
        // A consumer would receive data it cannot decode.
        let mut o = ChannelObserver::new("MIM:X", 64);
        o.observe_update(vec![1, 2, 3], 1.0);
        assert_eq!(
            status(&o, &Criterion::default(), 2.0),
            Status::NoIntrospection
        );
    }

    #[test]
    fn the_cap_keeps_variety_rather_than_the_first_arrivals() {
        // Once the buffer is full, a duplicate is replaced before a distinct
        // body. Keeping only the first N arrivals would preserve the channel's
        // initial state at the expense of later changes.
        let mut o = ChannelObserver::new("MIM:X", 4);
        o.observe_introspection(vec![0xFF], true);
        for _ in 0..3 {
            o.observe_update(vec![0x02, 1, 1], 0.0);
        }
        o.observe_update(vec![0x02, 2, 2], 1.0);
        assert_eq!(o.distinct_updates(), 2);
        o.observe_update(vec![0x02, 3, 3], 2.0);
        o.observe_update(vec![0x02, 4, 4], 3.0);
        assert!(
            o.distinct_updates() >= 3,
            "variety was evicted: {:?}",
            o.updates()
        );
    }

    #[test]
    fn changed_sets_are_counted_by_pattern() {
        let mut o = ChannelObserver::new("MIM:ALM:0", 64);
        o.observe_introspection(vec![0xFF], true);
        for k in 0..10 {
            // Nine ordinary updates and one with an alarm field change.
            let bitset: Vec<u8> = if k == 5 {
                vec![0x02, 0x9E, 0x03]
            } else {
                vec![0x02, 0x82, 0x03]
            };
            let mut body = bitset;
            body.push(k);
            o.observe_update(body, k as f64);
        }
        let sets = o.changed_sets();
        assert_eq!(sets.len(), 2, "two decode paths, not one");
        assert_eq!(sets[0], (vec![0x82, 0x03], 9));
        assert_eq!(sets[1], (vec![0x9E, 0x03], 1));
    }

    #[test]
    fn a_bitset_that_cannot_be_read_is_declined_rather_than_guessed() {
        let mut o = ChannelObserver::new("MIM:X", 8);
        o.observe_introspection(vec![0xFF], true);
        // Claims a 254-byte bitset, but the message contains only four bytes.
        o.observe_update(vec![254, 1, 2, 3], 0.0);
        assert!(o.changed_sets().is_empty());
    }

    #[test]
    fn a_varying_length_is_noticed() {
        let mut o = ChannelObserver::new("MIM:STRV:0", 8);
        o.observe_introspection(vec![0xFF], true);
        o.observe_update(vec![0x02, 0x82, 0x03, 5, 1, 2, 3, 4, 5], 0.0);
        o.observe_update(vec![0x02, 0x82, 0x03, 3, 1, 2, 3], 1.0);
        assert!(o.length_varies());
    }

    #[test]
    fn progress_names_what_is_missing() {
        // An incomplete calibration must identify the channels that still
        // need observations, not just report that calibration is unfinished.
        let observers = vec![moving("MIM:A", 40, 64), moving("MIM:B", 2, 64)];
        let p = progress(&observers, &Criterion::default(), 5.0);
        assert_eq!(p.done, 1);
        assert!(!p.is_complete());
        assert_eq!(p.outstanding[0].0, "MIM:B");
        assert_eq!(p.summary(), "1/2 channels observed");
    }

    #[test]
    fn a_server_that_never_caches_descriptors_is_noted_not_blocked() {
        // PVXS sends a full descriptor with a fresh ID for every subscription.
        // The cached form never appears. Treating its absence as a calibration
        // failure would leave every run waiting for a message PVXS never sends.
        let observers = vec![moving("MIM:A:0", 40, 64), moving("MIM:A:1", 40, 64)];
        let p = progress(&observers, &Criterion::default(), 100.0);
        assert_eq!(p.done, 2);
        assert!(p.is_complete(), "{p:?}");
        assert_eq!(
            p.types_without_cached_form.len(),
            1,
            "the absence should still be reported"
        );
    }

    #[test]
    fn a_server_that_does_cache_produces_no_note() {
        let mut first = moving("MIM:A:0", 40, 64);
        let mut second = moving("MIM:A:1", 40, 64);
        first.introspection_id_only = None;
        second.introspection_full = None;
        second.observe_introspection(vec![0xFF, 0xFE, 0x01, 0x00], false);
        let p = progress(&[first, second], &Criterion::default(), 100.0);
        assert!(p.is_complete());
        assert!(p.types_without_cached_form.is_empty());
    }
}

#[cfg(test)]
mod novelty_tests {
    use super::*;

    #[test]
    fn a_repeating_channel_settles_without_waiting_for_the_deadline() {
        // This case exposed a flaw in the original rule: it tracked incoming
        // traffic, so a channel repeating the same body forever appeared active
        // and blocked calibration until the deadline. What matters is whether
        // new information arrives, not whether messages keep arriving.
        let mut o = ChannelObserver::new("MIM:DEADBAND", 64);
        o.observe_introspection(vec![0xFF], true);
        for k in 0..200 {
            o.observe_update(vec![0x02, 0x82, 0x03, 7], k as f64);
        }
        let c = Criterion::default();
        assert!(o.quiet_for(200.0) <= 1.0, "the channel is still posting");
        assert!(o.stale_for(200.0) > c.settled_after, "but teaching nothing");
        assert!(status(&o, &c, 200.0).is_done());
    }

    #[test]
    fn large_bodies_are_retained_within_the_byte_budget() {
        let mut o = ChannelObserver::new("MIM:IMG:0", 64).with_byte_budget(10_000);
        o.observe_introspection(vec![0xFF], true);
        for k in 0..20u8 {
            let mut body = vec![0x01, 0x02];
            body.resize(3_000, k);
            o.observe_update(body, f64::from(k));
        }
        // Three bodies of 3000 B fit in 10000 B; the count cap is 64.
        assert_eq!(o.updates().len(), 3);
        assert_eq!(o.capacity(), 3);
        assert_eq!(o.wanted(&Criterion::default()), 3);
        assert_eq!(o.total_updates(), 20);

        // The criterion asks for thirty; the channel holds all it can.
        assert_eq!(
            status(&o, &Criterion::default(), 20.0),
            Status::Complete { distinct: 3 }
        );
    }

    #[test]
    fn the_opening_message_takes_its_room_in_the_byte_budget() {
        let mut o = ChannelObserver::new("MIM:IMG:0", 64).with_byte_budget(10_000);
        o.observe_introspection(vec![0xFF], true);
        // The opening message: the whole value, changed set {0}.
        let mut opening = vec![0x01, 0x01];
        opening.resize(3_000, 0xAA);
        o.observe_update(opening, 0.0);
        for k in 0..20u8 {
            let mut body = vec![0x01, 0x02];
            body.resize(3_000, k);
            o.observe_update(body, 1.0 + f64::from(k));
        }
        // Three fit; one is the opening, so the steady state has two, and
        // can never have more: two are all the criterion asks.
        assert_eq!(o.updates().len(), 3);
        assert_eq!(o.steady_state_distinct(), 2);
        assert_eq!(o.wanted(&Criterion::default()), 2);
        assert_eq!(
            status(&o, &Criterion::default(), 21.0),
            Status::Complete { distinct: 2 }
        );
    }

    #[test]
    fn two_bodies_are_retained_whatever_their_size() {
        let mut o = ChannelObserver::new("MIM:IMG:0", 64).with_byte_budget(100);
        o.observe_introspection(vec![0xFF], true);
        for k in 0..5u8 {
            o.observe_update(vec![k; 1_000], f64::from(k));
        }
        assert_eq!(o.updates().len(), 2);
        assert_eq!(o.capacity(), 2);
    }

    #[test]
    fn small_bodies_still_wait_for_the_criterion() {
        let mut o = ChannelObserver::new("MIM:F64:0", 64).with_byte_budget(128 << 20);
        o.observe_introspection(vec![0xFF], true);
        for k in 0..10u8 {
            o.observe_update(vec![0x02, 0x82, 0x03, k], f64::from(k));
        }
        assert_eq!(o.wanted(&Criterion::default()), 30);
        assert_eq!(
            status(&o, &Criterion::default(), 10.0),
            Status::Waiting { distinct: 10 }
        );
    }

    #[test]
    fn a_channel_still_producing_new_bodies_is_not_settled() {
        let mut o = ChannelObserver::new("MIM:F64:0", 64);
        o.observe_introspection(vec![0xFF], true);
        for k in 0..10u8 {
            o.observe_update(vec![0x02, 0x82, 0x03, k], k as f64 * 5.0);
        }
        // Ten distinct bodies, with the latest received five seconds ago.
        // The criterion requires thirty distinct bodies.
        assert!(matches!(
            status(&o, &Criterion::default(), 47.0),
            Status::Waiting { distinct: 10 }
        ));
    }
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    /// Builds a realistic monitor sequence: one full update followed by deltas.
    fn realistic() -> ChannelObserver {
        let mut o = ChannelObserver::new("MIM:F64:0", 64);
        o.observe_introspection(vec![0xFF, 0x80], true);

        // The initial update after START marks every leaf and is 174 bytes.
        let mut full = vec![0x04, 0xba, 0xfb, 0x36, 0x1e];
        full.extend_from_slice(&[0x11; 169]);
        o.observe_update(full, 0.0);

        // Subsequent deltas contain only the value and timestamp (28 bytes).
        for k in 1..6u8 {
            let mut delta = vec![0x02, 0x82, 0x03];
            delta.extend_from_slice(&[k; 25]);
            o.observe_update(delta, k as f64);
        }
        o
    }

    #[test]
    fn the_first_update_is_a_different_message_from_the_rest() {
        let o = realistic();
        let groups = o.by_changed_set();
        assert_eq!(groups.len(), 2, "two shapes, not one");
        // Deltas are the majority and represent the messages a load generator
        // emits most of the time.
        assert_eq!(groups[0].0, vec![0x82, 0x03]);
        assert_eq!(groups[0].1.len(), 5);
        assert_eq!(groups[1].0, vec![0xba, 0xfb, 0x36, 0x1e]);
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn steady_state_holds_one_shape_only() {
        // Diffing a full update against a delta is meaningless. It produced
        // a slot ranging from 0 to 146 bytes and two fields classified as
        // doubles whose bytes decoded to NaN. The resulting template matched
        // the observations but described no meaningful structure.
        let updates = realistic().steady_state();
        assert_eq!(updates.len(), 5);
        let len = updates[0].len();
        assert!(
            updates.iter().all(|u| u.len() == len),
            "steady state must be one length"
        );
        assert!(updates.iter().all(|u| u.starts_with(&[0x02, 0x82, 0x03])));
    }

    #[test]
    fn a_channel_with_one_shape_is_unaffected() {
        let mut o = ChannelObserver::new("MIM:X", 64);
        o.observe_introspection(vec![0xFF], true);
        for k in 0..5u8 {
            o.observe_update(vec![0x02, 0x82, 0x03, k], k as f64);
        }
        assert_eq!(o.by_changed_set().len(), 1);
        assert_eq!(o.steady_state().len(), 5);
    }

    #[test]
    fn grouping_is_deterministic() {
        let o = realistic();
        assert_eq!(o.by_changed_set(), o.by_changed_set());
    }
}

#[cfg(test)]
mod criterion_alignment_tests {
    use super::*;

    #[test]
    fn the_criterion_counts_what_the_template_is_built_from() {
        // A monitor sends a full value first, followed by deltas.
        // Counting both shapes together marked the channel complete
        // with five distinct bodies, although only four belonged to
        // the steady-state group. Profile validation then rejected
        // the result because it did not meet the required observation
        // count.
        let mut o = ChannelObserver::new("MIM:F64:0", 64);
        o.observe_introspection(vec![0xFF, 0x80], true);

        let mut full = vec![0x04, 0xba, 0xfb, 0x36, 0x1e];
        full.extend_from_slice(&[0x11; 169]);
        o.observe_update(full, 0.0);
        for k in 1..5u8 {
            let mut delta = vec![0x02, 0x82, 0x03];
            delta.extend_from_slice(&[k; 25]);
            o.observe_update(delta, k as f64);
        }

        assert_eq!(o.distinct_updates(), 5, "five bodies in total");
        assert_eq!(o.steady_state_distinct(), 4, "but only four to diff");

        let c = Criterion {
            min_distinct_updates: 5,
            ..Default::default()
        };
        assert!(
            !matches!(status(&o, &c, 5.0), Status::Complete { .. }),
            "four steady-state updates must not satisfy a floor of five"
        );
        assert_eq!(o.steady_state().len(), 4);
    }
}

#[cfg(test)]
mod missing_tests {
    use super::*;

    fn talker(pv: &str) -> ChannelObserver {
        let mut o = ChannelObserver::new(pv, 64);
        o.observe_introspection(vec![0xFF, 0x80], true);
        for k in 0..40u8 {
            o.observe_update(vec![0x02, 0x82, 0x03, k], k as f64);
        }
        o
    }

    #[test]
    fn a_channel_the_ioc_never_answered_for_does_not_block() {
        // Waiting for a channel that does not exist would block forever.
        // It contributes no template and cannot corrupt one, so it is a
        // coverage gap, not a reason to abort calibration.
        let observers = vec![
            talker("MIM:F64:0"),
            talker("MIM:F64:1"),
            ChannelObserver::new("MIM:NO:SUCH:PV", 64),
        ];
        let p = progress(&observers, &Criterion::default(), 60.0);
        assert_eq!(p.done, 2);
        assert!(p.is_complete(), "a missing channel should not block");
        assert_eq!(p.missing, vec!["MIM:NO:SUCH:PV".to_string()]);
    }

    #[test]
    fn a_half_observed_channel_does_block() {
        // Unlike a missing channel, this one has enough data to produce
        // a template, but not enough to produce a reliable one.
        let mut half = ChannelObserver::new("MIM:SLOW", 64);
        half.observe_introspection(vec![0xFF, 0x80], true);
        half.observe_update(vec![0x02, 0x82, 0x03, 1], 55.0);
        let p = progress(&[talker("MIM:F64:0"), half], &Criterion::default(), 56.0);
        assert!(!p.is_complete());
        assert_eq!(p.outstanding.len(), 1);
        assert!(p.missing.is_empty());
    }

    #[test]
    fn an_ioc_that_answered_nothing_is_recognizable() {
        // If every channel is silent, the address or channel catalogue may
        // be wrong. This must not be mistaken for successful calibration.
        let observers = vec![
            ChannelObserver::new("MIM:A", 64),
            ChannelObserver::new("MIM:B", 64),
        ];
        let p = progress(&observers, &Criterion::default(), 60.0);
        assert!(p.heard_nothing());
    }
}

#[cfg(test)]
mod first_update_tests {
    use super::*;

    #[test]
    fn the_opening_body_survives_eviction() {
        // Once the retention limit is reached, duplicates are replaced
        // to preserve variety. The opening body may therefore be evicted.
        // It is the complete value sent at START, against which all later
        // deltas are applied. Losing it leaves fields undefined for the
        // lifetime of the subscription.
        let mut o = ChannelObserver::new("MIM:F64:0", 4);
        o.observe_introspection(vec![0xFF, 0x80], true);

        let mut opening = vec![0x04, 0xba, 0xfb, 0x36, 0x1e];
        opening.extend_from_slice(&[0x77; 169]);
        o.observe_update(opening.clone(), 0.0);
        for k in 1..20u8 {
            o.observe_update(vec![0x02, 0x82, 0x03, k], k as f64);
        }

        assert_eq!(o.first_update(), Some(&opening));
        assert_eq!(o.total_updates(), 20);
    }

    #[test]
    fn a_channel_that_never_spoke_has_no_opening_body() {
        let o = ChannelObserver::new("MIM:NO:SUCH:PV", 8);
        assert!(o.first_update().is_none());
    }
}
