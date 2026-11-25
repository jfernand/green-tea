//! [`Fit`](crate::Fit)'s first fit over a LIFO list, hardened: the same region, the same
//! boundary tags and the same doubly linked free list, with five defences that can each
//! be switched on or off.
//!
//! The allocator is `Hardened<D>`, and `D` is a set of bits, one for each defence:
//!
//! - [`UNLINK`]: *safe unlinking*. Before a block is taken off the free list, the blocks
//!   on either side of it on the list must point back at it, the block must be inside the
//!   region and marked free, and its two tags must agree.
//! - [`ENCODE`]: the links are stored *encoded*, each XORed with a secret and with the
//!   address it is stored at. A link written by anything that doesn't know the secret
//!   decodes to an address that is almost certainly not a block's.
//! - [`CANARY`]: a secret word between the caller's bytes and the footer, checked when
//!   the block is freed. A write past the end of the block has to cross it.
//! - [`QUARANTINE`]: a freed block isn't put on the free list. It is filled with a known
//!   byte and held until enough later frees have pushed it out, and the fill is checked
//!   as it leaves: a write to the block while it was held shows.
//! - [`RANDOM`]: an allocation takes any free block that is large enough, chosen at
//!   random, and a freed block goes to a slot of the quarantine chosen at random.
//!
//! With `D` 0 it does what `Fit` does with `Policy::FirstFit` and `Order::Lifo`, block
//! for block. The bits are a const parameter so that each combination is compiled on its
//! own: `Hardened<0>::free` has no trace of the quarantine in it, and what a defence costs
//! in instructions can be read off the disassembly.
//!
//! What it detects is a [`Fault`], returned by the call that noticed. After a fault the
//! allocator makes no promise: the region is not what it thought, and a kernel should stop.
//!
//! The random numbers come from a generator seeded by the caller, so a run can be
//! repeated. The secrets come from the same seed, which means they are only secret if the
//! seed is.

use crate::trace::Random;
use crate::{ALIGN, Block, FreeError, Heap, MIN_BLOCK, OVERHEAD, Stats, USED, WORD, place};

/// Check a block's neighbours on the free list, and its tags, before unlinking it.
pub const UNLINK: u8 = 1;
/// Store the free list's links XORed with a secret.
pub const ENCODE: u8 = 2;
/// Put a secret word after the caller's bytes, and check it on free.
pub const CANARY: u8 = 4;
/// Hold freed blocks, filled with [`POISON`], before they can be reused.
pub const QUARANTINE: u8 = 8;
/// Choose at random among the free blocks that fit, and among the quarantine's slots.
pub const RANDOM: u8 = 16;
/// All five.
pub const ALL: u8 = UNLINK | ENCODE | CANARY | QUARANTINE | RANDOM;

/// The bit of a tag that says its block was freed and is in quarantine. Such a block is
/// also marked [`USED`], so that its neighbours don't merge with it.
const HELD: usize = 2;
/// The low bits of a tag that aren't its size.
const FLAGS: usize = ALIGN - 1;
/// What a quarantined block is filled with, a word at a time: the byte 0xdd.
pub const POISON: usize = usize::MAX / 0xff * 0xdd;
/// The most blocks the quarantine can hold.
pub const QUARANTINE_MAX: usize = 64;
/// How many it holds unless told otherwise.
pub const QUARANTINE_DEFAULT: usize = 16;

/// What the allocator found wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// `free` was given an address this allocator couldn't have handed out.
    NotABlock,
    /// `free` was given a block that is free already, or in quarantine.
    NotAllocated,
    /// A block's header and footer disagree, or its size makes no sense.
    Tags,
    /// The word after the caller's bytes isn't what `allocate` put there.
    Canary,
    /// A link of the free list doesn't lead to a free block inside the region.
    Link,
    /// A block's neighbours on the free list don't point back at it.
    Unlink,
    /// A block was written to while it was in quarantine.
    WrittenAfterFree,
}

/// What the defences did, beside the allocator's [`Stats`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Words filled with [`POISON`] as blocks entered the quarantine.
    pub poisoned: usize,
    /// Words read back as blocks left it.
    pub verified: usize,
    /// Blocks that left the quarantine and went to the free list.
    pub released: usize,
    /// The most bytes the quarantine ever held.
    pub peak_held: usize,
}

pub struct Hardened<const D: u8> {
    /// The header of the first block, and one past the last block, as in `Fit`.
    first: usize,
    end: usize,
    /// The first block on the free list, or 0. It is here and not in the region, so it
    /// is never encoded: nothing that overruns a block can reach it.
    head: usize,
    /// Bytes in blocks the caller holds, tags included. Quarantined blocks don't count.
    used: usize,
    /// Bytes in quarantined blocks.
    held: usize,
    high: usize,
    link_key: usize,
    canary_key: usize,
    random: Random,
    /// The quarantine: the header of each block held, or 0.
    quarantine: [usize; QUARANTINE_MAX],
    /// How much of `quarantine` is in use, and where the next block goes without `RANDOM`.
    slots: usize,
    cursor: usize,
    stats: Stats,
    counts: Counts,
}

