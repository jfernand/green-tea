//! An allocator for blocks of any size, over one region of memory.
//!
//! The region is a row of blocks, each either allocated or free. Every block starts with
//! a word, its *header*, and ends with a word, its *footer*, and both hold the same
//! value: the block's size, with the lowest bit set if the block is allocated. Those are
//! Knuth's *boundary tags*: from any block, the footer of the block before it and the
//! header of the block after it are each one word away, so freeing a block can merge it
//! with free neighbours without searching for them.
//!
//! ```text
//!           allocated                               free
//! +--------+------------------+--------+  +--------+------+------+-----+--------+
//! | size|1 | the caller's     | size|1 |  | size   | next | prev | ... | size   |
//! +--------+------------------+--------+  +--------+------+------+-----+--------+
//!          ^ the address handed out
//! ```
//!
//! The free blocks are on a doubly linked list, threaded through their second and third
//! words. [`Policy`] says which block on it an allocation takes, and [`Order`] where a
//! freed block goes on it.
//!
//! Addresses handed out are multiples of 16, so headers are at addresses that are 8 more
//! than a multiple of 16, and every size is a multiple of 16. The smallest block is 32
//! bytes: the two tags and the two links.
//!
//! green-tea-bare's heap is one of these (see its `heap` module).
//!
//! [`Fit`] keeps its free blocks on one list and searches it. Two more allocators here
//! don't search: [`buddy::Buddy`] has a list for each power of two, and [`tlsf::Tlsf`] a
//! list for each of a few hundred ranges of size, found through two bitmaps. All three
//! are a [`Heap`].
//!
//! [`trace`] replays a recorded sequence of allocations and frees against an allocator and
//! measures its fragmentation, and [`adversary`] is a program written to fragment one.
//! Those two allocate (a `Vec`, a `BTreeMap`), so they need a global allocator; the
//! allocator itself needs nothing.

#![no_std]

extern crate alloc;

pub mod adversary;
pub mod buddy;
pub mod pages;
pub mod tlsf;
pub mod trace;

pub(crate) const WORD: usize = size_of::<usize>();
/// Every address handed out is a multiple of this, and so is every block's size.
pub const ALIGN: usize = 2 * WORD;
/// What a block costs beyond what was asked for: its header and its footer.
pub const OVERHEAD: usize = 2 * WORD;
/// The smallest block: a header, two links and a footer.
pub const MIN_BLOCK: usize = 4 * WORD;
/// The bit of a tag that says its block is allocated.
pub(crate) const USED: usize = 1;

/// Which free block an allocation takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Policy {
    /// The first block on the list that is large enough.
    FirstFit,
    /// The first that is large enough, searching from where the last search stopped.
    NextFit,
    /// The smallest block on the list that is large enough.
    BestFit,
}

/// Where on the free list a freed block goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order {
    /// At the front: the block most recently freed is the first one searched.
    Lifo,
    /// In order of address, lowest first.
    Address,
}

/// Why [`Fit::free`] refused an address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreeError {
    /// The address isn't one this allocator could have handed out.
    NotABlock,
    /// The block there is already free.
    NotAllocated,
    /// The block's header and footer don't agree, or its size makes no sense: something
    /// wrote over a tag.
    Corrupt,
}

/// A block, as a walk over the region finds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Block {
    /// The address of the block's header. What the caller got is one word above.
    pub start: usize,
    /// The whole block, tags included.
    pub size: usize,
    pub used: bool,
}

/// What an allocator has done so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub allocations: usize,
    /// Allocations refused because no free block was large enough.
    pub failed: usize,
    pub frees: usize,
    /// Free blocks that allocations looked at while searching.
    pub examined: usize,
    /// Free blocks that frees walked past to find a freed block's place (`Order::Address`).
    pub passed: usize,
    /// Allocations that took part of a free block and left the rest free.
    pub splits: usize,
    /// Free neighbours that frees merged into the block being freed.
    pub merges: usize,
    /// The most bytes ever allocated at once, tags included.
    pub peak: usize,
    /// The most free blocks any one allocation looked at, and the most any one free
    /// walked past.
    pub longest_search: usize,
    pub longest_walk: usize,
}

