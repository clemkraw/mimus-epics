//! What clients write, shared by every connection of a run.
//!
//! A PUT changes what its channel carries from then on, on every connection:
//! an operator screen that writes a setpoint reads it back, and an archiver
//! subscribed to the same channel elsewhere records it. MIMUS keeps, for
//! each channel written to, the fields written under its `value`, by bit, and
//! writes them over the replayed ones in every update, read and RPC reply
//! that carries them.
//!
//! Almost no update is of a channel written to, so the question is asked
//! with one atomic load per update; the fields themselves are behind a lock
//! taken only for the channels that have some.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

/// The fields written to one channel: each field's encoding, by its bit in
/// the channel's changed set.
pub type Fields = BTreeMap<usize, Vec<u8>>;

/// The fields written to the channels of a fleet.
#[derive(Debug)]
pub struct Written {
    /// Whether anything was written to each channel, by instance.
    marks: Vec<AtomicBool>,
    fields: RwLock<HashMap<u32, Arc<Fields>>>,
    channels: AtomicUsize,
}

impl Written {
    /// A store for a fleet of `instances` channels, none written to.
    pub fn new(instances: usize) -> Arc<Self> {
        Arc::new(Self {
            marks: (0..instances).map(|_| AtomicBool::new(false)).collect(),
            fields: RwLock::new(HashMap::new()),
            channels: AtomicUsize::new(0),
        })
    }

    /// The fields written to a channel, or `None` if nothing was.
    pub fn get(&self, instance: u32) -> Option<Arc<Fields>> {
        if !self.marks.get(instance as usize)?.load(Ordering::Acquire) {
            return None;
        }
        let fields = self.fields.read().unwrap_or_else(|e| e.into_inner());
        fields.get(&instance).cloned()
    }

    /// Records fields written to a channel. A field written again replaces
    /// what was written before; the others stay.
    pub fn write(&self, instance: u32, written: impl IntoIterator<Item = (usize, Vec<u8>)>) {
        let Some(mark) = self.marks.get(instance as usize) else {
            return;
        };
        let mut written = written.into_iter().peekable();
        if written.peek().is_none() {
            return;
        }
        {
            let mut fields = self.fields.write().unwrap_or_else(|e| e.into_inner());
            let entry = Arc::make_mut(fields.entry(instance).or_default());
            entry.extend(written);
        }
        if !mark.swap(true, Ordering::Release) {
            self.channels.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How many channels hold something written.
    pub fn channels(&self) -> usize {
        self.channels.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_written_until_a_client_writes() {
        let w = Written::new(4);
        assert!(w.get(2).is_none());
        assert_eq!(w.channels(), 0);
        // Out of range is nothing written, not a panic.
        assert!(w.get(99).is_none());
        w.write(99, [(1, vec![1])]);
        assert_eq!(w.channels(), 0);
    }

    #[test]
    fn a_field_written_again_replaces_the_first_and_the_others_stay() {
        let w = Written::new(4);
        w.write(2, [(1, vec![1]), (3, vec![3])]);
        w.write(2, [(1, vec![9])]);
        let fields = w.get(2).unwrap();
        assert_eq!(fields.get(&1), Some(&vec![9]));
        assert_eq!(fields.get(&3), Some(&vec![3]));
        assert_eq!(w.channels(), 1, "one channel, written twice");
        assert!(w.get(1).is_none(), "the others are untouched");
    }

    #[test]
    fn writing_nothing_marks_nothing() {
        let w = Written::new(4);
        w.write(0, Vec::new());
        assert!(w.get(0).is_none());
        assert_eq!(w.channels(), 0);
    }

    #[test]
    fn what_a_reader_holds_is_not_changed_under_it() {
        let w = Written::new(1);
        w.write(0, [(1, vec![1])]);
        let before = w.get(0).unwrap();
        w.write(0, [(1, vec![2])]);
        assert_eq!(before.get(&1), Some(&vec![1]));
        assert_eq!(w.get(0).unwrap().get(&1), Some(&vec![2]));
    }
}
