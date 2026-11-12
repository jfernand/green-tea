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

pub const PAGE: usize = 4096;
pub const STACK_SIZE: usize = 16 * 1024;
pub const SLOT: usize = PAGE + STACK_SIZE;
pub const REGION: usize = 2 * 1024 * 1024; // as linker.ld's .stacks: one 2 MiB page
pub const SLOTS: usize = REGION / SLOT;

const _: () = assert!(SLOTS <= 128); // one bit each in `FREE`

unsafe extern "C" {
    static __stacks_start: u8;
}

/// The region's first byte, 2 MiB aligned.
pub fn region_start() -> usize {
    &raw const __stacks_start as usize
}

/// Bit `i` set: slot `i` is free.
static FREE: Global<u128> = Global::new(u128::MAX >> (128 - SLOTS));

/// A task's stack: a slot in the pool, given back on drop. Taking and giving back aren't
/// interrupt-safe: green-tea-preempt creates tasks with interrupts off, and the scheduler,
/// which drops them, runs with interrupts off.
pub struct Stack {
    slot: usize,
}

impl Stack {
    /// Take a free slot.
    ///
    /// # Panics
    /// If every slot is taken.
    pub fn take() -> Stack {
        let free = FREE.get();
        assert!(free != 0, "out of task stacks (all {SLOTS} in use)");
        let slot = free.trailing_zeros() as usize;
        FREE.set(free & !(1 << slot));
        Stack { slot }
    }

    /// One past the highest byte: where the stack starts.
    pub fn top(&self) -> *mut u8 {
        (region_start() + (self.slot + 1) * SLOT) as *mut u8
    }

    /// The guard page's addresses.
    pub fn guard(&self) -> Range<usize> {
        let start = region_start() + self.slot * SLOT;
        start..start + PAGE
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        FREE.set(FREE.get() | (1 << self.slot));
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
