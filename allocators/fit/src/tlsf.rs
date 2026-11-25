//! TLSF, *two-level segregated fit*: the blocks of [`Fit`](crate::Fit), with the one free
//! list replaced by a table of lists and two bitmaps that say which lists have a block.
//!
//! A free block goes on the list for its size. Sizes below 256 bytes have a list each
//! (every size is a multiple of 16). From 256 up, each power of two is a row of the table,
//! and the row is cut into 16 equal ranges: 256 to 271, 272 to 287, and so on up to 511;
//! then 512 to 543, 544 to 575, up to 1023. The first level is the power of two and the
//! second is the range within it.
//!
//! An allocation rounds the size it needs up to the start of a range, so that every block
//! on that range's list is large enough, and asks the bitmaps for the first list at or
//! after it that isn't empty: two masks and two counts of trailing zeros. It takes that
//! list's first block. Nothing is searched, so the work an allocation or a free does has
//! a bound that doesn't depend on how many blocks are free.

use crate::{ALIGN, Block, FreeError, Heap, MIN_BLOCK, OVERHEAD, Stats, USED, WORD, place};

/// Each power of two from 256 up is cut into this many ranges.
pub const RANGES: usize = 16;
/// Sizes below this have a list each.
pub const SMALL: usize = 256;
/// Rows of the table: one for the small sizes, and one for each power of two from 256
/// bytes to 2 GiB. The largest block it can list is a little under 4 GiB.
pub const ROWS: usize = 25;

const RANGE_BITS: u32 = RANGES.trailing_zeros();
const SMALL_BITS: u32 = SMALL.trailing_zeros();

/// The row and the column of the list that holds free blocks of `size` bytes.
pub fn class(size: usize) -> (usize, usize) {
    if size < SMALL {
        (0, size / ALIGN)
    } else {
        let power = size.ilog2();
        ((power - SMALL_BITS + 1) as usize, (size >> (power - RANGE_BITS)) - RANGES)
    }
}

/// The first list on which every block has room for `need` bytes: `need` rounded up to
/// the start of a range.
pub fn class_for(need: usize) -> (usize, usize) {
    if need < SMALL { class(need) } else { class(need + (1 << (need.ilog2() - RANGE_BITS)) - 1) }
}

/// The sizes on the list at `row` and `column`: the smallest, and how many bytes the
/// range spans.
pub fn range(row: usize, column: usize) -> (usize, usize) {
    if row == 0 {
        (column * ALIGN, ALIGN)
    } else {
        let power = row as u32 + SMALL_BITS - 1;
        ((1 << power) + (column << (power - RANGE_BITS)), 1 << (power - RANGE_BITS))
    }
}

/// A two-level segregated fit (TLSF) allocator, using size-class free lists and
/// bitmaps to find suitable blocks in bounded time.
pub struct Tlsf {
    /// The header of the first block, and one past the last: see `Fit`.
    first: usize,
    end: usize,
    /// Bit `r` is set if row `r` has a list with a block on it.
    rows: u32,
    /// For each row, bit `c` is set if the list in column `c` has a block on it.
    columns: [u16; ROWS],
    /// The first block of each list, or 0.
    heads: [[usize; RANGES]; ROWS],
    used: usize,
    high: usize,
    stats: Stats,
}

impl Tlsf {
    /// An allocator over the `len` bytes at `region`, all of it one free block.
    ///
    /// # Safety
    /// The region must be writable, and not in use by anything else for as long as the
    /// allocator is.
    ///
    /// # Panics
    /// If the region is too small to hold a block, or 4 GiB or more.
    pub unsafe fn new(region: usize, len: usize) -> Tlsf {
        let first = region.next_multiple_of(ALIGN) + WORD;
        let end = ((region + len) & !(ALIGN - 1)) - WORD;
        assert!(end >= first + MIN_BLOCK, "a region too small for one block");
        assert!(class(end - first).0 < ROWS, "a region too large for the table");
        let mut tlsf = Tlsf {
            first,
            end,
            rows: 0,
            columns: [0; ROWS],
            heads: [[0; RANGES]; ROWS],
            used: 0,
            high: first,
            stats: Stats::default(),
        };
        tlsf.write(first - WORD, USED);
        tlsf.write(end, USED);
        tlsf.set_block(first, end - first, false);
        tlsf.insert(first);
        tlsf
    }

