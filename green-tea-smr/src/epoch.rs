//! Epoch-based reclamation, in the form Trevor Brown calls DEBRA.
//!
//! There is one shared count, the *epoch*, and one shared word for each thread, its
//! *announcement*: the epoch the thread saw when it began its present operation, or a
//! bit that says it is between operations (*quiescent*). A thread keeps the nodes it
//! retires in three *limbo bags*, one for each of the last three epochs it announced.
//!
//! The epoch moves on only when every thread is either quiescent or has announced the
//! present epoch. A thread that begins an operation and finds the epoch changed since its
//! last one moves to its oldest bag, and frees what is in it. By then the thread has seen
//! the epoch change three times since it put the last node in that bag, so the epoch has
//! changed twice since that node was retired, and between those two changes every thread
//! was quiescent at least once. A quiescent thread holds no node's address.
//!
//! Nobody is in charge of moving the epoch. Each thread, as it begins an operation, looks
//! at one other thread's announcement, and remembers how far round it has got. A thread
//! that has seen all [`SLOTS`] agree with the epoch it read tries to add one to it.
//!
//! What it costs: a thread that stops in the middle of an operation stops the epoch, and
//! from then on no thread frees anything.

use crate::{Counts, Orphans, Reclaim, Retired, SLOTS};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// The low bit of an announcement: set while the thread is between operations.
const QUIESCENT: usize = 1;
/// The epoch counts in twos, to leave that bit free.
const EPOCH: usize = 2;

/// A word alone on its cache line, so that one thread's writes to its own don't take the
/// line from a thread reading another.
#[repr(align(64))]
struct Padded<T>(T);

/// What the threads share: the epoch, and each thread's announcement.
pub struct Epochs {
    epoch: Padded<AtomicUsize>,
    announcements: [Padded<AtomicUsize>; SLOTS],
    taken: [AtomicBool; SLOTS],
    orphans: Orphans,
}

impl Default for Epochs {
    fn default() -> Epochs {
        Epochs::new()
    }
}

impl Epochs {
    pub const fn new() -> Epochs {
        Epochs {
            epoch: Padded(AtomicUsize::new(0)),
            announcements: [const { Padded(AtomicUsize::new(QUIESCENT)) }; SLOTS],
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
        Local {
            shared: self,
            slot,
            announced: 0,
            checking: 0,
            bags: [const { Vec::new() }; 3],
            current: 0,
            counts: Counts::default(),
        }
    }

    /// How many times the epoch has moved on.
    pub fn epoch(&self) -> usize {
        self.epoch.0.load(Ordering::SeqCst) / EPOCH
    }

    /// Free the nodes that `Local`s left behind when they were dropped. Returns how many.
    /// `&mut self` is the proof that no `Local` is left.
    pub fn free_orphans(&mut self) -> usize {
        self.orphans.free()
    }
}

impl Drop for Epochs {
    fn drop(&mut self) {
        self.free_orphans();
    }
}

/// One thread's side: its place among the announcements, and its limbo bags.
pub struct Local<'a> {
    shared: &'a Epochs,
    slot: usize,
    /// The epoch this thread announced last.
    announced: usize,
    /// How many announcements, from slot 0 up, it has seen agree with `announced`.
    checking: usize,
    /// The limbo bags: the nodes retired under this thread's last three announcements.
    bags: [Vec<Retired>; 3],
    /// The bag that `retire` adds to. The next one round is the oldest.
    current: usize,
    counts: Counts,
}

impl Local<'_> {
    /// Make the oldest bag the current one, and free what is in it.
    fn rotate(&mut self) {
        self.current = (self.current + 1) % 3;
        self.counts.freed += self.bags[self.current].len();
        for node in self.bags[self.current].drain(..) {
            // SAFETY: this bag was the current one three announcements ago, so this
            // thread has seen the epoch change three times since the last of these nodes
            // was retired, and the epoch has changed twice since then. The second change
            // needed every thread to be quiescent, or to have announced the epoch the
            // first change made, which a thread does only on its way out of quiescence.
            // So every thread has been quiescent since the node was unlinked, and none
            // can have found it afterwards.
            unsafe { node.free() };
        }
    }

    /// Whether this thread is in an operation.
    pub fn is_active(&self) -> bool {
        self.shared.announcements[self.slot].0.load(Ordering::SeqCst) & QUIESCENT == 0
    }
}

