//! The board's other CPUs: starting them, and running a function on all of them at once.
//!
//! A kernel begins on one CPU, the *boot CPU*, which is CPU 0. [`start_all`] starts every
//! other CPU the board has, numbering them from 1. Each gets a stack of 64 KiB from the
//! heap, sets up what the boot CPU set up for itself, and then waits in [`enter`] to be
//! given work. [`run`] gives every CPU the same function, the boot CPU included, and
//! returns when all of them have returned from it.
//!
//! What a started CPU may use is short: the heap, which is behind a lock; the board's
//! counter (`Board::ticks`); atomics; and whatever the function it is given brings. It
//! runs with interrupts off and has no tasks. The scheduler, the console and every
//! [`Global`](crate::Global) are the boot CPU's alone.

use crate::platform::{Board, Platform};
use alloc::boxed::Box;
use alloc::vec;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The most CPUs `start_all` will start, the boot CPU included.
pub const MAX: usize = 8;

/// A started CPU's stack. It has no guard page.
const STACK: usize = 64 * 1024;

/// How many CPUs are running.
static COUNT: AtomicUsize = AtomicUsize::new(1);
/// Set by each started CPU when it reaches [`enter`].
static ONLINE: [AtomicBool; MAX] = [const { AtomicBool::new(false) }; MAX];
/// The function each started CPU is to run next, as an address, or 0 for none.
static WORK: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
/// How many CPUs have picked up the work `run` gave out. They all wait for the last.
static ARRIVED: AtomicUsize = AtomicUsize::new(0);
/// Set by [`park`]: the started CPUs stop for good.
static PARKED: AtomicBool = AtomicBool::new(false);

/// How many CPUs are running: 1 until [`start_all`].
pub fn count() -> usize {
    COUNT.load(Ordering::Acquire)
}

/// Start every other CPU the board has, up to [`MAX`] in all, and wait for each to be
/// ready for work. Returns how many CPUs are now running. Call it from the boot CPU.
///
/// # Panics
/// If a CPU that the board agreed to start doesn't arrive within a second.
pub fn start_all() -> usize {
    assert_eq!(Board::cpu(), 0, "only the boot CPU starts the others");
    crate::pages::map_all(); // a page fault is something only the boot CPU can take
    for (cpu, online) in ONLINE.iter().enumerate().skip(count()) {
        let stack = Box::leak(vec![0u128; STACK / 16].into_boxed_slice());
        let top = stack.as_mut_ptr() as usize + STACK;
        if Board::start_cpu(cpu, top).is_err() {
            // SAFETY: the board started nothing, so the stack is still only ours.
            drop(unsafe { Box::from_raw(stack) });
            break;
        }
        let deadline = Board::ticks() + 1000 * Board::ticks_per_ms();
        while !online.load(Ordering::Acquire) {
            assert!(Board::ticks() < deadline, "CPU {cpu} was started and didn't arrive");
            spin_loop();
        }
        COUNT.store(cpu + 1, Ordering::Release);
    }
    count()
}

/// Run `f` on every running CPU at once, passing each its number, and return when all of
/// them are done. No CPU begins `f` until all have arrived to run it.
pub fn run(f: fn(usize)) {
    assert_eq!(Board::cpu(), 0, "only the boot CPU hands out work");
    ARRIVED.store(0, Ordering::Release);
    for work in &WORK[1..count()] {
        work.store(f as usize, Ordering::Release);
    }
    together(f, 0);
    for work in &WORK[1..count()] {
        while work.load(Ordering::Acquire) != 0 {
            spin_loop();
        }
    }
}

/// Wait until every CPU is here, then run `f`.
fn together(f: fn(usize), cpu: usize) {
    ARRIVED.fetch_add(1, Ordering::AcqRel);
    while ARRIVED.load(Ordering::Acquire) < count() {
        spin_loop();
    }
    f(cpu);
}

/// Stop every started CPU for good. They can't be started again.
pub fn park() {
    PARKED.store(true, Ordering::Release);
}

/// Where a started CPU spends its life: waiting for work, and doing it. The board's
/// start-up code calls this once the CPU has a stack and can run Rust.
pub fn enter(cpu: usize) -> ! {
    ONLINE[cpu].store(true, Ordering::Release);
    loop {
        let work = WORK[cpu].load(Ordering::Acquire);
        if work != 0 {
            // SAFETY: `run` stored the address of a `fn(usize)` there.
            let f = unsafe { core::mem::transmute::<usize, fn(usize)>(work) };
            together(f, cpu);
            WORK[cpu].store(0, Ordering::Release);
        }
        if PARKED.load(Ordering::Acquire) {
            Board::halt();
        }
        spin_loop();
    }
}