impl<const D: u8> Hardened<D> {
    const fn has(defences: u8) -> bool {
        D & defences != 0
    }

    /// An allocator over the `len` bytes at `region`, all of it one free block. `seed`
    /// decides the secrets and every random choice.
    ///
    /// # Safety
    /// The region must be writable, and not in use by anything else for as long as the
    /// allocator is.
    ///
    /// # Panics
    /// If the region is too small to hold a block.
    pub unsafe fn new(region: usize, len: usize, seed: u64) -> Self {
        let first = region.next_multiple_of(ALIGN) + WORD;
        let end = ((region + len) & !(ALIGN - 1)) - WORD;
        assert!(end >= first + MIN_BLOCK, "a region too small for one block");
        let mut random = Random(seed);
        let (link_key, canary_key) = (word(&mut random), word(&mut random));
        let mut heap = Hardened {
            first,
            end,
            head: first,
            used: 0,
            held: 0,
            high: first,
            link_key,
            canary_key,
            random,
            quarantine: [0; QUARANTINE_MAX],
            slots: QUARANTINE_DEFAULT,
            cursor: 0,
            stats: Stats::default(),
            counts: Counts::default(),
        };
        heap.write(first - WORD, USED);
        heap.write(end, USED);
        heap.set_block(first, end - first, 0);
        heap.set_link(first + WORD, 0);
        heap.set_link(first + 2 * WORD, 0);
        heap
    }

    /// Hold up to `slots` freed blocks in quarantine, at most [`QUARANTINE_MAX`]. Only
    /// before the first free.
    pub fn set_quarantine(&mut self, slots: usize) {
        assert!(slots <= QUARANTINE_MAX && self.stats.frees == 0);
        self.slots = slots;
    }

    /// A block of at least `size` bytes at a multiple of `align`, which must be a power
    /// of two. `Ok(None)` if no free block has room for it.
    #[inline(never)]
    pub fn allocate(&mut self, size: usize, align: usize) -> Result<Option<usize>, Fault> {
        debug_assert!(align.is_power_of_two());
        let extra = OVERHEAD + if Self::has(CANARY) { WORD } else { 0 };
        let need = size.checked_add(extra + ALIGN - 1).map(|n| (n & !(ALIGN - 1)).max(MIN_BLOCK));
        let examined = self.stats.examined;
        let found = match need {
            Some(need) => self.find(need, align)?.map(|found| (need, found)),
            None => None,
        };
        self.stats.longest_search = self.stats.longest_search.max(self.stats.examined - examined);
        let Some((need, (block, start))) = found else {
            self.stats.failed += 1;
            return Ok(None);
        };
        let size = self.read(block) & !FLAGS;
        if Self::has(UNLINK) && (size < MIN_BLOCK || size > self.end - block || self.read(block + size - WORD) != size)
        {
            return Err(Fault::Tags);
        }
        // From here on it is `Fit::allocate`: split if what is left can be a block.
        let block_end = block + size;
        let mut end = start + need;
        if block_end - end < MIN_BLOCK {
            end = block_end;
        }
        if end < block_end {
            self.set_block(end, block_end - end, 0);
            self.stats.splits += 1;
        }
        if start > block {
            self.set_block(block, start - block, 0);
            if end < block_end {
                self.insert(end)?;
            }
        } else if end < block_end {
            self.replace(block, end)?;
        } else {
            self.unlink(block)?;
        }
        self.set_block(start, end - start, USED);
        if Self::has(CANARY) {
            self.write(end - 2 * WORD, self.canary(start));
        }
        self.used += end - start;
        self.high = self.high.max(end);
        self.stats.allocations += 1;
        self.stats.peak = self.stats.peak.max(self.used);
        Ok(Some(start + WORD))
    }

    /// Give back the block at `addr`.
    ///
    /// With `QUARANTINE` the block is held, and the block it pushes out of the quarantine
    /// is the one that goes to the free list. A fault found in that older block is
    /// returned by this call, and the older block is then never reused.
    ///
    /// # Safety
    /// `addr` must have come from `allocate` on this allocator, and not have been freed
    /// since. The allocator refuses what it can tell is wrong.
    #[inline(never)]
    pub unsafe fn free(&mut self, addr: usize) -> Result<(), Fault> {
        let block = addr.wrapping_sub(WORD);
        if !addr.is_multiple_of(ALIGN) || block < self.first || block >= self.end {
            return Err(Fault::NotABlock);
        }
        let tag = self.read(block);
        let size = tag & !FLAGS;
        if tag & USED == 0 || tag & HELD != 0 {
            return Err(Fault::NotAllocated);
        }
        if size < MIN_BLOCK || size > self.end - block || self.read(block + size - WORD) != tag {
            return Err(Fault::Tags);
        }
        if Self::has(CANARY) && self.read(block + size - 2 * WORD) != self.canary(block) {
            return Err(Fault::Canary);
        }
        self.used -= size;
        self.stats.frees += 1;
        if !Self::has(QUARANTINE) || self.slots == 0 {
            return self.merge(block, size);
        }
        self.set_block(block, size, USED | HELD);
        for word in (block + WORD..block + size - WORD).step_by(WORD) {
            self.write(word, POISON);
        }
        self.counts.poisoned += size / WORD - 2;
        self.held += size;
        self.counts.peak_held = self.counts.peak_held.max(self.held);
        let slot = if Self::has(RANDOM) {
            self.random.below(self.slots)
        } else {
            let slot = self.cursor;
            self.cursor = (slot + 1) % self.slots;
            slot
        };
        let pushed_out = core::mem::replace(&mut self.quarantine[slot], block);
        if pushed_out == 0 { Ok(()) } else { self.release(pushed_out) }
    }

