//! The binary buddy system: every block is a power of two in size, and a block is only
//! ever split in half or merged with the other half it was split from, its *buddy*.
//!
//! The region is one block of `2^k` bytes. An allocation rounds what it needs up to a
//! power of two and takes a free block of that size; if there is none, it takes the
//! smallest larger one and halves it until one half is the right size, leaving the other
//! halves free. There is a list of free blocks for each size.
//!
//! A block of size `s` is at an offset from the start of the region that is a multiple
//! of `s`, and its buddy is at that offset with one bit flipped: `offset ^ s`. So a free
//! finds the only block it could merge with by an exclusive or, and reads one word there
//! to see whether it is free and whole.
//!
//! Each block starts with a word that says how large it is and whether it is allocated.
//! What the caller gets is the rest: a request for 100 bytes needs 108, and takes 128.

use crate::{ALIGN, Block, FreeError, Heap, Stats, USED, WORD};

/// The smallest block: the word at its start, and two links for when it is free, rounded
/// up to a power of two.
pub const MIN_BLOCK: usize = 32;
/// The most sizes: 32 bytes to 2 GiB.
pub const ORDERS: usize = 27;

pub struct Buddy {
    /// The first block.
    base: usize,
    /// The region is one block of `MIN_BLOCK << top` bytes.
    top: usize,
    /// For each order, the first free block of `MIN_BLOCK << order` bytes, or 0.
    heads: [usize; ORDERS],
    used: usize,
    high: usize,
    stats: Stats,
}

impl Buddy {
    /// An allocator over the largest power of two of bytes that fits in the `len` bytes
    /// at `region`.
    ///
    /// # Safety
    /// The region must be writable, and not in use by anything else for as long as the
    /// allocator is.
    ///
    /// # Panics
    /// If the region is too small for a block, or too large for the table of lists.
    pub unsafe fn new(region: usize, len: usize) -> Buddy {
        // Blocks start a word before a multiple of 16, so that what follows the word does.
        let base = region.next_multiple_of(ALIGN) + WORD;
        let room = (region + len).checked_sub(base).filter(|room| *room >= MIN_BLOCK).expect("a region too small");
        let top = (room / MIN_BLOCK).ilog2() as usize;
        assert!(top < ORDERS, "a region too large for the table");
        let mut buddy = Buddy { base, top, heads: [0; ORDERS], used: 0, high: base, stats: Stats::default() };
        buddy.push(base, top);
        buddy
    }

    /// The order of the block a request for `size` bytes takes: the smallest power of two
    /// that holds the request and the word before it.
    pub fn order_for(size: usize) -> Option<usize> {
        let block = size.checked_add(WORD)?.checked_next_power_of_two()?.max(MIN_BLOCK);
        Some((block / MIN_BLOCK).ilog2() as usize)
    }

