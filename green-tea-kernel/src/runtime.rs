//! A port of green-tea-v2/src/runtime.rs: a cooperative round-robin run queue on top of
//! `Task`. The differences: `RUNTIME` is a [`Global`] instead of a `thread_local!`, and
//! `run` takes a hook that sees each task just before it's resumed, which the demo uses
//! to draw the switches.

use crate::global::Global;
use crate::task::Task;
use alloc::collections::VecDeque;
use core::cell::RefCell;
use core::ptr;

struct Runtime {
    queue: RefCell<VecDeque<Task>>, // runnable tasks, in the order they'll next run
}

/// The runtime in charge, or null outside `run`.
static RUNTIME: Global<*const Runtime> = Global::new(ptr::null());

/// Run `main` as the first task, and keep scheduling until every task has finished.
/// `before_resume` is called with each task's id just before it's resumed.
///
/// # Panics
/// If called from inside another `run`.
pub fn run(main: impl FnOnce() + 'static, mut before_resume: impl FnMut(usize)) {
    let runtime = Runtime { queue: RefCell::new(VecDeque::new()) };
    assert!(RUNTIME.get().is_null(), "run() called inside run()");
    RUNTIME.set(&runtime);

    spawn(main);
    loop {
        // A separate statement, so the queue borrow ends before the task runs: the task
        // may call `spawn`, which borrows the queue again.
        let next = runtime.queue.borrow_mut().pop_front();
        let Some(mut task) = next else { break };
        before_resume(task.id());
        task.resume();
        if !task.is_done() {
            runtime.queue.borrow_mut().push_back(task);
        }
    }
    RUNTIME.set(ptr::null()); // no unwinding here (panics halt), so no drop guard needed
}

/// Add a task to the back of the run queue. Works from `run`'s `main` and from any task.
///
/// # Panics
/// If called outside `run`.
pub fn spawn(func: impl FnOnce() + 'static) {
    let runtime = RUNTIME.get();
    assert!(!runtime.is_null(), "spawn called outside run()");
    let task = Task::new(func);
    unsafe { &*runtime }.queue.borrow_mut().push_back(task);
}
