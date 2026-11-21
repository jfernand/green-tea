#import "../lib.typ": *

= green-tea-v2 <crate-v2>

`green-tea-v2` runs cooperative tasks on x86_64, aarch64 and riscv64 under Linux, and its library also builds for macOS on Apple Silicon. It has three layers. @crate-arch saves and loads registers. `Task` is an asymmetric coroutine: whoever resumes it gets control back when it yields. `run` and `spawn` are a round-robin scheduler on top. The crate has no `static mut`, and its state is per thread.

#spec(
  ("Source", [#repo("green-tea-v2/src/"), 259 lines without their tests, and two demos]),
  ("Depends on", [`green-tea-arch`, `libc`, `tracing`, `tracing-subscriber`; `test-log` for the tests]),
  ("Used by", [nothing links it; `green-tea-bare` ports its `Task` and scheduler (@crate-bare)]),
  ("Tests", [13 host tests, run on three architectures in two profiles by `cargo xtask test`; the `coop-2-v2` and `coop-n-v2` binaries]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`run(main)`], [Runs `main` as the first task, and schedules until every task has finished],
  [`spawn(func)`], [Adds a task to the back of the queue of the current `run`],
  [`yield_now()`], [Suspends the current task and returns to whoever resumed it],
  [`raw::Task`], [An owned coroutine: `Task::new(func)`, `resume`, `is_done`. Dropping it frees its stack],
  [`raw::Context`, `raw::Entry`, `raw::swap_context`], [Re-exported from @crate-arch],
  [`raw::yield_now`], [The same function as `yield_now`],
)

== What changed from v1

#dtable(columns: (auto, 1fr, 1fr), ("", "green-tea-v1", "green-tea-v2"),
  [Architectures], [x86_64], [x86_64, aarch64, riscv64],
  [Switch], [its own `context.s`], [`green-tea-arch`, shared with the kernels],
  [Where a yield goes], [always to the global `MAIN`], [to `caller`, a context in each task],
  [Current task], [`static mut CURRENT`], [a `thread_local!` `Cell`],
  [A task drives other tasks], [crashes], [works, to any depth],
  [`spawn` from a task], [not possible], [works],
  [Scheduler state], [a local of `run`], [a `Runtime` reached through a `thread_local!`],
  [Task state], [a `Box<Task>` also reached by raw pointer], [an `Inner` touched only through raw pointers],
  [Failed `mmap`, `mprotect`], [ignored], [panics with the OS error],
  [Stack size], [as given], [rounded up to whole pages],
  [Misuse], [segmentation fault], [a panic with a message],
)

== `stack`

`Stack` has the layout of the one in @crate-v1, a guard page below the usable pages, with its failures handled. `new` rounds the size up to whole pages, since `mprotect` works in pages, and uses checked arithmetic:

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

