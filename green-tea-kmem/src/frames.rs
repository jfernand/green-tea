//! An allocator of page frames: a binary buddy system whose unit is the page.
//!
//! A block is `2^order` pages, for an order from 0 to [`MAX_ORDER`], and starts at a
//! frame number that is a multiple of its size. There is a list of free blocks for each
//! order. An allocation takes a block from the list of its order, or halves a larger one
//! until a half is that size. A free looks at the block's buddy, the other half of the
//! block it was split from, and merges with it if it is free and whole, and again with
//! the buddy of the result.
//!
//! Nothing is kept in the pages. There is a [`Frame`] for every page, in a table the
//! caller provides, and the free lists are threaded through the table by frame number.
//! So the allocator never touches a free page, the order of an allocated block is on
//! record (`free` takes an address and nothing else), and a free of a block that isn't
//! allocated is refused.

use crate::{PAGE, PageSource};

/// How many orders there are: blocks of 1 page up to `2^MAX_ORDER`.
pub const ORDERS: usize = 11;
/// The largest order: a block of 1,024 pages, 4 MiB.
pub const MAX_ORDER: usize = ORDERS - 1;

/// No frame: the end of a list.
const NONE: u32 = u32::MAX;

/// What the first frame of a block says about it. Every other frame of a block is a
/// `Tail`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum State {
    /// Not the first frame of a block.
    Tail,
    /// The first frame of a free block, which is on the list of its order.
    Free,
    /// The first frame of an allocated block.
    Used,
}

/// The allocator's record of one page frame: 12 bytes.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Frame {
    state: State,
    /// For the first frame of a block, the block's order.
    order: u8,
    /// For the first frame of a free block, the next and the previous block on its list.
    next: u32,
    prev: u32,
}

impl Frame {
    /// What a table is filled with before it is given to [`Frames::new`].
    pub const NEW: Frame = Frame { state: State::Used, order: 0, next: NONE, prev: NONE };
}

/// Why a free was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreeError {
    /// The address isn't the start of one of the allocator's frames.
    NotAFrame,
    /// The frame isn't the first of an allocated block: the block is free already, or the
    /// address is inside a block.
    NotAllocated,
}

/// What the allocator has done.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub allocations: usize,
    /// Requests refused: no free block of that order or a larger one.
    pub refused: usize,
    pub frees: usize,
    /// Blocks halved.
    pub splits: usize,
    /// Pairs of buddies made one block again.
    pub merges: usize,
}

pub struct Frames<'a> {
    /// The address of frame 0.
    base: usize,
    table: &'a mut [Frame],
    /// For each order, the first free block of that order, or `NONE`.
    heads: [u32; ORDERS],
    /// For each order, how many free blocks there are.
    free: [usize; ORDERS],
    /// Pages in allocated blocks.
    used: usize,
    stats: Stats,
}

