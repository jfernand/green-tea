//! A port of green-tea-v2/src/task.rs to bare metal. A task is an asymmetric coroutine:
//! someone `resume`s it, it runs until it calls `yield_now` or finishes, and control
//! returns to whoever resumed it.
//!
//! What changed from v2:
//! - `CURRENT` is a [`Global`] instead of a `thread_local!`: there is one hart.
//! - The stack comes from green-tea-bare's pool instead of an mmap. It has a guard page
//!   below it, as in v2, but only boards with paging make touching it fault (see
//!   [`stack`](crate::stack)).
//! - Each task has an `id`, so the demo can tell which one is about to run.
//!
//! Nothing here knows about interrupts. green-tea-preempt calls `yield_now` from its timer
//! handler, and keeps interrupts off whenever a switch is under way.

use crate::global::Global;
use crate::platform::{Board, Platform};
use crate::stack::Stack;
use alloc::boxed::Box;
use core::ops::Range;
use core::ptr::{self, NonNull};
use green_tea_arch::{Context, swap_context};

/// The task running right now, or null when we're on the boot stack.
static CURRENT: Global<*mut Inner> = Global::new(ptr::null_mut());

/// Ids handed out in creation order, starting at 0.
static NEXT_ID: Global<usize> = Global::new(0);

/// An owned handle to a task. Dropping it gives the task's stack back to the pool.
pub struct Task {
    inner: NonNull<Inner>,
}

struct Inner {
    context: Context, // the task's registers while it's suspended
    caller: Context,  // whoever resumed it; yield_now and finishing switch back here
    func: Option<Box<dyn FnOnce()>>,
    done: bool,
    id: usize,
    /// Set by `sleep_until` just before the task yields: don't run it again before this.
    wake_at: Option<u64>,
    stack: Stack,
}

impl Task {
    pub fn new(func: impl FnOnce() + 'static) -> Task {
        let id = NEXT_ID.replace(NEXT_ID.get() + 1);
        let stack = Stack::take();
        let top = stack.top(); // Context::new rounds it down to 16 bytes
        let inner = Box::into_raw(Box::new(Inner {
            context: Context::default(),
            caller: Context::default(),
            func: Some(Box::new(func)),
            done: false,
            id,
            wake_at: None,
            stack,
        }));
        unsafe {
            (*inner).context = Context::new(top, task_entry, inner.cast());
            Task { inner: NonNull::new_unchecked(inner) }
        }
    }

    pub fn id(&self) -> usize {
        unsafe { (*self.inner.as_ptr()).id }
    }

    pub fn is_done(&self) -> bool {
        unsafe { (*self.inner.as_ptr()).done }
    }

    /// If the task went to sleep when it last yielded, until when. Taking it clears it.
    pub fn take_wake_at(&mut self) -> Option<u64> {
        unsafe { (*self.inner.as_ptr()).wake_at.take() }
    }

    /// Run the task until it yields or finishes.
    pub fn resume(&mut self) {
        let inner = self.inner.as_ptr();
        assert!(!self.is_done(), "resumed a task that has already finished");
        let previous = CURRENT.replace(inner);
        unsafe { swap_context(&raw mut (*inner).caller, &raw const (*inner).context) };
        CURRENT.set(previous);
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        drop(unsafe { Box::from_raw(self.inner.as_ptr()) });
    }
}

/// Whether a task is running, as opposed to the code on the boot stack (`kmain`, the
/// scheduler).
pub fn in_task() -> bool {
    !CURRENT.get().is_null()
}

/// Suspend the current task and switch back to whoever resumed it.
///
/// # Panics
/// If called outside a task.
pub fn yield_now() {
    let inner = CURRENT.get();
    assert!(!inner.is_null(), "yield_now called outside a task");
    unsafe { swap_context(&raw mut (*inner).context, &raw const (*inner).caller) };
}

/// The guard page under the current task's stack.
///
/// # Panics
/// If called outside a task.
pub fn current_guard() -> Range<usize> {
    let inner = CURRENT.get();
    assert!(!inner.is_null(), "current_guard called outside a task");
    unsafe { (*inner).stack.guard() }
}

/// Suspend the current task, and tell the scheduler not to resume it before `ticks()`
/// reaches `deadline`.
///
/// # Panics
/// If called outside a task.
pub fn sleep_until(deadline: u64) {
    let inner = CURRENT.get();
    assert!(!inner.is_null(), "sleep_until called outside a task");
    unsafe { (*inner).wake_at = Some(deadline) };
    yield_now();
}

/// `sleep_until`, `ms` milliseconds from now.
pub fn sleep_ms(ms: u64) {
    sleep_until(Board::ticks() + ms * Board::ticks_per_ms());
}

/// Where every task starts, on its own stack: run the closure, mark the task done, and
/// switch back for the last time.
extern "C" fn task_entry(arg: *mut ()) -> ! {
    let inner: *mut Inner = arg.cast();
    unsafe {
        if let Some(func) = (*inner).func.take() {
            func(); // a panic here goes to the panic handler, which halts
        }
        (*inner).done = true;
        swap_context(&raw mut (*inner).context, &raw const (*inner).caller);
    }
    unreachable!("a finished task was resumed")
}
