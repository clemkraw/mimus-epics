//! Scenarios: a run as a sequence of phases rather than one steady load.
//!
//! Archivers seldom fail in steady state. They fail when thousands of
//! channels change at the same instant, when an IOC restarts and every
//! channel it served reconnects together, when a stalled IOC resumes, or
//! when the load climbs past what they absorb. A [`Scenario`] stages those
//! moments on a schedule, against the same fleet, and [`play`] records what
//! was sent during each phase. A report can then say which moment the
//! consumer did not get through, and how long it took to recover.
//!
//! Phases follow one another. Each starts from the configured rates, and
//! what a phase changes lasts as long as the phase.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mimus_emit::Counters;

use crate::shared::SharedCounters;
use crate::stage::Stages;

/// A sequence of phases, played once or several times.
#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    /// A name for reports.
    pub name: Option<String>,
    /// When the first phase begins.
    pub start: Start,
    /// How many times the phases are played, one pass after the other.
    pub repeat: u32,
    pub phases: Vec<Phase>,
}

/// When the first phase begins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Start {
    /// As soon as the IOCs listen.
    Now,
    /// Once the consumer holds `count` subscriptions, or after `timeout`,
    /// whichever comes first. A phase played against a consumer still
    /// subscribing measures its start-up, not the phase.
    Subscribed { count: Count, timeout: Duration },
}

/// A number of subscriptions to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    /// One for every channel the fleet serves.
    All,
    /// At least this many.
    Minimum(u64),
}

/// One phase of a scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct Phase {
    /// The phase's name in reports.
    pub label: String,
    pub duration: Duration,
    pub action: Action,
}

/// What a phase does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// The configured rates.
    Steady,
    /// Every channel of the IOCs targeted at `from` times its configured
    /// rate, moving linearly to `to` by the end of the phase.
    Gain { from: f64, to: f64, iocs: Iocs },
    /// When the phase starts, a share of the subscribed channels of the IOCs
    /// targeted send one update each, all at the same instant, as when a
    /// beam dump moves thousands of channels at once. The rest of the phase
    /// runs at the configured rates.
    Burst { share: f64, iocs: Iocs },
    /// The IOCs targeted send nothing for the length of the phase. When it
    /// ends they send one update per subscription at once, as a monitor
    /// queue holding each channel's latest value does, or carry on silently.
    Freeze { iocs: Iocs, resume: Resume },
    /// The IOCs targeted go down for `down`: their connections close, they
    /// refuse new ones and answer no search. Then they come back, and the
    /// consumer has to find them, reconnect and subscribe again, every
    /// channel at once.
    Restart { iocs: Iocs, down: Duration },
}

impl Action {
    /// The phase kind, as a scenario file names it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Steady => "steady",
            Self::Gain { .. } => "gain",
            Self::Burst { .. } => "burst",
            Self::Freeze { .. } => "freeze",
            Self::Restart { .. } => "restart",
        }
    }

    /// The IOCs the phase acts on, if it acts on any.
    pub fn iocs(&self) -> Option<&Iocs> {
        match self {
            Self::Steady => None,
            Self::Gain { iocs, .. }
            | Self::Burst { iocs, .. }
            | Self::Freeze { iocs, .. }
            | Self::Restart { iocs, .. } => Some(iocs),
        }
    }
}

/// The virtual IOCs a phase acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Iocs {
    All,
    These(Vec<u16>),
}

impl Iocs {
    /// Returns the indices targeted in a fleet of `iocs` IOCs.
    pub fn list(&self, iocs: u16) -> Vec<u16> {
        match self {
            Self::All => (0..iocs).collect(),
            Self::These(v) => v.iter().copied().filter(|&i| i < iocs).collect(),
        }
    }

    /// Returns whether IOC `ioc` is targeted.
    pub fn contains(&self, ioc: u16) -> bool {
        match self {
            Self::All => true,
            Self::These(v) => v.contains(&ioc),
        }
    }
}

impl std::fmt::Display for Iocs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::All => f.write_str("all IOCs"),
            Self::These(v) if v.len() == 1 => write!(f, "IOC {}", v[0]),
            Self::These(v) => {
                let list: Vec<String> = v.iter().map(u16::to_string).collect();
                write!(f, "IOCs {}", list.join(", "))
            }
        }
    }
}

