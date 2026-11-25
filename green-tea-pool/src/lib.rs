//! Pools: allocators for blocks that are all the same size. A pool manages a region cut
//! into `slots` blocks of `slot_size` bytes, and only has to remember which are free.
//! Two ways to remember are here, behind one trait:
//!
//! - [`Bitmap`]: one bit per slot, kept apart from the slots.
//! - [`FreeList`]: a linked list threaded through the free slots themselves.
//!
//! green-tea-bare's task stacks come from one of these (see its `stack` module).
//!
//! `take` and `give` are `#[inline(never)]` so that each can be found, whole, in a
//! disassembly of a kernel.

#![no_std]

/// Why a pool refused a block it was given back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GiveError {
    /// The address isn't the start of one of the pool's slots.
    NotASlot,
    /// The slot is already free: it was given back twice, or never taken.
    NotTaken,
}

pub trait Pool: Sized {
    /// A pool over `slots` blocks of `slot_size` bytes, the first at `base`, all free.
    /// `link_at` is where in a free slot the pool may keep a word of its own, as an
    /// offset from the slot's start; only a pool that keeps one uses it.
    ///
    /// # Safety
    /// The region must not be in use by anything else for as long as the pool is. A
    /// pool that keeps a word in free slots writes it at `link_at`, which must be
    /// word-aligned, in the slot and writable.
    ///
    /// # Panics
    /// If the pool can't manage that many slots.
    unsafe fn new(base: usize, slot_size: usize, slots: usize, link_at: usize) -> Self;

    /// The address of a free slot, which is then taken. `None` if every slot is.
    fn take(&mut self) -> Option<usize>;

    /// Give a slot back, by the address `take` returned.
    ///
    /// # Safety
    /// `slot` must have come from `take` on this pool, and not have been given back
    /// since. A pool reports the mistakes it can detect, and need not detect any.
    unsafe fn give(&mut self, slot: usize) -> Result<(), GiveError>;

    /// How many slots are free.
    fn free(&self) -> usize;
}

/// A pool that keeps one bit per slot: 1 for free. Up to 128 slots, the bits of a `u128`.
///
/// It takes the lowest free slot, by finding the lowest set bit. Its state lives apart
/// from the slots, so it never touches their memory, and it can tell when a slot is
/// given back twice.
///
/// A pool is uniquely owned; moving it does not duplicate its allocation state:
///
/// ```compile_fail
/// use green_tea_pool::{Bitmap, Pool};
/// let mut pool = unsafe { Bitmap::new(0x1000, 8, 2, 0) };
/// let duplicate = pool;
/// let _ = pool.take();
/// drop(duplicate);
/// ```
#[derive(Debug)]
pub struct Bitmap {
    base: usize,
    slot_size: usize,
    slots: usize,
    free: u128,
}

impl Pool for Bitmap {
    unsafe fn new(base: usize, slot_size: usize, slots: usize, _link_at: usize) -> Bitmap {
        assert!(slots <= 128, "a Bitmap has 128 bits");
        assert!(slots == 0 || slot_size != 0, "slots must have a size");
        assert!(slots.checked_mul(slot_size).and_then(|bytes| base.checked_add(bytes)).is_some(), "pool range overflows");
        let free = if slots == 0 { 0 } else { u128::MAX >> (128 - slots) };
        Bitmap { base, slot_size, slots, free }
    }

    #[inline(never)]
    fn take(&mut self) -> Option<usize> {
        if self.free == 0 {
            return None;
        }
        let slot = self.free.trailing_zeros() as usize;
        self.free &= !(1 << slot);
        Some(self.base + slot * self.slot_size)
    }

    #[inline(never)]
    unsafe fn give(&mut self, slot: usize) -> Result<(), GiveError> {
        if self.slots == 0 {
            return Err(GiveError::NotASlot);
        }
        let offset = slot.checked_sub(self.base).ok_or(GiveError::NotASlot)?;
        let index = offset / self.slot_size;
        if index >= self.slots || !offset.is_multiple_of(self.slot_size) {
            return Err(GiveError::NotASlot);
        }
        if self.free & (1 << index) != 0 {
            return Err(GiveError::NotTaken);
        }
        self.free |= 1 << index;
        Ok(())
    }

