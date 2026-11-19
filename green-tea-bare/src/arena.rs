//! An arena: a bump allocator over a block of its own, which can be reset. Resetting
//! frees every allocation made from it at once, by moving one pointer back.
//!
//! It is an [`Allocator`], so a collection can be told to allocate from it:
//! `Vec::new_in(&arena)`. That collection then borrows the arena, and [`Arena::reset`]
//! takes `&mut self`: the borrow checker won't let the arena be reset while anything
//! allocated from it is still alive.
//!
//! Not for use from an interrupt handler, or from two tasks that can preempt each other:
//! the bump is a plain load and store.

use crate::heap::align_up;
use alloc::alloc::{Layout, alloc, handle_alloc_error};
use core::alloc::{AllocError, Allocator};
use core::cell::Cell;
use core::ptr::{self, NonNull};

pub struct Arena {
    start: usize,
    end: usize,
    /// The first address not handed out since the last reset.
    next: Cell<usize>,
    /// The highest `next` has ever been.
    peak: Cell<usize>,
}

impl Arena {
    /// An arena of `capacity` bytes, taken from the global heap. green-tea's heap never
    /// frees, so the block is the arena's for good.
    ///
    /// # Panics
    /// If the heap doesn't have `capacity` bytes left.
    pub fn new(capacity: usize) -> Arena {
        let layout = Layout::from_size_align(capacity, 16).expect("an arena of a sane size");
        let start = unsafe { alloc(layout) };
        if start.is_null() {
            handle_alloc_error(layout);
        }
        let start = start as usize;
        Arena { start, end: start + capacity, next: Cell::new(start), peak: Cell::new(start) }
    }

    /// Free everything allocated from the arena.
    pub fn reset(&mut self) {
        self.next.set(self.start);
    }

    /// Bytes handed out since the last reset, padding included.
    pub fn used(&self) -> usize {
        self.next.get() - self.start
    }

    /// The most `used` has ever been.
    pub fn peak(&self) -> usize {
        self.peak.get() - self.start
    }

    pub fn capacity(&self) -> usize {
        self.end - self.start
    }

    /// Move `next` to `end`, if the arena has room.
    fn bump_to(&self, end: usize) -> Result<(), AllocError> {
        if end > self.end {
            return Err(AllocError);
        }
        self.next.set(end);
        self.peak.set(self.peak.get().max(end));
        Ok(())
    }

    /// Whether the block at `ptr`, of `size` bytes, is the last one handed out.
    fn is_last(&self, ptr: NonNull<u8>, size: usize) -> bool {
        ptr.as_ptr() as usize + size == self.next.get()
    }
}

unsafe impl Allocator for Arena {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let addr = align_up(self.next.get(), layout.align()).ok_or(AllocError)?;
        self.bump_to(addr.checked_add(layout.size()).ok_or(AllocError)?)?;
        let ptr = NonNull::new(addr as *mut u8).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    /// Only the last block can be given back before the reset: freeing it moves `next`
    /// down again. Any other block stays allocated until then.
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if self.is_last(ptr, layout.size()) {
            self.next.set(ptr.as_ptr() as usize);
        }
    }

    /// The last block grows where it is, by moving `next` up. Any other block is copied
    /// to a new one, and the old one stays allocated until the reset.
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let addr = ptr.as_ptr() as usize;
        if self.is_last(ptr, old.size()) && addr.is_multiple_of(new.align()) {
            self.bump_to(addr.checked_add(new.size()).ok_or(AllocError)?)?;
            return Ok(NonNull::slice_from_raw_parts(ptr, new.size()));
        }
        let block = self.allocate(new)?;
        unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), block.as_ptr().cast(), old.size()) };
        Ok(block)
    }
}
