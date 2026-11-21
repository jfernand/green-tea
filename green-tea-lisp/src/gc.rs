//! What the tracing heaps have in common.
//!
//! A tracing heap frees nothing when a reference is dropped. When it has no room it
//! *collects*: it starts from the root stack, follows every reference it finds, and keeps
//! what it reached. [`crate::mark::MarkHeap`] marks what it reaches and then sweeps or
//! compacts; [`crate::semispace::Semispace`] copies what it reaches to the other half of
//! its memory. Both find references the same way: a root slot or a field is a reference
//! when [`Value::as_obj`] says so, and nothing else is looked at. A reference that is
//! anywhere else when a collection starts, such as in a Rust variable of the interpreter,
//! is not followed, and is not corrected if its object moves.
//!
//! Everything is counted ([`Counts`]). Times are measured only if the heap is given a
//! clock, and are in the clock's ticks.

use crate::{Heap, Obj, Value};
use alloc::collections::BTreeSet;
use alloc::vec::Vec;

/// What a tracing heap counts beside [`crate::Stats`]. An object is *traced* when a
/// collection reaches it: it is marked, or copied.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    pub collections: u64,
    /// Objects traced, over all collections, and the words in them.
    pub traced: u64,
    pub traced_words: u64,
    /// Root slots read by collections.
    pub roots: u64,
    /// Objects passed over by the phases that walk the heap from one end to the other,
    /// reachable or not: one for each object at each sweep, three at each compaction.
    pub walked: u64,
    /// Words moved to another place: copied, or slid down by a compaction.
    pub moved_words: u64,
    /// The most objects one collection traced, and the most it traced and walked together.
    pub largest_traced: u64,
    pub largest_work: u64,
    /// The most entries the mark stack held, how many times an object didn't fit on it,
    /// and how many times the heap was searched for such objects afterwards.
    pub deepest: usize,
    pub overflows: u64,
    pub rescans: u64,
    /// With a clock: the ticks spent in collections, and in the longest of them.
    pub pause_total: u64,
    pub pause_longest: u64,
}

impl Counts {
    /// Objects traced and walked: what the collections did, in objects.
    pub fn work(&self) -> u64 {
        self.traced + self.walked
    }

    /// One collection is over: it traced `traced` objects and walked `walked`, and
    /// started at `started` on the clock.
    pub(crate) fn finish(&mut self, traced: u64, walked: u64, clock: Option<fn() -> u64>, started: u64) {
        self.collections += 1;
        self.largest_traced = self.largest_traced.max(traced);
        self.largest_work = self.largest_work.max(traced + walked);
        if let Some(clock) = clock {
            let pause = clock().wrapping_sub(started);
            self.pause_total += pause;
            self.pause_longest = self.pause_longest.max(pause);
        }
    }
}

/// A heap that collects.
pub trait Collect: Heap {
    /// Collect now. Like `alloc`, this ends every borrowed reference.
    fn collect(&mut self);

    fn counts(&self) -> Counts;

    /// How many words the heap was given, all told.
    fn capacity(&self) -> usize;

    /// Collect at every allocation. A reference that the interpreter kept where the heap
    /// can't see it then goes wrong at the first allocation, not at the rare one that
    /// happens to find the heap full.
    fn set_stress(&mut self, stress: bool);

    /// Time collections with `clock`, which returns ticks of some kind.
    fn set_clock(&mut self, clock: fn() -> u64);
}

/// The objects that can be reached from the root stack, and the words in them, counted
/// through the interface alone. Right after a collection this is all a heap should hold.
pub fn reachable<H: Heap>(heap: &H) -> (u64, usize) {
    let mut seen = BTreeSet::new();
    let mut work: Vec<Obj> = (0..heap.roots()).filter_map(|slot| heap.root(slot).as_obj()).collect();
    let mut words = 0;
    while let Some(obj) = work.pop() {
        if !seen.insert(obj.0) {
            continue;
        }
        let fields = heap.kind(obj).fields();
        words += fields + 1;
        work.extend((0..fields).filter_map(|i| heap.field(obj, i).as_obj()));
    }
    (seen.len() as u64, words)
}

/// The reference in a word, if it holds one.
pub(crate) fn reference(word: u64) -> Option<usize> {
    Value::from_bits(word).as_obj().map(|obj| obj.0 as usize)
}
