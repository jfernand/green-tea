#import "../lib.typ": *

= green-tea-scheduler-preempt <crate-scheduler-preempt>

`green-tea-scheduler-preempt` makes the tasks of @crate-scheduler-coop preemptible. A timer interrupt ends each task's time slice, and its handler switches away with the same `yield_now` that a cooperative task calls itself. The crate adds no queue and no task type of its own: it is a timer handler, and wrappers that keep interrupts off while the cooperative runtime moves tasks around. It works on any board that implements `TimerInterrupt` (@crate-bare), which all four do.

#spec(
  ("Source", [#repo("schedulers/preempt/src/lib.rs"), 82 lines]),
  ("Depends on", [`green-tea-scheduler-coop` (@crate-scheduler-coop), `green-tea-bare` (@crate-bare)]),
  ("Used by", [the `green-tea-preempt` kernel (@crate-preempt)]),
  ("Tests", [none of its own: it builds only for the bare-metal targets. The `preempt` kernel runs on it, on riscv64, aarch64 and x86_64]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`run::<B>(main, slice_ms, on_switch)`], [run `main` and its tasks with a time slice of `slice_ms`, calling `on_switch` with interrupts off before each switch],
  [`spawn::<B>(f)`], [add a task that runs with interrupts on, so the timer can preempt it anywhere],
  [`yield_now::<B>()`, `sleep_ms::<B>(ms)`], [the cooperative calls, made with interrupts off around the switch],
  [`in_task()`], [whether a task is running, as opposed to the boot stack],
  [`preemptions()`], [how many ticks switched away from a running task in the last `run`],
  [`Switch`], [re-exported from @crate-scheduler-coop],
)

Every function but `in_task` and `preemptions` is generic over `B: TimerInterrupt`, the board, so a kernel that calls them on a board without a timer interrupt doesn't compile.

== A preemption

The handler is short:

#srcfile("schedulers/preempt/src/lib.rs", lines: ((77, 82),), notes: ((78, [A tick that arrives on the boot stack has nothing to preempt.]), (80, [The ordinary context switch. It returns when the scheduler next resumes this task.]),))[```rust
fn on_tick<B: TimerInterrupt>() {
    if task::in_task() {
        PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        yield_now::<B>();
    }
}
```]

A preemption goes through four steps:

+ The scheduler arms the timer for one slice, and resumes a task.
+ The slice ends. The CPU traps into `green-tea-bare`'s entry *on the task's stack*, which saves every register there and calls `on_tick` (@crate-bare shows the three entries).
+ `on_tick` calls `yield_now`, which switches to the scheduler. The task's state is now in two layers: the trap frame on its stack, holding everything it was using, and the callee-saved registers of `on_tick`'s frame in its `Context`.
+ Later the scheduler resumes it. `swap_context` returns into `on_tick`, which returns to the trap entry, which restores the frame and returns from the trap (`sret`, `eret` or `iretq`) into the middle of whatever the task was doing.

#layers(caption: [A preempted task's stack, highest address first],
  ([The task's own frames], [stopped at an arbitrary instruction], "light"),
  ([Trap frame], [every register: 544 bytes on riscv64, 800 on aarch64, 20 words on x86_64], "amber"),
  ([`on_tick`, `yield_now`], [ordinary frames, down to the `swap_context` call], "dark"),
)

== Arming the slice

`run` installs the handler and wraps the `run` of @crate-scheduler-coop. Its hook calls the kernel's hook first, then arms the timer, so that whatever the kernel draws doesn't eat into the slice:

#srcfile("schedulers/preempt/src/lib.rs", lines: ((20, 44),), notes: ((25, [The scheduler runs with interrupts off from here on.]), (30, [The main task is preemptible too, like the tasks it spawns.]), (37, [One slice from now, in the board's ticks.]), (38, [Nothing to run: no deadline, and no tick.]),))[```rust
pub fn run<B: TimerInterrupt>(
    main: impl FnOnce() + 'static,
    slice_ms: u64,
    mut on_switch: impl FnMut(Switch),
) {
    B::interrupts_off();
    PREEMPTIONS.store(0, Ordering::Relaxed);
    B::init_timer(on_tick::<B>);
    runtime::run(
        move || {
            B::interrupts_on();
            main();
            B::interrupts_off();
        },
        |switch| {
            on_switch(switch);
            let deadline = match switch {
                Switch::To(_) => B::ticks() + slice_ms * B::ticks_per_ms(),
                Switch::Idle => u64::MAX,
            };
            B::set_deadline(deadline);
        },
    );
    B::set_deadline(u64::MAX);
}
```]

The handler counts in `PREEMPTIONS`, an `AtomicUsize`, which it can update whatever the interrupted task was doing. A `Global` would not do: one that the interrupted code held would make the handler's access a panic (@bare-smp).

== The rule about interrupts

The scheduler runs with interrupts off, and tasks with them on. A tick while the scheduler moves tasks in and out of the run queue would call `yield_now` from no task at all, or re-enter the queue's `RefCell`. So every way out of a task leaves interrupts off, and every way back in turns them on:

- a preempted task leaves through `on_tick`, where the trap turned them off, and returns through the trap's return, which restores them;
- a new task starts with them off and turns them on first;
- a task that finishes turns them off before it returns;
- a task that yields or sleeps turns them off around the call, and they come back on when it is resumed and leaves the critical section.

The first three are in `spawn` (and in `run`, for the main task), and the fourth is `yield_now` and `sleep_ms`:

#srcfile("schedulers/preempt/src/lib.rs", lines: ((52, 60), (68, 75),), notes: ((53, [`spawn` pushes on the run queue, so it runs with interrupts off too.]), (57, [Back to the scheduler's state before `task_entry` switches away.]), (74, [`sleep_ms` yields inside the critical section, and `without_interrupts` restores the state when the task is resumed.]),))[```rust
pub fn spawn<B: TimerInterrupt>(func: impl FnOnce() + 'static) {
    B::without_interrupts(|| {
        runtime::spawn(move || {
            B::interrupts_on();
            func();
            B::interrupts_off();
        })
    });
}
// …
pub fn yield_now<B: TimerInterrupt>() {
    B::without_interrupts(task::yield_now);
}

/// Sleep from a preemptible task, preserving the interrupt state around the switch.
pub fn sleep_ms<B: TimerInterrupt>(ms: u64) {
    B::without_interrupts(|| task::sleep_ms(ms));
}
```]

== Exercised by

The `preempt` kernel (@crate-preempt) runs four workers that never yield and one that sleeps, on all three architectures, and judges its own run:

#output(title: "cargo xtask kernel --kernel preempt --target riscv64")[
```
all tasks finished after 181 switches: 44 preemptions, 61 idle waits
time slices per worker: [6, 10, 14, 18, 71]
PASS: 44 preemptions; every busy worker ran in several slices, and the sleeper used 55 ms of CPU in 70 wakeups
```
]

`preemptions` is the 44. Each busy worker's slices but its last ended in a preemption, 5 + 9 + 13 + 17 = 44, and the sleeper's 71 slices each ended in a sleep or its finish. @crate-preempt walks through the rest of the run.

== Limits

- *One CPU*, as for @crate-scheduler-coop.
- *Round robin with a fixed slice*, the same for every task. A task that wakes goes to the back of the queue.
- *A task is preempted only while its interrupts are on.* One that loops inside `without_interrupts` keeps the CPU for good.
- *With nothing runnable and a sleeper, the scheduler spins* with interrupts off, in the loop of @crate-scheduler-coop, instead of waiting for an interrupt.
- *A `Global` that a task reads with interrupts on, and the hook or handler also uses, can panic.* If the tick falls inside the task's few instructions of `get` or `set`, the task is switched away holding the borrow flag, and the next access from the hook finds it taken. `in_task` itself reads `CURRENT`, a `Global`, so a task that calls it with interrupts on races `on_tick`'s own call. Nothing has hit this in a recorded run; the window is a few instructions in a 100 ms slice.
- *`run` leaves interrupts off* when it returns, whatever they were before.
- *Hardware.* This code has run only in QEMU, whose timings are not a machine's.

A timer, a six-line handler, and one rule about interrupts. @crate-coop is the first kernel built on these schedulers.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: c4132c8086fa
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
