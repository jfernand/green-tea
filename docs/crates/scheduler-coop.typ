#import "../lib.typ": *

= green-tea-scheduler-coop <crate-scheduler-coop>

`green-tea-scheduler-coop` is the task runtime of the bare-metal kernels: cooperative tasks, a round-robin run queue in which tasks can sleep or wait for an interrupt, and a console read that waits without spinning. It is `green-tea-v2`'s task and runtime (@crate-v2) moved onto `green-tea-bare` (@crate-bare), and it lived inside that crate until it was split out. It depends on `green-tea-bare` for stacks, task records, events and the board, and `green-tea-bare` doesn't depend on it.

#spec(
  ("Source", [#repo("schedulers/coop/src/"), 4 files, 386 lines]),
  ("Depends on", [`green-tea-bare` (@crate-bare), `green-tea-arch` (@crate-arch)]),
  ("Used by", [`green-tea-scheduler-preempt` (@crate-scheduler-preempt), and the kernels `green-tea-coop` (@crate-coop), `-shell` (@crate-shell) and `-black` (@crate-black)]),
  ("Tests", [none of its own: it builds only for the three bare-metal targets. The `coop`, `slab`, `fit`, `lisp` and `safety` kernels run tasks on it, and the shell waits for keys with it]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`task::Task`], [an owned handle to a task: `new(f)`, `id`, `is_done`, `resume`, and `take_wake_at` and `take_waits_for`, which tell the scheduler why the task last yielded],
  [`task::yield_now`], [suspend the current task, back to whoever resumed it],
  [`task::sleep_until`, `sleep_ms`], [suspend the current task until `ticks` reaches a deadline, absolute or from now],
  [`task::wait_event`], [suspend the current task until an `Event` is set; return at once if it is],
  [`task::in_task`], [whether a task is running, as opposed to the code on the boot stack],
  [`task::current_guard`], [the guard page under the current task's stack],
  [`task::cache`, `reap`], [look at the slab cache of task records; destroy its unused records, which gives back their stacks],
  [`runtime::run(main, on_switch)`], [run `main` as the first task, and schedule until no task is left, calling `on_switch` before each switch],
  [`runtime::spawn(f)`], [add a task to the back of the run queue],
  [`runtime::Switch`], [what the hook is told: `To(id)`, or `Idle`],
  [`console::read_byte`], [the next byte typed at the console, waiting as a task],
)

The three modules are `task`, `runtime` and `console`. The features `green`, `black`, `white` and `board-pc` pass through to `green-tea-bare`.

== Tasks

A `Task` is an asymmetric coroutine: someone `resume`s it, it runs until it yields or finishes, and control returns to whoever resumed it. The switch is `green-tea-arch`'s `swap_context` (@crate-arch), and the two halves are a few lines each:

#srcfile("schedulers/coop/src/task.rs", lines: ((124, 130), (156, 160),), notes: ((127, [`CURRENT` is a `Global<GlobalPtr<Inner>>`, where v2 had a `thread_local!`. It is null on the boot stack.]), (128, [The scheduler's registers go into `caller`, the task's come out of `context`.]), (159, [The reverse swap. It returns when the task is next resumed.]),))[```rust
    pub fn resume(&mut self) {
        let inner = self.inner.as_ptr();
        assert!(!self.is_done(), "resumed a task that has already finished");
        let previous = CURRENT.replace(GlobalPtr::new(inner));
        unsafe { swap_context(&raw mut (*inner).caller, &raw const (*inner).context) };
        CURRENT.set(previous);
    }
// …
pub fn yield_now() {
    let inner = CURRENT.get().as_ptr();
    assert!(!inner.is_null(), "yield_now called outside a task");
    unsafe { swap_context(&raw mut (*inner).context, &raw const (*inner).caller) };
}
```]

A task starts in `task_entry`, on its own stack, runs its closure, marks itself done, and switches back for the last time:

#srcfile("schedulers/coop/src/task.rs", lines: ((205, 215),))[```rust
extern "C" fn task_entry(arg: *mut ()) -> ! {
    let inner: *mut Inner = arg.cast();
    unsafe {
        if let Some(func) = (*inner).func.take() {
            func(); // a panic here goes to the panic handler, which halts
        }
        (*inner).done = true;
        swap_context(&raw mut (*inner).context, &raw const (*inner).caller);
    }
    unreachable!("a finished task was resumed")
}
```]

A task's record, `Inner`, holds its two `Context`s, its closure, `done`, its `id`, the reason it last yielded, and its `Stack` from `green-tea-bare`'s pool. Records come from a slab cache, `TASKS`, a `KernelCache` (@crate-kmem), whose constructor gives each record a stack:

#srcfile("schedulers/coop/src/task.rs", lines: ((41, 47),), notes: ((42, [The pool has 102 stacks. A record that can't have one isn't made, and neither is its slab.]),))[```rust
fn construct(record: *mut u8) -> bool {
    let Some(stack) = Stack::try_take() else { return false };
    let inner = Inner { context: Context::default(), caller: Context::default(), func: None, done: false, id: 0, wake_at: None, waits_for: None, stack };
    // SAFETY: the cache hands the constructor memory of the size and alignment of an `Inner`.
    unsafe { record.cast::<Inner>().write(inner) };
    true
}
```]

The cache keeps a freed record constructed, and a constructed record *keeps its stack*. So `Task::new` takes stacks from the pool only when the cache has to make a new slab, and a thousand tasks run one after another use one stack. `Task::new` resets the rest of the record: a new `Context` at the stack's top, the closure, the next `id`, and no reason to wait. Dropping a `Task` frees the record back to the cache, closure and all. `reap` destroys the records no task is using, which gives their stacks to the pool and their pages to `kmem`; `run` calls it when every task has finished.

`sleep_until` writes the deadline into the record and yields, and `sleep_ms` computes the deadline from `ticks_per_ms`. `wait_event` writes the `Event` and yields, unless the event is already set. Nothing in `task` knows about interrupts: @crate-scheduler-preempt calls `yield_now` from its timer handler, and keeps interrupts off whenever a switch is under way.

== The run queue

`runtime` keeps three collections in a `Runtime` on `run`'s stack: `queue`, a `VecDeque` of runnable tasks in the order they will run; `sleeping`, a `Vec` of tasks with their deadlines; and `waiting`, a `Vec` of tasks with the events they wait for. A static `RUNTIME`, a `Global<GlobalPtr<Runtime>>`, points at it while `run` runs, so that `spawn` can find it from any task. `run` is the loop:

#srcfile("schedulers/coop/src/runtime.rs", lines: ((53, 84),), notes: ((54, [Move every sleeper that is due, and every waiter whose event is set, to the back of the queue.]), (57, [A separate statement, so the queue's borrow ends before the task runs: the task may call `spawn`.]), (64, [Every task is waiting for an event, so only an interrupt can change anything.]), (69, [Someone is asleep: spin until the first deadline, or until an event is set.]), (74, [The hook sees each `Switch`. The demos draw here, and @crate-scheduler-preempt arms the time slice.]), (79, [Why the task yielded decides where it goes. A plain `yield_now` goes to the back of the queue.]),))[```rust
    loop {
        runtime.wake(Board::ticks());
        // A separate statement, so the queue borrow ends before the task runs: the task
        // may call `spawn`, which borrows the queue again.
        let next = runtime.queue.borrow_mut().pop_front();
        let Some(mut task) = next else {
            let first_due = runtime.sleeping.borrow().iter().map(|&(due, _)| due).min();
            match first_due {
                // Every task is waiting for an event: only an interrupt can change that.
                None if !runtime.waiting.borrow().is_empty() => {
                    on_switch(Switch::Idle);
                    Board::wait_for_interrupt();
                }
                None => break, // nothing runnable, asleep or waiting: done
                Some(due) => {
                    on_switch(Switch::Idle);
                    while Board::ticks() < due && !runtime.any_event_set() {}
                }
            }
            continue;
        };
        on_switch(Switch::To(task.id()));
        task.resume();
        if task.is_done() {
            continue;
        }
        match (task.take_wake_at(), task.take_waits_for()) {
            (Some(due), _) => runtime.sleeping.borrow_mut().push((due, task)),
            (None, Some(event)) => runtime.waiting.borrow_mut().push((event, task)),
            (None, None) => runtime.queue.borrow_mut().push_back(task),
        }
    }
```]

A sleeping or waiting task is off the queue, so it costs no turns, and the tasks that have work share the CPU. A finished task is dropped at the `continue`, which returns its record. `run` panics if it is called inside another `run`, and `spawn` if it is called outside one. When `wait_for_interrupt` is reached, every task is waiting for an interrupt; on `pc` that halts the CPU until one comes (@bare-interrupts), and on the QEMU boards it is one spin-loop hint, so the loop polls.

== Reading the console

`console::read_byte` is the kernel's keyboard. On a board with input interrupts, `Board::input_event` returns an `Event`, and the task waits on it; on the others it polls, sleeping a millisecond between looks so that other tasks run:

#srcfile("schedulers/coop/src/console.rs", lines: ((15, 31),), notes: ((18, [Cleared before the look, so that a byte arriving after it still ends the wait.]), (22, [Returns at once if the event is set again.]), (25, [The polled path, for the boards with no input interrupts.]),))[```rust
pub fn read_byte() -> u8 {
    if let Some(input) = Board::input_event() {
        loop {
            input.clear(); // before looking, so that a byte arriving after the look still wakes us
            if let Some(byte) = Board::getc() {
                return byte;
            }
            task::wait_event(input);
        }
    }
    loop {
        if let Some(byte) = Board::getc() {
            return byte;
        }
        task::sleep_ms(POLL_MS);
    }
}
```]

The order is what keeps a key from being lost. A byte that arrives after `getc` looked sets the event again, and `wait_event` returns at once. The shell's prompt is this function (@crate-shell).

== Exercised by

The `coop` kernel's demo is five workers that yield, and counts the switches it expects (@crate-coop):

#output(title: "cargo xtask kernel --kernel coop --target riscv64")[
```
running 5 workers; expecting 57 switches
all tasks finished after 57 switches (expected 57)
PASS: 57 switches, as expected
```
]

The `slab` kernel checks the records' cache. A record is 512 bytes, and a slab holds 7, so the first slab takes 7 stacks. A thousand tasks, one after another, then reuse one record and its stack: every one has its locals at the same address. The 1,001 records include the main task. When the last finishes, `run`'s `reap` gives the 7 stacks back. Then as many tasks as whole slabs can have stacks for run at once:

#output(title: "cargo xtask kernel --kernel slab --target riscv64")[
```
the kernel's cache of task records: a record is 512 bytes, a buffer 520, and a slab holds 7
  a constructed record has a stack, of which the pool has 102
1000 tasks, one after another, every one with its locals at 0x80a09f2e:
  1001 records allocated, 7 constructed, in 1 slab; 7 stacks taken from the pool
  the last task finished, the scheduler reaps: 1 slab and 7 records destroyed, and the pool has 102 stacks
98 tasks at once:
  14 slabs, all full, in 14 pages; the pool has 4 stacks left, too few for another slab
```
]

The `fit` kernel runs tasks to watch the kernel heap, `lisp` runs a deep recursion on a task's stack, over its guard page, and `safety` runs two tasks that share an allocator. The shell waits for every key with `read_byte`, and the `interrupts` kernel, which is the shell on the `pc` board, checks that wait in QEMU (@crate-ioapic). @crate-scheduler-preempt runs the `preempt` kernel's tasks on this run queue.

== Limits

- *One CPU.* The run queue, `CURRENT` and the records' cache belong to the boot CPU, as every `Global` does (@bare-smp).
- *Round robin, no priorities.* A task that wakes goes to the back of the queue. Fixed priorities are @crate-rt-bare.
- *Idle is a spin when anyone sleeps.* With nothing runnable and a sleeper, the loop spins on `ticks` (`runtime.rs:69`). Tasks run with interrupts off, so a device interrupt can't set an event during that spin, and a task waiting for input can't wake before the sleeper is due (@bare-limits).
- *Sleepers and waiters are unsorted vectors*, scanned whole on every pass of the loop.
- *At most 98 tasks at once.* Each record needs a stack, the pool has 102, and records come 7 to a slab. `Task::new` panics when the cache can't make a record.
- *A task that panics halts the CPU*, as every panic does; there is no unwinding.
- *Hardware.* The shell that ran on the laptop used this code while it was still in `green-tea-bare`. The crate as it is at this commit has run only in QEMU.

One switch, three lists, and a byte at a time from the keyboard. @crate-scheduler-preempt takes the `yield_now` out of the tasks' hands.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: a98d25da2977
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
