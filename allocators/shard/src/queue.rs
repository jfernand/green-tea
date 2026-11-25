//! snmalloc's queue of freed blocks: many CPUs put blocks in, and one takes them out.
//!
//! The queue is a chain of blocks, linked through their first words, oldest first. `front`
//! is the oldest and belongs to the one CPU that takes blocks out. `back` is the newest,
//! and is shared.
//!
//! A CPU adds a whole chain of blocks at once, however long, with one atomic exchange:
//! it swaps the address of its chain's last block into `back`, and then writes the address
//! of its chain's first block into the block that was `back` before. Nothing is compared
//! and nothing is retried.
//!
//! Taking a block out uses no atomic exchange at all. The block at the front is handed
//! over once it has a successor, which becomes the front. So the queue is never empty:
//! the newest block stays in it until another arrives. It starts with a block that the
//! owner supplies.
//!
//! Between a sender's swap and its write, the chain it is adding, and everything added
//! after it, is in the queue and can't be reached from the front. The owner sees them
//! when the write has been made.

use core::cell::Cell;
use core::sync::atomic::{AtomicUsize, Ordering};

fn link(block: usize) -> &'static AtomicUsize {
    // SAFETY: a block in a queue is at least a word, at a multiple of a word's size, and
    // stays allocated for as long as it is in the queue.
    unsafe { AtomicUsize::from_ptr(block as *mut usize) }
}

pub struct Queue {
    back: AtomicUsize,
    /// Read and written only by the queue's owner.
    front: Cell<usize>,
}

// SAFETY: `back` is atomic, and `front` is used only by `take`, which the caller promises
// to call from one thread at a time.
unsafe impl Sync for Queue {}

impl Queue {
    /// A queue holding `first`, a block that stays in it until another is added.
    ///
    /// # Safety
    /// `first` must be the address of a word of memory that outlives its stay in the queue.
    pub unsafe fn new(first: usize) -> Queue {
        link(first).store(0, Ordering::Relaxed);
        Queue { back: AtomicUsize::new(first), front: Cell::new(first) }
    }

    /// Add the chain of blocks from `first` to `last`, linked through their first words.
    /// Any CPU may call this at any time.
    ///
    /// # Safety
    /// The blocks must be the caller's, each at least a word, and none in the queue already.
    pub unsafe fn add(&self, first: usize, last: usize) {
        // SAFETY: the caller's promise.
        unsafe { self.add_with(first, last, || ()) }
    }

    /// [`add`](Queue::add), calling `between` after the swap and before the write. The
    /// tests use it to do, at that moment, what another CPU might.
    ///
    /// # Safety
    /// As for [`add`](Queue::add).
    pub unsafe fn add_with(&self, first: usize, last: usize, between: impl FnOnce()) {
        link(last).store(0, Ordering::Relaxed);
        let before = self.back.swap(last, Ordering::AcqRel);
        between();
        link(before).store(first, Ordering::Release);
    }

    /// The oldest block, unless it is the only one that can be reached.
    ///
    /// # Safety
    /// Only one thread may call this at a time: the queue's owner.
    pub unsafe fn take(&self) -> Option<usize> {
        let oldest = self.front.get();
        let after = link(oldest).load(Ordering::Acquire);
        if after == 0 {
            return None;
        }
        self.front.set(after);
        Some(oldest)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::Queue;
    use std::boxed::Box;
    use std::thread;
    use std::vec;
    use std::vec::Vec;

    /// `count` blocks of two words each. The second word is the test's to mark.
    fn blocks(count: usize) -> Vec<usize> {
        let memory = Box::leak(vec![[0usize; 2]; count].into_boxed_slice());
        memory.iter_mut().map(|block| block.as_mut_ptr() as usize).collect()
    }

    fn chain(blocks: &[usize]) -> (usize, usize) {
        for pair in blocks.windows(2) {
            unsafe { *(pair[0] as *mut usize) = pair[1] };
        }
        (blocks[0], *blocks.last().unwrap())
    }

    fn drain(queue: &Queue) -> Vec<usize> {
        core::iter::from_fn(|| unsafe { queue.take() }).collect()
    }

    #[test]
    fn blocks_come_out_in_the_order_they_went_in_and_the_newest_stays() {
        let b = blocks(6);
        let queue = unsafe { Queue::new(b[0]) };
        assert_eq!(drain(&queue), []);
        let (first, last) = chain(&b[1..4]);
        unsafe { queue.add(first, last) };
        assert_eq!(drain(&queue), [b[0], b[1], b[2]]);
        unsafe { queue.add(b[4], b[4]) };
        unsafe { queue.add(b[5], b[5]) };
        assert_eq!(drain(&queue), [b[3], b[4]]);
    }

    /// The figure in the snmalloc paper, by hand: thread A has swapped and not yet
    /// written when thread B adds its blocks, completely. B's blocks can't be seen until
    /// A writes.
    #[test]
    fn a_chain_added_behind_an_unfinished_one_waits_for_it() {
        let b = blocks(5);
        let queue = unsafe { Queue::new(b[0]) };
        let (a_first, a_last) = chain(&b[1..3]);
        let mut seen_in_between = vec![];
        unsafe {
            queue.add_with(a_first, a_last, || {
                let (b_first, b_last) = chain(&b[3..5]);
                queue.add(b_first, b_last);
                seen_in_between = drain(&queue);
            })
        };
        assert_eq!(seen_in_between, [], "B's blocks are behind A's, and A's are not linked in yet");
        assert_eq!(drain(&queue), [b[0], b[1], b[2], b[3]]);
    }

    #[test]
    fn four_senders_lose_nothing_and_each_ones_blocks_stay_in_order() {
        const SENDERS: usize = 4;
        const CHAINS: usize = 2_000;
        const LENGTH: usize = 8;
        let all = blocks(1 + SENDERS * CHAINS * LENGTH);
        let queue = unsafe { Queue::new(all[0]) };
        let mut taken: Vec<usize> = Vec::new();
        thread::scope(|s| {
            for (sender, mine) in all[1..].chunks(CHAINS * LENGTH).enumerate() {
                let queue = &queue;
                s.spawn(move || {
                    for (number, blocks) in mine.chunks(LENGTH).enumerate() {
                        for (place, &block) in blocks.iter().enumerate() {
                            // Mark each block with who sent it and when.
                            unsafe { *(block as *mut usize).add(1) = sender << 32 | (number * LENGTH + place) };
                        }
                        let (first, last) = chain(blocks);
                        unsafe { queue.add(first, last) };
                    }
                });
            }
            // The owner takes blocks out while the senders are still adding.
            while taken.len() < 1000 {
                taken.extend(unsafe { queue.take() });
            }
        });
        taken.extend(drain(&queue));
        assert_eq!(taken.len(), SENDERS * CHAINS * LENGTH, "all but the newest block come out");
        let mut next = [0; SENDERS];
        for &block in &taken[1..] {
            let mark = unsafe { *(block as *const usize).add(1) };
            assert_eq!(mark & 0xffff_ffff, next[mark >> 32], "a sender's blocks came out of order");
            next[mark >> 32] += 1;
        }
    }
}
