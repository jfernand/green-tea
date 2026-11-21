//! A task is an asymmetric coroutine: someone `resume`s it, it runs until it calls
//! `yield_now` or finishes, and control returns to whoever resumed it.

use crate::arch::{Context, swap_context};
use crate::stack::Stack;
use std::any::Any;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::ptr::{self, NonNull};

const STACK_SIZE: usize = 32 * 1024;

thread_local! {
    /// The task running on this thread right now, or null when we're on the thread's own stack.
    static CURRENT: Cell<*mut Inner> = const { Cell::new(ptr::null_mut()) };
}

/// An owned handle to a task. Dropping it frees the task's stack, unless the task is suspended
/// partway through its closure: then the stack is leaked (see `Drop`).
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
    panic: Option<Box<dyn Any + Send>>, // a panic on its way to whoever resumed the task
    _stack: Stack,
}

impl Task {
    pub fn new(func: impl FnOnce() + 'static) -> Task {
        let inner = Box::into_raw(Box::new(Inner {
            context: Context::default(),
            caller: Context::default(),
            func: Some(Box::new(func)),
            done: false,
            panic: None,
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
    ///
    /// # Panics
    /// If the task has already finished, and with the task's own panic if it panics.
    pub fn resume(&mut self) {
        let inner = self.inner.as_ptr();
        assert!(!self.is_done(), "resumed a task that has already finished");
        let previous = CURRENT.replace(inner);
        unsafe { swap_context(&raw mut (*inner).caller, &raw const (*inner).context) };
        CURRENT.set(previous);
        if let Some(payload) = unsafe { (*inner).panic.take() } {
            resume_unwind(payload);
        }
    }
}

impl Drop for Task {
    /// A task dropped while suspended mid-closure never gets to run its locals' destructors,
    /// so whatever they guard stays in force: a `thread::scope` in one of its frames may still
    /// have threads using that frame. Its state and stack are therefore leaked, not freed.
    fn drop(&mut self) {
        let inner = self.inner.as_ptr();
        let suspended = unsafe { (*inner).func.is_none() && !(*inner).done };
        if !suspended {
            drop(unsafe { Box::from_raw(inner) });
        }
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

/// Where every task starts, on its own stack: run the closure, mark the task done (a panic
/// finishes it too), and switch back for the last time.
extern "C" fn task_entry(arg: *mut ()) -> ! {
    let inner: *mut Inner = arg.cast();
    unsafe {
        if let Some(func) = (*inner).func.take() {
            // There is no frame to unwind into below this one: the stack ends here. So stop a
            // panic at this point, and let `resume` continue it on the resumer's stack.
            (*inner).panic = catch_unwind(AssertUnwindSafe(func)).err();
        }
        (*inner).done = true;
        swap_context(&raw mut (*inner).context, &raw const (*inner).caller);
    }
    unreachable!("a finished task was resumed")
}

#[cfg(test)]
mod tests {
    use super::{Task, yield_now};
    use core::arch::asm;
    use libc::{c_int, c_void};
    use std::cell::{Cell, RefCell};
    use std::panic::{AssertUnwindSafe, catch_unwind};
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

    /// Whether the page holding `addr` is mapped. Asks the kernel, because reading an unmapped
    /// address would kill the whole test process.
    fn is_mapped(addr: usize) -> bool {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let mut resident = 0;
        unsafe { libc::mincore((addr & !(page - 1)) as *mut c_void, 1, &mut resident) == 0 }
    }

    /// Safe code can lend a task's stack to a scoped thread and then drop the suspended task
    /// from the outside, so the frame must stay mapped and intact.
    #[test]
    fn dropping_a_suspended_task_keeps_its_stack() {
        let local_addr = Rc::new(Cell::new(0usize));
        let addr = local_addr.clone();
        let mut task = Task::new(move || {
            let local = std::hint::black_box(7u64);
            addr.set(&raw const local as usize);
            yield_now();
            std::hint::black_box(&local);
        });
        task.resume();
        let addr = local_addr.get();
        drop(task);
        assert!(is_mapped(addr), "the suspended task's stack was unmapped");
        assert_eq!(unsafe { (addr as *const u64).read_volatile() }, 7);
    }

    /// A panic in a task reaches whoever resumed it, after running the task's destructors.
    #[test]
    fn a_panic_in_a_task_unwinds_to_the_resumer() {
        struct SetOnDrop(Rc<Cell<bool>>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let dropped = Rc::new(Cell::new(false));
        let flag = SetOnDrop(dropped.clone());
        let mut task = Task::new(move || {
            let _flag = flag;
            panic!("boom");
        });
        let payload = catch_unwind(AssertUnwindSafe(|| task.resume())).unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"boom"));
        assert!(dropped.get(), "the task's destructors did not run");
        assert!(task.is_done());
    }

    /// The floating-point control state: the rounding mode and friends, without the sticky
    /// status flags, which ordinary arithmetic sets.
    fn fp_control() -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            let (mut mxcsr, mut cw) = (0u32, 0u16);
            unsafe {
                asm!("stmxcsr [{}]", in(reg) &raw mut mxcsr);
                asm!("fnstcw [{}]", in(reg) &raw mut cw);
            }
            u64::from(mxcsr & !0x3F) | u64::from(cw) << 32
        }
        #[cfg(target_arch = "aarch64")]
        {
            let fpcr: u64;
            unsafe { asm!("mrs {}, fpcr", out(reg) fpcr) };
            fpcr
        }
        #[cfg(target_arch = "riscv64")]
        {
            let frm: u64;
            unsafe { asm!(".option push", ".option arch, +d", "frrm {}", ".option pop", out(reg) frm) };
            frm
        }
    }

    /// Round toward zero, in every floating-point unit the architecture has.
    fn round_toward_zero() {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let (mut mxcsr, mut cw) = (0u32, 0u16);
            asm!("stmxcsr [{}]", in(reg) &raw mut mxcsr);
            asm!("fnstcw [{}]", in(reg) &raw mut cw);
            mxcsr |= 0x6000;
            cw |= 0x0C00;
            asm!("ldmxcsr [{}]", in(reg) &raw const mxcsr);
            asm!("fldcw [{}]", in(reg) &raw const cw);
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            let fpcr: u64;
            asm!("mrs {}, fpcr", out(reg) fpcr);
            asm!("msr fpcr, {}", in(reg) fpcr | 3 << 22);
        }
        #[cfg(target_arch = "riscv64")]
        unsafe {
            asm!(".option push", ".option arch, +d", "fsrmi 1", ".option pop");
        }
    }

    /// Each task has its own rounding mode, and so does its resumer. On a thread of its own,
    /// so a failure can't leave the harness's thread rounding the wrong way.
    #[test]
    fn fp_control_state_is_per_task() {
        std::thread::spawn(|| {
            let resumer = fp_control();
            let in_task = Rc::new(Cell::new(0));
            let seen = in_task.clone();
            let mut task = Task::new(move || {
                round_toward_zero();
                seen.set(fp_control());
                yield_now();
                seen.set(fp_control());
            });
            task.resume();
            let changed = in_task.get();
            assert_ne!(changed, resumer, "the task could not change its rounding mode");
            assert_eq!(fp_control(), resumer, "the task's rounding mode leaked into its resumer");
            task.resume();
            assert_eq!(in_task.get(), changed, "the task lost its rounding mode across a yield");
            assert_eq!(fp_control(), resumer);
        })
        .join()
        .unwrap();
    }

    /// A new task starts in the default mode, whatever its creator has set.
    #[test]
    fn a_fresh_task_has_the_default_fp_control_state() {
        std::thread::spawn(|| {
            let default = fp_control();
            round_toward_zero();
            let in_task = Rc::new(Cell::new(0));
            let seen = in_task.clone();
            let mut task = Task::new(move || seen.set(fp_control()));
            task.resume();
            assert_eq!(in_task.get(), default);
        })
        .join()
        .unwrap();
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
