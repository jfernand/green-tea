//! Task stacks, from a pool that linker.ld reserves: a 2 MiB region (`.stacks`), cut into
//! slots of one guard page plus the stack above it. Stacks grow down, so a task that
//! overflows runs into its own guard page first.
//!
//! The guard only protects anything on a board with `Platform::GUARD_PAGES`, which maps
//! the region with the guard pages left out, so touching one faults (riscv64 and x86_64).
//! On aarch64 the MMU is off, and an overflow writes into the guard page and
//! then into the slot below, silently, as it would with any heap-allocated stack.

use crate::global::Global;
use core::ops::Range;
use green_tea_pool::{FreeList, Pool};

pub const PAGE: usize = 4096;
pub const STACK_SIZE: usize = 16 * 1024;
pub const SLOT: usize = PAGE + STACK_SIZE;
pub const REGION: usize = 2 * 1024 * 1024; // as linker.ld's .stacks: one 2 MiB page
pub const SLOTS: usize = REGION / SLOT;

unsafe extern "C" {
    static __stacks_start: u8;
}

/// The region's first byte, 2 MiB aligned.
pub fn region_start() -> usize {
    &raw const __stacks_start as usize
}

/// How the pool remembers which slots are free: a list threaded through the free slots.
/// green-tea-pool's `Bitmap` does the same job with one bit per slot; either works here.
type Slots = FreeList;

/// The pool, made the first time it's used.
static POOL: Global<Option<Slots>> = Global::new(None);

/// Run `f` on the pool. A free slot's link goes in the first word above its guard page:
/// on a board with guard pages, the guard page itself can't be written.
fn with_pool<R>(f: impl FnOnce(&mut Slots) -> R) -> R {
    let mut pool = POOL.get().unwrap_or_else(|| unsafe { Slots::new(region_start(), SLOT, SLOTS, PAGE) });
    let result = f(&mut pool);
    POOL.set(Some(pool));
    result
}

/// How many stacks the pool has left.
pub fn free_slots() -> usize {
    with_pool(|pool| pool.free())
}

/// A task's stack: a slot in the pool, given back on drop. Taking and giving back aren't
/// interrupt-safe: green-tea-preempt creates tasks with interrupts off, and the scheduler,
/// which drops them, runs with interrupts off.
pub struct Stack {
    /// The slot's first byte, which is its guard page's.
    base: usize,
}

impl Stack {
    /// Take a free slot.
    ///
    /// # Panics
    /// If every slot is taken.
    pub fn take() -> Stack {
        match with_pool(|pool| pool.take()) {
            Some(base) => Stack { base },
            None => panic!("out of task stacks (all {SLOTS} in use)"),
        }
    }

    /// One past the highest byte: where the stack starts.
    pub fn top(&self) -> *mut u8 {
        (self.base + SLOT) as *mut u8
    }

    /// The guard page's addresses.
    pub fn guard(&self) -> Range<usize> {
        self.base..self.base + PAGE
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // SAFETY: `base` came from this pool's `take`, and a Stack is dropped once.
        let given = with_pool(|pool| unsafe { pool.give(self.base) });
        assert!(given.is_ok(), "the stack pool refused a stack: {given:?}");
    }
}

/// Every guard page in the pool, lowest first: what a board with guard pages leaves
/// unmapped.
pub fn guard_pages() -> impl Iterator<Item = usize> {
    (0..SLOTS).map(|slot| region_start() + slot * SLOT)
}

/// If `addr` is in a guard page, that page's stack slot.
pub fn guard_slot(addr: usize) -> Option<usize> {
    let offset = addr.checked_sub(region_start())?;
    let slot = offset / SLOT;
    (slot < SLOTS && offset % SLOT < PAGE).then_some(slot)
}

/// What a board's fault handler calls when a fault hits a guard page. It doesn't return:
/// the task's stack is gone, so there is nothing to go back to.
static ON_OVERFLOW: Global<Option<fn(usize) -> !>> = Global::new(None);

/// Replace the default reaction to a stack overflow (panic) with `f`, which gets the
/// faulting address. green-tea-coop's overflow test uses it to report PASS.
pub fn on_overflow(f: fn(addr: usize) -> !) {
    ON_OVERFLOW.set(Some(f));
}

/// Called by a board's fault handler, on its own stack, when a fault hits a guard page.
pub fn overflowed(addr: usize) -> ! {
    match ON_OVERFLOW.get() {
        Some(f) => f(addr),
        None => panic!("stack overflow: a task ran into its guard page at {addr:#x}"),
    }
}