    /// A block of at least `size` bytes at a multiple of `align`, which must be a power
    /// of two. `None` if no list that is sure to have room has a block.
    #[inline(never)]
    pub fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        debug_assert!(align.is_power_of_two());
        let need = size.checked_add(OVERHEAD + ALIGN - 1).map(|n| (n & !(ALIGN - 1)).max(MIN_BLOCK));
        // An address that is a multiple of more than 16 may be some way into the block:
        // ask for enough that it fits wherever the block starts.
        let room = need.and_then(|need| if align > ALIGN { need.checked_add(align + MIN_BLOCK) } else { Some(need) });
        let found = room.filter(|room| *room <= self.end - self.first).and_then(|room| self.find(room));
        let (Some(need), Some(block)) = (need, found) else {
            self.stats.failed += 1;
            return None;
        };
        self.stats.examined += 1;
        self.stats.longest_search = 1;
        let block_end = block + self.size(block);
        let start = place(block, block_end - block, need, align).expect("a block from a list too small for its class");
        self.remove(block);
        let mut end = start + need;
        if block_end - end < MIN_BLOCK {
            end = block_end;
        }
        if start > block {
            self.set_block(block, start - block, false);
            self.insert(block);
        }
        if end < block_end {
            self.set_block(end, block_end - end, false);
            self.insert(end);
            self.stats.splits += 1;
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
    /// As for [`Fit::free`](crate::Fit::free).
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
        self.set_block(block, size, false); // so that a second free finds a free tag

        let (before, before_used) = self.tag(block - WORD);
        if !before_used {
            block -= before;
            size += before;
            self.remove(block);
            self.stats.merges += 1;
        }
        let (after, after_used) = self.tag(block + size);
        if !after_used {
            self.remove(block + size);
            size += after;
            self.stats.merges += 1;
        }
        self.set_block(block, size, false);
        self.insert(block);
        Ok(())
    }

    /// The first block of the first list, at or after the one for `room` bytes, that has
    /// a block. Two masks and two counts of trailing zeros.
    fn find(&self, room: usize) -> Option<usize> {
        let (mut row, column) = class_for(room);
        if row >= ROWS {
            return None;
        }
        let mut columns = self.columns[row] & (!0 << column);
        if columns == 0 {
            let rows = self.rows & (!0 << (row + 1));
            if rows == 0 {
                return None;
            }
            row = rows.trailing_zeros() as usize;
            columns = self.columns[row];
        }
        Some(self.heads[row][columns.trailing_zeros() as usize])
    }

    /// Put a free block at the head of the list for its size.
    fn insert(&mut self, block: usize) {
        let (row, column) = class(self.size(block));
        let next = self.heads[row][column];
        self.write(block + WORD, next);
        self.write(block + 2 * WORD, 0);
        if next != 0 {
            self.write(next + 2 * WORD, block);
        }
        self.heads[row][column] = block;
        self.columns[row] |= 1 << column;
        self.rows |= 1 << row;
    }

    /// Take a free block off its list.
    fn remove(&mut self, block: usize) {
        let (row, column) = class(self.size(block));
        let (prev, next) = (self.read(block + 2 * WORD), self.read(block + WORD));
        if next != 0 {
            self.write(next + 2 * WORD, prev);
        }
        if prev != 0 {
            self.write(prev + WORD, next);
            return;
        }
        self.heads[row][column] = next;
        if next == 0 {
            self.columns[row] &= !(1 << column);
            if self.columns[row] == 0 {
                self.rows &= !(1 << row);
            }
        }
    }