/// What the three allocators of this crate have in common: enough to replay a trace
/// against one and measure what it needed.
pub trait Heap {
    /// A block of at least `size` bytes at a multiple of `align`, or `None`.
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize>;
    /// Give back a block.
    ///
    /// # Safety
    /// `addr` must have come from `allocate` on this allocator, and not have been freed since.
    unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError>;
    /// Bytes in allocated blocks, whatever the allocator adds to each included.
    fn used(&self) -> usize;
    /// The distance from the start of the region to the highest address an allocated
    /// block has reached.
    fn footprint(&self) -> usize;
    fn stats(&self) -> Stats;
    /// Check everything the allocator relies on.
    fn check(&self) -> Result<(), &'static str>;
}

impl Heap for Fit {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        Fit::allocate(self, size, align)
    }
    unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError> {
        unsafe { Fit::free(self, addr) }
    }
    fn used(&self) -> usize {
        Fit::used(self)
    }
    fn footprint(&self) -> usize {
        Fit::footprint(self)
    }
    fn stats(&self) -> Stats {
        Fit::stats(self)
    }
    fn check(&self) -> Result<(), &'static str> {
        Fit::check(self)
    }
}

pub struct Fit {
    /// The header of the first block.
    first: usize,
    /// One past the last block: a header-sized word marked allocated, so that the last
    /// block's neighbour is never free. The word before `first` is marked the same way.
    end: usize,
    /// The first block on the free list, or 0.
    head: usize,
    /// Where the next `NextFit` search starts: a block on the free list, or 0.
    rover: usize,
    policy: Policy,
    order: Order,
    /// Bytes in allocated blocks, tags included.
    used: usize,
    /// The highest address any allocated block has reached: one past its footer.
    high: usize,
    stats: Stats,
}

impl Fit {
    /// An allocator over the `len` bytes at `region`, all of it one free block.
    ///
    /// # Safety
    /// The region must be writable, and not in use by anything else for as long as the
    /// allocator is.
    ///
    /// # Panics
    /// If the region is too small to hold a block.
    pub unsafe fn new(region: usize, len: usize, policy: Policy, order: Order) -> Fit {
        let first = region.next_multiple_of(ALIGN) + WORD;
        let end = ((region + len) & !(ALIGN - 1)) - WORD;
        assert!(end >= first + MIN_BLOCK, "a region too small for one block");
        let mut fit = Fit { first, end, head: 0, rover: 0, policy, order, used: 0, high: first, stats: Stats::default() };
        fit.write(first - WORD, USED);
        fit.write(end, USED);
        fit.set_block(first, end - first, false);
        fit.insert(first);
        fit.stats = Stats::default();
        fit
    }