    /// A block of at least `size` bytes, at a multiple of 16. `None` if there is no free
    /// block large enough, or if `align` is more than 16, which this allocator can't do.
    #[inline(never)]
    pub fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        let order = Buddy::order_for(size).filter(|order| *order <= self.top && align <= ALIGN);
        // The smallest free block that is large enough: look at each list from `order` up.
        let mut examined = 0;
        let found = order.and_then(|order| {
            (order..=self.top).find(|at| {
                examined += 1;
                self.heads[*at] != 0
            })
        });
        self.stats.examined += examined;
        self.stats.longest_search = self.stats.longest_search.max(examined);
        let (Some(order), Some(mut at)) = (order, found) else {
            self.stats.failed += 1;
            return None;
        };
        let block = self.heads[at];
        self.unlink(block, at);
        // Halve it until it is the size wanted. The upper half of each cut stays free.
        while at > order {
            at -= 1;
            self.push(block + (MIN_BLOCK << at), at);
            self.stats.splits += 1;
        }
        self.write(block, order << 1 | USED);
        self.used += MIN_BLOCK << order;
        self.high = self.high.max(block + (MIN_BLOCK << order));
        self.stats.allocations += 1;
        self.stats.peak = self.stats.peak.max(self.used);
        Some(block + WORD)
    }

    /// Give back the block at `addr`, merging it with its buddy for as long as the buddy
    /// is free.
    ///
    /// # Safety
    /// `addr` must have come from `allocate` on this allocator, and not have been freed since.
    #[inline(never)]
    pub unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError> {
        let mut block = addr.wrapping_sub(WORD);
        let size = MIN_BLOCK << self.top;
        if block < self.base || block >= self.base + size || !(block - self.base).is_multiple_of(MIN_BLOCK) {
            return Err(FreeError::NotABlock);
        }
        let header = self.read(block);
        if header & USED == 0 {
            return Err(FreeError::NotAllocated);
        }
        let mut order = header >> 1;
        if order > self.top || !(block - self.base).is_multiple_of(MIN_BLOCK << order) {
            return Err(FreeError::Corrupt);
        }
        self.used -= MIN_BLOCK << order;
        self.stats.frees += 1;
        self.write(block, order << 1); // free, where a second free of `addr` will look
        while order < self.top {
            let buddy = self.base + ((block - self.base) ^ (MIN_BLOCK << order));
            // The buddy can be merged only if it is free and hasn't been split.
            if self.read(buddy) != order << 1 {
                break;
            }
            self.unlink(buddy, order);
            block = block.min(buddy);
            order += 1;
            self.stats.merges += 1;
        }
        self.push(block, order);
        Ok(())
    }

    /// Mark a block free and put it at the head of its order's list.
    fn push(&mut self, block: usize, order: usize) {
        let next = self.heads[order];
        self.write(block, order << 1);
        self.write(block + WORD, next);
        self.write(block + 2 * WORD, 0);
        if next != 0 {
            self.write(next + 2 * WORD, block);
        }
        self.heads[order] = block;
    }

    /// Take a free block off its order's list.
    fn unlink(&mut self, block: usize, order: usize) {
        let (next, prev) = (self.read(block + WORD), self.read(block + 2 * WORD));
        if next != 0 {
            self.write(next + 2 * WORD, prev);
        }
        if prev != 0 {
            self.write(prev + WORD, next);
        } else {
            self.heads[order] = next;
        }
    }

    /// Every block in the region, in address order.
    pub fn blocks(&self) -> Blocks<'_> {
        Blocks { buddy: self, at: self.base }
    }

    /// The free blocks, in address order.
    pub fn free_list(&self) -> impl Iterator<Item = Block> + '_ {
        self.blocks().filter(|block| !block.used)
    }

    /// The region's size.
    pub fn capacity(&self) -> usize {
        MIN_BLOCK << self.top
    }

    /// Bytes in allocated blocks: each a power of two.
    pub fn used(&self) -> usize {
        self.used
    }

    /// See [`Fit::footprint`](crate::Fit::footprint).
    pub fn footprint(&self) -> usize {
        self.high - self.base
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The first block.
    pub fn first(&self) -> usize {
        self.base
    }

    /// The size of the largest free block, or 0.
    pub fn largest_free(&self) -> usize {
        (0..=self.top).rev().find(|order| self.heads[*order] != 0).map_or(0, |order| MIN_BLOCK << order)
    }

    /// Check everything the allocator relies on: the region, then every list.
    pub fn check(&self) -> Result<(), &'static str> {
        let end = self.base + self.capacity();
        let (mut at, mut used, mut free_blocks) = (self.base, 0, 0usize);
        while at < end {
            let header = self.read(at);
            let (order, is_used) = (header >> 1, header & USED != 0);
            if order > self.top || !(at - self.base).is_multiple_of(MIN_BLOCK << order) {
                return Err("a block's size makes no sense");
            }
            if is_used {
                used += MIN_BLOCK << order;
            } else {
                free_blocks += 1;
                let buddy = self.base + ((at - self.base) ^ (MIN_BLOCK << order));
                if order < self.top && self.read(buddy) == header {
                    return Err("a free block's buddy is free too");
                }
            }
            at += MIN_BLOCK << order;
        }
        if used != self.used {
            return Err("the allocated blocks don't add up to `used`");
        }
        let mut listed = 0;
        for order in 0..ORDERS {
            let (mut at, mut prev) = (self.heads[order], 0);
            while at != 0 {
                if at < self.base || at >= end || !(at - self.base).is_multiple_of(MIN_BLOCK) {
                    return Err("a free list leaves the region");
                }
                if self.read(at) != order << 1 {
                    return Err("a block on a free list is allocated, or of another size");
                }
                if self.read(at + 2 * WORD) != prev {
                    return Err("a block's back link isn't the block before it on the list");
                }
                listed += 1;
                if listed > free_blocks {
                    return Err("the free lists have more blocks than are free");
                }
                (prev, at) = (at, self.read(at + WORD));
            }
        }
        if listed != free_blocks {
            return Err("a free block isn't on a free list");
        }
        Ok(())
    }

    fn read(&self, addr: usize) -> usize {
        // SAFETY: every address read is inside the region `new` was given.
        unsafe { (addr as *const usize).read() }
    }

    fn write(&mut self, addr: usize, value: usize) {
        // SAFETY: as for `read`.
        unsafe { (addr as *mut usize).write(value) }
    }
}

