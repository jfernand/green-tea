//! What replaces `thread_local!` here: the kernel uses one hart for its global state, and
//! an atomic borrow flag rejects concurrent or reentrant access.
//!
//! A kernel that starts the board's other CPUs (`cpus`) keeps that true by rule: every
//! `Global` belongs to the boot CPU, and code run on a started CPU must not touch one.
//!
//! With interrupts on (green-tea-preempt) a handler can run between any two instructions,
//! so a `Global` that both a handler and the interrupted code use must be accessed with
//! interrupts off (`TimerInterrupt::without_interrupts`). The borrow flag rejects access
//! from a handler that interrupts a scoped mutable borrow.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

pub struct Global<T> {
    value: UnsafeCell<T>,
    borrowed: AtomicBool,
}

unsafe trait GlobalValue {}

unsafe impl<T: Send> GlobalValue for T {}

// SAFETY: the atomic borrow flag gives one thread exclusive access at a time. `T: Send`
// allows protected values to move between threads.
unsafe impl<T: GlobalValue> Sync for Global<T> {}

/// A pointer value for a global slot. It does not own or validate the pointee, and does
/// not make it safe to access.
pub struct GlobalPtr<T>(*mut T);

impl<T> Copy for GlobalPtr<T> {}

impl<T> Clone for GlobalPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

// SAFETY: this wrapper contains only an address; dereferencing it remains unsafe and must
// follow the pointee's own sharing and lifetime requirements.
unsafe impl<T> Send for GlobalPtr<T> {}

impl<T> GlobalPtr<T> {
    /// A null pointer value.
    pub const fn null() -> Self {
        GlobalPtr(core::ptr::null_mut())
    }

    /// Store a raw pointer value without taking ownership of its pointee.
    pub const fn new(pointer: *mut T) -> Self {
        GlobalPtr(pointer)
    }

    /// Whether the stored pointer is null.
    pub fn is_null(self) -> bool {
        self.0.is_null()
    }

    /// Return the raw pointer. Its validity and access requirements remain the caller's
    /// responsibility.
    pub fn as_ptr(self) -> *mut T {
        self.0
    }
}

impl<T> Global<T> {
    pub const fn new(value: T) -> Global<T> {
        Global { value: UnsafeCell::new(value), borrowed: AtomicBool::new(false) }
    }

    fn borrow(&self) -> Borrowed<'_> {
        assert!(
            self.borrowed.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok(),
            "a Global was borrowed reentrantly or concurrently"
        );
        Borrowed(&self.borrowed)
    }
}

impl<T: Copy> Global<T> {
    pub fn get(&self) -> T {
        let _borrow = self.borrow();
        // SAFETY: the borrow flag grants exclusive access.
        unsafe { *self.value.get() }
    }

    pub fn set(&self, value: T) {
        let _borrow = self.borrow();
        // SAFETY: the borrow flag grants exclusive access.
        unsafe { *self.value.get() = value };
    }

    pub fn replace(&self, value: T) -> T {
        let _borrow = self.borrow();
        // SAFETY: the borrow flag grants exclusive access.
        unsafe { self.value.get().replace(value) }
    }
}

impl<T> Global<T> {
    /// Access the value in place. The borrow cannot escape this closure.
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let _borrow = self.borrow();
        // SAFETY: the borrow flag grants this closure exclusive access.
        f(unsafe { &mut *self.value.get() })
    }
}

struct Borrowed<'a>(&'a AtomicBool);

impl Drop for Borrowed<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
