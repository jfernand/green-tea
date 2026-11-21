//! `Shards::reset` must not grow the process's heap. In a file of its own because it needs
//! a global allocator that counts, and no other test running beside it.

use green_tea_measure::subjects;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is passed on to `System` unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn a_reset_of_shards_leaks_nothing() {
    let mut subject = subjects::build("shards", 1 << 20, 1 << 16).unwrap();
    subject.reset();
    let before = LIVE.load(Ordering::Relaxed);
    for _ in 0..1000 {
        subject.reset();
    }
    let after = LIVE.load(Ordering::Relaxed);
    assert_eq!(after, before, "1000 resets left {} bytes allocated", after - before);
}