    fn free(&self) -> usize {
        self.free.count_ones() as usize
    }
}

/// A pool that links its free slots into a list, through a word in each: the list is
/// *intrusive*. The pool itself holds only the list's head.
///
/// It takes the slot most recently given back. Slots that have never been taken aren't
/// on the list at all: they are handed out in address order by a counter, `fresh`, so
/// that making a pool doesn't mean writing a link into every slot.
///
/// It can't tell whether a slot it is given is one of its own, or already free.
#[derive(Debug)]
pub struct FreeList {
    base: usize,
    slot_size: usize,
    slots: usize,
    link_at: usize,
    /// The first slot on the list, or 0 if the list is empty.
    head: usize,
    /// Slots from this index up have never been taken.
    fresh: usize,
    free: usize,
}

impl Pool for FreeList {
    unsafe fn new(base: usize, slot_size: usize, slots: usize, link_at: usize) -> FreeList {
        assert!(link_at.checked_add(size_of::<usize>()).is_some_and(|end| end <= slot_size), "the link must fit in a slot");
        assert!(
            base.checked_add(link_at).is_some_and(|address| address.is_multiple_of(align_of::<usize>())),
            "the link must be word-aligned"
        );
        assert!(slot_size.is_multiple_of(align_of::<usize>()), "the link must be word-aligned");
        assert!(slots == 0 || slot_size != 0, "slots must have a size");
        assert!(slots.checked_mul(slot_size).and_then(|bytes| base.checked_add(bytes)).is_some(), "pool range overflows");
        FreeList { base, slot_size, slots, link_at, head: 0, fresh: 0, free: slots }
    }

    #[inline(never)]
    fn take(&mut self) -> Option<usize> {
        let slot = if self.head != 0 {
            let slot = self.head;
            // The slot is free, so the word at `link_at` is ours: the next free slot.
            self.head = unsafe { ((slot + self.link_at) as *const usize).read() };
            slot
        } else if self.fresh < self.slots {
            self.fresh += 1;
            self.base + (self.fresh - 1) * self.slot_size
        } else {
            return None;
        };
        self.free -= 1;
        Some(slot)
    }

    #[inline(never)]
    unsafe fn give(&mut self, slot: usize) -> Result<(), GiveError> {
        unsafe { ((slot + self.link_at) as *mut usize).write(self.head) };
        self.head = slot;
        self.free += 1;
        Ok(())
    }