/// What a frozen IOC does when it resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// One update per subscription, all at once.
    Burst,
    /// Nothing more than the configured rates.
    Silent,
}

impl Scenario {
    /// Checks the scenario against a fleet of `iocs` virtual IOCs, naming
    /// the phase at fault.
    pub fn check(&self, iocs: u16) -> Result<(), String> {
        if self.phases.is_empty() {
            return Err("the scenario has no phase".into());
        }
        if self.repeat == 0 {
            return Err("repeat is 0, so no phase would ever run".into());
        }
        if let Start::Subscribed { count, timeout } = self.start {
            if count == Count::Minimum(0) {
                return Err("start: waiting for 0 subscriptions is not waiting".into());
            }
            if timeout.is_zero() {
                return Err("start: the timeout is zero".into());
            }
        }
        for (n, phase) in self.phases.iter().enumerate() {
            let at = |why: String| format!("phase {} ({}): {why}", n + 1, phase.label);
            match &phase.action {
                Action::Burst { .. } => {}
                Action::Restart { down, .. } => {
                    if down.is_zero() {
                        return Err(at("down is zero, so the IOC would never be down".into()));
                    }
                    if *down > phase.duration {
                        return Err(at(format!(
                            "down is {:?} but the phase lasts {:?}; the IOC must be back before \
                             the next phase starts",
                            down, phase.duration
                        )));
                    }
                }
                _ if phase.duration.is_zero() => return Err(at("the phase lasts no time".into())),
                _ => {}
            }
            match &phase.action {
                Action::Gain { from, to, .. } => {
                    for g in [from, to] {
                        if !(g.is_finite() && *g > 0.0) {
                            return Err(at(format!(
                                "a gain of {g} is not a positive factor; a phase that stops \
                                 the updates is a freeze"
                            )));
                        }
                    }
                }
                Action::Burst { share, .. }
                    if !(share.is_finite() && *share > 0.0 && *share <= 1.0) =>
                {
                    return Err(at(format!(
                        "share {share} is not a fraction above 0, up to 1"
                    )));
                }
                _ => {}
            }
            if let Some(Iocs::These(list)) = phase.action.iocs() {
                if list.is_empty() {
                    return Err(at("the list of IOCs is empty".into()));
                }
                for (k, &i) in list.iter().enumerate() {
                    if i >= iocs {
                        return Err(at(format!(
                            "IOC {i} does not exist: the run has {iocs}, numbered from 0"
                        )));
                    }
                    if list[..k].contains(&i) {
                        return Err(at(format!("IOC {i} is listed twice")));
                    }
                }
            }
        }
        Ok(())
    }

    /// The largest rate factor any phase asks for, and at least 1: the
    /// factor every session's schedule is built at.
    pub fn headroom(&self) -> f64 {
        self.phases
            .iter()
            .filter_map(|p| match p.action {
                Action::Gain { from, to, .. } => Some(from.max(to)),
                _ => None,
            })
            .fold(1.0, f64::max)
    }

    /// The length of one pass over the phases.
    pub fn cycle(&self) -> Duration {
        self.phases.iter().map(|p| p.duration).sum()
    }

    /// The length of the whole scenario, not counting the wait at its start.
    pub fn length(&self) -> Duration {
        self.cycle() * self.repeat
    }
}

/// The run a scenario is played against.
#[derive(Debug, Clone)]
pub struct Target {
    pub stages: Arc<Stages>,
    /// The shared counters of each virtual IOC, in IOC order.
    pub totals: Vec<Arc<SharedCounters>>,
    /// Updates per second each IOC sends at the configured rates, with every
    /// channel subscribed.
    pub offered_eps: Vec<f64>,
    /// Channels the fleet serves: what a start waiting for all of them waits
    /// for.
    pub channels: u64,
}

impl Target {
    fn iocs(&self) -> u16 {
        self.totals.len() as u16
    }

    fn counters(&self) -> Counters {
        let mut c = Counters::default();
        for t in &self.totals {
            c.merge(&t.snapshot());
        }
        c
    }

    fn closed(&self) -> u64 {
        self.totals
            .iter()
            .map(|t| t.sessions_dropped() + t.sessions_closed_down())
            .sum()
    }

    fn subscriptions(&self) -> u64 {
        self.totals.iter().map(|t| t.subscriptions()).sum()
    }