    /// A block of at least `size` bytes at a multiple of `align`, which must be a power
    /// of two. `None` if no free block has room for it.
    #[inline(never)]
    pub fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        debug_assert!(align.is_power_of_two());
        // What the block must hold: the caller's bytes and the two tags, rounded up.
        let need = size.checked_add(OVERHEAD + ALIGN - 1).map(|n| (n & !(ALIGN - 1)).max(MIN_BLOCK));
        let examined = self.stats.examined;
        let found = need.and_then(|need| Some((need, self.find(need, align)?)));
        self.stats.longest_search = self.stats.longest_search.max(self.stats.examined - examined);
        let Some((need, (block, start))) = found else {
            self.stats.failed += 1;
            return None;
        };
        // The new block runs from `start` to `end`. It takes the rest of the free block
        // too if the rest couldn't be a block of its own.
        let block_end = block + self.size(block);
        let mut end = start + need;
        if block_end - end < MIN_BLOCK {
            end = block_end;
        }
        if end < block_end {
            self.set_block(end, block_end - end, false);
            self.stats.splits += 1;
        }
        if start > block {
            // Only when `align` is more than 16: the free block's beginning stays free,
            // in its place on the list, and what is left after the new block is another.
            self.set_block(block, start - block, false);
            if end < block_end {
                self.insert(end);
            }
        } else if end < block_end {
            self.replace(block, end); // what is left takes the free block's place
        } else {
            self.unlink(block);
        }
        self.set_block(start, end - start, true);
        self.used += end - start;
        self.high = self.high.max(end);
        self.stats.allocations += 1;
        self.stats.peak = self.stats.peak.max(self.used);
        Some(start + WORD)
    }

    /// Give back the block at `addr`, merging it with whichever of its neighbours are free.
    ///
    /// # Safety
    /// `addr` must have come from `allocate` on this allocator, and not have been freed
    /// since. The allocator refuses what it can tell is wrong, which is not everything:
    /// its only evidence is the tags around `addr`.
    #[inline(never)]
    pub unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError> {
        let mut block = addr.wrapping_sub(WORD);
        if !addr.is_multiple_of(ALIGN) || block < self.first || block >= self.end {
            return Err(FreeError::NotABlock);
        }
        let (mut size, used) = self.tag(block);
        if !used {
            return Err(FreeError::NotAllocated);
        }
        if size < MIN_BLOCK || !size.is_multiple_of(ALIGN) || size > self.end - block {
            return Err(FreeError::Corrupt);
        }
        if self.tag(block + size - WORD) != (size, used) {
            return Err(FreeError::Corrupt);
        }
        self.used -= size;
        self.stats.frees += 1;
        // Mark the block free where it is, even if it is about to be merged: a second
        // free of `addr` then finds a free tag for as long as nothing overwrites it.
        self.set_block(block, size, false);

        let (before, before_used) = self.tag(block - WORD); // the footer of the block before
        if !before_used {
            block -= before;
            size += before;
            self.unlink(block);
            self.stats.merges += 1;
        }
        let (after, after_used) = self.tag(block + size); // the header of the block after
        if !after_used {
            self.unlink(block + size);
            size += after;
            self.stats.merges += 1;
        }
        self.set_block(block, size, false);
        let passed = self.stats.passed;
        self.insert(block);
        self.stats.longest_walk = self.stats.longest_walk.max(self.stats.passed - passed);
        Ok(())
    }

    /// Every block in the region, in address order.
    pub fn blocks(&self) -> Blocks<'_> {
        Blocks { fit: self, at: self.first }
    }

    /// The free blocks, in the order of the free list.
    pub fn free_list(&self) -> FreeBlocks<'_> {
        FreeBlocks { fit: self, at: self.head }
    }

    /// The region's size: every block, tags included.
    pub fn capacity(&self) -> usize {
        self.end - self.first
    }

    /// Bytes in allocated blocks, tags included.
    pub fn used(&self) -> usize {
        self.used
    }

    /// How much of the region has ever been in an allocated block: the distance from its
    /// start to the highest address a block has reached. It never goes down. A region of
    /// this size would have been enough for everything asked of the allocator so far.
    pub fn footprint(&self) -> usize {
        self.high - self.first
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The header of the first block.
    pub fn first(&self) -> usize {
        self.first
    }

    /// The size of the largest free block, or 0.
    pub fn largest_free(&self) -> usize {
        self.free_list().map(|b| b.size).max().unwrap_or(0)
    }

    /// Check everything the allocator relies on, by walking the region and then the list.
    pub fn check(&self) -> Result<(), &'static str> {
        let (mut at, mut used, mut free_blocks, mut last_free) = (self.first, 0, 0usize, false);
        while at < self.end {
            let (size, is_used) = self.tag(at);
            if size < MIN_BLOCK || !size.is_multiple_of(ALIGN) || size > self.end - at {
                return Err("a block's size makes no sense");
            }
            if self.tag(at + size - WORD) != (size, is_used) {
                return Err("a block's header and footer disagree");
            }
            if !is_used && last_free {
                return Err("two free blocks are neighbours");
            }
            if is_used {
                used += size;
            } else {
                free_blocks += 1;
            }
            last_free = !is_used;
            at += size;
        }
        if used != self.used {
            return Err("the allocated blocks don't add up to `used`");
        }
        let (mut at, mut prev, mut listed, mut rover_listed) = (self.head, 0, 0usize, self.rover == 0);
        while at != 0 {
            if at < self.first || at >= self.end || !(at - WORD).is_multiple_of(ALIGN) {
                return Err("the free list leaves the region");
            }
            if self.tag(at).1 {
                return Err("an allocated block is on the free list");
            }
            if self.prev(at) != prev {
                return Err("a block's back link isn't the block before it on the list");
            }
            if self.order == Order::Address && at <= prev {
                return Err("the free list isn't in address order");
            }
            listed += 1;
            if listed > free_blocks {
                return Err("the free list has more blocks than are free");
            }
            rover_listed |= at == self.rover;
            (prev, at) = (at, self.next(at));
        }
        if listed != free_blocks {
            return Err("a free block isn't on the free list");
        }
        if !rover_listed {
            return Err("the rover isn't on the free list");
        }
        Ok(())
    }

    /// The free block to allocate `need` bytes from, and where in it the new block starts.
    fn find(&mut self, need: usize, align: usize) -> Option<(usize, usize)> {
        match self.policy {
            Policy::FirstFit => {
                let mut block = self.head;
                while block != 0 {
                    self.stats.examined += 1;
                    if let Some(start) = place(block, self.size(block), need, align) {
                        return Some((block, start));
                    }
                    block = self.next(block);
                }
                None
            }
            Policy::NextFit => {
                let from = if self.rover != 0 { self.rover } else { self.head };
                let mut block = from;
                while block != 0 {
                    self.stats.examined += 1;
                    if let Some(start) = place(block, self.size(block), need, align) {
                        // `replace` or `unlink` moves the rover on if the block is used up.
                        self.rover = block;
                        return Some((block, start));
                    }
                    block = self.next(block);
                    if block == 0 {
                        block = self.head; // past the end of the list: go round
                    }
                    if block == from {
                        break;
                    }
                }
                None
            }
            Policy::BestFit => {
                let (mut block, mut best) = (self.head, None);
                while block != 0 {
                    self.stats.examined += 1;
                    let size = self.size(block);
                    if best.is_none_or(|(_, _, smallest)| size < smallest)
                        && let Some(start) = place(block, size, need, align)
                    {
                        best = Some((block, start, size));
                        if size == need {
                            break; // none fits better
                        }
                    }
                    block = self.next(block);
                }
                best.map(|(block, start, _)| (block, start))
            }
        }
    }

    /// Put a free block on the list, where `order` says.
    fn insert(&mut self, block: usize) {
        let (mut prev, mut next) = (0, self.head);
        if self.order == Order::Address {
            while next != 0 && next < block {
                self.stats.passed += 1;
                (prev, next) = (next, self.next(next));
            }
        }
        self.write(block + WORD, next);
        self.write(block + 2 * WORD, prev);
        if next != 0 {
            self.write(next + 2 * WORD, block);
        }
        if prev != 0 {
            self.write(prev + WORD, block);
        } else {
            self.head = block;
        }
    }

    /// Take a free block off the list.
    fn unlink(&mut self, block: usize) {
        let (prev, next) = (self.prev(block), self.next(block));
        if prev != 0 {
            self.write(prev + WORD, next);
        } else {
            self.head = next;
        }
        if next != 0 {
            self.write(next + 2 * WORD, prev);
        }
        if self.rover == block {
            self.rover = next;
        }
    }

    /// Put the free block `new` where `old` is on the list. `new` is part of what `old`
    /// was, so the list's order doesn't change.
    fn replace(&mut self, old: usize, new: usize) {
        let (prev, next) = (self.prev(old), self.next(old));
        self.write(new + WORD, next);
        self.write(new + 2 * WORD, prev);
        if next != 0 {
            self.write(next + 2 * WORD, new);
        }
        if prev != 0 {
            self.write(prev + WORD, new);
        } else {
            self.head = new;
        }
        if self.rover == old {
            self.rover = new;
        }
    }

    /// Write a block's header and footer.
    fn set_block(&mut self, block: usize, size: usize, used: bool) {
        let tag = size | if used { USED } else { 0 };
        self.write(block, tag);
        self.write(block + size - WORD, tag);
    }

    /// The tag at `addr`: a size, and whether the block is allocated.
    fn tag(&self, addr: usize) -> (usize, bool) {
        let tag = self.read(addr);
        (tag & !USED, tag & USED != 0)
    }

    fn size(&self, block: usize) -> usize {
        self.tag(block).0
    }

    /// The block after a free block on the list, or 0.
    fn next(&self, block: usize) -> usize {
        self.read(block + WORD)
    }

    /// The block before a free block on the list, or 0.
    fn prev(&self, block: usize) -> usize {
        self.read(block + 2 * WORD)
    }

    fn read(&self, addr: usize) -> usize {
        // SAFETY: `new`'s caller gave us the region; every address read is a tag or a
        // link inside it.
        unsafe { (addr as *const usize).read() }
    }

    fn write(&mut self, addr: usize, value: usize) {
        // SAFETY: as for `read`.
        unsafe { (addr as *mut usize).write(value) }
    }
}

