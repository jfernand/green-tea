#import "../lib.typ": *

= green-tea-v2 <crate-v2>

`green-tea-v2` runs cooperative tasks on x86_64, aarch64 and riscv64 under Linux, and its library also builds for macOS on Apple Silicon. It has three layers. @crate-arch saves and loads registers. `Task` is an asymmetric coroutine: whoever resumes it gets control back when it yields. `run` and `spawn` are a round-robin scheduler on top. The crate has no `static mut`, and its state is per thread.

#spec(
  ("Source", [#repo("green-tea-v2/src/"), 278 lines without their tests, and two demos]),
  ("Depends on", [`green-tea-arch`, `libc`, `tracing`, `tracing-subscriber`; `test-log` for the tests]),
  ("Used by", [nothing links it; `green-tea-bare` ports its `Task` and scheduler (@crate-bare)]),
  ("Tests", [18 host tests, run on three architectures in two profiles by `cargo xtask test`; the `coop-2-v2` and `coop-n-v2` binaries]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`run(main)`], [Runs `main` as the first task, and schedules until every task has finished, or one panics],
  [`spawn(func)`], [Adds a task to the back of the queue of the current `run`],
  [`yield_now()`], [Suspends the current task and returns to whoever resumed it],
  [`raw::Task`], [An owned coroutine: `Task::new(func)`, `resume`, `is_done`. Dropping it frees its stack, unless it is suspended partway through its closure],
  [`raw::Context`, `raw::Entry`, `raw::swap_context`], [Re-exported from @crate-arch],
  [`raw::yield_now`], [The same function as `yield_now`],
)

== What changed from v1

#dtable(columns: (auto, 1fr, 1fr), ("", "green-tea-v1", "green-tea-v2"),
  [Architectures], [x86_64], [x86_64, aarch64, riscv64],
  [Switch], [its own `context.s`], [`green-tea-arch`, shared with the kernels],
  [Where a yield goes], [always to the global `MAIN`], [to `caller`, a context in each task],
  [Current task], [`static mut CURRENT`], [a `thread_local!` `Cell`],
  [A task drives other tasks], [a nested `run`, which saves and restores the globals], [`resume`, to any depth],
  [`spawn` from a task], [not possible], [works],
  [Scheduler state], [a local of `run`], [a `Runtime` reached through a `thread_local!`],
  [Task state], [a `Box<Task>` also reached by raw pointer], [an `Inner` touched only through raw pointers],
  [Stack size], [as given], [rounded up to whole pages],
  [A panic in a task], [aborts the process], [continues in whoever resumed the task],
)

== `stack`

`Stack` has the layout of the one in @crate-v1, a guard page below the usable pages, and handles failures the same way. `new` also rounds the size up to whole pages, since `mprotect` works in pages, and uses checked arithmetic:

