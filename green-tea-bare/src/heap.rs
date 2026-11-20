//! The kernel's heap: a [`Fit`] over the region linker.ld reserves. It is the global
//! allocator, so it is what `Box`, `Vec` and the rest of the `alloc` crate allocate from.
//!
//! Blocks are of any size, and a freed block is reused: the allocator searches a list of
//! free blocks for the first one large enough, and merges a freed block with its free
//! neighbours. green-tea-fit has the allocator itself, and the other ways to search.
//!
//! Every operation runs with interrupts off, so that a timer interrupt can't switch to
//! another task half way through one (green-tea-preempt). There is one hart, so that is
//! the only way two tasks could be in the allocator at once.

use crate::global::Global;
use crate::platform::{Board, TimerInterrupt};
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;
use green_tea_fit::{Fit, Order, Policy};

unsafe extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

/// Which free block the heap takes, and where a freed block goes on its list.
pub const POLICY: Policy = Policy::FirstFit;
pub const ORDER: Order = Order::Address;

/// `addr` rounded up to a multiple of `align`, which must be a power of two, as a
/// `Layout`'s alignment always is. `None` if that overflows.
///
/// `checked_next_multiple_of` does the same for any `align`, with a division: the compiler
/// can't see that a `Layout`'s alignment is a power of two. This is an add and a mask.
pub fn align_up(addr: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    Some(addr.checked_add(align - 1)? & !(align - 1))
}

struct Heap {
    fit: UnsafeCell<Option<Fit>>, // None until the first use
}

// SAFETY: one hart, and `with` keeps interrupts off while anything touches `fit`.
unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap { fit: UnsafeCell::new(None) };

/// Set while `with` is running: the heap can't be used from inside itself.
static BUSY: Global<bool> = Global::new(false);

/// Run `f` on the heap's allocator, with interrupts off. `f` must not allocate or free:
/// that would be a second use of the allocator inside the first, and panics.
pub fn with<R>(f: impl FnOnce(&mut Fit) -> R) -> R {
    Board::without_interrupts(|| {
        assert!(!BUSY.replace(true), "the heap was used from inside heap::with");
        // SAFETY: interrupts are off and BUSY was clear, so this is the only reference.
        let fit = unsafe { &mut *HEAP.fit.get() }.get_or_insert_with(|| {
            let start = &raw const __heap_start as usize;
            let end = &raw const __heap_end as usize;
            // SAFETY: linker.ld reserves the region for the heap, and nothing else uses it.
            unsafe { Fit::new(start, end - start, POLICY, ORDER) }
        });
        let result = f(fit);
        BUSY.set(false);
        result
    })
}

/// Bytes of the heap in allocated blocks, each block's two tags included.
pub fn used() -> usize {
    with(|fit| fit.used())
}

/// The heap's size: what linker.ld reserves, less a word at each end.
pub fn capacity() -> usize {
    with(|fit| fit.capacity())
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Null if nothing fits: the alloc crate turns that into an allocation-error panic.
        with(|fit| fit.allocate(layout.size(), layout.align())).map_or(ptr::null_mut(), |addr| addr as *mut u8)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: the caller's promise is ours to the allocator.
        if let Err(e) = with(|fit| unsafe { fit.free(ptr as usize) }) {
            panic!("free of {ptr:p}: {e:?}");
        }
    }
}
