//! What replaces `thread_local!` here: there is one hart and interrupts stay off, so a
//! plain `Cell` in a static is never touched by two threads of execution at once. Rust
//! can't see that, hence the `unsafe impl Sync`.

use core::cell::Cell;

pub struct Global<T>(Cell<T>);

// SAFETY: single hart, no interrupt handlers, so only one thing ever runs at a time.
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