    fn subscriptions_of(&self, ioc: u16) -> u64 {
        self.totals
            .get(usize::from(ioc))
            .map_or(0, |t| t.subscriptions())
    }

    fn offered_by(&self, ioc: u16) -> f64 {
        self.offered_eps
            .get(usize::from(ioc))
            .copied()
            .unwrap_or(0.0)
    }
}

/// What was sent during one phase.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseRecord {
    pub label: String,
    pub kind: &'static str,
    /// The pass over the phases this one belongs to, from 0.
    pub cycle: u32,
    /// When the phase started, from the start of the first phase.
    pub start: Duration,
    pub length: Duration,
    /// Updates the scenario asked the fleet for during the phase, if every
    /// channel was subscribed throughout: the configured rates with the
    /// phase's factor, nothing from a frozen or a down IOC, and the updates
    /// of its bursts.
    pub offered: f64,
    /// What the fleet counted during the phase.
    pub counters: Counters,
    /// Connections MIMUS closed during the phase, under `--drop-connections`
    /// or because their IOC went down.
    pub closed: u64,
    /// Subscriptions the consumer held when the phase ended.
    pub subscriptions: u64,
    /// Whether the run ended before the phase did.
    pub interrupted: bool,
}

/// How the consumer came back to an IOC after a restart.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovery {
    pub ioc: u16,
    /// The label of the restart phase.
    pub phase: String,
    /// Subscriptions the IOC held when it went down.
    pub before: u64,
    /// From the IOC's return to the first connection to it.
    pub reconnected: Option<Duration>,
    /// From the IOC's return to the moment it held as many subscriptions as
    /// before.
    pub recovered: Option<Duration>,
    /// Subscriptions it held when it recovered, or when the run ended.
    pub after: u64,
}

/// How long the scenario waited for the consumer before its first phase.
#[derive(Debug, Clone, PartialEq)]
pub struct Wait {
    pub waited: Duration,
    /// Subscriptions waited for.
    pub wanted: u64,
    /// Subscriptions held when the first phase started.
    pub held: u64,
}

/// What a played scenario recorded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Played {
    pub wait: Option<Wait>,
    pub phases: Vec<PhaseRecord>,
    pub recoveries: Vec<Recovery>,
}

/// The record of a scenario being played, readable at any time.
///
/// The run can end in the middle of a phase, on Ctrl-C or SIGTERM. The
/// record keeps the phase that is running open, so [`Record::finish`] can
/// still account for it.
#[derive(Debug, Default)]
pub struct Record {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    played: Played,
    open: Option<Open>,
    ended: bool,
}

#[derive(Debug)]
struct Open {
    record: PhaseRecord,
    action: Action,
    duration: Duration,
    began: tokio::time::Instant,
    counters: Counters,
    closed: u64,
    /// Updates asked for on top of the rates: bursts, and resumes.
    extra: f64,
}

impl Record {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panicked writer leaves counts behind, still worth reporting.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn open(&self, phase: &Phase, cycle: u32, start: Duration, target: &Target) {
        let mut inner = self.lock();
        inner.open = Some(Open {
            record: PhaseRecord {
                label: phase.label.clone(),
                kind: phase.action.kind(),
                cycle,
                start,
                length: Duration::ZERO,
                offered: 0.0,
                counters: Counters::default(),
                closed: 0,
                subscriptions: 0,
                interrupted: false,
            },
            action: phase.action.clone(),
            duration: phase.duration,
            began: tokio::time::Instant::now(),
            counters: target.counters(),
            closed: target.closed(),
            extra: 0.0,
        });
    }

    fn add_extra(&self, updates: f64) {
        if let Some(open) = self.lock().open.as_mut() {
            open.extra += updates;
        }
    }

    fn close(&self, target: &Target, interrupted: bool) {
        let mut inner = self.lock();
        let Some(open) = inner.open.take() else {
            return;
        };
        let mut record = open.record;
        record.length = open.began.elapsed();
        record.offered = offered(&open.action, record.length, open.duration, target) + open.extra;
        record.counters = target.counters().since(&open.counters);
        record.closed = target.closed().saturating_sub(open.closed);
        record.subscriptions = target.subscriptions();
        record.interrupted = interrupted;
        inner.played.phases.push(record);
    }