#srcfile("green-tea-v2/src/task.rs", lines: ((22, 32),), notes: ((23, [The handle is one pointer. Moving a `Task` doesn't move the state its own code is using.]), (27, [Valid while the task is suspended.]), (28, [Valid while the task is running.]),))[```rust
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
```]

The split between `Task` and `Inner` is about aliasing. `resume(&mut self)` holds a unique borrow of the handle for the whole time the task runs, and the task's code writes to its own state during that time. If the state were inside the handle, those writes would alias a live `&mut`. Keeping it behind a raw pointer, in an allocation no reference covers, removes the conflict. The `&mut` also has a useful effect of its own: safe code can't resume a task that is already running, because the handle is borrowed.

`Task::new` allocates the state, then primes the context with the address it will keep:

#srcfile("green-tea-v2/src/task.rs", lines: ((35, 47),), notes: ((36, [`Box::into_raw` gives up the `Box`, so nothing asserts unique access to the allocation any more.]), (44, [`task_entry` will receive `inner` as its argument.]),))[```rust
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
```]

`resume` and `yield_now` are the two halves of the switch:

#srcfile("green-tea-v2/src/task.rs", lines: ((56, 62), (77, 81),), notes: ((59, [`CURRENT` becomes this task, and the previous value stays in a local of the resumer.]), (60, [Execution continues in the task. This call returns when the task yields or finishes.]), (61, [Back in the resumer, which is the current task again, or the thread.]), (80, [The mirror image of line 60, with the two contexts exchanged.]),))[```rust
    pub fn resume(&mut self) {
        let inner = self.inner.as_ptr();
        assert!(!self.is_done(), "resumed a task that has already finished");
        let previous = CURRENT.replace(inner);
        unsafe { swap_context(&raw mut (*inner).caller, &raw const (*inner).context) };
        CURRENT.set(previous);
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

#srcfile("green-tea-v2/src/task.rs", lines: ((85, 95),), notes: ((88, [`take` moves the closure out, so its captures are dropped when the call ends.]), (91, [`resume` asserts on this flag, so a finished task can't be entered again.]), (92, [The last switch. The stack stays mapped until the owner drops the `Task`, which it can do only from another stack.]),))[```rust
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
```]

== `runtime`: the scheduler

`Runtime` is one `RefCell<VecDeque<Task>>`, created on the stack of `run`. A thread-local pointer lets `spawn` find it from any task:

#srcfile("green-tea-v2/src/runtime.rs", lines: ((32, 49),), notes: ((34, [One runtime per thread at a time.]), (36, [A guard: its `drop` clears `RUNTIME` however `run` exits.]), (42, [A separate statement, so the borrow ends before the task runs. The task may call `spawn`, which borrows the queue again.]), (47, [A finished task is dropped here, on the scheduler's stack, and its stack is unmapped.]),))[```rust
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

#srcfile("green-tea-v2/src/runtime.rs", lines: ((55, 60),))[```rust
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

The 13 tests, by module:

#dtable(columns: (auto, 1fr), ("Test", "What it establishes"),
  [`test_stack_creation`, `size_is_rounded_up_to_whole_pages`], [A stack maps, and its size is whole pages plus the guard],
  [`a_failed_mapping_panics`, `an_absurd_size_panics_instead_of_wrapping`], [A size of $2^60$ reaches the `mmap` error, and `usize::MAX` the overflow check],
  [`runs_to_completion`, `yields_back_to_the_resumer`], [A closure runs, and three yields return in order],
  [`a_task_can_resume_another`], [Nested resumes return to the right resumer],
  [`float_registers_survive_switches`], [Two tasks interleave floating-point work and each gets the result it computes alone],
  [`yield_outside_a_task_panics`], [The null check in `yield_now`],
  [`overflow_hits_the_guard_page`], [A task's stack overflow faults inside its guard page],
  [`round_robin`, `tasks_can_spawn_tasks`], [Scheduling order, and `spawn` from a task],
  [`spawn_outside_run_panics`], [The null check in `spawn`],
)

`float_registers_survive_switches` keeps eight `f64` values live across each yield. With optimization, the compiler places them in callee-saved registers on aarch64 and riscv64, so a switch that forgot `d8`–`d15` or `fs0`–`fs11` would hand each task the other's values. That is why `cargo xtask test` also runs in release. On x86_64 the System V ABI has no callee-saved float registers, so the values are on the stack and the test can't fail for this cause.

`overflow_hits_the_guard_page` needs a process it can lose, so it forks. The child installs a `SIGSEGV` and `SIGBUS` handler on an alternate signal stack, because the task's own stack is the one that is full, and reports through its exit code whether the faulting address was in the guard page:

#srcfile("green-tea-v2/src/task.rs", lines: ((230, 236), (255, 255), (272, 273), (276, 281),), notes: ((231, [The address that faulted, from the kernel.]), (234, [`_exit` is async-signal-safe, and no allocation happens in the child.]), (281, [42: the fault was in the guard page.]),))[```rust
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
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
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

- *Dropping a suspended task is unsound.* `Drop` unmaps the stack without running the destructors of the locals on it, as the comment on `drop` in `task.rs` says. The consequence is larger than a missed destructor: `thread::scope` relies on returning, after it joins its threads, before its frame's memory goes away. A task that yields inside a scope and is then dropped leaves the scoped thread reading unmapped or reused memory, all from safe code. The thread segfaults, or, if a new `Task` has been mapped at that address, as in this run, reads 0 from a local that holds 7:

#output(title: "a task yields inside thread::scope, and is dropped")[
```
the task was dropped with its scope open
the scoped thread reads the task's local: 0
exit status 0
```
]

- *A panic in a task aborts the process*, and no destructors run. The documented panic of `run` called inside `run` is always in this case, because the only code running under `run` is tasks. The comment in `task_entry` says a panic can't unwind past its `extern "C"` frame. The abort comes earlier: `bootstrap_entry` has no unwind information, so the search for a handler reaches the end of the stack (`_URC_END_OF_STACK` is error 5):

#output(title: "a task calls run")[
```
a task calls run
run() called inside run()
fatal runtime error: failed to initiate panic, error 5, aborting
exit status 134
```
]

- Scheduling is cooperative: a task that never yields keeps the thread. @crate-preempt adds a timer.
- A task is tied to the thread that created it. `Task` holds a raw pointer, so it is neither `Send` nor `Sync`.
- Every stack is 32 KiB (`STACK_SIZE`), fixed, and doesn't grow.
- `run` offers no way to wait, join or pass a value between tasks.
- Floating-point control state is shared between tasks (@crate-arch).

One switch, a coroutine and a queue. @crate-bare ports them to a machine with no operating system.
