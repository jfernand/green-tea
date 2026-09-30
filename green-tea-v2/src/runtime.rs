//! Scheduling policy: a cooperative round-robin run queue on top of `Task`.

use crate::task::Task;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ptr;

struct Runtime {
    queue: RefCell<VecDeque<Task>>, // runnable tasks, in the order they'll next run
}

thread_local! {
    /// The runtime driving this thread, or null outside `run`. Only ever turned into a
    /// shared `&Runtime`; all mutation goes through the `RefCell`, whose borrows are
    /// always released before switching to a task.
    static RUNTIME: Cell<*const Runtime> = const { Cell::new(ptr::null()) };
}

/// Clears `RUNTIME` when `run` returns, even by panicking.
struct Installed;

impl Drop for Installed {
    fn drop(&mut self) {
        RUNTIME.set(ptr::null());
    }
}

/// Run `main` as the first task, and keep scheduling until every task has finished.
///
/// # Panics
/// If called from inside another `run` on the same thread.
pub fn run(main: impl FnOnce() + 'static) {
    let runtime = Runtime { queue: RefCell::new(VecDeque::new()) };
    assert!(RUNTIME.get().is_null(), "run() called inside run()");
    RUNTIME.set(&runtime);
    let _installed = Installed;

    spawn(main);
    loop {
        // A separate statement, so the queue borrow ends before the task runs: the task
        // may call `spawn`, which borrows the queue again.
        let next = runtime.queue.borrow_mut().pop_front();
        let Some(mut task) = next else { break };
        task.resume();
        if !task.is_done() {
            runtime.queue.borrow_mut().push_back(task);
        } // else: dropped here, which unmaps its stack
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

#[cfg(test)]
mod tests {
    use super::{run, spawn};
    use crate::yield_now;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test_log::test]
    fn round_robin() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        run(move || {
            for name in ["a", "b"] {
                let l = l.clone();
                spawn(move || {
                    for i in 1..=2 {
                        l.borrow_mut().push(format!("{name}{i}"));
                        yield_now();
                    }
                });
            }
        });
        assert_eq!(*log.borrow(), ["a1", "b1", "a2", "b2"]);
    }

    #[test_log::test]
    fn tasks_can_spawn_tasks() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        run(move || {
            let l2 = l.clone();
            spawn(move || {
                l2.borrow_mut().push("child");
            });
            l.borrow_mut().push("parent");
        });
        assert_eq!(*log.borrow(), ["parent", "child"]);
    }

    #[test]
    #[should_panic(expected = "spawn called outside run()")]
    fn spawn_outside_run_panics() {
        spawn(|| {});
    }
}
