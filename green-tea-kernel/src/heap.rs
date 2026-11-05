//! A bump allocator over the region linker.ld reserves. It never frees: the demo allocates
//! a few task stacks and closures and then stops, so that's enough.

use crate::global::Global;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

unsafe extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

struct Bump {
    next: Global<usize>, // 0 until the first allocation
}

#[global_allocator]
static HEAP: Bump = Bump { next: Global::new(0) };

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let start = &raw const __heap_start as usize;
        let end = &raw const __heap_end as usize;
        let next = match self.next.get() {
            0 => start,
            n => n,
        };
        let Some(addr) = next.checked_next_multiple_of(layout.align()) else { return ptr::null_mut() };
        let Some(new_next) = addr.checked_add(layout.size()) else { return ptr::null_mut() };
        if new_next > end {
            return ptr::null_mut(); // the alloc crate turns this into an allocation-error panic
        }
        self.next.set(new_next);
        addr as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}
