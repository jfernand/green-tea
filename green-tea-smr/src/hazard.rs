//! Hazard pointers, after Maged Michael.
//!
//! Each thread has [`EACH`] shared words that only it writes, its *hazard pointers*.
//! Before a thread follows a pointer it read from a shared place, it writes the pointer
//! into one of them, and then reads the place again. If the place still holds the same
//! pointer, the node was linked at a moment when the hazard pointer already named it, and
//! from then on nobody will free it. If not, the thread starts over.
//!
//! A thread keeps the nodes it retires in a list of its own. When the list reaches
//! [`BATCH`] nodes, it reads every thread's hazard pointers once, and frees each node on
//! the list that none of them names.
//!
//! What it costs: a store and a second load for every pointer followed, where the epochs
//! cost two loads and two stores for a whole operation. What it buys: a thread that
//! stops can keep [`EACH`] nodes from being freed, and no more.

use crate::{Counts, Orphans, Reclaim, Retired, SLOTS};
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// Hazard pointers for each thread. The queue needs two; the stack, one.
pub const EACH: usize = 2;
/// Hazard pointers in all.
pub const ALL: usize = SLOTS * EACH;
/// A thread looks for nodes it can free when it has this many retired: twice as many as
/// there are hazard pointers, so that every look frees at least half the list.
pub const BATCH: usize = 2 * ALL;

/// One thread's hazard pointers, alone on a cache line.
#[repr(align(64))]
struct Slot([AtomicPtr<u8>; EACH]);

/// What the threads share: everyone's hazard pointers.
pub struct Hazards {
    slots: [Slot; SLOTS],
    taken: [AtomicBool; SLOTS],
    orphans: Orphans,
}

impl Default for Hazards {
    fn default() -> Hazards {
        Hazards::new()
    }
}

impl Hazards {
    pub const fn new() -> Hazards {
        Hazards {
            slots: [const { Slot([const { AtomicPtr::new(ptr::null_mut()) }; EACH]) }; SLOTS],
            taken: [const { AtomicBool::new(false) }; SLOTS],
            orphans: Orphans::new(),
        }
    }

    /// The side of the scheme that belongs to thread `slot`.
    ///
    /// # Panics
    /// If `slot` isn't below [`SLOTS`], or another `Local` has it.
    pub fn local(&self, slot: usize) -> Local<'_> {
        assert!(!self.taken[slot].swap(true, Ordering::SeqCst), "two threads with one slot");
        Local { shared: self, slot, retired: Vec::new(), counts: Counts::default() }
    }

    /// Free the nodes that `Local`s left behind when they were dropped. Returns how many.
    /// `&mut self` is the proof that no `Local` is left.
    pub fn free_orphans(&mut self) -> usize {
        self.orphans.free()
    }
}

impl Drop for Hazards {
    fn drop(&mut self) {
        self.free_orphans();
    }
}

/// One thread's side: its hazard pointers, and the nodes it has retired.
pub struct Local<'a> {
    shared: &'a Hazards,
    slot: usize,
    retired: Vec<Retired>,
    counts: Counts,
}

impl Local<'_> {
    /// `protect`, calling `pause` between each read of `source` and the store to the
    /// hazard pointer. A test does there what another thread could.
    pub fn protect_paused<T>(&mut self, index: usize, source: &AtomicPtr<T>, mut pause: impl FnMut()) -> *mut T {
        self.counts.protected += 1;
        let hazard = &self.shared.slots[self.slot].0[index];
        let mut node = source.load(Ordering::SeqCst);
        loop {
            pause();
            hazard.store(node as *mut u8, Ordering::SeqCst);
            // The node may have been unlinked, retired and freed between the load and
            // the store. If `source` still holds it now, it hadn't been.
            let again = source.load(Ordering::SeqCst);
            if again == node {
                return node;
            }
            node = again;
        }
    }

    /// Read every hazard pointer, and free the retired nodes that none of them names.
    pub fn scan(&mut self) {
        self.counts.rounds += 1;
        let mut named = [ptr::null_mut(); ALL];
        for (slot, hazards) in self.shared.slots.iter().enumerate() {
            for (index, hazard) in hazards.0.iter().enumerate() {
                named[slot * EACH + index] = hazard.load(Ordering::SeqCst);
            }
        }
        self.counts.looked += ALL;
        named.sort_unstable();
        let mut next = 0;
        while next < self.retired.len() {
            if named.binary_search(&self.retired[next].node).is_ok() {
                next += 1;
                continue;
            }
            self.counts.freed += 1;
            // SAFETY: the node was unlinked before it was retired, and no hazard pointer
            // named it when we looked, after that. A thread that names it later read its
            // address from somewhere it no longer is, and will find that out when it
            // reads the place again.
            unsafe { self.retired.swap_remove(next).free() };
        }
    }
}

