#import "../lib.typ": *

= green-tea-v1 <crate-v1>

`green-tea-v1` runs cooperative tasks on one thread, on x86_64 under Linux. A task is a closure with a 32 KiB stack of its own, a function in assembly switches between stacks, and a round-robin loop decides who runs next. All its state is in two global variables, which is the main difference from @crate-v2.

#spec(
  ("Source", [#repo("green-tea-v1/src/"), 161 lines of Rust without their tests, 32 of assembly, and two demos]),
  ("Depends on", [`libc`, `tracing`, `tracing-subscriber`, `test-log`]),
  ("Used by", [nothing; it is kept to compare with `green-tea-v2`]),
  ("Tests", [3 host tests, the `coop-2-v1` and `coop-n-v1` binaries]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it is"),
  [`context::Context`], [The seven registers a suspended task keeps: `rsp`, `r15`, `r14`, `r13`, `r12`, `rbx`, `rbp`],
  [`context::swap_context`], [Assembly: saves the current registers into `from`, loads those in `to`, returns on the new stack],
  [`context::Task`], [A stack, a `Context`, the closure, and a public `done` flag],
  [`context::spawn`], [Builds a boxed `Task` whose first switch starts the closure],
  [`context::yield_now`], [Switches from the current task to the scheduler],
  [`context::run`], [Runs a set of tasks round-robin until all are done],
  [`context::MAIN`], [`static mut`: the scheduler's saved registers],
  [`context::CURRENT`], [`static mut`: a pointer to the running task],
)

The `stack` module is private. Its `Stack` type has `new` and `top`, and unmaps itself on drop.

== `stack`: a mapping with a guard page

A stack is an anonymous mapping with one inaccessible page at its low end. A task that runs off the bottom of its stack faults there instead of overwriting the neighbouring mapping:

#srcfile("green-tea-v1/src/stack.rs", lines: ((11, 11), (14, 14), (17, 26), (28, 37), (39, 41),), notes: ((14, [One page more than requested: the guard.]), (21, [The whole range starts inaccessible.]), (30, [Everything above the guard becomes readable and writable.]), (40, [The address one past the highest byte. Stacks grow down, so this is where a push starts.]),))[```rust
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
    } as *mut u8;
// …
    unsafe {
        libc::mprotect(
            base.add(guard) as *mut libc::c_void,
            size,
            PROT_READ | PROT_WRITE,
        )
    };

    Stack { base, len }
}
// …
pub(crate) fn top(&self) -> *const u8 {
    (self.base as usize + self.len) as *const u8 // sp starts here and grows down
}
```]

Neither `mmap` nor `mprotect` has its result checked. @crate-v2 checks both.

== `context`: priming and switching

`Context` is `repr(C)` because the assembly addresses its fields by offset, from `0x00` for `rsp` to `0x30` for `rbp`. `swap_context` stores the current registers through its first argument, loads new ones through its second, and executes `ret`. The file `context.s` is the same as `green-tea-arch/src/x86_64.s` but for the words of one comment, and @crate-arch walks it register by register.

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

`yield_now` is one call, and always goes to `MAIN`:

#srcfile("green-tea-v1/src/context.rs", lines: ((81, 83),))[```rust
pub fn yield_now() {
    unsafe { swap_context(&raw mut (*CURRENT).context, &raw const MAIN) };
}
```]

`run` holds the tasks in a `VecDeque`. It takes the one at the front, records it in `CURRENT`, and switches to it. Control comes back when the task yields or finishes. A finished task is dropped, which unmaps its stack; that is safe here because the code is on the thread's own stack by then.

#srcfile("green-tea-v1/src/context.rs", lines: ((87, 101),), notes: ((94, [The scheduler's registers go into `MAIN`, where `yield_now` and `task_entry` find them.]),))[```rust
pub fn run(tasks: impl IntoIterator<Item = Box<Task>>) {
    let mut queue: VecDeque<Box<Task>> = tasks
        .into_iter()
        .collect();
    while let Some(mut task) = queue.pop_front() {
        unsafe {
            CURRENT = &raw mut *task;
            swap_context(&raw mut MAIN, &raw const task.context);
            CURRENT = core::ptr::null_mut();
        }
        if !task.done {
            queue.push_back(task);
        }
    }
}
```]

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

The three tests cover the stack and the switch. `test_stack_creation` maps a stack. `test_swap_context` primes a task by hand and checks that its closure ran. `test_multiple_yields` enters a worker with its own pair of contexts and checks that it logged 1, 2, 3 and 100, in that order, across four switches back.

#output(title: "cargo test -p green-tea-v1 -- --test-threads=1")[
```
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

== Limits

- It builds only for x86_64: `lib.rs` includes `context.s` with no `cfg`, and the assembler rejects it on other targets. The comment in `stack.rs` about 16 KiB pages on Apple Silicon comes from a platform this crate can't target natively.
- A failed `mmap` or `mprotect` goes unnoticed, and the next write to the stack faults at a meaningless address.
- `MAIN` and `CURRENT` are `static mut`, so two threads can't run tasks at the same time.
- `yield_now` outside a task dereferences a null `CURRENT`. A task that calls `run` overwrites `MAIN`, and the inner `run` leaves `CURRENT` null when it returns, so the task's next `yield_now` dereferences the null too. Both end in a segmentation fault (139 is 128 + `SIGSEGV`), recorded from a small program outside the crate:

#output(title: "a task that calls run, then yields")[
```
outer task: before the inner run
inner task
outer task: after the inner run
exit status 139
```
]

- A task can't spawn another: `spawn` returns a `Box<Task>`, and the queue is a local of `run`.
- A task dropped before it finishes doesn't run the destructors of the locals on its stack.
- `test-log` is a normal dependency in `Cargo.toml`, though only the tests use it.
- The comment on `ret` in `context.s` says the return address "was put there by `call`". On a task's first switch `spawn` put it there.

Fifteen instructions and a queue. @crate-arch takes the switch to three architectures.