#srcfile("green-tea-v2/src/stack.rs", lines: ((18, 25), (39, 41), (45, 56),), notes: ((23, [A size near `usize::MAX` panics here instead of wrapping around to a small mapping.]), (39, [`mmap` reports failure with `MAP_FAILED`, which is `(void*)-1` and not null.]), (53, [`errno` is read before `munmap` can change it.]),))[```rust
pub(crate) fn new(size: usize) -> Stack {
    let guard = page_size(); // 16 KiB on Apple Silicon
    info!("guard: {}k", guard / 1024);
    // mprotect works on whole pages, so give the usable part whole pages too.
    let size = size
        .checked_next_multiple_of(guard)
        .expect("stack size overflows usize");
    let len = guard.checked_add(size).expect("stack size overflows usize");
// …
    if base == MAP_FAILED {
        panic!("mmap of a {len}-byte stack failed: {}", io::Error::last_os_error());
    }
// …
    let rc = unsafe {
        libc::mprotect(
            base.add(guard) as *mut libc::c_void,
            size,
            PROT_READ | PROT_WRITE,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error(); // before munmap can overwrite errno
        unsafe { libc::munmap(base as *mut libc::c_void, len) };
        panic!("mprotect of a {size}-byte stack failed: {err}");
    }
```]

`top` returns the end of the mapping, which is page-aligned. Under `cfg(test)`, `guard` returns the range of the guard page.

== `task`: an asymmetric coroutine

A coroutine is *asymmetric* when its switches come in two kinds: `resume` enters it and `yield_now` leaves it, always towards the resumer. Each task stores two contexts, its own and the resumer's:

#srcfile("green-tea-v2/src/task.rs", lines: ((25, 36),), notes: ((26, [The handle is one pointer. Moving a `Task` doesn't move the state its own code is using.]), (30, [Valid while the task is suspended.]), (31, [Valid while the task is running.]), (34, [Set by `task_entry`, and taken by `resume`.]),))[```rust
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
```]

The split between `Task` and `Inner` is about aliasing. `resume(&mut self)` holds a unique borrow of the handle for the whole time the task runs, and the task's code writes to its own state during that time. If the state were inside the handle, those writes would alias a live `&mut`. Keeping it behind a raw pointer, in an allocation no reference covers, removes the conflict. The `&mut` also has a useful effect of its own: safe code can't resume a task that is already running, because the handle is borrowed.

`Task::new` allocates the state, then primes the context with the address it will keep:

#srcfile("green-tea-v2/src/task.rs", lines: ((39, 52),), notes: ((40, [`Box::into_raw` gives up the `Box`, so nothing asserts unique access to the allocation any more.]), (49, [`task_entry` will receive `inner` as its argument.]),))[```rust
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
```]

`resume` and `yield_now` are the two halves of the switch:

#srcfile("green-tea-v2/src/task.rs", lines: ((64, 73), (93, 97),), notes: ((67, [`CURRENT` becomes this task, and the previous value stays in a local of the resumer.]), (68, [Execution continues in the task. This call returns when the task yields or finishes.]), (69, [Back in the resumer, which is the current task again, or the thread.]), (71, [The task's panic continues here, on the resumer's stack.]), (96, [The mirror image of line 68, with the two contexts exchanged.]),))[```rust
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
// …
pub fn yield_now() {
    let inner = CURRENT.get();
    assert!(!inner.is_null(), "yield_now called outside a task");
    unsafe { swap_context(&raw mut (*inner).context, &raw const (*inner).caller) };
}
```]

Keeping `previous` on the resumer's stack is what makes nesting work. Each `resume` in progress holds one saved value, so the chain of resumers is stored in the stacks themselves. In the test `a_task_can_resume_another`, an inner task yields to the outer task that resumed it, and the outer task then yields to the test; the log reads `inner 1`, `outer`, `test`, `inner 2`.

Every task starts in `task_entry`, through the trampoline of @crate-arch:

#srcfile("green-tea-v2/src/task.rs", lines: ((101, 113),), notes: ((104, [`take` moves the closure out, so its captures are dropped when the call ends.]), (107, [A panic unwinds as far as this line, running the destructors of the task's frames.]), (109, [`resume` asserts on this flag, so a finished task can't be entered again.]), (110, [The last switch. The stack stays mapped until the owner drops the `Task`, which it can do only from another stack.]),))[```rust
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
```]

A panic in a task therefore stops at `catch_unwind`, which the task's own stack still has below it, and travels as a value. `resume` takes the payload after the switch and continues the panic with `resume_unwind` in the resumer, so the resumer can catch it, and if it doesn't, the panic goes on up the resumer's stack.

The other way a task can end early is to be dropped. `Drop` frees a task that hasn't started or has finished, and leaves a task that is suspended partway through its closure exactly as it is:

#srcfile("green-tea-v2/src/task.rs", lines: ((76, 87),), notes: ((82, [`func` is taken at the first resume, and `done` is set at the end, so this is a task between the two.]), (84, [Only a task with no frames in use gets its state and stack freed.]),))[```rust
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
```]

The frames of a suspended task hold locals whose destructors never ran. One of them can be a `thread::scope`, which relies on running to its end before its frame's memory goes away, and whose threads may still be using that frame. So the 32 KiB stack and the task's state stay allocated for the life of the process, and safe code can't reach freed memory through them. A program outside the crate has a task yield inside a scope and drops the task; the scoped thread then reads a local of the task's stack, and finds the value 7 it holds:

#output(title: "a task yields inside thread::scope, and is dropped")[
```
the task was dropped with its scope open
the scoped thread reads the task's local: 7
exit status 0
```
]

== `runtime`: the scheduler

`Runtime` is one `RefCell<VecDeque<Task>>`, created on the stack of `run`. A thread-local pointer lets `spawn` find it from any task:

#srcfile("green-tea-v2/src/runtime.rs", lines: ((33, 50),), notes: ((35, [One runtime per thread at a time.]), (37, [A guard: its `drop` clears `RUNTIME` however `run` exits.]), (43, [A separate statement, so the borrow ends before the task runs. The task may call `spawn`, which borrows the queue again.]), (45, [A task's panic continues from here. It ends `run`, and the queue's suspended tasks are dropped without being resumed.]), (48, [A finished task is dropped here, on the scheduler's stack, and its stack is unmapped.]),))[```rust
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
```]

#srcfile("green-tea-v2/src/runtime.rs", lines: ((56, 61),))[```rust
pub fn spawn(func: impl FnOnce() + 'static) {
    let runtime = RUNTIME.get();
    assert!(!runtime.is_null(), "spawn called outside run()");
    let task = Task::new(func);
    unsafe { &*runtime }.queue.borrow_mut().push_back(task);
}
```]

`spawn` puts the new task at the back, so `main` keeps running until it yields or finishes. The order is first in, first out, and the test `round_robin` pins it: two tasks that each log twice produce `a1`, `b1`, `a2`, `b2`.

== Demos and tests

As with v1, `coop-2-v2` drives two tasks by hand, through `raw::Task`, and `coop-n-v2` uses `run` and `spawn`. Their output is the same as in @crate-v1:

#output(title: "cargo run -p green-tea-v2 --bin coop-n-v2")[
```
counter: 1
words: ping
counter: 2
words: pong
counter: 3
main: all tasks finished
```
]

The 18 tests, by module:

#dtable(columns: (auto, 1fr), ("Test", "What it establishes"),
  [`test_stack_creation`, `size_is_rounded_up_to_whole_pages`], [A stack maps, and its size is whole pages plus the guard],
  [`a_failed_mapping_panics`, `an_absurd_size_panics_instead_of_wrapping`], [A size of $2^60$ reaches the `mmap` error, and `usize::MAX` the overflow check],
  [`runs_to_completion`, `yields_back_to_the_resumer`], [A closure runs, and three yields return in order],
  [`a_task_can_resume_another`], [Nested resumes return to the right resumer],
  [`float_registers_survive_switches`], [Two tasks interleave floating-point work and each gets the result it computes alone],
  [`dropping_a_suspended_task_keeps_its_stack`], [A dropped suspended task's frame stays mapped, with its local intact],
  [`a_panic_in_a_task_unwinds_to_the_resumer`], [The payload reaches the resumer, after the task's destructors ran],
  [`fp_control_state_is_per_task`, `a_fresh_task_has_the_default_fp_control_state`], [A task's rounding mode stays with it, across a yield, and a new task starts in the default mode],
  [`yield_outside_a_task_panics`], [The null check in `yield_now`],
  [`overflow_hits_the_guard_page`], [A task's stack overflow faults inside its guard page],
  [`round_robin`, `tasks_can_spawn_tasks`], [Scheduling order, and `spawn` from a task],
  [`run_inside_run_panics`, `spawn_outside_run_panics`], [The checks in `run` and `spawn`],
)

`float_registers_survive_switches` keeps eight `f64` values live across each yield. With optimization, the compiler places them in callee-saved registers on aarch64 and riscv64, so a switch that forgot `d8`–`d15` or `fs0`–`fs11` would hand each task the other's values. That is why `cargo xtask test` also runs in release. On x86_64 the System V ABI has no callee-saved float registers, so the values are on the stack and the test can't fail for this cause.

`overflow_hits_the_guard_page` needs a process it can lose, so it forks. The child installs a `SIGSEGV` and `SIGBUS` handler on an alternate signal stack, because the task's own stack is the one that is full, and reports through its exit code whether the faulting address was in the guard page:

#srcfile("green-tea-v2/src/task.rs", lines: ((390, 396), (415, 415), (432, 433), (436, 441),), notes: ((391, [The address that faulted, from the kernel.]), (394, [`_exit` is async-signal-safe, and no allocation happens in the child.]), (441, [42: the fault was in the guard page.]),))[```rust
extern "C" fn on_fault(_signal: c_int, info: *mut libc::siginfo_t, _ctx: *mut c_void) {
    let addr = unsafe { (*info).si_addr() } as usize;
    let guard = GUARD_START.load(Relaxed)..GUARD_END.load(Relaxed);
    unsafe {
        libc::_exit(if guard.contains(&addr) { FAULT_IN_GUARD } else { FAULT_ELSEWHERE })
    };
}
// …
    let pid = unsafe { libc::fork() };
// …
        task.resume();
        unsafe { libc::_exit(NO_FAULT) };
// …
    let mut status = 0;
    let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
    assert_eq!(rc, pid, "waitpid failed: {}", std::io::Error::last_os_error());
    assert!(libc::WIFEXITED(status), "the child died unexpectedly (status {status:#x})");
    match libc::WEXITSTATUS(status) {
        FAULT_IN_GUARD => {}
```]

#output(title: "cargo test -p green-tea-v2 --release -- --test-threads=1")[
```
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

`cargo xtask test` runs the tests and both demos in debug and release, on the host and under QEMU for aarch64 and riscv64. All 20 steps of the matrix pass:

#output(title: "cargo xtask test")[
```
=== summary
ok    host                         debug   test
ok    host                         release test
ok    aarch64-unknown-linux-gnu    debug   test
ok    aarch64-unknown-linux-gnu    release run coop-n-v2
ok    riscv64gc-unknown-linux-gnu  debug   test
ok    riscv64gc-unknown-linux-gnu  release run coop-n-v2
ok    aarch64-apple-darwin         release build lib
```
]

== Limits

- *A task dropped while suspended leaks.* Its 32 KiB stack and its state are never freed, and the destructors of its locals never run. That is the price of the soundness shown above. When a task's panic ends `run`, every task still suspended in the queue leaks this way.
- `run` called inside `run` panics in the task that called it, and the panic ends the outer `run` in the thread that started it:

#output(title: "a task calls run")[
```
a task calls run

thread 'main' panicked at green-tea-v2/src/runtime.rs:35:5:
run() called inside run()
exit status 101
```
]

- Scheduling is cooperative: a task that never yields keeps the thread. @crate-preempt adds a timer.
- A task is tied to the thread that created it. `Task` holds a raw pointer, so it is neither `Send` nor `Sync`.
- Every stack is 32 KiB (`STACK_SIZE`), fixed, and doesn't grow.
- `run` offers no way to wait, join or pass a value between tasks.
- On aarch64, the floating-point status register `fpsr` is shared between tasks (@crate-arch).

One switch, a coroutine and a queue. @crate-bare ports them to a machine with no operating system.