    /// Empty the quarantine onto the free list. Returns the first fault found, after
    /// trying every block.
    pub fn drain(&mut self) -> Result<(), Fault> {
        let mut result = Ok(());
        for slot in 0..QUARANTINE_MAX {
            let block = core::mem::replace(&mut self.quarantine[slot], 0);
            if block != 0 {
                result = result.and(self.release(block));
            }
        }
        result
    }

    /// Every block in the region, in address order, and whether it is in quarantine.
    pub fn blocks(&self) -> impl Iterator<Item = (Block, bool)> + '_ {
        let mut at = self.first;
        core::iter::from_fn(move || {
            if at >= self.end {
                return None;
            }
            let tag = self.read(at);
            let size = tag & !FLAGS;
            let block = Block { start: at, size, used: tag & USED != 0 };
            at = if size == 0 { self.end } else { at + size };
            Some((block, tag & HELD != 0))
        })
    }

    /// The headers of the free blocks, in the order of the free list, for as far as the
    /// list can be followed.
    pub fn free_list(&self) -> impl Iterator<Item = usize> + '_ {
        let (mut at, mut left) = (self.head, self.capacity() / MIN_BLOCK);
        core::iter::from_fn(move || {
            if at == 0 || left == 0 {
                return None;
            }
            let block = at;
            (at, left) = (self.link(at + WORD).unwrap_or(0), left - 1);
            Some(block)
        })
    }

    /// The region's size: every block, tags included.
    pub fn capacity(&self) -> usize {
        self.end - self.first
    }

    /// Bytes in blocks the caller holds, tags included.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Bytes in quarantined blocks.
    pub fn held(&self) -> usize {
        self.held
    }

    /// As [`Fit::footprint`](crate::Fit::footprint).
    pub fn footprint(&self) -> usize {
        self.high - self.first
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The header of the first block.
    pub fn first(&self) -> usize {
        self.first
    }

    /// The block `allocate` makes for a request of `size` bytes, when it has to split.
    pub const fn block_size(size: usize) -> usize {
        let extra = OVERHEAD + if Self::has(CANARY) { WORD } else { 0 };
        let need = (size + extra + ALIGN - 1) & !(ALIGN - 1);
        if need < MIN_BLOCK { MIN_BLOCK } else { need }
    }

    /// Check everything the allocator relies on, and everything its defences would find
    /// if every block were freed now: walk the region, then the list.
    pub fn check(&self) -> Result<(), &'static str> {
        let (mut at, mut used, mut free_blocks, mut last_free) = (self.first, 0, 0usize, false);
        while at < self.end {
            let tag = self.read(at);
            let (size, is_used, is_held) = (tag & !FLAGS, tag & USED != 0, tag & HELD != 0);
            if size < MIN_BLOCK || size > self.end - at || (is_held && !is_used) {
                return Err("a block's size makes no sense");
            }
            if self.read(at + size - WORD) != tag {
                return Err("a block's header and footer disagree");
            }
            if !is_used && last_free {
                return Err("two free blocks are neighbours");
            }
            if is_held && (at + WORD..at + size - WORD).step_by(WORD).any(|word| self.read(word) != POISON) {
                return Err("a quarantined block was written to");
            }
            if is_used && !is_held {
                if Self::has(CANARY) && self.read(at + size - 2 * WORD) != self.canary(at) {
                    return Err("a block's canary was changed");
                }
                used += size;
            }
            free_blocks += usize::from(!is_used);
            last_free = !is_used;
            at += size;
        }
        if used != self.used {
            return Err("the allocated blocks don't add up to `used`");
        }
        let (mut at, mut prev, mut listed) = (self.head, 0, 0usize);
        while at != 0 {
            if !self.plausible(at) {
                return Err("the free list leaves the region");
            }
            if self.read(at) & FLAGS != 0 {
                return Err("an allocated block is on the free list");
            }
            if self.link(at + 2 * WORD) != Ok(prev) {
                return Err("a block's back link isn't the block before it on the list");
            }
            listed += 1;
            if listed > free_blocks {
                return Err("the free list has more blocks than are free");
            }
            let Ok(next) = self.link(at + WORD) else {
                return Err("a link of the free list doesn't decode to a block");
            };
            (prev, at) = (at, next);
        }
        if listed != free_blocks {
            return Err("a free block isn't on the free list");
        }
        Ok(())
    }

    /// The free block to allocate `need` bytes from, and where in it the new block
    /// starts: the first that fits, or with `RANDOM` any that fits.
    #[inline(always)]
    fn find(&mut self, need: usize, align: usize) -> Result<Option<(usize, usize)>, Fault> {
        let (mut block, mut chosen, mut fits) = (self.head, None, 0);
        let mut left = self.capacity() / MIN_BLOCK; // the region has no room for more blocks than this
        while block != 0 {
            self.stats.examined += 1;
            let tag = self.read(block);
            if Self::has(UNLINK) {
                if tag & FLAGS != 0 || left == 0 {
                    return Err(Fault::Link); // an allocated block on the list, or a list that goes round
                }
                left -= 1;
            }
            if let Some(start) = place(block, tag & !FLAGS, need, align) {
                if !Self::has(RANDOM) {
                    return Ok(Some((block, start)));
                }
                // The `fits`th block that fits replaces the choice so far with probability
                // 1/fits, which leaves each of them equally likely at the end.
                fits += 1;
                if self.random.below(fits) == 0 {
                    chosen = Some((block, start));
                }
            }
            block = self.link(block + WORD)?;
        }
        Ok(chosen)
    }

    /// Take a block out of quarantine: check that nothing wrote to it, and free it.
    #[inline(always)]
    fn release(&mut self, block: usize) -> Result<(), Fault> {
        let tag = self.read(block);
        let size = tag & !FLAGS;
        if tag & FLAGS != USED | HELD
            || size < MIN_BLOCK
            || size > self.end - block
            || self.read(block + size - WORD) != tag
        {
            return Err(Fault::Tags);
        }
        self.counts.verified += size / WORD - 2;
        for word in (block + WORD..block + size - WORD).step_by(WORD) {
            if self.read(word) != POISON {
                return Err(Fault::WrittenAfterFree);
            }
        }
        self.held -= size;
        self.counts.released += 1;
        self.merge(block, size)
    }

    /// Put the block on the free list, merged with whichever of its neighbours are free:
    /// the second half of `Fit::free`.
    #[inline(always)]
    fn merge(&mut self, mut block: usize, mut size: usize) -> Result<(), Fault> {
        // Marked free where it is before it is merged, as in `Fit::free`.
        self.set_block(block, size, 0);
        let before = self.read(block - WORD); // the footer of the block before
        if before & USED == 0 {
            // With `UNLINK`, that footer is believed only if a header agrees with it.
            if Self::has(UNLINK)
                && (before < MIN_BLOCK || before > block - self.first || self.read(block - before) != before)
            {
                return Err(Fault::Tags);
            }
            block -= before;
            size += before;
            self.unlink(block)?;
            self.stats.merges += 1;
        }
        let after = self.read(block + size); // the header of the block after
        if after & USED == 0 {
            if Self::has(UNLINK)
                && (after < MIN_BLOCK
                    || after > self.end - (block + size)
                    || self.read(block + size + after - WORD) != after)
            {
                return Err(Fault::Tags);
            }
            self.unlink(block + size)?;
            size += after;
            self.stats.merges += 1;
        }
        self.set_block(block, size, 0);
        self.insert(block)
    }

    /// Put a free block at the front of the list.
    #[inline(always)]
    fn insert(&mut self, block: usize) -> Result<(), Fault> {
        let next = self.head;
        if next != 0 {
            if Self::has(UNLINK) && self.link(next + 2 * WORD)? != 0 {
                return Err(Fault::Unlink); // the first block on the list has one before it
            }
            self.set_link(next + 2 * WORD, block);
        }
        self.set_link(block + WORD, next);
        self.set_link(block + 2 * WORD, 0);
        self.head = block;
        Ok(())
    }

    /// The blocks before and after `block` on the list. With `UNLINK`, a fault unless
    /// each of them points back at `block`.
    #[inline(always)]
    fn neighbours(&self, block: usize) -> Result<(usize, usize), Fault> {
        let (prev, next) = (self.link(block + 2 * WORD)?, self.link(block + WORD)?);
        if Self::has(UNLINK) {
            let forward = if prev != 0 { self.link(prev + WORD)? } else { self.head };
            let back = if next != 0 { self.link(next + 2 * WORD)? } else { block };
            if forward != block || back != block {
                return Err(Fault::Unlink);
            }
        }
        Ok((prev, next))
    }

    /// Take a free block off the list.
    #[inline(always)]
    fn unlink(&mut self, block: usize) -> Result<(), Fault> {
        let (prev, next) = self.neighbours(block)?;
        if prev != 0 {
            self.set_link(prev + WORD, next);
        } else {
            self.head = next;
        }
        if next != 0 {
            self.set_link(next + 2 * WORD, prev);
        }
        Ok(())
    }

    /// Put the free block `new` where `old` is on the list.
    #[inline(always)]
    fn replace(&mut self, old: usize, new: usize) -> Result<(), Fault> {
        let (prev, next) = self.neighbours(old)?;
        self.set_link(new + WORD, next);
        self.set_link(new + 2 * WORD, prev);
        if next != 0 {
            self.set_link(next + 2 * WORD, new);
        }
        if prev != 0 {
            self.set_link(prev + WORD, new);
        } else {
            self.head = new;
        }
        Ok(())
    }

    /// The link stored at `slot`: a block's header, or 0. With `ENCODE` it is decoded
    /// first. With `ENCODE` or `UNLINK` it is a fault if it couldn't be a block of this
    /// region.
    #[inline(always)]
    fn link(&self, slot: usize) -> Result<usize, Fault> {
        let mut link = self.read(slot);
        if Self::has(ENCODE) {
            link ^= self.link_key ^ slot;
        }
        if Self::has(UNLINK | ENCODE) && link != 0 && !self.plausible(link) {
            return Err(Fault::Link);
        }
        Ok(link)
    }

    #[inline(always)]
    fn set_link(&mut self, slot: usize, link: usize) {
        self.write(slot, if Self::has(ENCODE) { link ^ self.link_key ^ slot } else { link });
    }

    /// Whether `block` could be the header of a block: inside the region, and a word past
    /// a multiple of 16.
    #[inline(always)]
    fn plausible(&self, block: usize) -> bool {
        block >= self.first && block < self.end && block & FLAGS == WORD
    }

    /// The canary of the block at `block`: the secret, XORed with the block's address so
    /// that no two blocks have the same one.
    #[inline(always)]
    fn canary(&self, block: usize) -> usize {
        self.canary_key ^ block
    }

    /// Write a block's header and footer.
    #[inline(always)]
    fn set_block(&mut self, block: usize, size: usize, flags: usize) {
        self.write(block, size | flags);
        self.write(block + size - WORD, size | flags);
    }

    #[inline(always)]
    fn read(&self, addr: usize) -> usize {
        // SAFETY: `new`'s caller gave us the region. Every address read is inside it, as
        // long as the tags and links are what the allocator wrote, or pass its checks.
        unsafe { (addr as *const usize).read() }
    }

    #[inline(always)]
    fn write(&mut self, addr: usize, value: usize) {
        // SAFETY: as for `read`.
        unsafe { (addr as *mut usize).write(value) }
    }
}

