//! A bump allocator over the region linker.ld reserves. It never frees: the demos allocate
//! a few task stacks and closures and then stop, so that's enough.
//!
//! The bump is a compare-and-swap rather than a plain read and write, so that it stays
//! correct when a timer interrupt preempts one allocation and another task allocates
//! (green-tea-preempt). One hart means no contention: the loop retries only after such a
//! preemption.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

unsafe extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

/// `addr` rounded up to a multiple of `align`, which must be a power of two, as a
/// `Layout`'s alignment always is. `None` if that overflows.
///
/// `checked_next_multiple_of` does the same for any `align`, with a division: the compiler
/// can't see that a `Layout`'s alignment is a power of two. This is an add and a mask.
pub fn align_up(addr: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    Some(addr.checked_add(align - 1)? & !(align - 1))
}

struct Bump {
    next: AtomicUsize, // 0 until the first allocation
}

#[global_allocator]
static HEAP: Bump = Bump { next: AtomicUsize::new(0) };

/// Bytes of the heap handed out so far, padding included. They never come back.
pub fn used() -> usize {
    match HEAP.next.load(Ordering::Relaxed) {
        0 => 0,
        next => next - (&raw const __heap_start as usize),
    }
}

/// The heap's size: what linker.ld reserves.
pub fn capacity() -> usize {
    (&raw const __heap_end as usize) - (&raw const __heap_start as usize)
}

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let start = &raw const __heap_start as usize;
        let end = &raw const __heap_end as usize;
        let mut current = self.next.load(Ordering::Relaxed);
        loop {
            let next = if current == 0 { start } else { current };
            let Some(addr) = align_up(next, layout.align()) else { return ptr::null_mut() };
            let Some(new_next) = addr.checked_add(layout.size()) else { return ptr::null_mut() };
            if new_next > end {
                return ptr::null_mut(); // the alloc crate turns this into an allocation-error panic
            }
            match self.next.compare_exchange_weak(current, new_next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return addr as *mut u8,
                Err(actual) => current = actual,
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}