impl Reclaim for Local<'_> {
    fn enter(&mut self) {
        self.counts.operations += 1;
        let shared = self.shared;
        let epoch = shared.epoch.0.load(Ordering::SeqCst);
        if epoch != self.announced {
            self.announced = epoch;
            self.checking = 0;
            self.rotate();
        }
        // Look at one thread's announcement. It agrees if the thread is between
        // operations, or began its operation in this epoch.
        self.counts.looked += 1;
        let other = shared.announcements[self.checking % SLOTS].0.load(Ordering::SeqCst);
        if other & QUIESCENT != 0 || other == epoch {
            self.checking += 1;
            if self.checking >= SLOTS
                && shared.epoch.0.compare_exchange(epoch, epoch + EPOCH, Ordering::SeqCst, Ordering::SeqCst).is_ok()
            {
                self.counts.rounds += 1;
            }
        }
        // Announce the epoch, and that an operation is under way. Every pointer this
        // operation reads, it reads after this store.
        shared.announcements[self.slot].0.store(epoch, Ordering::SeqCst);
    }

    fn protect<T>(&mut self, _index: usize, source: &AtomicPtr<T>) -> *mut T {
        self.counts.protected += 1;
        source.load(Ordering::SeqCst)
    }

    fn exit(&mut self) {
        self.shared.announcements[self.slot].0.store(self.announced | QUIESCENT, Ordering::Release);
    }

    unsafe fn retire<T>(&mut self, node: *mut T) {
        self.counts.retire();
        self.bags[self.current].push(Retired::new(node));
    }

    fn counts(&self) -> Counts {
        self.counts
    }
}

impl Drop for Local<'_> {
    fn drop(&mut self) {
        // What is still in limbo can't be freed from here: another thread may be in the
        // middle of an operation. Leave it with the `Epochs`.
        for bag in &mut self.bags {
            self.shared.orphans.leave(core::mem::take(bag));
        }
        self.shared.announcements[self.slot].0.store(QUIESCENT, Ordering::SeqCst);
        self.shared.taken[self.slot].store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::Epochs;
    use crate::stack::Stack;
    use crate::{Reclaim, SLOTS};

    /// An operation that does nothing but begin and end.
    fn operation(smr: &mut impl Reclaim) {
        smr.enter();
        smr.exit();
    }

    #[test]
    fn a_thread_alone_moves_the_epoch_every_eight_operations() {
        let epochs = Epochs::new();
        let mut local = epochs.local(0);
        for _ in 0..10 * SLOTS {
            operation(&mut local);
        }
        // It looks at one announcement in each operation, and needs all eight.
        assert_eq!(epochs.epoch(), 10);
        assert_eq!(local.counts().looked, 10 * SLOTS);
        assert_eq!(local.counts().rounds, 10);
    }

    #[test]
    fn a_retired_node_is_freed_when_its_thread_has_seen_three_new_epochs() {
        let epochs = Epochs::new();
        let mut local = epochs.local(0);
        let stack = Stack::new();
        stack.push(7);
        assert_eq!(stack.pop(&mut local), Some(7));
        let mut operations = 0;
        while local.counts().freed == 0 {
            assert_eq!(local.counts().waiting(), 1);
            operation(&mut local);
            operations += 1;
            assert!(operations < 100, "the node was never freed");
        }
        // The pop was the first of the eight operations that end epoch 0. The seventh
        // after it moves the epoch, the eighth sees it moved (one), eight more see it
        // moved again (two), and eight more a third time.
        assert_eq!(operations, 3 * SLOTS);
        assert_eq!(epochs.epoch(), 3);
    }

    #[test]
    fn a_thread_that_stops_in_an_operation_stops_every_free() {
        let epochs = Epochs::new();
        let (mut stalled, mut working) = (epochs.local(0), epochs.local(1));
        let stack = Stack::new();
        stalled.enter();
        for value in 0..1000 {
            stack.push(value);
            assert_eq!(stack.pop(&mut working), Some(value));
        }
        // The epoch could move once, since the stalled thread had announced it.
        assert_eq!(epochs.epoch(), 1);
        assert_eq!((working.counts().retired, working.counts().freed), (1000, 0));
        stalled.exit();
        for _ in 0..4 * SLOTS {
            operation(&mut working);
        }
        assert_eq!(working.counts().waiting(), 0);
    }

    #[test]
    fn a_thread_between_operations_holds_nothing_up() {
        let epochs = Epochs::new();
        let (idle, mut working) = (epochs.local(0), epochs.local(1));
        let stack = Stack::new();
        for value in 0..1000 {
            stack.push(value);
            assert_eq!(stack.pop(&mut working), Some(value));
        }
        assert!(!idle.is_active());
        assert!(working.counts().waiting() <= 3 * SLOTS);
        assert_eq!(epochs.epoch(), 1000 / SLOTS);
    }

    #[test]
    fn what_a_local_leaves_behind_is_freed_with_the_epochs() {
        let mut epochs = Epochs::new();
        let stack = Stack::new();
        {
            let mut local = epochs.local(0);
            for value in 0..5 {
                stack.push(value);
                stack.pop(&mut local);
            }
            assert_eq!(local.counts().freed, 0);
        }
        assert_eq!(epochs.free_orphans(), 5);
        assert_eq!(epochs.free_orphans(), 0);
        drop(epochs.local(0)); // the slot is free again
    }

    #[test]
    #[should_panic(expected = "two threads with one slot")]
    fn a_slot_has_one_local_at_a_time() {
        let epochs = Epochs::new();
        let _first = epochs.local(3);
        let _second = epochs.local(3);
    }
}