/// A word of random bits. [`Random::below`] gives 31 at a time.
fn word(random: &mut Random) -> usize {
    (random.below(1 << 31) << 33) | (random.below(1 << 31) << 2) | random.below(4)
}

impl<const D: u8> Heap for Hardened<D> {
    /// # Panics
    /// If the allocator finds a fault: the trait has no way to return one.
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        match Hardened::allocate(self, size, align) {
            Ok(addr) => addr,
            Err(fault) => panic!("the heap is corrupt: {fault:?}"),
        }
    }
    unsafe fn free(&mut self, addr: usize) -> Result<(), FreeError> {
        unsafe { Hardened::free(self, addr) }.map_err(|fault| match fault {
            Fault::NotABlock => FreeError::NotABlock,
            Fault::NotAllocated => FreeError::NotAllocated,
            _ => FreeError::Corrupt,
        })
    }
    fn used(&self) -> usize {
        self.used
    }
    fn footprint(&self) -> usize {
        Hardened::footprint(self)
    }
    fn stats(&self) -> Stats {
        self.stats
    }
    fn check(&self) -> Result<(), &'static str> {
        Hardened::check(self)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{Fit, Order, Policy};
    use std::vec;
    use std::vec::Vec;

    const SEED: u64 = 0x2545_f491_4f6c_dd1d;

    fn with<const D: u8, R>(len: usize, f: impl FnOnce(&mut Hardened<D>) -> R) -> R {
        let mut memory = vec![0u128; len / 16];
        let mut heap = unsafe { Hardened::<D>::new(memory.as_mut_ptr() as usize, len, SEED) };
        f(&mut heap)
    }

    fn get<const D: u8>(heap: &mut Hardened<D>, size: usize) -> usize {
        heap.allocate(size, 8).unwrap().unwrap()
    }

    fn put(addr: usize, word: usize, value: usize) {
        unsafe { ((addr + word * WORD) as *mut usize).write(value) }
    }

    /// The same few thousand random allocations and frees as `Fit`'s test, checked after
    /// every one, with every block filled to show that no two overlap.
    fn random_workload<const D: u8>() {
        with::<D, _>(64 * 1024, |heap| {
            let mut random = Random(SEED);
            let mut live: Vec<(usize, usize, u8)> = Vec::new();
            for step in 0..4000 {
                if live.len() < 40 || (live.len() < 120 && random.below(2) == 0) {
                    let (size, align) = (1 + random.below(400), 1 << random.below(8));
                    if let Some(addr) = heap.allocate(size, align).unwrap() {
                        assert!(addr.is_multiple_of(align));
                        unsafe { core::ptr::write_bytes(addr as *mut u8, step as u8, size) };
                        live.push((addr, size, step as u8));
                    }
                } else {
                    let (addr, size, fill) = live.swap_remove(random.below(live.len()));
                    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                    assert!(bytes.iter().all(|&b| b == fill), "a block was written over");
                    assert_eq!(unsafe { heap.free(addr) }, Ok(()));
                }
                assert_eq!(heap.check(), Ok(()), "after step {step}");
            }
            for (addr, _, _) in live {
                assert_eq!(unsafe { heap.free(addr) }, Ok(()));
            }
            assert_eq!(heap.drain(), Ok(()));
            assert_eq!(heap.check(), Ok(()));
            assert_eq!(heap.free_list().collect::<Vec<_>>(), [heap.first()]);
            assert_eq!((heap.used(), heap.held()), (0, 0));
        });
    }

    #[test]
    fn random_allocations_and_frees_keep_the_heap_consistent_under_every_defence() {
        random_workload::<0>();
        random_workload::<UNLINK>();
        random_workload::<ENCODE>();
        random_workload::<CANARY>();
        random_workload::<QUARANTINE>();
        random_workload::<RANDOM>();
        random_workload::<{ UNLINK | ENCODE }>();
        random_workload::<{ UNLINK | ENCODE | CANARY }>();
        random_workload::<{ UNLINK | ENCODE | CANARY | QUARANTINE }>();
        random_workload::<ALL>();
    }

    /// With no defence it is `Fit` with first fit over a LIFO list: the same addresses
    /// for the same requests.
    #[test]
    fn with_no_defence_it_places_blocks_where_fit_does() {
        let mut memory = vec![0u128; 4096];
        let mut other = vec![0u128; 4096];
        let mut fit = unsafe { Fit::new(memory.as_mut_ptr() as usize, 65536, Policy::FirstFit, Order::Lifo) };
        let mut heap = unsafe { Hardened::<0>::new(other.as_mut_ptr() as usize, 65536, SEED) };
        let offset = heap.first().wrapping_sub(fit.first());
        let mut random = Random(SEED);
        let mut live: Vec<usize> = Vec::new();
        for _ in 0..4000 {
            if live.len() < 40 || (live.len() < 120 && random.below(2) == 0) {
                let (size, align) = (1 + random.below(400), 1 << random.below(5));
                let addr = fit.allocate(size, align);
                assert_eq!(heap.allocate(size, align), Ok(addr.map(|addr| addr.wrapping_add(offset))));
                live.extend(addr);
            } else {
                let addr = live.swap_remove(random.below(live.len()));
                unsafe {
                    fit.free(addr).unwrap();
                    heap.free(addr.wrapping_add(offset)).unwrap();
                }
            }
        }
        assert_eq!((fit.used(), fit.footprint(), fit.stats()), (heap.used(), heap.footprint(), heap.stats()));
    }

    #[test]
    fn a_canary_costs_a_word_which_the_rounding_sometimes_hides() {
        for (size, plain, canary) in
            [(0, 32, 32), (8, 32, 32), (9, 32, 48), (16, 32, 48), (24, 48, 48), (32, 48, 64), (100, 128, 128)]
        {
            assert_eq!(Hardened::<0>::block_size(size), plain, "a request for {size} bytes");
            assert_eq!(Hardened::<CANARY>::block_size(size), canary, "a request for {size} bytes, with a canary");
            with::<CANARY, _>(4096, |heap| {
                get(heap, size);
                assert_eq!(heap.used(), canary);
            });
        }
    }

    #[test]
    fn the_same_seed_gives_the_same_run_and_another_seed_another() {
        let run = |seed: u64| {
            let mut memory = vec![0u128; 1024];
            let base = memory.as_mut_ptr() as usize;
            let mut heap = unsafe { Hardened::<ALL>::new(base, 16384, seed) };
            let blocks: Vec<usize> = (0..64).map(|_| get(&mut heap, 48)).collect();
            for &block in blocks.iter().step_by(2) {
                unsafe { heap.free(block).unwrap() };
            }
            (0..8).map(|_| get(&mut heap, 48) - base).collect::<Vec<_>>()
        };
        assert_eq!(run(1), run(1));
        assert_ne!(run(1), run(2));
    }

    #[test]
    fn a_second_free_is_refused_under_every_defence() {
        fn twice<const D: u8>() {
            with::<D, _>(4096, |heap| {
                let [_a, b, _c] = [0; 3].map(|_| get(heap, 48));
                unsafe {
                    assert_eq!(heap.free(b), Ok(()));
                    assert_eq!(heap.free(b), Err(Fault::NotAllocated));
                    assert_eq!(heap.free(b + 8), Err(Fault::NotABlock));
                }
            });
        }
        twice::<0>();
        twice::<QUARANTINE>();
        twice::<ALL>();
    }

    /// The mistake `Fit` can't see: a block is freed, and its first word, where the
    /// allocator keeps the link to the next free block, is then set to the address of
    /// something made to look like a free block. Returns what the next two allocations
    /// of the same size did.
    fn stale_link<const D: u8>(inside: bool) -> [Result<Option<usize>, Fault>; 2] {
        let mut outside = [64usize, 0, 0, 0, 0, 0, 0, 64];
        with::<D, _>(4096, |heap| {
            heap.set_quarantine(0);
            let own = get(heap, 96);
            let [f, _g] = [0; 2].map(|_| get(heap, 48));
            let fake = if inside { own + WORD } else { core::hint::black_box(&raw mut outside) as usize };
            unsafe { heap.free(f).unwrap() };
            // A free block of 64 bytes, whose back link is the block that will lead to it.
            for (word, value) in [(0, 64), (1, 0), (2, f - WORD), (7, 64)] {
                put(fake, word, value);
            }
            put(f, 0, fake);
            let got = [heap.allocate(48, 8), heap.allocate(48, 8)];
            if got[1] == Ok(Some(fake + WORD)) {
                assert!(heap.check().is_err());
            }
            got.map(|r| r.map(|addr| addr.map(|addr| if addr == fake + WORD { 1 } else { 0 })))
        })
    }

    #[test]
    fn a_forged_link_out_of_the_region_is_followed_unless_links_are_checked() {
        assert_eq!(stale_link::<0>(false), [Ok(Some(0)), Ok(Some(1))]);
        assert_eq!(stale_link::<UNLINK>(false)[0], Err(Fault::Link));
        assert_eq!(stale_link::<ENCODE>(false)[0], Err(Fault::Link));
    }

    /// Safe unlinking checks what the forger can also write: a forged block inside the
    /// region, with a back link to the block that leads to it, passes. Encoded links
    /// don't decode to it.
    #[test]
    fn a_forged_block_inside_the_region_passes_safe_unlinking_and_not_encoding() {
        assert_eq!(stale_link::<0>(true), [Ok(Some(0)), Ok(Some(1))]);
        assert_eq!(stale_link::<UNLINK>(true), [Ok(Some(0)), Ok(Some(1))]);
        assert_eq!(stale_link::<ENCODE>(true)[0], Err(Fault::Link));
        assert_eq!(stale_link::<{ UNLINK | ENCODE }>(true)[0], Err(Fault::Link));
    }

    /// A freed block whose first word is set to its own header: a link that passes every
    /// test of where it leads, and only fails the test of what points back.
    #[test]
    fn a_link_from_a_block_to_itself_hands_it_out_twice_unless_unlinking_is_safe() {
        fn twice<const D: u8>() -> [Result<Option<usize>, Fault>; 2] {
            with::<D, _>(4096, |heap| {
                let [f, _g] = [0; 2].map(|_| get(heap, 48));
                unsafe { heap.free(f).unwrap() };
                put(f, 0, f - WORD);
                [heap.allocate(48, 8), heap.allocate(48, 8)].map(|r| r.map(|addr| addr.map(|addr| addr - f)))
            })
        }
        assert_eq!(twice::<0>(), [Ok(Some(0)), Ok(Some(0))]);
        assert_eq!(twice::<UNLINK>()[0], Err(Fault::Unlink));
        assert_eq!(twice::<ENCODE>()[0], Err(Fault::Link));
    }

    #[test]
    fn safe_unlinking_refuses_a_block_whose_neighbours_dont_point_back() {
        with::<UNLINK, _>(4096, |heap| {
            let [a, _b, c, _d, e, _f] = [0; 6].map(|_| get(heap, 48));
            unsafe {
                heap.free(a).unwrap();
                heap.free(c).unwrap();
                heap.free(e).unwrap(); // the list is e, c, a, and the rest of the region
            }
            put(c, 0, e - WORD); // c's link to the next block now leads back to e
            assert_eq!(heap.allocate(48, 8), Ok(Some(e))); // e is unlinked: c's back link is still right
            assert_eq!(heap.allocate(200, 8), Err(Fault::Link)); // the search passes c, to e, which is allocated
        });
        with::<UNLINK, _>(4096, |heap| {
            let [a, _b, c, _d, e, _f] = [0; 6].map(|_| get(heap, 48));
            unsafe {
                heap.free(a).unwrap();
                heap.free(c).unwrap();
                heap.free(e).unwrap();
            }
            put(c, 1, a - WORD); // c's back link says a is before it, and e is
            assert_eq!(heap.allocate(48, 8), Err(Fault::Unlink)); // e can't be unlinked: c doesn't point back
        });
    }

    /// A write past the end that puts back the footer it crosses: the tags agree
    /// afterwards, and only the canary has changed.
    #[test]
    fn a_canary_catches_an_overrun_that_leaves_the_tags_alone() {
        fn overrun<const D: u8>() -> Result<(), Fault> {
            with::<D, _>(4096, |heap| {
                let [a, b] = [0; 2].map(|_| get(heap, 24));
                assert_eq!(b - a, 48);
                // Four words past a's 24 bytes: padding or canary, a's footer, b's header, b's first word.
                for (word, value) in [(3, 0x31), (4, 0x31), (5, 0x31), (6, 0x6666)] {
                    put(a, word, value);
                }
                unsafe { heap.free(a) }
            })
        }
        assert_eq!(overrun::<0>(), Ok(()));
        assert_eq!(overrun::<{ UNLINK | ENCODE }>(), Ok(()));
        assert_eq!(overrun::<CANARY>(), Err(Fault::Canary));
    }

    #[test]
    fn the_quarantine_holds_a_block_for_as_many_frees_as_it_has_slots() {
        with::<QUARANTINE, _>(16 * 1024, |heap| {
            heap.set_quarantine(8);
            let blocks: Vec<usize> = (0..32).map(|_| get(heap, 48)).collect();
            unsafe { heap.free(blocks[0]).unwrap() };
            assert_eq!(unsafe { (blocks[0] as *const usize).read() }, POISON);
            for i in 1..=8 {
                // Still held: a new block is not the old one, and the old one can't be freed again.
                assert_ne!(get(heap, 48), blocks[0]);
                assert_eq!(unsafe { heap.free(blocks[0]) }, Err(Fault::NotAllocated));
                assert_eq!(heap.held(), 64 * i);
                unsafe { heap.free(blocks[i]).unwrap() };
            }
            // The eighth free after it pushed it out, and it is the first block on the list.
            assert_eq!(get(heap, 48), blocks[0]);
            assert_eq!(heap.counts().released, 1);
            assert_eq!(heap.check(), Ok(()));
        });
    }

    #[test]
    fn a_write_to_a_quarantined_block_is_found_when_it_leaves() {
        with::<QUARANTINE, _>(16 * 1024, |heap| {
            heap.set_quarantine(4);
            let blocks: Vec<usize> = (0..8).map(|_| get(heap, 48)).collect();
            unsafe { heap.free(blocks[0]).unwrap() };
            put(blocks[0], 3, 7); // not where a link would be: any word of the block shows
            assert_eq!(heap.check(), Err("a quarantined block was written to"));
            for &block in &blocks[1..4] {
                assert_eq!(unsafe { heap.free(block) }, Ok(()));
            }
            assert_eq!(unsafe { heap.free(blocks[4]) }, Err(Fault::WrittenAfterFree));
            // The block is never reused.
            for _ in 0..64 {
                assert_ne!(get(heap, 48), blocks[0]);
            }
        });
    }

    /// What no defence here sees: a write through a stale pointer after the block has
    /// left the quarantine and been handed to someone else.
    #[test]
    fn a_write_after_the_block_is_reused_is_seen_by_nothing() {
        with::<ALL, _>(16 * 1024, |heap| {
            heap.set_quarantine(4);
            let stale = get(heap, 48);
            unsafe { heap.free(stale).unwrap() };
            let mut held = Vec::new();
            let reused = loop {
                let block = get(heap, 48);
                if block == stale {
                    break block;
                }
                held.push(block);
                if held.len() > 4 {
                    unsafe { heap.free(held.remove(0)).unwrap() };
                }
                assert!(held.len() < 4000);
            };
            put(stale, 0, 0x6666);
            assert_eq!(unsafe { (reused as *const usize).read() }, 0x6666);
            assert_eq!(heap.check(), Ok(()));
            assert_eq!(unsafe { heap.free(reused) }, Ok(()));
            assert_eq!(heap.drain(), Ok(()));
        });
    }

    #[test]
    fn a_random_choice_spreads_allocations_over_the_holes() {
        fn chosen<const D: u8>() -> usize {
            with::<D, _>(64 * 1024, |heap| {
                heap.set_quarantine(0);
                let blocks: Vec<usize> = (0..512).map(|_| get(heap, 48)).collect();
                for &block in blocks.iter().step_by(2) {
                    unsafe { heap.free(block).unwrap() };
                }
                let holes: Vec<usize> = (0..64).map(|_| get(heap, 48)).collect();
                assert!(holes.iter().all(|hole| blocks.contains(hole) || *hole > blocks[511]));
                // How many times two blocks allocated one after the other went to holes next to each other.
                holes.windows(2).filter(|pair| pair[0].abs_diff(pair[1]) <= 128).count()
            })
        }
        assert_eq!(chosen::<0>(), 63);
        assert!(chosen::<RANDOM>() < 4);
    }

    #[test]
    fn check_finds_what_the_defences_would() {
        with::<ALL, _>(4096, |heap| {
            let [a, b] = [0; 2].map(|_| get(heap, 24));
            put(a, 3, 0);
            assert_eq!(heap.check(), Err("a block's canary was changed"));
            put(a, 3, heap.canary(a - WORD));
            assert_eq!(heap.check(), Ok(()));
            unsafe { heap.free(b).unwrap() };
            put(b, 0, 0);
            assert_eq!(heap.check(), Err("a quarantined block was written to"));
        });
        with::<ENCODE, _>(4096, |heap| {
            let [a, _b] = [0; 2].map(|_| get(heap, 24));
            unsafe { heap.free(a).unwrap() };
            put(a, 0, heap.first());
            assert_eq!(heap.check(), Err("a link of the free list doesn't decode to a block"));
        });
    }
}