impl<'a> Frames<'a> {
    /// An allocator over `table.len()` pages, the first at `base`. The first `reserved`
    /// of them stay allocated for good (the kernel keeps `table` itself there); the rest
    /// are free.
    ///
    /// # Safety
    /// Nothing else may hand out the pages for as long as the allocator does.
    ///
    /// # Panics
    /// If `base` isn't page-aligned, or there are more frames than a `u32` can number.
    pub unsafe fn new(base: usize, table: &'a mut [Frame], reserved: usize) -> Frames<'a> {
        assert!(base.is_multiple_of(PAGE), "frame 0 must start on a page");
        assert!(table.len() < NONE as usize && reserved <= table.len(), "a table too large");
        table.fill(Frame::NEW);
        let pages = table.len();
        let mut frames =
            Frames { base, table, heads: [NONE; ORDERS], free: [0; ORDERS], used: pages, stats: Stats::default() };
        // Every page starts as an allocated block of order 0. Freeing the ones that aren't
        // reserved, one at a time, merges them into the largest blocks that fit.
        for frame in reserved..pages {
            frames.release(frame);
        }
        frames.stats = Stats::default();
        frames
    }

    /// The address of a block of `2^order` pages that nobody else has. `None` if there is
    /// no free block of that order or a larger one.
    #[inline(never)]
    pub fn allocate(&mut self, order: usize) -> Option<usize> {
        // The smallest free block that is large enough.
        let found = (order..ORDERS).find(|at| self.heads[*at] != NONE);
        let Some(from) = found else {
            self.stats.refused += 1;
            return None;
        };
        let frame = self.heads[from] as usize;
        self.unlink(frame, from);
        // Halve it until it is the size asked for. The upper half of each halving is a
        // free block of its own.
        for half in (order..from).rev() {
            self.push(frame + (1 << half), half);
            self.stats.splits += 1;
        }
        self.table[frame] = Frame { state: State::Used, order: order as u8, next: NONE, prev: NONE };
        self.used += 1 << order;
        self.stats.allocations += 1;
        Some(self.base + frame * PAGE)
    }

    /// Free the block that starts at `addr`. Returns its order, which the table knows.
    #[inline(never)]
    pub fn free(&mut self, addr: usize) -> Result<usize, FreeError> {
        let offset = addr.checked_sub(self.base).ok_or(FreeError::NotAFrame)?;
        let frame = offset / PAGE;
        if !offset.is_multiple_of(PAGE) || frame >= self.table.len() {
            return Err(FreeError::NotAFrame);
        }
        if self.table[frame].state != State::Used {
            return Err(FreeError::NotAllocated);
        }
        self.stats.frees += 1;
        Ok(self.release(frame))
    }

    /// Make the allocated block at `frame` free, merged with its buddy as far as it goes.
    fn release(&mut self, frame: usize) -> usize {
        let freed = self.table[frame].order as usize;
        self.used -= 1 << freed;
        let (mut frame, mut order) = (frame, freed);
        while order < MAX_ORDER {
            // The other half of the block this one was split from.
            let buddy = frame ^ (1 << order);
            if buddy + (1 << order) > self.table.len() {
                break; // the region ends before the buddy does
            }
            let there = self.table[buddy];
            if there.state != State::Free || there.order as usize != order {
                break; // allocated, or split into smaller blocks of which the first is free
            }
            self.unlink(buddy, order);
            // The merged block starts at the lower of the two, and the other is inside it.
            self.table[frame.max(buddy)].state = State::Tail;
            frame = frame.min(buddy);
            order += 1;
            self.stats.merges += 1;
        }
        self.push(frame, order);
        freed
    }

    /// Put the block at `frame` at the front of the free list of `order`.
    fn push(&mut self, frame: usize, order: usize) {
        let head = self.heads[order];
        self.table[frame] = Frame { state: State::Free, order: order as u8, next: head, prev: NONE };
        if head != NONE {
            self.table[head as usize].prev = frame as u32;
        }
        self.heads[order] = frame as u32;
        self.free[order] += 1;
    }

    /// Take the block at `frame` off the free list of `order`, wherever in it it is.
    fn unlink(&mut self, frame: usize, order: usize) {
        let Frame { next, prev, .. } = self.table[frame];
        match prev {
            NONE => self.heads[order] = next,
            prev => self.table[prev as usize].next = next,
        }
        if next != NONE {
            self.table[next as usize].prev = prev;
        }
        self.free[order] -= 1;
    }

    /// The address of frame 0.
    pub fn base(&self) -> usize {
        self.base
    }

    /// How many pages the allocator manages, the reserved ones included.
    pub fn pages(&self) -> usize {
        self.table.len()
    }

    /// Pages in allocated blocks, the reserved ones included.
    pub fn used_pages(&self) -> usize {
        self.used
    }

    /// Pages in free blocks.
    pub fn free_pages(&self) -> usize {
        self.table.len() - self.used
    }

    /// How many free blocks there are of each order.
    pub fn free_blocks(&self) -> [usize; ORDERS] {
        self.free
    }

    /// The order of the largest free block.
    pub fn largest_free(&self) -> Option<usize> {
        (0..ORDERS).rev().find(|order| self.free[*order] != 0)
    }

    /// The order of the allocated block that starts at `addr`.
    pub fn order_of(&self, addr: usize) -> Option<usize> {
        let offset = addr.checked_sub(self.base)?;
        let frame = self.table.get(offset / PAGE).filter(|_| offset.is_multiple_of(PAGE))?;
        (frame.state == State::Used).then_some(frame.order as usize)
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Walk the table and the lists, and check what must always be true: blocks tile the
    /// region, each starts on a multiple of its size, a free block is on the list of its
    /// order, and no two free buddies are apart.
    pub fn check(&self) -> Result<(), &'static str> {
        let pages = self.table.len();
        let (mut frame, mut used, mut free) = (0, 0, [0; ORDERS]);
        while frame < pages {
            let Frame { state, order, .. } = self.table[frame];
            let (order, size) = (order as usize, 1usize << order);
            if state == State::Tail || order > MAX_ORDER {
                return Err("a block doesn't start where the one before it ends");
            }
            if !frame.is_multiple_of(size) || frame + size > pages {
                return Err("a block isn't on a multiple of its size");
            }
            if self.table[frame + 1..frame + size].iter().any(|inside| inside.state != State::Tail) {
                return Err("a block starts inside another");
            }
            if state == State::Used {
                used += size;
            } else {
                free[order] += 1;
                let buddy = frame ^ size;
                let whole = buddy + size <= pages && self.table[buddy].state == State::Free;
                if order < MAX_ORDER && whole && self.table[buddy].order as usize == order {
                    return Err("two free buddies weren't merged");
                }
            }
            frame += size;
        }
        if used != self.used || free != self.free {
            return Err("the counts don't match the table");
        }
        for (order, &head) in self.heads.iter().enumerate() {
            let (mut at, mut prev, mut count) = (head, NONE, 0);
            while at != NONE {
                let on_list = self.table[at as usize];
                if on_list.state != State::Free || on_list.order as usize != order || on_list.prev != prev {
                    return Err("a free list holds a block that doesn't belong on it");
                }
                count += 1;
                if count > self.free[order] {
                    return Err("a free list is longer than its count");
                }
                (prev, at) = (at, on_list.next);
            }
            if count != self.free[order] {
                return Err("a free block is on no list");
            }
        }
        Ok(())
    }
}

/// One page at a time, for a slab cache.
impl PageSource for Frames<'_> {
    fn take_page(&mut self) -> Option<usize> {
        self.allocate(0)
    }