impl Heap for Buddy {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        Buddy::allocate(self, size, align)
    }
    unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError> {
        unsafe { Buddy::free(self, addr) }
    }
    fn used(&self) -> usize {
        Buddy::used(self)
    }
    fn footprint(&self) -> usize {
        Buddy::footprint(self)
    }
    fn stats(&self) -> Stats {
        Buddy::stats(self)
    }
    fn check(&self) -> Result<(), &'static str> {
        Buddy::check(self)
    }
}

pub struct Blocks<'a> {
    buddy: &'a Buddy,
    at: usize,
}

impl Iterator for Blocks<'_> {
    type Item = Block;

    fn next(&mut self) -> Option<Block> {
        if self.at >= self.buddy.base + self.buddy.capacity() {
            return None;
        }
        let header = self.buddy.read(self.at);
        let order = (header >> 1).min(self.buddy.top); // a header written over could say anything
        let block = Block { start: self.at, size: MIN_BLOCK << order, used: header & USED != 0 };
        self.at += block.size;
        Some(block)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{Buddy, MIN_BLOCK};
    use crate::FreeError;
    use crate::trace::Random;
    use std::vec;
    use std::vec::Vec;

    fn with_buddy<R>(len: usize, f: impl FnOnce(&mut Buddy) -> R) -> R {
        let mut memory = vec![0u128; len / 16 + 1];
        let mut buddy = unsafe { Buddy::new(memory.as_mut_ptr() as usize, len + 16) };
        f(&mut buddy)
    }

    fn sizes(buddy: &Buddy) -> Vec<(usize, bool)> {
        buddy.blocks().map(|b| (b.size, b.used)).collect()
    }

    #[test]
    fn a_request_takes_the_power_of_two_that_holds_it_and_a_word() {
        assert_eq!(Buddy::order_for(1), Some(0));
        assert_eq!(Buddy::order_for(24), Some(0));
        assert_eq!(Buddy::order_for(25), Some(1));
        assert_eq!(Buddy::order_for(100), Some(2)); // 108 bytes: 128
        assert_eq!(Buddy::order_for(120), Some(2));
        assert_eq!(Buddy::order_for(121), Some(3)); // 129 bytes: 256
        assert_eq!(Buddy::order_for(usize::MAX), None);
    }

    #[test]
    fn a_block_is_halved_until_it_fits_and_merged_back_when_freed() {
        with_buddy(1024, |buddy| {
            assert_eq!((buddy.capacity(), sizes(buddy)), (1024, vec![(1024, false)]));
            let a = buddy.allocate(100, 8).unwrap();
            assert!(a.is_multiple_of(16));
            assert_eq!(sizes(buddy), [(128, true), (128, false), (256, false), (512, false)]);
            assert_eq!(buddy.stats().splits, 3);
            let b = buddy.allocate(100, 8).unwrap();
            assert_eq!(b - a, 128); // a's buddy
            unsafe { buddy.free(a).unwrap() };
            assert_eq!(sizes(buddy), [(128, false), (128, true), (256, false), (512, false)]);
            unsafe { buddy.free(b).unwrap() };
            assert_eq!(sizes(buddy), [(1024, false)]);
            assert_eq!((buddy.stats().merges, buddy.used(), buddy.footprint()), (3, 0, 256));
            buddy.check().unwrap();
        });
    }

    #[test]
    fn neighbours_that_are_not_buddies_do_not_merge() {
        with_buddy(1024, |buddy| {
            let blocks: Vec<usize> = (0..4).map(|_| buddy.allocate(100, 8).unwrap()).collect();
            // The second and third are neighbours, and halves of different blocks of 256.
            unsafe { buddy.free(blocks[1]).unwrap() };
            unsafe { buddy.free(blocks[2]).unwrap() };
            assert_eq!(sizes(buddy), [(128, true), (128, false), (128, false), (128, true), (512, false)]);
            assert_eq!(buddy.allocate(200, 8).map(|addr| addr - blocks[0]), Some(512));
            buddy.check().unwrap();
        });
    }

    #[test]
    fn mistakes_are_refused() {
        with_buddy(1024, |buddy| {
            let a = buddy.allocate(40, 8).unwrap();
            let b = buddy.allocate(40, 8).unwrap();
            assert_eq!(unsafe { buddy.free(a + 16) }, Err(FreeError::NotABlock));
            assert_eq!(unsafe { buddy.free(a) }, Ok(()));
            assert_eq!(unsafe { buddy.free(a) }, Err(FreeError::NotAllocated));
            assert_eq!(unsafe { buddy.free(b) }, Ok(()));
            assert_eq!(unsafe { buddy.free(b) }, Err(FreeError::NotAllocated)); // after merging
            assert_eq!(buddy.allocate(2000, 8), None);
            assert_eq!(buddy.allocate(8, 64), None);
            assert_eq!(buddy.stats().failed, 2);
        });
    }

    #[test]
    fn random_allocations_and_frees_keep_the_heap_consistent() {
        with_buddy(1 << 18, |buddy| {
            let mut random = Random(42);
            let mut live: Vec<(usize, usize, u8)> = Vec::new();
            for step in 0..4000 {
                if live.len() < 100 && random.below(3) != 0 {
                    let size = if random.below(8) == 0 { 128 + random.below(2000) } else { 1 + random.below(200) };
                    let addr = buddy.allocate(size, 16).unwrap();
                    assert!(addr.is_multiple_of(16));
                    unsafe { core::ptr::write_bytes(addr as *mut u8, step as u8, size) };
                    live.push((addr, size, step as u8));
                } else if !live.is_empty() {
                    let (addr, size, byte) = live.swap_remove(random.below(live.len()));
                    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                    assert!(bytes.iter().all(|b| *b == byte), "a block was written over");
                    unsafe { buddy.free(addr).unwrap() };
                }
                buddy.check().unwrap();
            }
            for (addr, _, _) in live {
                unsafe { buddy.free(addr).unwrap() };
            }
            assert_eq!(buddy.blocks().count(), 1);
            assert_eq!(buddy.largest_free(), 1 << 18);
            assert!(buddy.stats().longest_search <= 14 && MIN_BLOCK == 32);
        });
    }
}