impl Reclaim for Local<'_> {
    fn enter(&mut self) {
        self.counts.operations += 1;
    }

    fn protect<T>(&mut self, index: usize, source: &AtomicPtr<T>) -> *mut T {
        self.protect_paused(index, source, || ())
    }

    fn exit(&mut self) {
        for hazard in &self.shared.slots[self.slot].0 {
            hazard.store(ptr::null_mut(), Ordering::Release);
        }
    }

    unsafe fn retire<T>(&mut self, node: *mut T) {
        self.counts.retire();
        self.retired.push(Retired::new(node));
        if self.retired.len() >= BATCH {
            self.scan();
        }
    }

    fn counts(&self) -> Counts {
        self.counts
    }
}

impl Drop for Local<'_> {
    fn drop(&mut self) {
        self.exit();
        // What is still waiting stays with the `Hazards`: a `scan` here would free most
        // of it, and the counts a caller has already read would no longer say so.
        self.shared.orphans.leave(core::mem::take(&mut self.retired));
        self.shared.taken[self.slot].store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{ALL, BATCH, EACH, Hazards};
    use crate::Reclaim;
    use crate::stack::Stack;
    use core::sync::atomic::Ordering;

    #[test]
    fn a_thread_looks_at_the_hazard_pointers_once_for_every_batch_it_retires() {
        let hazards = Hazards::new();
        let mut local = hazards.local(0);
        let stack = Stack::new();
        for value in 0..10 * BATCH {
            stack.push(value);
            assert_eq!(stack.pop(&mut local), Some(value));
        }
        let counts = local.counts();
        assert_eq!((counts.retired, counts.freed, counts.peak), (10 * BATCH, 10 * BATCH, BATCH));
        assert_eq!((counts.rounds, counts.looked), (10, 10 * ALL));
    }

    #[test]
    fn a_node_that_a_hazard_pointer_names_is_not_freed_until_it_is_cleared() {
        // Whichever of a thread's hazard pointers names it.
        for index in 0..EACH {
            let hazards = Hazards::new();
            let (mut stalled, mut working) = (hazards.local(0), hazards.local(1));
            let stack = Stack::new();
            stack.push(0);
            // A pop that stops for good between its read and its compare-and-swap: its
            // hazard pointer names the node that holds 0.
            stalled.enter();
            let named = stalled.protect(index, stack.head());
            for value in 1..=1000 {
                assert_eq!(stack.pop(&mut working), Some(value - 1));
                stack.push(value);
            }
            working.scan();
            // One node waits, however many were retired.
            assert_eq!((working.counts().retired, working.counts().waiting()), (1000, 1));
            assert!(working.counts().peak <= BATCH);
            assert_eq!(working.retired[0].node, named as *mut u8);
            stalled.exit();
            working.scan();
            assert_eq!(working.counts().waiting(), 0);
        }
    }

    #[test]
    fn a_node_freed_between_the_read_and_the_hazard_pointer_is_not_returned() {
        let hazards = Hazards::new();
        let (mut mine, mut theirs) = (hazards.local(0), hazards.local(1));
        let stack = Stack::new();
        stack.push(1);
        let mut interrupted = false;
        let node = mine.protect_paused(0, stack.head(), || {
            if !core::mem::replace(&mut interrupted, true) {
                // No hazard pointer names the node yet, so the other thread's look frees it.
                assert_eq!(stack.pop(&mut theirs), Some(1));
                theirs.scan();
                assert_eq!(theirs.counts().freed, 1);
                // Something else takes the freed address, so that the next node can't.
                let filler = std::boxed::Box::new([0usize; 2]);
                stack.push(2);
                drop(filler);
            }
        });
        // The hazard pointer was set to a node that was gone. `protect` must have noticed,
        // and gone round again.
        assert_eq!(node as *const _, stack.head().load(Ordering::SeqCst) as *const _);
        assert_eq!(stack.pop(&mut mine), Some(2));
    }

    #[test]
    fn what_a_local_leaves_behind_is_freed_with_the_hazards() {
        let mut hazards = Hazards::new();
        let stack = Stack::new();
        stack.push(0);
        let mut stalled = hazards.local(0);
        stalled.protect(0, stack.head());
        {
            let mut working = hazards.local(1);
            assert_eq!(stack.pop(&mut working), Some(0));
        }
        // The working thread's `Local` is gone, and the node it retired was still named.
        drop(stalled);
        assert_eq!(hazards.free_orphans(), 1);
    }

    #[test]
    #[should_panic(expected = "two threads with one slot")]
    fn a_slot_has_one_local_at_a_time() {
        let hazards = Hazards::new();
        let _first = hazards.local(3);
        let _second = hazards.local(3);
    }
}