    unsafe fn give_page(&mut self, page: usize) {
        let freed = self.free(page);
        assert!(freed == Ok(0), "a page given back that wasn't taken: {freed:?}");
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{Frame, Frames, FreeError, MAX_ORDER, ORDERS};
    use crate::PAGE;
    use std::vec;
    use std::vec::Vec;

    /// Frame 0 of the tests' allocators. No test touches a page, so no memory is there.
    const BASE: usize = 0x8000_0000;

    fn with_frames<R>(pages: usize, reserved: usize, f: impl FnOnce(&mut Frames) -> R) -> R {
        let mut table = vec![Frame::NEW; pages];
        let mut frames = unsafe { Frames::new(BASE, &mut table, reserved) };
        frames.check().unwrap();
        f(&mut frames)
    }

    fn by_order(counts: &[(usize, usize)]) -> [usize; ORDERS] {
        let mut free = [0; ORDERS];
        for &(order, count) in counts {
            free[order] = count;
        }
        free
    }

    #[test]
    fn a_new_region_is_in_the_largest_blocks_that_fit() {
        with_frames(4096, 0, |frames| {
            assert_eq!(frames.free_blocks(), by_order(&[(10, 4)]));
            assert_eq!((frames.free_pages(), frames.used_pages()), (4096, 0));
        });
        // 12 pages reserved at the start: 4 free pages up to 16, then 16, 32, ... 512, and
        // three blocks of 1,024.
        with_frames(4096, 12, |frames| {
            assert_eq!(
                frames.free_blocks(),
                by_order(&[(2, 1), (4, 1), (5, 1), (6, 1), (7, 1), (8, 1), (9, 1), (10, 3)])
            );
            assert_eq!((frames.free_pages(), frames.used_pages()), (4084, 12));
            assert_eq!(frames.stats().merges, 0, "the statistics start after the region is set up");
        });
        // A region that isn't a power of two: 1,000 pages is 512 + 256 + 128 + 64 + 32 + 8.
        with_frames(1000, 0, |frames| {
            assert_eq!(frames.free_blocks(), by_order(&[(3, 1), (5, 1), (6, 1), (7, 1), (8, 1), (9, 1)]));
        });
    }

    #[test]
    fn an_allocation_halves_a_larger_block() {
        with_frames(1024, 0, |frames| {
            assert_eq!(frames.allocate(0), Some(BASE));
            // One block of each order below 10 is left: the upper half of each halving.
            assert_eq!(
                frames.free_blocks(),
                by_order(&[(0, 1), (1, 1), (2, 1), (3, 1), (4, 1), (5, 1), (6, 1), (7, 1), (8, 1), (9, 1)])
            );
            assert_eq!(frames.stats().splits, 10);
            assert_eq!(frames.allocate(0), Some(BASE + PAGE), "the other half of the last halving");
            assert_eq!(frames.allocate(1), Some(BASE + 2 * PAGE));
            assert_eq!(frames.stats().splits, 10, "blocks of the right size were free");
            frames.check().unwrap();
        });
    }

    #[test]
    fn a_free_merges_with_its_buddy_and_no_other_neighbour() {
        with_frames(8, 0, |frames| {
            let pages: Vec<usize> = (0..8).map(|_| frames.allocate(0).unwrap()).collect();
            assert_eq!(pages, (0..8).map(|i| BASE + i * PAGE).collect::<Vec<_>>());
            // Pages 1 and 2 are neighbours and not buddies: 1's buddy is 0, and 2's is 3.
            assert_eq!(frames.free(pages[1]), Ok(0));
            assert_eq!(frames.free(pages[2]), Ok(0));
            assert_eq!(frames.free_blocks(), by_order(&[(0, 2)]));
            assert_eq!(frames.allocate(1), None, "two free pages side by side, and no block of two");
            // Page 3 is 2's buddy, and the pair's buddy is the pair 0 and 1, of which 0 is in use.
            assert_eq!(frames.free(pages[3]), Ok(0));
            assert_eq!(frames.free_blocks(), by_order(&[(0, 1), (1, 1)]));
            assert_eq!(frames.free(pages[0]), Ok(0));
            assert_eq!(frames.free_blocks(), by_order(&[(2, 1)]));
            assert_eq!(frames.stats().merges, 3);
            frames.check().unwrap();
        });
    }

    #[test]
    fn a_buddy_that_is_free_but_split_is_not_merged() {
        with_frames(4, 0, |frames| {
            let a = frames.allocate(1).unwrap(); // pages 0 and 1
            let b = frames.allocate(0).unwrap(); // page 2
            let _c = frames.allocate(0).unwrap(); // page 3
            // Page 2 is free and is where a's buddy would start, but a's buddy is the pair
            // 2 and 3, and 3 is in use.
            assert_eq!(frames.free(b), Ok(0));
            assert_eq!(frames.free(a), Ok(1));
            assert_eq!(frames.free_blocks(), by_order(&[(0, 1), (1, 1)]));
            frames.check().unwrap();
        });
    }

    #[test]
    fn the_order_of_a_block_is_on_record() {
        with_frames(64, 0, |frames| {
            let block = frames.allocate(3).unwrap();
            assert_eq!(frames.order_of(block), Some(3));
            assert_eq!(frames.order_of(block + PAGE), None);
            assert_eq!(frames.used_pages(), 8);
            assert_eq!(frames.free(block), Ok(3));
            assert_eq!(frames.used_pages(), 0);
            assert_eq!(frames.free_blocks(), by_order(&[(6, 1)]));
        });
    }

    #[test]
    fn mistaken_frees_are_refused() {
        with_frames(64, 4, |frames| {
            let block = frames.allocate(2).unwrap();
            assert_eq!(frames.free(block + PAGE), Err(FreeError::NotAllocated), "inside a block");
            assert_eq!(frames.free(block + 1), Err(FreeError::NotAFrame));
            assert_eq!(frames.free(BASE - PAGE), Err(FreeError::NotAFrame));
            assert_eq!(frames.free(BASE + 64 * PAGE), Err(FreeError::NotAFrame));
            assert_eq!(frames.free(block), Ok(2));
            assert_eq!(frames.free(block), Err(FreeError::NotAllocated), "a second free");
            assert_eq!(frames.stats().frees, 1);
            // The reserved pages are allocated blocks of one page: freeing one is the
            // caller's mistake, and the table can't tell.
            frames.check().unwrap();
        });
    }

    #[test]
    fn requests_that_cannot_be_met_are_refused() {
        with_frames(1000, 0, |frames| {
            assert_eq!(frames.allocate(MAX_ORDER), None, "1,000 pages hold no block of 1,024");
            assert_eq!(frames.allocate(ORDERS), None);
            assert!(frames.allocate(9).is_some());
            assert_eq!(frames.allocate(9), None);
            assert_eq!(frames.stats().refused, 3);
        });
    }

    #[test]
    fn every_other_page_free_holds_no_block_of_two() {
        with_frames(1024, 0, |frames| {
            let pages: Vec<usize> = (0..1024).map(|_| frames.allocate(0).unwrap()).collect();
            assert_eq!(frames.allocate(0), None);
            for page in pages.iter().step_by(2) {
                frames.free(*page).unwrap();
            }
            assert_eq!(frames.free_blocks(), by_order(&[(0, 512)]));
            assert_eq!(frames.allocate(1), None);
            for page in pages.iter().skip(1).step_by(2) {
                frames.free(*page).unwrap();
            }
            assert_eq!(frames.free_blocks(), by_order(&[(10, 1)]));
            assert_eq!(frames.stats().merges, 1023);
            frames.check().unwrap();
        });
    }

    #[test]
    fn a_random_workload_leaves_the_region_as_it_found_it() {
        with_frames(4096, 12, |frames| {
            let before = frames.free_blocks();
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let mut held: Vec<(usize, usize)> = Vec::new();
            for step in 0..20_000 {
                if held.len() < 300 && next() % 3 != 0 {
                    let order = (next() % 6) as usize;
                    if let Some(addr) = frames.allocate(order) {
                        assert!((addr - BASE).is_multiple_of(PAGE << order));
                        held.push((addr, order));
                    }
                } else if !held.is_empty() {
                    let (addr, order) = held.swap_remove(next() as usize % held.len());
                    assert_eq!(frames.free(addr), Ok(order));
                }
                if step % 500 == 0 {
                    frames.check().unwrap();
                    let used: usize = held.iter().map(|&(_, order)| 1 << order).sum();
                    assert_eq!(frames.used_pages(), used + 12);
                }
            }
            // No two blocks handed out overlap.
            held.sort();
            for pair in held.windows(2) {
                assert!(pair[0].0 + (PAGE << pair[0].1) <= pair[1].0);
            }
            for (addr, _) in held {
                frames.free(addr).unwrap();
            }
            frames.check().unwrap();
            assert_eq!(frames.free_blocks(), before);
        });
    }
}
