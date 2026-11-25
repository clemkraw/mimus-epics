//! What a scenario asks of each virtual IOC.
//!
//! A scenario is played by one task, and every session of the run has to
//! follow it: stop sending when its IOC freezes, send at once when it
//! bursts, close when it goes down. Each IOC therefore has a watch channel
//! holding its current [`Stage`]. A session copies the stage when it changes
//! and reads its copy on every tick, so following a scenario costs the hot
//! path nothing. A session with nothing scheduled does not tick, but it
//! still wakes for a change, so an idle connection closes too when its IOC
//! goes down.

use std::sync::Arc;

use tokio::sync::watch;

/// The state a scenario puts one virtual IOC in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stage {
    /// Factor applied to the configured rate of every channel; 1 leaves it
    /// as configured.
    pub gain: f64,
    /// While frozen, the IOC sends no update. What falls due is counted as
    /// held and never sent, as an IOC that stops processing does not send
    /// late what it did not process.
    pub frozen: bool,
    /// While down, the IOC's connections are closed, it accepts no new one,
    /// and it answers no search: to a client, it has gone.
    pub down: bool,
    /// Number of bursts so far. A session that sees it change sends one
    /// update for each of its subscriptions the burst selects.
    pub bursts: u64,
    /// Share of the subscriptions the latest burst selects, from 0 to 1.
    pub burst_share: f64,
}

impl Default for Stage {
    fn default() -> Self {
        Self {
            gain: 1.0,
            frozen: false,
            down: false,
            bursts: 0,
            burst_share: 1.0,
        }
    }
}

impl Stage {
    /// Returns the stage with nothing applied: the configured rate, sending,
    /// up. The burst count is kept, so the return to normal is not itself
    /// read as a burst.
    pub fn normal(self) -> Self {
        Self {
            bursts: self.bursts,
            burst_share: self.burst_share,
            ..Self::default()
        }
    }
}

/// The stages of every virtual IOC of a run.
#[derive(Debug)]
pub struct Stages {
    iocs: Vec<watch::Sender<Stage>>,
}

impl Stages {
    /// Creates the stages of `iocs` virtual IOCs, each in the normal stage.
    pub fn new(iocs: u16) -> Arc<Self> {
        Arc::new(Self {
            iocs: (0..iocs)
                .map(|_| watch::Sender::new(Stage::default()))
                .collect(),
        })
    }

    /// Returns the number of IOCs.
    pub fn len(&self) -> usize {
        self.iocs.len()
    }

    /// Returns whether there are no IOCs at all.
    pub fn is_empty(&self) -> bool {
        self.iocs.is_empty()
    }

    /// Follows the stage of IOC `ioc`, or `None` past the last IOC.
    pub fn watch(&self, ioc: u16) -> Option<watch::Receiver<Stage>> {
        self.iocs
            .get(usize::from(ioc))
            .map(watch::Sender::subscribe)
    }

    /// Returns the current stage of IOC `ioc`; an IOC past the last one is
    /// always normal.
    pub fn get(&self, ioc: u16) -> Stage {
        self.iocs
            .get(usize::from(ioc))
            .map(|s| *s.borrow())
            .unwrap_or_default()
    }

    /// Returns whether IOC `ioc` is down.
    pub fn is_down(&self, ioc: u16) -> bool {
        self.get(ioc).down
    }

    /// Changes the stage of IOC `ioc`, and wakes its followers if that
    /// changed anything. An IOC past the last one is ignored.
    pub fn modify(&self, ioc: u16, change: impl FnOnce(&mut Stage)) {
        if let Some(sender) = self.iocs.get(usize::from(ioc)) {
            sender.send_if_modified(|stage| {
                let before = *stage;
                change(stage);
                *stage != before
            });
        }
    }

    /// Asks IOC `ioc` for one update from `share` of its subscriptions, all
    /// at the same instant.
    pub fn burst(&self, ioc: u16, share: f64) {
        self.modify(ioc, |s| {
            s.bursts += 1;
            s.burst_share = share;
        });
    }

    /// Returns IOC `ioc` to the normal stage.
    pub fn normal(&self, ioc: u16) {
        self.modify(ioc, |s| *s = s.normal());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ioc_starts_normal() {
        let stages = Stages::new(3);
        assert_eq!(stages.len(), 3);
        for i in 0..3 {
            assert_eq!(stages.get(i), Stage::default());
        }
        assert_eq!(stages.get(7), Stage::default(), "past the last IOC");
        assert!(stages.watch(7).is_none());
    }

    #[test]
    fn a_change_reaches_its_followers_and_no_one_else() {
        let stages = Stages::new(2);
        let zero = stages.watch(0).unwrap();
        let mut one = stages.watch(1).unwrap();
        stages.modify(1, |s| s.frozen = true);
        assert!(!zero.has_changed().unwrap());
        assert!(one.has_changed().unwrap());
        assert!(one.borrow_and_update().frozen);
    }

    #[test]
    fn a_change_that_changes_nothing_wakes_no_one() {
        let stages = Stages::new(1);
        let rx = stages.watch(0).unwrap();
        stages.modify(0, |s| s.gain = 1.0);
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn each_burst_is_seen_as_new() {
        let stages = Stages::new(1);
        let mut rx = stages.watch(0).unwrap();
        stages.burst(0, 0.5);
        assert_eq!(rx.borrow_and_update().bursts, 1);
        stages.burst(0, 0.5);
        assert!(
            rx.has_changed().unwrap(),
            "the same share twice is two bursts"
        );
        assert_eq!(rx.borrow_and_update().bursts, 2);
    }

    #[test]
    fn returning_to_normal_keeps_the_burst_count() {
        let stages = Stages::new(1);
        stages.burst(0, 0.25);
        stages.modify(0, |s| {
            s.gain = 4.0;
            s.down = true;
        });
        stages.normal(0);
        let s = stages.get(0);
        assert_eq!(s.gain, 1.0);
        assert!(!s.down && !s.frozen);
        assert_eq!(s.bursts, 1, "not read as a new burst");
    }
}