    /// Closes the phase still running, if any, as interrupted, and returns
    /// everything recorded. A consumer that has not recovered from a restart
    /// by now is recorded with the subscriptions it holds.
    pub fn finish(&self, target: &Target) -> Played {
        self.close(target, true);
        let mut inner = self.lock();
        inner.ended = true;
        for r in &mut inner.played.recoveries {
            if r.recovered.is_none() {
                r.after = target.subscriptions_of(r.ioc);
            }
        }
        inner.played.clone()
    }
}

/// Updates the configured rates ask for during the first `played` of a
/// phase lasting `full` and doing `action`, every channel being subscribed.
fn offered(action: &Action, played: Duration, full: Duration, target: &Target) -> f64 {
    let secs = played.as_secs_f64();
    (0..target.iocs())
        .map(|i| {
            let eps = target.offered_by(i);
            let acts = action.iocs().is_some_and(|t| t.contains(i));
            eps * match action {
                Action::Gain { from, to, .. } if acts => {
                    // The mean factor over the part of the ramp played.
                    let f = if full.is_zero() {
                        1.0
                    } else {
                        (secs / full.as_secs_f64()).min(1.0)
                    };
                    secs * (from + (to - from) * f / 2.0)
                }
                // Nothing while frozen; the configured rates in the moments
                // after, while the resume is sent.
                Action::Freeze { .. } if acts => (secs - full.as_secs_f64()).max(0.0),
                Action::Restart { down, .. } if acts => (secs - down.as_secs_f64()).max(0.0),
                _ => secs,
            }
        })
        .sum()
}

/// Plays `scenario` against `target`, recording each phase in `record`.
///
/// Returns when the last phase has ended. Dropping the future part way
/// leaves the phase that was running open in the record; the IOCs keep the
/// stage they were in, which matters only if the run goes on.
pub async fn play(scenario: &Scenario, target: &Target, record: &Arc<Record>) {
    if let Start::Subscribed { count, timeout } = scenario.start {
        let wanted = match count {
            Count::All => target.channels,
            Count::Minimum(n) => n,
        };
        let began = tokio::time::Instant::now();
        let mut poll = tokio::time::interval(Duration::from_millis(50));
        loop {
            poll.tick().await;
            if target.subscriptions() >= wanted || began.elapsed() >= timeout {
                break;
            }
        }
        record.lock().played.wait = Some(Wait {
            waited: began.elapsed(),
            wanted,
            held: target.subscriptions(),
        });
    }

    let t0 = tokio::time::Instant::now();
    let mut at = Duration::ZERO;
    for cycle in 0..scenario.repeat {
        for phase in &scenario.phases {
            let begin = t0 + at;
            let end = begin + phase.duration;
            record.open(phase, cycle, at, target);
            perform(phase, begin, end, target, record).await;
            record.close(target, false);
            at += phase.duration;
        }
    }
}

/// Puts the IOCs through one phase, from `begin` until `end`.
async fn perform(
    phase: &Phase,
    begin: tokio::time::Instant,
    end: tokio::time::Instant,
    target: &Target,
    record: &Arc<Record>,
) {
    let stages = &target.stages;
    let n = target.iocs();
    match &phase.action {
        Action::Steady => tokio::time::sleep_until(end).await,
        Action::Gain { from, to, iocs } => {
            let targeted = iocs.list(n);
            if from == to {
                for &i in &targeted {
                    stages.modify(i, |s| s.gain = *from);
                }
                tokio::time::sleep_until(end).await;
            } else {
                // A ramp moves in steps of 10 ms, finer than any rate a
                // consumer is measured at.
                let length = end - begin;
                let mut step = tokio::time::interval(Duration::from_millis(10));
                loop {
                    let now = tokio::time::Instant::now();
                    if now >= end {
                        break;
                    }
                    let f = (now - begin).as_secs_f64() / length.as_secs_f64();
                    let gain = from + (to - from) * f.clamp(0.0, 1.0);
                    for &i in &targeted {
                        stages.modify(i, |s| s.gain = gain);
                    }
                    tokio::select! {
                        _ = step.tick() => {}
                        () = tokio::time::sleep_until(end) => break,
                    }
                }
            }
            for &i in &targeted {
                stages.normal(i);
            }
        }
        Action::Burst { share, iocs } => {
            let before = target.counters().burst;
            for i in iocs.list(n) {
                record.add_extra(share * target.subscriptions_of(i) as f64);
                stages.burst(i, *share);
            }
            settle(target, before).await;
            tokio::time::sleep_until(end).await;
        }
        Action::Freeze { iocs, resume } => {
            let targeted = iocs.list(n);
            for &i in &targeted {
                stages.modify(i, |s| s.frozen = true);
            }
            tokio::time::sleep_until(end).await;
            let before = target.counters().burst;
            for &i in &targeted {
                if *resume == Resume::Burst {
                    record.add_extra(target.subscriptions_of(i) as f64);
                }
                stages.modify(i, |s| {
                    *s = s.normal();
                    if *resume == Resume::Burst {
                        s.bursts += 1;
                        s.burst_share = 1.0;
                    }
                });
            }
            if *resume == Resume::Burst {
                settle(target, before).await;
            }
        }
        Action::Restart { iocs, down } => {
            let targeted = iocs.list(n);
            let before: Vec<u64> = targeted
                .iter()
                .map(|&i| target.subscriptions_of(i))
                .collect();
            for &i in &targeted {
                stages.modify(i, |s| s.down = true);
            }
            tokio::time::sleep_until(begin + *down).await;
            for (&i, &before) in targeted.iter().zip(&before) {
                let sessions = target.totals[usize::from(i)].sessions_total();
                stages.normal(i);
                track_recovery(i, &phase.label, before, sessions, target, record);
            }
            tokio::time::sleep_until(end).await;
        }
    }
}

