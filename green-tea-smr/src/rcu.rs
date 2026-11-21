//! Read, copy, update: a value that many threads read and that is never changed in
//! place.
//!
//! A reader follows one pointer to the value and reads it. A writer makes a new value
//! from the old one, apart, and then swings the pointer to it with a compare-and-swap.
//! A reader therefore sees the old value whole or the new value whole, and never waits.
//! The old value can't be freed while a reader that found it is still reading, so the
//! writer retires it, and the [`Reclaim`] decides how long that is.
//!
//! This is the shape of the Linux kernel's RCU, with [`Reclaim::enter`] and
//! [`Reclaim::exit`] where the kernel has `rcu_read_lock` and `rcu_read_unlock`, and
//! `retire` where it has `call_rcu`. The kernel's readers pay less than ours do: there,
//! a reader announces nothing, and the kernel infers that it has finished from the CPU
//! having switched to another task.

use crate::Reclaim;
use alloc::boxed::Box;
use core::sync::atomic::{AtomicPtr, Ordering};

pub struct Rcu<T> {
    current: AtomicPtr<T>,
}

// SAFETY: readers share the value by reference, and a writer moves a new one in.
unsafe impl<T: Send + Sync> Send for Rcu<T> {}
unsafe impl<T: Send + Sync> Sync for Rcu<T> {}

impl<T> Rcu<T> {
    pub fn new(value: T) -> Rcu<T> {
        Rcu { current: AtomicPtr::new(Box::into_raw(Box::new(value))) }
    }

    /// Call `f` on the value as it is now.
    pub fn read<R: Reclaim, V>(&self, smr: &mut R, f: impl FnOnce(&T) -> V) -> V {
        smr.enter();
        let current = smr.protect(0, &self.current);
        // SAFETY: `protect` says the value may be read until `exit`.
        let result = f(unsafe { &*current });
        smr.exit();
        result
    }

    /// Replace the value with what `f` makes of it. If another writer gets in first, `f`
    /// is called again on the value that writer left.
    ///
    /// The old value is retired, and whichever thread frees it drops it, so `T` must be
    /// `Send`. This must not compile:
    ///
    /// ```compile_fail,E0277
    /// use green_tea_smr::{epoch::Epochs, rcu::Rcu};
    /// use std::rc::Rc;
    ///
    /// let epochs = Epochs::new();
    /// let mut local = epochs.local(0);
    /// let value = Rcu::new(Rc::new(1));
    /// value.update(&mut local, |old| old.clone());
    /// ```
    pub fn update<R: Reclaim>(&self, smr: &mut R, f: impl Fn(&T) -> T)
    where
        T: Send,
    {
        smr.enter();
        loop {
            let old = smr.protect(0, &self.current);
            // SAFETY: as for `read`.
            let new = Box::into_raw(Box::new(f(unsafe { &*old })));
            if self.current.compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                smr.exit();
                // SAFETY: the compare-and-swap unlinked it, and it came from a `Box`.
                unsafe { smr.retire(old) };
                return;
            }
            // SAFETY: nobody else has seen the copy we made.
            drop(unsafe { Box::from_raw(new) });
        }
    }
}

impl<T> Drop for Rcu<T> {
    fn drop(&mut self) {
        // SAFETY: `&mut self`, so the value is ours alone.
        drop(unsafe { Box::from_raw(*self.current.get_mut()) });
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::Rcu;
    use crate::Reclaim;
    use crate::epoch::Epochs;
    use crate::hazard::Hazards;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    /// One thread replaces a pair of numbers 20,000 times, keeping the second twice the
    /// first, while three others read it. No reader may see half of one pair and half of
    /// another, or a pair older than one it has already seen.
    fn one_writer_three_readers<R: Reclaim>(local: impl Fn(usize) -> R + Sync) -> crate::Counts {
        const UPDATES: usize = 20_000;
        let pair = Rcu::new((0usize, 0usize));
        let done = AtomicBool::new(false);
        let mut counts = crate::Counts::default();
        thread::scope(|s| {
            let (pair, done, local) = (&pair, &done, &local);
            for thread in 1..4 {
                s.spawn(move || {
                    let mut smr = local(thread);
                    let mut last = 0;
                    while !done.load(Ordering::Relaxed) {
                        let (first, second) = pair.read(&mut smr, |&pair| pair);
                        assert_eq!(second, 2 * first, "half of one pair and half of another");
                        assert!(first >= last, "an older pair after a newer one");
                        last = first;
                    }
                });
            }
            let writer = s.spawn(move || {
                let mut smr = local(0);
                for _ in 0..UPDATES {
                    pair.update(&mut smr, |&(first, _)| (first + 1, 2 * (first + 1)));
                }
                done.store(true, Ordering::Relaxed);
                smr.counts()
            });
            counts = writer.join().unwrap();
        });
        assert_eq!(pair.read(&mut local(0), |&pair| pair), (UPDATES, 2 * UPDATES));
        assert_eq!(counts.retired, UPDATES);
        counts
    }

    #[test]
    fn readers_see_whole_values_while_a_writer_replaces_them_with_epochs() {
        let mut epochs = Epochs::new();
        let counts = one_writer_three_readers(|thread| epochs.local(thread));
        assert_eq!(counts.freed + epochs.free_orphans(), counts.retired);
    }

    #[test]
    fn readers_see_whole_values_while_a_writer_replaces_them_with_hazard_pointers() {
        let mut hazards = Hazards::new();
        let counts = one_writer_three_readers(|thread| hazards.local(thread));
        assert_eq!(counts.freed + hazards.free_orphans(), counts.retired);
    }
}
