//! A task is an asymmetric coroutine: someone `resume`s it, it runs until it calls
//! `yield_now` or finishes, and control returns to whoever resumed it.

use crate::arch::{Context, swap_context};
use crate::stack::Stack;
use std::cell::Cell;
use std::ptr::{self, NonNull};

const STACK_SIZE: usize = 32 * 1024;

thread_local! {
    /// The task running on this thread right now, or null when we're on the thread's own stack.
    static CURRENT: Cell<*mut Inner> = const { Cell::new(ptr::null_mut()) };
}

/// An owned handle to a task. Dropping it frees the task's stack.
///
/// The task's state lives in a separate heap allocation (`Inner`) that is only ever touched
/// through raw pointers. That way `resume(&mut self)` borrows just this handle, while the
/// task's own code, running inside that call, reaches its state through `CURRENT` without
/// aliasing a live `&mut`.
pub struct Task {
    inner: NonNull<Inner>,
}

struct Inner {
    context: Context, // the task's registers while it's suspended
    caller: Context,  // whoever resumed it; yield_now and finishing switch back here
    func: Option<Box<dyn FnOnce()>>,
    done: bool,
    _stack: Stack,
}

impl Task {
    pub fn new(func: impl FnOnce() + 'static) -> Task {
        let inner = Box::into_raw(Box::new(Inner {
            context: Context::default(),
            caller: Context::default(),
            func: Some(Box::new(func)),
            done: false,
            _stack: Stack::new(STACK_SIZE),
        }));
        unsafe {
            (*inner).context = Context::new((*inner)._stack.top(), task_entry, inner.cast());
            Task { inner: NonNull::new_unchecked(inner) }
        }
    }

    pub fn is_done(&self) -> bool {
        unsafe { (*self.inner.as_ptr()).done }
    }

    /// Run the task until it yields or finishes.
    ///
    /// Tasks can resume other tasks: `CURRENT` is saved and restored around the switch.
    pub fn resume(&mut self) {
        let inner = self.inner.as_ptr();
        assert!(!self.is_done(), "resumed a task that has already finished");
        let previous = CURRENT.replace(inner);
        unsafe { swap_context(&raw mut (*inner).caller, &raw const (*inner).context) };
        CURRENT.set(previous);
    }
}

impl Drop for Task {
    /// A task dropped while suspended mid-closure never gets to run its locals' destructors;
    /// its stack is simply unmapped.
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
            func(); // a panic here can't unwind past this extern "C" frame, so it aborts
        }
        (*inner).done = true;
        swap_context(&raw mut (*inner).context, &raw const (*inner).caller);
    }
    unreachable!("a finished task was resumed")
}

#[cfg(test)]
mod tests {
    use super::{Task, yield_now};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test_log::test]
    fn runs_to_completion() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        let mut task = Task::new(move || l.borrow_mut().push(1));
        task.resume();
        assert!(task.is_done());
        assert_eq!(*log.borrow(), [1]);
    }

    #[test_log::test]
    fn yields_back_to_the_resumer() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        let mut task = Task::new(move || {
            for i in 1..=3 {
                l.borrow_mut().push(i);
                yield_now();
            }
            l.borrow_mut().push(100);
        });
        while !task.is_done() {
            task.resume();
        }
        assert_eq!(*log.borrow(), [1, 2, 3, 100]);
    }

    #[test_log::test]
    fn a_task_can_resume_another() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        let mut outer = Task::new(move || {
            let li = l.clone();
            let mut inner = Task::new(move || {
                li.borrow_mut().push("inner 1");
                yield_now(); // back to `outer`, not to the test
                li.borrow_mut().push("inner 2");
            });
            inner.resume();
            l.borrow_mut().push("outer");
            yield_now(); // back to the test
            inner.resume();
        });
        outer.resume();
        log.borrow_mut().push("test");
        outer.resume();
        assert!(outer.is_done());
        assert_eq!(*log.borrow(), ["inner 1", "outer", "test", "inner 2"]);
    }

    #[test]
    #[should_panic(expected = "yield_now called outside a task")]
    fn yield_outside_a_task_panics() {
        yield_now();
    }
}
