//! Something a task can wait for, set from an interrupt handler.
//!
//! The flag stays set until the waiter clears it, so a signal that arrives before the task
//! gets around to waiting isn't lost. A task clears it first, then looks for its work, and
//! waits only if there was none: a signal at any point after the clear ends the wait.

use core::sync::atomic::{AtomicBool, Ordering};

pub struct Event(AtomicBool);

impl Event {
    pub const fn new() -> Event {
        Event(AtomicBool::new(false))
    }

    pub fn signal(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn clear(&self) {
        self.0.store(false, Ordering::Release);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for Event {
    fn default() -> Event {
        Event::new()
    }
}