    /// Every block in the region, in address order.
    pub fn blocks(&self) -> Blocks<'_> {
        Blocks { tlsf: self, at: self.first }
    }

    /// The free blocks, in address order.
    pub fn free_list(&self) -> impl Iterator<Item = Block> + '_ {
        self.blocks().filter(|block| !block.used)
    }

    /// The lists that have blocks: row, column, and how many blocks.
    pub fn lists(&self) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        (0..ROWS).flat_map(move |row| (0..RANGES).map(move |column| (row, column))).filter_map(|(row, column)| {
            let (mut at, mut count) = (self.heads[row][column], 0);
            while at != 0 {
                count += 1;
                at = self.read(at + WORD);
            }
            (count != 0).then_some((row, column, count))
        })
    }

    /// The region's size: every block, tags included.
    pub fn capacity(&self) -> usize {
        self.end - self.first
    }

    /// Bytes in allocated blocks, tags included.
    pub fn used(&self) -> usize {
        self.used
    }

    /// See [`Fit::footprint`](crate::Fit::footprint).
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

    /// Check everything the allocator relies on: the region, then every list, then the
    /// bitmaps.
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
        let mut listed = 0;
        for row in 0..ROWS {
            for column in 0..RANGES {
                let (mut at, mut prev) = (self.heads[row][column], 0);
                if (at != 0) != (self.columns[row] & (1 << column) != 0) {
                    return Err("a list and its bit disagree about whether it is empty");
                }
                while at != 0 {
                    if at < self.first || at >= self.end || !(at - WORD).is_multiple_of(ALIGN) {
                        return Err("a free list leaves the region");
                    }
                    let (size, is_used) = self.tag(at);
                    if is_used {
                        return Err("an allocated block is on a free list");
                    }
                    if class(size) != (row, column) {
                        return Err("a free block is on the list for another size");
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
            if (self.columns[row] != 0) != (self.rows & (1 << row) != 0) {
                return Err("a row and its bit disagree about whether it is empty");
            }
        }
        if listed != free_blocks {
            return Err("a free block isn't on a free list");
        }
        Ok(())
    }

    fn set_block(&mut self, block: usize, size: usize, used: bool) {
        let tag = size | if used { USED } else { 0 };
        self.write(block, tag);
        self.write(block + size - WORD, tag);
    }

    fn tag(&self, addr: usize) -> (usize, bool) {
        let tag = self.read(addr);
        (tag & !USED, tag & USED != 0)
    }

    fn size(&self, block: usize) -> usize {
        self.tag(block).0
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

impl Heap for Tlsf {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        Tlsf::allocate(self, size, align)
    }
    unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError> {
        unsafe { Tlsf::free(self, addr) }
    }
    fn used(&self) -> usize {
        Tlsf::used(self)
    }
    fn footprint(&self) -> usize {
        Tlsf::footprint(self)
    }
    fn stats(&self) -> Stats {
        Tlsf::stats(self)
    }
    fn check(&self) -> Result<(), &'static str> {
        Tlsf::check(self)
    }
}

pub struct Blocks<'a> {
    tlsf: &'a Tlsf,
    at: usize,
}