    fn free(&self) -> usize {
        self.free
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{Bitmap, FreeList, GiveError, Pool};
    use std::vec;
    use std::vec::Vec;

    const SLOT: usize = 64;
    const SLOTS: usize = 8;

    /// Run `f` on a pool of `SLOTS` slots over memory of its own.
    fn with_pool<P: Pool, R>(f: impl FnOnce(&mut P, usize) -> R) -> R {
        let mut memory = vec![0usize; SLOTS * SLOT / size_of::<usize>()];
        let base = memory.as_mut_ptr() as usize;
        let mut pool = unsafe { P::new(base, SLOT, SLOTS, 0) };
        f(&mut pool, base)
    }

    fn takes_every_slot_once<P: Pool>() {
        with_pool::<P, _>(|pool, base| {
            let mut taken: Vec<usize> = (0..SLOTS).map(|_| pool.take().unwrap()).collect();
            assert_eq!(pool.take(), None, "a ninth slot from a pool of eight");
            assert_eq!(pool.free(), 0);
            taken.sort();
            let expected: Vec<usize> = (0..SLOTS).map(|i| base + i * SLOT).collect();
            assert_eq!(taken, expected);
        });
    }

    fn reuses_what_is_given_back<P: Pool>() {
        with_pool::<P, _>(|pool, _| {
            let taken: Vec<usize> = (0..SLOTS).map(|_| pool.take().unwrap()).collect();
            unsafe { pool.give(taken[3]).unwrap() };
            assert_eq!(pool.free(), 1);
            assert_eq!(pool.take(), Some(taken[3]));
            assert_eq!(pool.take(), None);
        });
    }

    #[test]
    fn a_bitmap_takes_every_slot_once() {
        takes_every_slot_once::<Bitmap>();
    }

    #[test]
    fn a_free_list_takes_every_slot_once() {
        takes_every_slot_once::<FreeList>();
    }

    #[test]
    fn a_bitmap_reuses_what_is_given_back() {
        reuses_what_is_given_back::<Bitmap>();
    }

    #[test]
    fn a_free_list_reuses_what_is_given_back() {
        reuses_what_is_given_back::<FreeList>();
    }

    #[test]
    #[should_panic(expected = "slots must have a size")]
    fn a_bitmap_rejects_nonempty_zero_sized_slots() {
        let _ = unsafe { Bitmap::new(0x1000, 0, 1, 0) };
    }

    #[test]
    #[should_panic(expected = "pool range overflows")]
    fn a_bitmap_rejects_an_unrepresentable_pool_range() {
        let _ = unsafe { Bitmap::new(usize::MAX - 7, 8, 2, 0) };
    }

    #[test]
    fn an_empty_zero_sized_bitmap_refuses_returns() {
        let mut pool = unsafe { Bitmap::new(0x1000, 0, 0, 0) };
        assert_eq!(unsafe { pool.give(0x1000) }, Err(GiveError::NotASlot));
    }

    /// Give back slots 2, 5 and 0, in that order, and take three.
    fn order_after_giving_back_2_5_0<P: Pool>() -> [usize; 3] {
        with_pool::<P, _>(|pool, base| {
            for _ in 0..SLOTS {
                pool.take();
            }
            for i in [2, 5, 0] {
                unsafe { pool.give(base + i * SLOT).unwrap() };
            }
            [0; 3].map(|_| (pool.take().unwrap() - base) / SLOT)
        })
    }

    #[test]
    fn a_bitmap_takes_the_lowest_slot_first() {
        assert_eq!(order_after_giving_back_2_5_0::<Bitmap>(), [0, 2, 5]);
    }

    #[test]
    fn a_free_list_takes_the_last_slot_given_back_first() {
        assert_eq!(order_after_giving_back_2_5_0::<FreeList>(), [0, 5, 2]);
    }

    #[test]
    fn a_bitmap_refuses_a_slot_given_back_twice() {
        with_pool::<Bitmap, _>(|pool, base| {
            let slot = pool.take().unwrap();
            assert_eq!(unsafe { pool.give(slot) }, Ok(()));
            assert_eq!(unsafe { pool.give(slot) }, Err(GiveError::NotTaken));
            assert_eq!(unsafe { pool.give(slot + 8) }, Err(GiveError::NotASlot));
            assert_eq!(unsafe { pool.give(base + SLOTS * SLOT) }, Err(GiveError::NotASlot));
            assert_eq!(pool.free(), SLOTS);
        });
    }

    /// What goes wrong when a free list is given a slot twice: the slot's link points at
    /// itself, and the pool hands the same slot to two callers, forever.
    #[test]
    fn a_free_list_given_a_slot_twice_hands_it_out_twice() {
        with_pool::<FreeList, _>(|pool, _| {
            let slot = pool.take().unwrap();
            unsafe {
                pool.give(slot).unwrap();
                pool.give(slot).unwrap();
            }
            assert_eq!(pool.take(), Some(slot));
            assert_eq!(pool.take(), Some(slot));
            assert_eq!(pool.take(), Some(slot));
        });
    }

    #[test]
    #[should_panic(expected = "128 bits")]
    fn a_bitmap_has_room_for_128_slots() {
        unsafe { Bitmap::new(0, SLOT, 129, 0) };
    }
}
