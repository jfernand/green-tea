//! A stack of small numbers that any CPU may push to and pop from, with no lock.
//!
//! The numbers are indexes into a table that the caller keeps, and each entry of the
//! table has a link: the index below it on the stack. The stack itself is one word, the
//! *head*, which holds the index on top and, beside it, a *tag* that counts pops.
//!
//! A push writes the entry's link and then replaces the head, with a compare-and-swap
//! that succeeds only if the head is still what the link was copied from. A pop reads
//! the head, reads the top entry's link, and replaces the head with that link, again with
//! a compare-and-swap.
//!
//! The pop is where the tag is needed. Between its two reads and its swap, other CPUs
//! can pop the top entry, pop or push others, and push the first one back. The index on
//! top is then what the pop saw, and the link it read is stale. Comparing the index alone
//! would let the swap through (the ABA problem). Every pop adds one to the tag, so a head
//! that has been popped from since it was read no longer compares equal.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// The table a [`Stack`] threads itself through.
pub trait Links {
    /// The link of entry `index`: the index below it, plus one, or 0 at the bottom.
    fn link(&self, index: usize) -> &AtomicU32;
}

impl Links for [AtomicU32] {
    fn link(&self, index: usize) -> &AtomicU32 {
        &self[index]
    }
}

/// The low half of the head: the index on top, plus one, or 0 for an empty stack.
const TOP: u64 = 0xffff_ffff;
/// One more pop, in the high half.
const POP: u64 = 1 << 32;

pub struct Stack {
    head: AtomicU64,
    /// Times a push or a pop found the head changed under it, and went round again.
    retries: AtomicUsize,
}

impl Default for Stack {
    fn default() -> Stack {
        Stack::new()
    }
}

impl Stack {
    pub const fn new() -> Stack {
        Stack { head: AtomicU64::new(0), retries: AtomicUsize::new(0) }
    }

    /// Put entry `index` on top. The entry must not be on the stack already.
    pub fn push<L: Links + ?Sized>(&self, links: &L, index: usize) {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            links.link(index).store((head & TOP) as u32, Ordering::Release);
            // The tag stays as it is: a push invalidates nobody's link.
            let new = (head & !TOP) | (index as u64 + 1);
            match self.head.compare_exchange(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(seen) => head = seen,
            }
            self.retries.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Take the entry on top, if there is one.
    pub fn pop<L: Links + ?Sized>(&self, links: &L) -> Option<usize> {
        self.pop_with(links, || ())
    }

    /// [`pop`](Stack::pop), calling `between` after each time it has read the head and the
    /// top entry's link, and before it tries to replace the head. The tests use it to do,
    /// at that moment, what another CPU might.
    pub fn pop_with<L: Links + ?Sized>(&self, links: &L, mut between: impl FnMut()) -> Option<usize> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let top = (head & TOP) as usize;
            if top == 0 {
                return None;
            }
            // The entry may have been popped by someone else by now. Its link is in the
            // table, which is never freed, so the read is safe, and may be stale.
            let below = links.link(top - 1).load(Ordering::Acquire);
            between();
            let new = (head & !TOP).wrapping_add(POP) | below as u64;
            match self.head.compare_exchange(head, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(top - 1),
                Err(seen) => head = seen,
            }
            self.retries.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How many pops the stack has served.
    pub fn pops(&self) -> usize {
        (self.head.load(Ordering::Relaxed) >> 32) as usize
    }

    /// Times an operation had to start again because another CPU got in first.
    pub fn retries(&self) -> usize {
        self.retries.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::Stack;
    use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::thread;
    use std::vec::Vec;

    fn table(entries: usize) -> Vec<AtomicU32> {
        (0..entries).map(|_| AtomicU32::new(0)).collect()
    }

    #[test]
    fn what_was_pushed_last_is_popped_first() {
        let (stack, links) = (Stack::new(), table(3));
        assert_eq!(stack.pop(&links[..]), None);
        for index in [2, 1, 0] {
            stack.push(&links[..], index);
        }
        let popped: Vec<_> = (0..4).map(|_| stack.pop(&links[..])).collect();
        assert_eq!(popped, [Some(0), Some(1), Some(2), None]);
        assert_eq!((stack.pops(), stack.retries()), (3, 0));
    }

    /// The ABA problem, by hand. A pop reads that 0 is on top with 1 below it. Before it
    /// swaps, "another CPU" pops 0, pops 1 and keeps it, and pushes 0 back. The top is 0
    /// again, and what is below it is now 2.
    #[test]
    fn a_pop_that_was_overtaken_starts_again() {
        let (stack, links) = (Stack::new(), table(3));
        for index in [2, 1, 0] {
            stack.push(&links[..], index);
        }
        let mut overtaken = false;
        let popped = stack.pop_with(&links[..], || {
            if !overtaken {
                overtaken = true;
                assert_eq!(stack.pop(&links[..]), Some(0));
                assert_eq!(stack.pop(&links[..]), Some(1)); // and entry 1 is now in use
                stack.push(&links[..], 0);
            }
        });
        assert_eq!(popped, Some(0));
        assert_eq!(stack.retries(), 1, "the swap should have failed once: the head had been popped from");
        // Entry 1 is in use. A stack that hands it out again has given one thing to two owners.
        assert_eq!(stack.pop(&links[..]), Some(2), "the stack handed out an entry that was not on it");
        assert_eq!(stack.pop(&links[..]), None);
    }

    /// Eight threads pop an entry, mark it as theirs, check nobody else has it, and push
    /// it back, over and over. An entry handed to two threads at once is seen by one of them.
    #[test]
    fn eight_threads_never_hold_the_same_entry() {
        const ENTRIES: usize = 4;
        const ROUNDS: usize = 200_000;
        let (stack, links) = (Stack::new(), table(ENTRIES));
        let holders: Vec<AtomicUsize> = (0..ENTRIES).map(|_| AtomicUsize::new(0)).collect();
        for index in 0..ENTRIES {
            stack.push(&links[..], index);
        }
        let shared: usize = thread::scope(|s| {
            let spawned: Vec<_> = (1..=8)
                .map(|me| {
                    let (stack, links, holders) = (&stack, &links, &holders);
                    s.spawn(move || {
                        let mut shared = 0;
                        for _ in 0..ROUNDS {
                            let Some(index) = stack.pop(&links[..]) else { continue };
                            if holders[index].swap(me, Ordering::AcqRel) != 0 {
                                shared += 1;
                            }
                            if holders[index].swap(0, Ordering::AcqRel) != me {
                                shared += 1;
                            }
                            stack.push(&links[..], index);
                        }
                        shared
                    })
                })
                .collect();
            spawned.into_iter().map(|t| t.join().unwrap()).sum()
        });
        assert_eq!(shared, 0, "an entry was held by two threads at once");
        let mut left: Vec<_> = (0..ENTRIES).map(|_| stack.pop(&links[..]).unwrap()).collect();
        left.sort_unstable();
        assert_eq!(left, [0, 1, 2, 3]);
        assert_eq!(stack.pop(&links[..]), None);
    }
}
