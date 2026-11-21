//! A spin lock: one flag, set with an atomic swap by whoever takes the lock, and cleared
//! by a store when they are done. A CPU that finds the flag set waits by reading it until
//! it is clear, and then tries the swap again (test and test-and-set).
//!
//! The lock counts how often it was taken, and how often whoever took it found it held
//! first. The first count is the same on every run of the same program. The second
//! depends on how the CPUs' work happened to overlap.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// What a CPU does between two looks at a lock it is waiting for.
pub trait Relax {
    /// `spins` is how many times this wait has looked so far.
    fn relax(spins: usize);
}

/// Tell the processor this is a spin loop, and look again. Right for a kernel, where the
/// holder of the lock is running on another hart.
pub struct Spin;

impl Relax for Spin {
    fn relax(_spins: usize) {
        core::hint::spin_loop();
    }
}

pub struct SpinLock<T, R = Spin> {
    held: AtomicBool,
    /// Times the lock was taken. Written only by whoever holds the lock.
    acquisitions: AtomicUsize,
    /// Times a CPU found the lock held and had to wait.
    contended: AtomicUsize,
    value: UnsafeCell<T>,
    relax: PhantomData<fn() -> R>,
}

// SAFETY: the lock hands out one `&mut T` at a time, to whichever thread holds it.
unsafe impl<T: Send, R> Sync for SpinLock<T, R> {}

/// Proof that the lock is held. Dropping it releases the lock.
pub struct Guard<'a, T, R = Spin> {
    lock: &'a SpinLock<T, R>,
}

impl<T, R: Relax> SpinLock<T, R> {
    pub const fn new(value: T) -> SpinLock<T, R> {
        SpinLock {
            held: AtomicBool::new(false),
            acquisitions: AtomicUsize::new(0),
            contended: AtomicUsize::new(0),
            value: UnsafeCell::new(value),
            relax: PhantomData,
        }
    }

    /// Take the lock, waiting for as long as another CPU holds it. Taking it twice from
    /// the same CPU waits forever.
    pub fn lock(&self) -> Guard<'_, T, R> {
        // The swap writes `true` and returns what was there, in one step that no other
        // CPU can come between. If it returns `false`, the flag was clear and is now ours.
        if self.held.swap(true, Ordering::Acquire) {
            self.contended.fetch_add(1, Ordering::Relaxed);
            let mut spins = 0;
            loop {
                // Wait with loads, which leave the flag's cache line shared between the
                // waiting CPUs. A swap would take the line for itself each time.
                while self.held.load(Ordering::Relaxed) {
                    R::relax(spins);
                    spins += 1;
                }
                if !self.held.swap(true, Ordering::Acquire) {
                    break;
                }
            }
        }
        // We hold the lock, so nobody else is writing this count.
        self.acquisitions.store(self.acquisitions.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        Guard { lock: self }
    }

    /// Run `f` on the value, holding the lock.
    pub fn with<V>(&self, f: impl FnOnce(&mut T) -> V) -> V {
        f(&mut self.lock())
    }

    /// How many times the lock has been taken, and how many of those had to wait.
    pub fn counts(&self) -> (usize, usize) {
        (self.acquisitions.load(Ordering::Relaxed), self.contended.load(Ordering::Relaxed))
    }

    /// The value, for someone who owns the lock and so needs no locking.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

impl<T, R> Deref for Guard<'_, T, R> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard exists, so the lock is held, by us.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T, R> DerefMut for Guard<'_, T, R> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T, R> Drop for Guard<'_, T, R> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{Relax, SpinLock};
    use std::thread;

    /// The tests' threads may outnumber the machine's free CPUs, and a thread that spins
    /// while the lock's holder is descheduled waits a time slice for nothing.
    struct Yield;

    impl Relax for Yield {
        fn relax(spins: usize) {
            if spins % 64 == 63 { thread::yield_now() } else { core::hint::spin_loop() }
        }
    }

    #[test]
    fn a_lock_alone_counts_its_acquisitions_and_never_waits() {
        let lock = SpinLock::<u32>::new(0);
        for _ in 0..10 {
            *lock.lock() += 1;
        }
        assert_eq!(lock.with(|v| *v), 10);
        assert_eq!(lock.counts(), (11, 0));
    }

    #[test]
    fn four_threads_adding_under_the_lock_lose_nothing() {
        const EACH: usize = 200_000;
        let lock = SpinLock::<usize, Yield>::new(0);
        thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..EACH {
                        // A read and a write, apart: without the lock, updates are lost.
                        let mut guard = lock.lock();
                        let seen = *guard;
                        core::hint::black_box(&seen);
                        *guard = seen + 1;
                    }
                });
            }
        });
        assert_eq!(lock.with(|v| *v), 4 * EACH);
        assert_eq!(lock.counts().0, 4 * EACH + 1);
    }
}
