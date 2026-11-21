#import "../lib.typ": *

= green-tea-v1 <crate-v1>

`green-tea-v1` runs cooperative tasks on one thread, on x86_64 under Linux. A task is a closure with a 32 KiB stack of its own, a function in assembly switches between stacks, and a round-robin loop decides who runs next. All its state is in two global variables, which is the main difference from @crate-v2.

#spec(
  ("Source", [#repo("green-tea-v1/src/"), 186 lines of Rust without their tests, 32 of assembly, and two demos]),
  ("Depends on", [`libc`, `tracing`, `tracing-subscriber`; `test-log` for the tests]),
  ("Used by", [nothing; it is kept to compare with `green-tea-v2`]),
  ("Tests", [6 host tests, the `coop-2-v1` and `coop-n-v1` binaries]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it is"),
  [`context::Context`], [The seven registers a suspended task keeps: `rsp`, `r15`, `r14`, `r13`, `r12`, `rbx`, `rbp`],
  [`context::swap_context`], [Assembly: saves the current registers into `from`, loads those in `to`, returns on the new stack],
  [`context::Task`], [A stack, a `Context`, the closure, and a public `done` flag],
  [`context::spawn`], [Builds a boxed `Task` whose first switch starts the closure],
  [`context::yield_now`], [Switches from the current task to the scheduler. Panics outside a task],
  [`context::run`], [Runs a set of tasks round-robin until all are done. A task may call it],
  [`context::MAIN`], [`static mut`: the scheduler's saved registers],
  [`context::CURRENT`], [`static mut`: a pointer to the running task],
)

The `stack` module is private. Its `Stack` type has `new`, which panics if the kernel refuses the mapping, and `top`, and unmaps itself on drop.

== `stack`: a mapping with a guard page

A stack is an anonymous mapping with one inaccessible page at its low end. A task that runs off the bottom of its stack faults there instead of overwriting the neighbouring mapping:

#srcfile("green-tea-v1/src/stack.rs", lines: ((16, 16), (19, 19), (22, 36), (39, 53), (55, 57),), notes: ((19, [One page more than requested: the guard.]), (26, [The whole range starts inaccessible.]), (33, [`mmap` reports failure with `MAP_FAILED`, which is `(void*)-1` and not null.]), (41, [Everything above the guard becomes readable and writable.]), (47, [`errno` is read before `munmap` can change it.]), (56, [The address one past the highest byte. Stacks grow down, so this is where a push starts.]),))[```rust
pub(crate) fn new(size: usize) -> Stack {
// …
    let len = guard + size;
// …
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            PROT_NONE,
            MAP_PRIVATE | MAP_ANON,
            -1,
            0,
        )
    };
    // mmap signals failure with MAP_FAILED, which is (void*)-1, not null.
    if base == MAP_FAILED {
        panic!("mmap of a {len}-byte stack failed: {}", io::Error::last_os_error());
    }
    let base = base as *mut u8;
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

    Stack { base, len }
}
// …
pub(crate) fn top(&self) -> *const u8 {
    (self.base as usize + self.len) as *const u8 // sp starts here and grows down
}
```]

Both system calls are checked, and a failure panics with the error from the OS. The test `a_failed_mapping_panics` asks for $2^60$ bytes, more than any x86_64 address space holds.

== `context`: priming and switching

`Context` is `repr(C)` because the assembly addresses its fields by offset, from `0x00` for `rsp` to `0x30` for `rbp`. `swap_context` stores the current registers through its first argument, loads new ones through its second, and executes `ret`. The file `context.s` is `green-tea-arch/src/x86_64.s` without the floating-point control state, and @crate-arch walks that file register by register.

A task that has never run has no saved registers, so `spawn` writes the ones its first switch will load:

#srcfile("green-tea-v1/src/context.rs", lines: ((55, 58), (60, 79),), notes: ((57, [24 bytes below a 16-byte boundary, so the slot is at an address ≡ 8 (mod 16).]), (70, [The slot holds the address `ret` will jump to.]), (75, [`r15` carries the task's address. The `Box` keeps it stable while the handle moves.]), (77, [`r14` carries the function the trampoline will call.]),))[```rust
fn initial_sp(stack: &Stack) -> *mut u64 {
    let top = (stack.top() as usize) & !0xF;
    (top - 24) as *mut u64
}
// …
pub fn spawn(func: impl FnOnce() + 'static) -> Box<Task> {
    let stack = Stack::new(32 * 1024);
    let mut task = Box::new(Task {
        stack,
        context: Context::default(),
        func: Some(Box::new(func)),
        done: false,
    });
    unsafe {
        let sp = initial_sp(&task.stack);
        sp.write(bootstrap_entry as *const () as usize as u64);
        task.context
            .rsp = sp as u64;
    }
    task.context
        .r15 = &raw mut *task as usize as u64; // the first switch "returns" here
    task.context
        .r14 = task_entry as *const () as usize as u64; // and then jumps here
    task
}
```]

The first `swap_context` into this task loads `rsp`, `r15` and `r14`, and its `ret` pops the slot and arrives in `bootstrap_entry`:

#srcfile("green-tea-v1/src/context.s", lines: ((28, 32),), notes: ((29, [The System V ABI passes the first argument in `rdi`.]), (31, [`task_entry(task)`, on the task's stack.]), (32, [`task_entry` never returns. If it did, this instruction would raise an invalid-opcode fault.]),))[```text
bootstrap_entry:
    mov rdi, r15 // task pointer from r15 -> first argument
    and rsp, -16 // SysV ABI: rsp must be 16-byte aligned right before a call
    call r14 // call function pointer
    ud2 // trigger invalid opcode execution
```]

`task_entry` is the only Rust frame at the bottom of every task:

#srcfile("green-tea-v1/src/context.rs", lines: ((31, 44),), notes: ((39, [The scheduler reads this flag after the switch below.]), (40, [The task will never run again, so its registers are saved into a local and forgotten.]),))[```rust
extern "C" fn task_entry(task: *mut Task) -> ! {
    unsafe {
        if let Some(func) = (*task)
            .func
            .take()
        {
            func();
        }
        (*task).done = true;
        let mut dummy = Context::default();
        swap_context(&mut dummy, &raw const MAIN);
        unreachable!()
    }
}
```]

== The scheduler

`yield_now` checks that a task is running, and always goes to `MAIN`:

#srcfile("green-tea-v1/src/context.rs", lines: ((85, 88),))[```rust
pub fn yield_now() {
    assert!(unsafe { !CURRENT.is_null() }, "yield_now called outside a task");
    unsafe { swap_context(&raw mut (*CURRENT).context, &raw const MAIN) };
}
```]

`run` holds the tasks in a `VecDeque`. It takes the one at the front, records it in `CURRENT`, and switches to it. Control comes back when the task yields or finishes. A finished task is dropped, which unmaps its stack; that is safe here because the code is on the thread's own stack by then. On entry `run` keeps the `MAIN` and `CURRENT` it found, and on the way out it puts them back, so a task can call `run` and yield afterwards.

#srcfile("green-tea-v1/src/context.rs", lines: ((94, 110),), notes: ((95, [What an enclosing `run` needs back, kept on this stack.]), (102, [The scheduler's registers go into `MAIN`, where `yield_now` and `task_entry` find them.]), (103, [Back in the scheduler, which is the outer task, or no task at all.]), (109, [The outer scheduler's registers, for the outer task's next `yield_now`.]),))[```rust
pub fn run(tasks: impl IntoIterator<Item = Box<Task>>) {
    let (outer_main, outer_current) = unsafe { ((&raw const MAIN).read(), CURRENT) };
    let mut queue: VecDeque<Box<Task>> = tasks
        .into_iter()
        .collect();
    while let Some(mut task) = queue.pop_front() {
        unsafe {
            CURRENT = &raw mut *task;
            swap_context(&raw mut MAIN, &raw const task.context);
            CURRENT = outer_current;
        }
        if !task.done {
            queue.push_back(task);
        }
    }
    unsafe { (&raw mut MAIN).write(outer_main) };
}
```]

A small program outside the crate has a task call `run` and then yield:

#output(title: "a task that calls run, then yields")[
```
outer task: before the inner run
inner task
outer task: after the inner run
outer task: resumed
main: returned
exit status 0
```
]

== Demos and tests

The two binaries run the same pair of tasks. `coop-2-v1` writes the scheduling loop by hand, with `CURRENT`, `MAIN` and `swap_context` in plain view; `coop-n-v1` calls `run`. Their output is identical apart from the timestamps of the two log lines that report each guard page's size:

#output(title: "cargo run -p green-tea-v1 --bin coop-n-v1")[
```
counter: 1
words: ping
counter: 2
words: pong
counter: 3
main: all tasks finished
```
]

The counter task runs a fourth time to leave its loop, after `words` has finished. Each task's `println!` runs on its own 32 KiB stack.

The six tests cover the stack, the switch and the scheduler. `test_stack_creation` maps a stack, and `a_failed_mapping_panics` is the failure above. `test_swap_context` primes a task by hand and checks that its closure ran. `test_multiple_yields` enters a worker with its own pair of contexts and checks that it logged 1, 2, 3 and 100, in that order, across four switches back. `yield_outside_a_task_panics` is the check in `yield_now`. `a_nested_run_leaves_the_outer_task_able_to_yield` runs in a child process of its own, because `MAIN` and `CURRENT` are process-wide and the harness runs tests on several threads.

#output(title: "cargo test -p green-tea-v1 -- --test-threads=1")[
```
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

== Limits

- It builds only for x86_64: `lib.rs` includes `context.s` with no `cfg`, and the assembler rejects it on other targets.
- `MAIN` and `CURRENT` are `static mut`, so two threads can't run tasks at the same time.
- A panic in a task aborts the process, with exit status 134: `bootstrap_entry` has no unwind information, so the search for a handler reaches the end of the stack. @crate-v2 carries the panic back to the resumer.
- A task can't spawn another: `spawn` returns a `Box<Task>`, and the queue is a local of `run`.
- A task dropped before it finishes doesn't run the destructors of the locals on its stack.

Fifteen instructions and a queue. @crate-arch takes the switch to three architectures.