/// Waits until the sessions have sent the burst just asked for, so that its
/// updates are recorded in the phase that asked for them rather than the
/// next one.
///
/// Sessions send a burst as soon as they see it, without waiting for a
/// tick. The wait ends when the burst count has stopped moving for 20 ms,
/// or after 200 ms if it never moved, as when nothing is subscribed, and
/// after two seconds whatever happens.
async fn settle(target: &Target, before: u64) {
    let began = tokio::time::Instant::now();
    let mut last = before;
    let mut still = began;
    loop {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let now = target.counters().burst;
        if now != last {
            last = now;
            still = tokio::time::Instant::now();
        }
        let moved = last > before;
        if (moved && still.elapsed() >= Duration::from_millis(20))
            || (!moved && began.elapsed() >= Duration::from_millis(200))
            || began.elapsed() >= Duration::from_secs(2)
        {
            return;
        }
    }
}

/// Follows IOC `ioc` from its return until it holds `before` subscriptions
/// again, or the run ends, and records how long that took.
fn track_recovery(
    ioc: u16,
    label: &str,
    before: u64,
    sessions_before: u64,
    target: &Target,
    record: &Arc<Record>,
) {
    let slot = {
        let mut inner = record.lock();
        inner.played.recoveries.push(Recovery {
            ioc,
            phase: label.to_string(),
            before,
            reconnected: None,
            recovered: None,
            after: 0,
        });
        inner.played.recoveries.len() - 1
    };
    let totals = Arc::clone(&target.totals[usize::from(ioc)]);
    let record = Arc::clone(record);
    let back = tokio::time::Instant::now();
    tokio::spawn(async move {
        // Polled every 10 ms: the resolution of the times recorded.
        let mut poll = tokio::time::interval(Duration::from_millis(10));
        loop {
            let held = totals.subscriptions();
            let reconnected = totals.sessions_total() > sessions_before;
            let done = {
                let mut inner = record.lock();
                let ended = inner.ended;
                let r = &mut inner.played.recoveries[slot];
                if !ended && r.reconnected.is_none() && reconnected {
                    r.reconnected = Some(back.elapsed());
                }
                if !ended && held >= before {
                    r.recovered = Some(back.elapsed());
                    r.after = held;
                }
                ended || r.recovered.is_some()
            };
            if done {
                return;
            }
            poll.tick().await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(label: &str, ms: u64, action: Action) -> Phase {
        Phase {
            label: label.into(),
            duration: Duration::from_millis(ms),
            action,
        }
    }

    fn scenario(phases: Vec<Phase>) -> Scenario {
        Scenario {
            name: None,
            start: Start::Now,
            repeat: 1,
            phases,
        }
    }

    fn target(iocs: u16, eps: f64) -> Target {
        Target {
            stages: Stages::new(iocs),
            totals: (0..iocs).map(|_| SharedCounters::new()).collect(),
            offered_eps: vec![eps; usize::from(iocs)],
            channels: 0,
        }
    }

    #[test]
    fn a_scenario_names_the_phase_at_fault() {
        let s = scenario(vec![
            phase("steady", 100, Action::Steady),
            phase(
                "restart",
                100,
                Action::Restart {
                    iocs: Iocs::These(vec![3]),
                    down: Duration::from_millis(50),
                },
            ),
        ]);
        let e = s.check(2).unwrap_err();
        assert!(e.starts_with("phase 2 (restart):"), "{e}");
        assert!(e.contains("IOC 3 does not exist"), "{e}");
        assert!(s.check(4).is_ok());
    }

    #[test]
    fn what_cannot_be_played_is_refused() {
        let cases: Vec<(Action, u64, &str)> = vec![
            (
                Action::Gain {
                    from: 0.0,
                    to: 1.0,
                    iocs: Iocs::All,
                },
                10,
                "not a positive factor",
            ),
            (
                Action::Burst {
                    share: 1.5,
                    iocs: Iocs::All,
                },
                0,
                "not a fraction",
            ),
            (
                Action::Restart {
                    iocs: Iocs::All,
                    down: Duration::from_millis(20),
                },
                10,
                "must be back",
            ),
            (
                Action::Freeze {
                    iocs: Iocs::These(vec![0, 0]),
                    resume: Resume::Burst,
                },
                10,
                "listed twice",
            ),
            (
                Action::Freeze {
                    iocs: Iocs::These(vec![]),
                    resume: Resume::Burst,
                },
                10,
                "empty",
            ),
            (Action::Steady, 0, "lasts no time"),
        ];
        for (action, ms, why) in cases {
            let e = scenario(vec![phase("p", ms, action)]).check(2).unwrap_err();
            assert!(e.contains(why), "{e} should say {why}");
        }
        let mut none = scenario(vec![phase("p", 10, Action::Steady)]);
        none.repeat = 0;
        assert!(none.check(1).unwrap_err().contains("repeat"));
        assert!(scenario(vec![]).check(1).is_err());
    }

    #[test]
    fn a_burst_may_take_no_time_and_the_headroom_is_the_highest_gain() {
        let s = scenario(vec![
            phase(
                "b",
                0,
                Action::Burst {
                    share: 1.0,
                    iocs: Iocs::All,
                },
            ),
            phase(
                "ramp",
                100,
                Action::Gain {
                    from: 2.0,
                    to: 8.0,
                    iocs: Iocs::All,
                },
            ),
            phase(
                "slow",
                100,
                Action::Gain {
                    from: 0.5,
                    to: 0.5,
                    iocs: Iocs::All,
                },
            ),
        ]);
        assert!(s.check(1).is_ok());
        assert_eq!(s.headroom(), 8.0);
        assert_eq!(s.cycle(), Duration::from_millis(200));
        assert_eq!(
            scenario(vec![phase("s", 10, Action::Steady)]).headroom(),
            1.0,
            "never below 1"
        );
    }

    #[test]
    fn the_offered_load_follows_what_each_phase_asks() {
        let t = target(2, 100.0);
        let ten = Duration::from_secs(10);
        assert_eq!(offered(&Action::Steady, ten, ten, &t), 2000.0);
        let ramp = Action::Gain {
            from: 1.0,
            to: 3.0,
            iocs: Iocs::These(vec![1]),
        };
        assert_eq!(
            offered(&ramp, ten, ten, &t),
            1000.0 + 2000.0,
            "mean factor 2 on IOC 1"
        );
        let half = Duration::from_secs(5);
        assert_eq!(
            offered(&ramp, half, ten, &t),
            500.0 + 500.0 * 1.5,
            "the first half of the ramp, at a mean factor of 1.5"
        );
        let freeze = Action::Freeze {
            iocs: Iocs::All,
            resume: Resume::Burst,
        };
        assert_eq!(offered(&freeze, ten, ten, &t), 0.0);
        let restart = Action::Restart {
            iocs: Iocs::These(vec![0]),
            down: Duration::from_secs(4),
        };
        assert_eq!(offered(&restart, ten, ten, &t), 600.0 + 1000.0);
    }

    #[tokio::test]
    async fn phases_set_the_stages_and_put_them_back() {
        let t = target(2, 0.0);
        let s = scenario(vec![
            phase(
                "fast",
                60,
                Action::Gain {
                    from: 4.0,
                    to: 4.0,
                    iocs: Iocs::These(vec![1]),
                },
            ),
            phase(
                "freeze",
                60,
                Action::Freeze {
                    iocs: Iocs::All,
                    resume: Resume::Burst,
                },
            ),
        ]);
        let record = Record::new();
        let play = tokio::spawn({
            let (s, t, record) = (s.clone(), t.clone(), Arc::clone(&record));
            async move { play(&s, &t, &record).await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(t.stages.get(0).gain, 1.0);
        assert_eq!(t.stages.get(1).gain, 4.0);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(t.stages.get(0).frozen && t.stages.get(1).frozen);
        assert_eq!(
            t.stages.get(1).gain,
            1.0,
            "a gain lasts as long as its phase"
        );
        play.await.unwrap();
        for i in 0..2 {
            let st = t.stages.get(i);
            assert!(!st.frozen);
            assert_eq!(st.bursts, 1, "the freeze resumed with a burst");
        }
        let played = record.finish(&t);
        let labels: Vec<&str> = played.phases.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["fast", "freeze"]);
        assert!(played.phases.iter().all(|p| !p.interrupted));
        assert!(played.phases[1].start >= Duration::from_millis(60));
    }

    #[tokio::test]
    async fn a_restart_records_how_long_the_consumer_took_to_come_back() {
        let t = target(1, 0.0);
        let shared = Arc::clone(&t.totals[0]);
        for _ in 0..3 {
            shared.subscription_started();
        }
        let s = scenario(vec![phase(
            "restart",
            300,
            Action::Restart {
                iocs: Iocs::All,
                down: Duration::from_millis(50),
            },
        )]);
        let record = Record::new();
        let play = tokio::spawn({
            let (s, t, record) = (s.clone(), t.clone(), Arc::clone(&record));
            async move { play(&s, &t, &record).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(t.stages.is_down(0));
        // The consumer loses its subscriptions with the connection...
        for _ in 0..3 {
            shared.subscription_stopped();
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!t.stages.is_down(0), "back after 50 ms");
        // ...reconnects, and subscribes again, a while later.
        shared.session_opened();
        tokio::time::sleep(Duration::from_millis(60)).await;
        for _ in 0..3 {
            shared.subscription_started();
        }
        play.await.unwrap();
        let played = record.finish(&t);
        let r = &played.recoveries[0];
        assert_eq!((r.ioc, r.before, r.after), (0, 3, 3));
        let reconnected = r.reconnected.expect("reconnected");
        let recovered = r.recovered.expect("recovered");
        assert!(
            reconnected < recovered,
            "{reconnected:?} then {recovered:?}"
        );
        assert!(recovered >= Duration::from_millis(50), "{recovered:?}");
    }

    #[tokio::test]
    async fn an_interrupted_phase_is_still_recorded() {
        let t = target(1, 10.0);
        let s = scenario(vec![phase("long", 10_000, Action::Steady)]);
        let record = Record::new();
        let play = tokio::spawn({
            let (s, t, record) = (s.clone(), t.clone(), Arc::clone(&record));
            async move { play(&s, &t, &record).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        play.abort();
        let played = record.finish(&t);
        assert_eq!(played.phases.len(), 1);
        let p = &played.phases[0];
        assert!(p.interrupted);
        assert!(p.length < Duration::from_secs(1));
        assert!(
            p.offered < 10.0,
            "offered only for the time played: {}",
            p.offered
        );
    }

    #[tokio::test]
    async fn a_start_can_wait_for_the_consumer() {
        let mut t = target(1, 0.0);
        t.channels = 2;
        let mut s = scenario(vec![phase("s", 10, Action::Steady)]);
        s.start = Start::Subscribed {
            count: Count::All,
            timeout: Duration::from_secs(5),
        };
        let record = Record::new();
        let play = tokio::spawn({
            let (s, t, record) = (s.clone(), t.clone(), Arc::clone(&record));
            async move { play(&s, &t, &record).await }
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(record.lock().played.phases.is_empty() && record.lock().open.is_none());
        t.totals[0].subscription_started();
        t.totals[0].subscription_started();
        play.await.unwrap();
        let w = record.finish(&t).wait.expect("waited");
        assert_eq!((w.wanted, w.held), (2, 2));
        assert!(w.waited >= Duration::from_millis(100));
    }
}
