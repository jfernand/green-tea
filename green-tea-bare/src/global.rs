//! What replaces `thread_local!` here: there is one hart, so a plain `Cell` in a static is
//! never touched by two CPUs at once. Rust can't see that, hence the `unsafe impl Sync`.
//!
//! A kernel that starts the board's other CPUs (`cpus`) keeps that true by rule: every
//! `Global` belongs to the boot CPU, and code run on a started CPU must not touch one.
//!
//! With interrupts on (green-tea-preempt) a handler can run between any two instructions,
//! so a `Global` that both a handler and the interrupted code use must be updated with
//! interrupts off (`TimerInterrupt::without_interrupts`). A single `get` or `set` of a
//! word-sized value is one load or store, and is safe either way.

use core::cell::Cell;

pub struct Global<T>(Cell<T>);

// SAFETY: only the boot hart uses a `Global`, so nothing runs in parallel; interrupts are
// dealt with above.
unsafe impl<T> Sync for Global<T> {}

impl<T: Copy> Global<T> {
    pub const fn new(value: T) -> Global<T> {
        Global(Cell::new(value))
    }

    pub fn get(&self) -> T {
        self.0.get()
    }

    pub fn set(&self, value: T) {
        self.0.set(value)
    }

    pub fn replace(&self, value: T) -> T {
        self.0.replace(value)
    }
}