impl Iterator for Blocks<'_> {
    type Item = Block;

    fn next(&mut self) -> Option<Block> {
        if self.at >= self.tlsf.end {
            return None;
        }
        let (size, used) = self.tlsf.tag(self.at);
        let block = Block { start: self.at, size, used };
        self.at = if size == 0 { self.tlsf.end } else { self.at + size };
        Some(block)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{RANGES, ROWS, SMALL, Tlsf, class, class_for};
    use crate::trace::Random;
    use crate::{FreeError, MIN_BLOCK};
    use std::vec;
    use std::vec::Vec;

    fn with_tlsf<R>(len: usize, f: impl FnOnce(&mut Tlsf) -> R) -> R {
        let mut memory = vec![0u128; len / 16];
        let mut tlsf = unsafe { Tlsf::new(memory.as_mut_ptr() as usize, len) };
        f(&mut tlsf)
    }

    #[test]
    fn small_sizes_have_a_list_each_and_larger_ones_share_sixteen_to_a_power_of_two() {
        assert_eq!((super::range(0, 2), super::range(1, 1), super::range(2, 15)), ((32, 16), (272, 16), (992, 32)));
        assert_eq!(class(32), (0, 2));
        assert_eq!(class(240), (0, 15));
        assert_eq!(class(256), (1, 0));
        assert_eq!(class(271), (1, 0));
        assert_eq!(class(272), (1, 1));
        assert_eq!(class(511), (1, 15));
        assert_eq!(class(512), (2, 0));
        assert_eq!(class(544), (2, 1));
        assert_eq!(class(1 << 20), (13, 0));
        assert_eq!(class((1 << 32) - 16), (ROWS - 1, RANGES - 1));
        // Classes never go backwards as sizes grow.
        let classes: Vec<_> = (MIN_BLOCK..1 << 16).step_by(16).map(class).collect();
        assert!(classes.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn a_search_starts_at_the_first_list_whose_every_block_fits() {
        assert_eq!(class_for(48), (0, 3)); // small sizes are exact
        assert_eq!(class_for(256), (1, 0));
        assert_eq!(class_for(272), (1, 1));
        assert_eq!(class_for(528), (2, 1)); // 528 is in 512..544, which may hold a 512: start at 544
        assert_eq!(class_for(1008), (3, 0)); // rounds up into the next power of two
        for need in (MIN_BLOCK..1 << 16).step_by(16) {
            let (row, column) = class_for(need);
            // The smallest size on that list is at least `need`.
            let smallest = (MIN_BLOCK..).step_by(16).find(|size| class(*size) == (row, column)).unwrap();
            assert!(smallest >= need, "{need}: list ({row}, {column}) starts at {smallest}");
            assert!(need < SMALL || smallest - need < need / 16 + 16);
        }
    }

    #[test]
    fn a_new_allocator_is_one_free_block_on_one_list() {
        with_tlsf(4096, |tlsf| {
            assert_eq!(tlsf.capacity(), 4080);
            assert_eq!(tlsf.lists().collect::<Vec<_>>(), [(4, 15, 1)]); // 4080 is in 3968..4096
            tlsf.check().unwrap();
        });
    }

    #[test]
    fn blocks_are_split_merged_and_listed_by_size() {
        with_tlsf(4096, |tlsf| {
            let a = tlsf.allocate(100, 8).unwrap();
            let b = tlsf.allocate(300, 8).unwrap();
            let c = tlsf.allocate(100, 8).unwrap();
            assert_eq!((b - a, c - b), (128, 320));
            unsafe { tlsf.free(a).unwrap() };
            unsafe { tlsf.free(c).unwrap() }; // merges with the rest of the region
            assert_eq!(tlsf.lists().collect::<Vec<_>>(), [(0, 8, 1), (4, 12, 1)]);
            // 100 bytes again: the block of 128 is an exact fit, and the first list looked at.
            assert_eq!(tlsf.allocate(100, 8), Some(a));
            unsafe { tlsf.free(a).unwrap() };
            unsafe { tlsf.free(b).unwrap() };
            assert_eq!(tlsf.lists().collect::<Vec<_>>(), [(4, 15, 1)]);
            assert_eq!((tlsf.used(), tlsf.footprint()), (0, 128 + 320 + 128));
            tlsf.check().unwrap();
        });
    }

    #[test]
    fn a_block_that_would_fit_is_passed_over_if_its_list_might_hold_smaller_ones() {
        with_tlsf(8192, |tlsf| {
            // Leave a free block of 528 between two allocated ones.
            let a = tlsf.allocate(512, 8).unwrap();
            let _b = tlsf.allocate(16, 8).unwrap();
            unsafe { tlsf.free(a).unwrap() };
            assert_eq!(tlsf.lists().next(), Some((2, 0, 1)));
            // A request that needs 528 doesn't take it: the list for 512..544 could hold a
            // block of 512. It is cut from the large block instead.
            assert_ne!(tlsf.allocate(512, 8), Some(a));
            // A request that needs 512 or less does.
            assert_eq!(tlsf.allocate(400, 8), Some(a));
            tlsf.check().unwrap();
        });
    }

    #[test]
    fn large_alignments_are_honoured() {
        with_tlsf(1 << 16, |tlsf| {
            for align in [32, 64, 4096] {
                let addr = tlsf.allocate(100, align).unwrap();
                assert!(addr.is_multiple_of(align));
                tlsf.check().unwrap();
                unsafe { tlsf.free(addr).unwrap() };
            }
            assert_eq!(tlsf.lists().count(), 1);
        });
    }

    #[test]
    fn mistakes_are_refused_as_fit_refuses_them() {
        with_tlsf(4096, |tlsf| {
            let a = tlsf.allocate(40, 8).unwrap();
            let _b = tlsf.allocate(40, 8).unwrap();
            assert_eq!(unsafe { tlsf.free(a + 8) }, Err(FreeError::NotABlock));
            assert_eq!(unsafe { tlsf.free(a) }, Ok(()));
            assert_eq!(unsafe { tlsf.free(a) }, Err(FreeError::NotAllocated));
            assert_eq!(tlsf.allocate(1 << 20, 8), None);
            assert_eq!(tlsf.stats().failed, 1);
        });
    }

    #[test]
    fn random_allocations_and_frees_keep_the_heap_consistent() {
        with_tlsf(1 << 18, |tlsf| {
            let mut random = Random(42);
            let mut live: Vec<(usize, usize, u8)> = Vec::new();
            for step in 0..4000 {
                if live.len() < 200 && random.below(3) != 0 {
                    let size = if random.below(8) == 0 { 128 + random.below(4000) } else { 1 + random.below(200) };
                    let align = 1 << random.below(8);
                    let addr = tlsf.allocate(size, align).unwrap();
                    assert!(addr.is_multiple_of(align));
                    unsafe { core::ptr::write_bytes(addr as *mut u8, step as u8, size) };
                    live.push((addr, size, step as u8));
                } else if !live.is_empty() {
                    let (addr, size, byte) = live.swap_remove(random.below(live.len()));
                    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                    assert!(bytes.iter().all(|b| *b == byte), "a block was written over");
                    unsafe { tlsf.free(addr).unwrap() };
                }
                tlsf.check().unwrap();
            }
            for (addr, _, _) in live {
                unsafe { tlsf.free(addr).unwrap() };
            }
            tlsf.check().unwrap();
            assert_eq!(tlsf.lists().count(), 1);
            assert_eq!(tlsf.stats().longest_search, 1);
        });
    }
}
