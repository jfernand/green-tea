//! A port of green-tea-v2/src/task.rs to bare metal. A task is an asymmetric coroutine:
//! someone `resume`s it, it runs until it calls `yield_now` or finishes, and control
//! returns to whoever resumed it.
//!
//! What changed from v2:
//! - `CURRENT` is a [`Global`] instead of a `thread_local!`: there is one hart.
//! - The stack is a heap `Box<[u8]>` instead of an mmap with a guard page. Without paging
//!   there's nothing to make a page fault, so an overflow silently runs into whatever
//!   sits below the stack.
//! - Each task has an `id`, so the demo can tell which one is about to run.

use crate::arch::{Context, swap_context};
use crate::global::Global;
use alloc::boxed::Box;
use alloc::vec;
use core::ptr::{self, NonNull};

const STACK_SIZE: usize = 16 * 1024;

/// The task running right now, or null when we're on the boot stack.
static CURRENT: Global<*mut Inner> = Global::new(ptr::null_mut());

/// Ids handed out in creation order, starting at 0.
static NEXT_ID: Global<usize> = Global::new(0);

/// An owned handle to a task. Dropping it frees the task's stack (a no-op with the bump
/// allocator, but the ownership is the same as in v2).
pub struct Task {
    inner: NonNull<Inner>,
}

struct Inner {
    context: Context, // the task's registers while it's suspended
    caller: Context,  // whoever resumed it; yield_now and finishing switch back here
    func: Option<Box<dyn FnOnce()>>,
    done: bool,
    id: usize,
    _stack: Box<[u8]>,
}

impl Task {
    pub fn new(func: impl FnOnce() + 'static) -> Task {
        let id = NEXT_ID.replace(NEXT_ID.get() + 1);
        let mut stack = vec![0u8; STACK_SIZE].into_boxed_slice();
        let top = stack.as_mut_ptr_range().end; // Context::new rounds it down to 16 bytes
        let inner = Box::into_raw(Box::new(Inner {
            context: Context::default(),
            caller: Context::default(),
            func: Some(Box::new(func)),
            done: false,
            id,
            _stack: stack, // moving the Box doesn't move the bytes `top` points into
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

/// Suspend the current task and switch back to whoever resumed it.
///
/// # Panics
/// If called outside a task.
pub fn yield_now() {
    let inner = CURRENT.get();
    assert!(!inner.is_null(), "yield_now called outside a task");
    unsafe { swap_context(&raw mut (*inner).context, &raw const (*inner).caller) };
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
