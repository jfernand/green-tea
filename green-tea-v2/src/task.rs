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
    use libc::{c_int, c_void};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::{mem, ptr};

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

    /// Eight f64s that stay live across every `pause()`. A value live across a call has to
    /// sit in a callee-saved register (d8–d15 on aarch64, fs0–fs11 on riscv64) or on the
    /// stack; with optimizations the compiler picks the registers. `pause` is a function
    /// pointer so the call can't be inlined away.
    #[inline(never)]
    fn float_work(seed: f64, rounds: u32, pause: fn()) -> f64 {
        let (mut a, mut b, mut c, mut d) = (seed, seed * 2.0, seed * 3.0, seed * 4.0);
        let (mut e, mut f, mut g, mut h) = (seed * 5.0, seed * 6.0, seed * 7.0, seed * 8.0);
        for _ in 0..rounds {
            pause();
            (a, b, c, d) = (a * 0.5 + b, b * 0.75 + c, c * 0.5 + d, d * 0.25 + e);
            (e, f, g, h) = (e * 0.5 + f, f * 0.75 + g, g * 0.5 + h, h * 0.25 + a);
        }
        a + b + c + d + e + f + g + h
    }

    /// Two tasks interleave float work. If `swap_context` didn't save the callee-saved float
    /// registers, each task would resume with the other's values. Only meaningful in release:
    /// unoptimized code keeps everything on the stack anyway.
    #[test_log::test]
    fn float_registers_survive_switches() {
        let rounds = 50;
        let expected = |seed| float_work(seed, rounds, || {});
        let results = Rc::new(RefCell::new(Vec::new()));
        let mut tasks: Vec<Task> = [1.0, -3.0]
            .into_iter()
            .map(|seed| {
                let r = results.clone();
                Task::new(move || {
                    let got = float_work(seed, rounds, yield_now);
                    r.borrow_mut().push((seed, got));
                })
            })
            .collect();
        while tasks.iter().any(|t| !t.is_done()) {
            for task in tasks.iter_mut().filter(|t| !t.is_done()) {
                task.resume();
            }
        }
        let results = results.borrow();
        assert_eq!(results.len(), 2);
        for &(seed, got) in results.iter() {
            assert_eq!(got.to_bits(), expected(seed).to_bits(), "seed {seed}");
        }
    }

    #[test]
    #[should_panic(expected = "yield_now called outside a task")]
    fn yield_outside_a_task_panics() {
        yield_now();
    }

    /// Recurse with a 256-byte frame (well under a page, so it can't skip the guard page)
    /// that stays live across the call, so the recursion can't become a loop.
    #[inline(never)]
    fn recurse(depth: u64) -> u64 {
        let frame = std::hint::black_box([depth as u8; 256]);
        if depth == u64::MAX {
            return 0;
        }
        recurse(depth + 1) + u64::from(frame[0])
    }

    // Exit codes for the overflowing child.
    const FAULT_IN_GUARD: i32 = 42;
    const FAULT_ELSEWHERE: i32 = 43;
    const NO_FAULT: i32 = 44;

    /// The guard page of the task the child overflows, for the signal handler.
    static GUARD_START: AtomicUsize = AtomicUsize::new(0);
    static GUARD_END: AtomicUsize = AtomicUsize::new(0);

    /// Runs on the alternate signal stack (the task's own stack is the one that's full).
    /// Only async-signal-safe calls in here: atomics and `_exit`.
    extern "C" fn on_fault(_signal: c_int, info: *mut libc::siginfo_t, _ctx: *mut c_void) {
        let addr = unsafe { (*info).si_addr() } as usize;
        let guard = GUARD_START.load(Relaxed)..GUARD_END.load(Relaxed);
        unsafe {
            libc::_exit(if guard.contains(&addr) { FAULT_IN_GUARD } else { FAULT_ELSEWHERE })
        };
    }

    /// A task that overflows its stack must fault on its guard page, not just somewhere.
    /// The overflow ends the process, so it happens in a forked child whose fault handler
    /// reports, through its exit code, whether the faulting address was in the guard page.
    ///
    /// Everything that allocates (the task, its stack, the signal stack) happens before
    /// the fork: the child of a multi-threaded process (the test harness) must not
    /// allocate, since another thread may have held the allocator's lock at the fork.
    #[test]
    fn overflow_hits_the_guard_page() {
        let mut task = Task::new(|| {
            std::hint::black_box(recurse(0));
        });
        let guard = unsafe { (*task.inner.as_ptr())._stack.guard() };
        GUARD_START.store(guard.start, Relaxed);
        GUARD_END.store(guard.end, Relaxed);
        let mut alt_stack = vec![0u8; 64 * 1024];

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", std::io::Error::last_os_error());
        if pid == 0 {
            unsafe {
                let ss = libc::stack_t {
                    ss_sp: alt_stack.as_mut_ptr().cast(),
                    ss_flags: 0,
                    ss_size: alt_stack.len(),
                };
                libc::sigaltstack(&ss, ptr::null_mut());
                let mut action: libc::sigaction = mem::zeroed();
                action.sa_sigaction = on_fault as extern "C" fn(_, _, _) as usize;
                action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                // Linux raises SIGSEGV for a PROT_NONE page; macOS raises SIGBUS.
                libc::sigaction(libc::SIGSEGV, &action, ptr::null_mut());
                libc::sigaction(libc::SIGBUS, &action, ptr::null_mut());
            }
            task.resume();
            unsafe { libc::_exit(NO_FAULT) };
        }

        let mut status = 0;
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(rc, pid, "waitpid failed: {}", std::io::Error::last_os_error());
        assert!(libc::WIFEXITED(status), "the child died unexpectedly (status {status:#x})");
        match libc::WEXITSTATUS(status) {
            FAULT_IN_GUARD => {}
            FAULT_ELSEWHERE => panic!("the overflow faulted outside the guard page"),
            NO_FAULT => panic!("the overflow went unnoticed"),
            other => panic!("the child exited with {other}"),
        }
        drop(task); // only the child ran it; here it's still suspended at its start
    }
}