/// Where a block of `need` bytes, whose caller wants `align`, starts in the free block
/// of `size` bytes at `block`: as low as it can. `None` if it doesn't fit.
///
/// With an `align` of 16 or less that is the start of the free block, since every block
/// is aligned that far already. With more, the start may have to move up, and what it
/// leaves below must be a block itself, 32 bytes at least.
pub(crate) fn place(block: usize, size: usize, need: usize, align: usize) -> Option<usize> {
    let mut start = (block + WORD).next_multiple_of(align) - WORD;
    if start != block && start - block < MIN_BLOCK {
        start += align;
    }
    (start + need <= block + size).then_some(start)
}

pub struct Blocks<'a> {
    fit: &'a Fit,
    at: usize,
}

impl Iterator for Blocks<'_> {
    type Item = Block;

    fn next(&mut self) -> Option<Block> {
        if self.at >= self.fit.end {
            return None;
        }
        let (size, used) = self.fit.tag(self.at);
        let block = Block { start: self.at, size, used };
        // A tag that was written over could say 0, and the walk would never end.
        self.at = if size == 0 { self.fit.end } else { self.at + size };
        Some(block)
    }
}

pub struct FreeBlocks<'a> {
    fit: &'a Fit,
    at: usize,
}

impl Iterator for FreeBlocks<'_> {
    type Item = Block;

    fn next(&mut self) -> Option<Block> {
        if self.at == 0 {
            return None;
        }
        let block = Block { start: self.at, size: self.fit.size(self.at), used: false };
        self.at = self.fit.next(self.at);
        Some(block)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{ALIGN, Fit, FreeError, MIN_BLOCK, OVERHEAD, Order, Policy, WORD};
    use std::vec;
    use std::vec::Vec;

    const POLICIES: [Policy; 3] = [Policy::FirstFit, Policy::NextFit, Policy::BestFit];
    const ORDERS: [Order; 2] = [Order::Lifo, Order::Address];

    /// Run `f` on an allocator over `len` bytes of its own.
    fn with_fit<R>(len: usize, policy: Policy, order: Order, f: impl FnOnce(&mut Fit) -> R) -> R {
        let mut memory = vec![0u128; len / 16];
        let mut fit = unsafe { Fit::new(memory.as_mut_ptr() as usize, len, policy, order) };
        f(&mut fit)
    }

    /// Run `f` once for every policy and order.
    fn every_way(len: usize, f: impl Fn(&mut Fit)) {
        for policy in POLICIES {
            for order in ORDERS {
                with_fit(len, policy, order, &f);
            }
        }
    }

    fn free_sizes(fit: &Fit) -> Vec<usize> {
        fit.free_list().map(|b| b.size).collect()
    }

    #[test]
    fn a_new_allocator_is_one_free_block() {
        every_way(4096, |fit| {
            assert_eq!(fit.capacity(), 4096 - 2 * WORD);
            assert_eq!(free_sizes(fit), [fit.capacity()]);
            assert_eq!(fit.check(), Ok(()));
        });
    }

    #[test]
    fn blocks_are_aligned_and_disjoint() {
        every_way(4096, |fit| {
            let mut blocks: Vec<(usize, usize)> = Vec::new();
            for (size, align) in [(1, 1), (24, 8), (100, 16), (7, 64), (200, 256), (33, 32), (0, 1)] {
                let addr = fit.allocate(size, align).unwrap();
                assert!(addr.is_multiple_of(align.max(ALIGN)));
                unsafe { core::ptr::write_bytes(addr as *mut u8, 0xaa, size) };
                blocks.push((addr, size));
                assert_eq!(fit.check(), Ok(()));
            }
            blocks.sort();
            for pair in blocks.windows(2) {
                assert!(pair[0].0 + pair[0].1 <= pair[1].0);
            }
        });
    }

    #[test]
    fn a_block_costs_its_size_rounded_up_plus_two_tags() {
        with_fit(4096, Policy::FirstFit, Order::Address, |fit| {
            for (size, block) in [(0, 32), (1, 32), (16, 32), (17, 48), (32, 48), (100, 128)] {
                let before = fit.used();
                fit.allocate(size, 1).unwrap();
                assert_eq!(fit.used() - before, block, "a request for {size} bytes");
                assert!(block >= size + OVERHEAD && block >= MIN_BLOCK);
            }
        });
    }

    #[test]
    fn freeing_everything_leaves_one_block_whatever_the_order() {
        every_way(4096, |fit| {
            let all = fit.capacity();
            let blocks: Vec<usize> = (0..12).map(|i| fit.allocate(40 + 8 * i, 8).unwrap()).collect();
            for i in [5, 0, 11, 6, 4, 1, 2, 10, 8, 9, 7, 3] {
                assert_eq!(unsafe { fit.free(blocks[i]) }, Ok(()));
                assert_eq!(fit.check(), Ok(()));
            }
            assert_eq!(free_sizes(fit), [all]);
            assert_eq!(fit.used(), 0);
        });
    }

    #[test]
    fn a_freed_block_merges_with_free_neighbours() {
        with_fit(4096, Policy::FirstFit, Order::Address, |fit| {
            let [_guard, a, b, c, _rest] = [0; 5].map(|_| fit.allocate(48, 8).unwrap());
            unsafe {
                fit.free(a).unwrap();
                fit.free(c).unwrap();
                assert_eq!(fit.stats().merges, 0);
                assert_eq!(fit.free_list().filter(|b| b.size == 64).count(), 2);
                fit.free(b).unwrap();
            }
            assert_eq!(fit.stats().merges, 2);
            assert_eq!(fit.free_list().filter(|b| b.size == 192).count(), 1);
            assert_eq!(fit.check(), Ok(()));
        });
    }

    /// Free blocks of 64, 256 and 96 bytes, in address order, with allocated blocks
    /// between them, and the rest of the region free above them.
    fn three_holes(fit: &mut Fit) -> [usize; 3] {
        let blocks = [64 - 16, 16, 256 - 16, 16, 96 - 16, 16].map(|size| fit.allocate(size, 8).unwrap());
        for hole in [0, 2, 4] {
            unsafe { fit.free(blocks[hole]).unwrap() };
        }
        [blocks[0], blocks[2], blocks[4]]
    }

    #[test]
    fn first_fit_takes_the_first_block_that_is_large_enough() {
        with_fit(4096, Policy::FirstFit, Order::Address, |fit| {
            let [_small, large, _medium] = three_holes(fit);
            assert_eq!(fit.allocate(80 - 16, 8), Some(large));
        });
    }

    #[test]
    fn best_fit_takes_the_smallest_block_that_is_large_enough() {
        for order in ORDERS {
            with_fit(4096, Policy::BestFit, order, |fit| {
                let [_small, _large, medium] = three_holes(fit);
                assert_eq!(fit.allocate(80 - 16, 8), Some(medium));
                assert_eq!(fit.stats().splits, 6, "16 bytes can't be left: the block takes all 96");
            });
        }
    }

    #[test]
    fn next_fit_searches_from_where_it_stopped() {
        with_fit(4096, Policy::NextFit, Order::Address, |fit| {
            let [small, large, medium] = three_holes(fit);
            // The list is 64, 256, 96, and then the rest of the region, where the last
            // search stopped. Use the rest up, and the next search starts at the front.
            let rest = fit.largest_free();
            fit.allocate(rest - 16, 8).unwrap();
            assert_eq!(fit.allocate(200 - 16, 8), Some(large));
            // The search after that starts at what is left of the 256-byte hole, and
            // never looks at the 64-byte hole, which a first fit would take.
            assert_eq!(fit.allocate(48, 8), Some(medium));
            // Past the end of the list, a search goes round to its start.
            assert_eq!(fit.allocate(48, 8), Some(small));
        });
    }

    #[test]
    fn lifo_order_searches_the_last_block_freed_first() {
        with_fit(4096, Policy::FirstFit, Order::Lifo, |fit| {
            // Seven, so that block 5 has an allocated block above it to keep it apart
            // from the rest of the region.
            let blocks: Vec<usize> = (0..7).map(|_| fit.allocate(48, 8).unwrap()).collect();
            unsafe {
                fit.free(blocks[1]).unwrap();
                fit.free(blocks[5]).unwrap();
                fit.free(blocks[3]).unwrap();
            }
            assert_eq!([0; 3].map(|_| fit.allocate(48, 8).unwrap()), [blocks[3], blocks[5], blocks[1]]);
        });
    }

    #[test]
    fn address_order_searches_the_lowest_block_first() {
        with_fit(4096, Policy::FirstFit, Order::Address, |fit| {
            let blocks: Vec<usize> = (0..7).map(|_| fit.allocate(48, 8).unwrap()).collect();
            unsafe {
                fit.free(blocks[1]).unwrap();
                fit.free(blocks[5]).unwrap();
                fit.free(blocks[3]).unwrap();
            }
            assert_eq!([0; 3].map(|_| fit.allocate(48, 8).unwrap()), [blocks[1], blocks[3], blocks[5]]);
        });
    }

    #[test]
    fn the_footprint_is_the_highest_address_ever_reached() {
        every_way(4096, |fit| {
            assert_eq!(fit.footprint(), 0);
            let a = fit.allocate(100, 8).unwrap();
            let b = fit.allocate(100, 8).unwrap();
            assert_eq!(fit.footprint(), 2 * 128);
            unsafe { fit.free(a).unwrap() };
            unsafe { fit.free(b).unwrap() };
            assert_eq!(fit.footprint(), 2 * 128); // freeing gives nothing back
            // A block that fits below the mark doesn't move it, wherever the policy puts it.
            fit.allocate(40, 8).unwrap();
            assert_eq!(fit.footprint(), 2 * 128);
            fit.allocate(400, 8).unwrap();
            assert!(fit.footprint() >= 64 + 416);
        });
    }

    #[test]
    fn a_full_region_refuses_and_recovers() {
        every_way(1024, |fit| {
            assert_eq!(fit.allocate(2000, 8), None);
            assert_eq!(fit.allocate(usize::MAX - 4, 8), None);
            let mut blocks = Vec::new();
            while let Some(addr) = fit.allocate(100, 8) {
                blocks.push(addr);
            }
            assert_eq!(blocks.len(), (1024 - 2 * WORD) / 128);
            assert_eq!(fit.stats().failed, 3);
            for addr in blocks {
                unsafe { fit.free(addr).unwrap() };
            }
            assert!(fit.allocate(900, 8).is_some());
            assert_eq!(fit.check(), Ok(()));
        });
    }

    #[test]
    fn a_second_free_is_refused() {
        every_way(4096, |fit| {
            let [a, b, _c] = [0; 3].map(|_| fit.allocate(48, 8).unwrap());
            unsafe {
                assert_eq!(fit.free(b), Ok(()));
                assert_eq!(fit.free(b), Err(FreeError::NotAllocated));
                // Even after the block has been merged into a neighbour.
                assert_eq!(fit.free(a), Ok(()));
                assert_eq!(fit.free(b), Err(FreeError::NotAllocated));
                assert_eq!(fit.free(a), Err(FreeError::NotAllocated));
            }
            assert_eq!(fit.check(), Ok(()));
        });
    }

    /// The hardest second free to catch: of a block that was merged with the free blocks
    /// on both sides of it. Neither of its tags is a tag of the merged block, so nothing
    /// has rewritten them since; `free` marks them free itself before it merges.
    #[test]
    fn a_second_free_is_refused_after_merging_on_both_sides() {
        every_way(4096, |fit| {
            let [a, b, c, _d] = [0; 4].map(|_| fit.allocate(48, 8).unwrap());
            unsafe {
                assert_eq!(fit.free(a), Ok(()));
                assert_eq!(fit.free(c), Ok(()));
                assert_eq!(fit.free(b), Ok(()));
                assert_eq!(fit.free(b), Err(FreeError::NotAllocated));
            }
            assert_eq!(fit.check(), Ok(()));
        });
    }

    #[test]
    fn addresses_that_were_never_handed_out_are_refused() {
        with_fit(4096, Policy::FirstFit, Order::Address, |fit| {
            let a = fit.allocate(48, 8).unwrap();
            unsafe {
                assert_eq!(fit.free(a + 8), Err(FreeError::NotABlock));
                assert_eq!(fit.free(fit.first() - 4096), Err(FreeError::NotABlock));
                assert_eq!(fit.free(fit.first() + fit.capacity() + WORD), Err(FreeError::NotABlock));
                assert_eq!(fit.free(0), Err(FreeError::NotABlock));
            }
        });
    }

    /// A caller that writes one word past the end of its block writes over the block's
    /// footer. The next free of that block finds a header and a footer that disagree.
    #[test]
    fn a_write_past_the_end_of_a_block_is_caught_when_it_is_freed() {
        with_fit(4096, Policy::FirstFit, Order::Address, |fit| {
            let a = fit.allocate(48, 8).unwrap();
            unsafe {
                ((a + 48) as *mut usize).write(0x4141_4141_4141_4141);
                assert_eq!(fit.free(a), Err(FreeError::Corrupt));
            }
            assert_eq!(fit.check(), Err("a block's header and footer disagree"));
        });
    }

    /// A few thousand allocations and frees of random sizes, with the allocator checked
    /// after every one, and every block filled with a byte of its own to show that no two
    /// overlap.
    #[test]
    fn random_allocations_and_frees_keep_the_heap_consistent() {
        every_way(64 * 1024, |fit| {
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            let mut random = move |below: usize| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (seed >> 33) as usize % below
            };
            let mut live: Vec<(usize, usize, u8)> = Vec::new();
            for step in 0..4000 {
                if live.len() < 40 || (live.len() < 120 && random(2) == 0) {
                    let (size, align) = (1 + random(400), 1 << random(8));
                    if let Some(addr) = fit.allocate(size, align) {
                        assert!(addr.is_multiple_of(align));
                        let fill = step as u8;
                        unsafe { core::ptr::write_bytes(addr as *mut u8, fill, size) };
                        live.push((addr, size, fill));
                    }
                } else {
                    let (addr, size, fill) = live.swap_remove(random(live.len()));
                    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                    assert!(bytes.iter().all(|&b| b == fill), "a block was written over");
                    assert_eq!(unsafe { fit.free(addr) }, Ok(()));
                }
                assert_eq!(fit.check(), Ok(()), "after step {step}");
            }
            for (addr, _, _) in live {
                assert_eq!(unsafe { fit.free(addr) }, Ok(()));
            }
            assert_eq!(fit.check(), Ok(()));
            assert_eq!(free_sizes(fit), [fit.capacity()]);
        });
    }
}
