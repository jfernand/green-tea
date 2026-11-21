//! A port of green-tea-v2/src/runtime.rs: a round-robin run queue on top of `Task`. The
//! differences: `RUNTIME` is a [`Global`] instead of a `thread_local!`; tasks can sleep;
//! and `run` takes a hook that sees each switch, which the demos use to draw them (and
//! green-tea-preempt to start each task's time slice).
//!
//! Sleeping is what makes round robin fair to a task that waits: a task in
//! [`sleep_ms`](crate::task::sleep_ms) is parked off the run queue, so it costs no time
//! slices, and the tasks that do have work share the CPU. A busy-waiting task, in
//! contrast, burns its whole slice every round. When every task is asleep, the scheduler
//! idles until the first one is due.

use crate::global::Global;
use crate::platform::{Board, Platform};
use crate::task::Task;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr;

struct Runtime {
    queue: RefCell<VecDeque<Task>>, // runnable tasks, in the order they'll next run
    sleeping: RefCell<Vec<(u64, Task)>>, // tasks waiting for a deadline, in no order
}

/// The runtime in charge, or null outside `run`.
static RUNTIME: Global<*const Runtime> = Global::new(ptr::null());

/// What the scheduler is about to do, as told to `run`'s hook.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Switch {
    /// Resume the task with this id.
    To(usize),
    /// Nothing is runnable: wait for the next sleeper's deadline.
    Idle,
}

/// Run `main` as the first task, and keep scheduling until every task has finished.
/// `on_switch` is called just before each task is resumed, and before each idle wait.
///
/// # Panics
/// If called from inside another `run`.
pub fn run(main: impl FnOnce() + 'static, mut on_switch: impl FnMut(Switch)) {
    let runtime = Runtime { queue: RefCell::new(VecDeque::new()), sleeping: RefCell::new(Vec::new()) };
    assert!(RUNTIME.get().is_null(), "run() called inside run()");
    RUNTIME.set(&runtime);

    spawn(main);
    loop {
        runtime.wake(Board::ticks());
        // A separate statement, so the queue borrow ends before the task runs: the task
        // may call `spawn`, which borrows the queue again.
        let next = runtime.queue.borrow_mut().pop_front();
        let Some(mut task) = next else {
            let first_due = runtime.sleeping.borrow().iter().map(|&(due, _)| due).min();
            let Some(due) = first_due else { break }; // nothing runnable or asleep: done
            on_switch(Switch::Idle);
            while Board::ticks() < due {}
            continue;
        };
        on_switch(Switch::To(task.id()));
        task.resume();
        if task.is_done() {
            continue;
        }
        match task.take_wake_at() {
            Some(due) => runtime.sleeping.borrow_mut().push((due, task)),
            None => runtime.queue.borrow_mut().push_back(task),
        }
    }
    crate::task::reap(); // every task has finished: their records' stacks go back to the pool
    RUNTIME.set(ptr::null()); // no unwinding here (panics halt), so no drop guard needed
}

impl Runtime {
    /// Move every sleeper that's due to the back of the run queue.
    fn wake(&self, now: u64) {
        let mut sleeping = self.sleeping.borrow_mut();
        let mut queue = self.queue.borrow_mut();
        let mut i = 0;
        while i < sleeping.len() {
            if sleeping[i].0 <= now {
                queue.push_back(sleeping.swap_remove(i).1);
            } else {
                i += 1;
            }
        }
    }
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
