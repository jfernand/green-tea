#import "../lib.typ": *

= green-tea-preempt <crate-preempt>

`green-tea-preempt` is a kernel whose tasks never yield. A timer interrupt ends each task's time slice, and the handler switches to the next task with the same `yield_now` that the cooperative kernel's tasks call themselves. It runs on all three architectures, and the display shows each time slice as a band of shading in a worker's bar.

#spec(
  ("Source", [#repo("green-tea-preempt/src/main.rs"), 233 lines]),
  ("Depends on", [`green-tea-bare` (@crate-bare), for `TimerInterrupt`, tasks and the screen]),
  ("Used by", [nothing: it is a binary]),
  ("Tests", [the `preempt` kernel, which judges its own run, on riscv64, aarch64 and x86_64]),
)

The crate has no public interface. It has `kmain`, a handler, two wrappers around `green-tea-bare`'s scheduling calls, and a demo.

== A preemption

The handler is short:

#srcfile("green-tea-preempt/src/main.rs", lines: ((43, 54),), notes: ((50, [A tick that arrives outside a task has nothing to preempt.]), (52, [The ordinary context switch. It returns when the scheduler next resumes this task.]),))[```rust
const SLICE_MS: u64 = 100;

static PREEMPTIONS: Global<usize> = Global::new(0);

/// The timer handler: the current task's slice is up, so switch away from it. Runs with
/// interrupts off, on the task's stack.
fn on_tick() {
    if in_task() {
        PREEMPTIONS.set(PREEMPTIONS.get() + 1);
        yield_now(); // back to the scheduler; returns when this task is next resumed
    }
}
```]

A preemption goes through four steps:

+ The scheduler arms the timer for one slice, and resumes a task.
+ The slice ends. The CPU traps into `green-tea-bare`'s entry *on the task's stack*, which saves every register there and calls `on_tick`, and `on_tick` calls `yield_now`.
+ `yield_now` switches to the scheduler. The task's state is now in two layers: the trap frame on its stack, holding everything it was using, and the callee-saved registers of `on_tick`'s frame in its `Context`.
+ Later the scheduler resumes it. `swap_context` returns into `on_tick`, which returns to the trap entry, which restores the frame and returns from the trap (`sret`, `eret` or `iretq`) into the middle of whatever the task was doing.

#layers(caption: [A preempted task's stack, highest address first],
  ([The task's own frames], [`busy_worker`, `delay_ms`: stopped at an arbitrary instruction], "light"),
  ([Trap frame], [every register: 544 bytes on riscv64, 800 on aarch64, 20 words on x86_64], "amber"),
  ([`on_tick`, `yield_now`], [ordinary frames, down to the `swap_context` call], "dark"),
)

== The rule about interrupts

The scheduler runs with interrupts off, and tasks with them on. A tick while the scheduler moves tasks in and out of the run queue would call `yield_now` from no task at all, or re-enter the queue's `RefCell`. So every way out of a task leaves interrupts off, and every way back in turns them on:

- a preempted task leaves through `on_tick`, where the trap turned them off, and returns through the trap's return, which restores them;
- a new task starts with them off and turns them on first;
- a task that finishes turns them off before it returns;
- a task that sleeps turns them off around the call, and they come back on when it is resumed and leaves the critical section.

The first three are in `spawn_preemptible`, and the fourth is `sleep_preemptible`:

#srcfile("green-tea-preempt/src/main.rs", lines: ((68, 76), (80, 82),), notes: ((69, [`spawn` pushes on the run queue, so it runs with interrupts off too.]), (73, [Back to the scheduler's state before `task_entry` switches away.]), (81, [`sleep_ms` yields inside the critical section, and `without_interrupts` restores the state when the task is resumed.]),))[```rust
fn spawn_preemptible<B: TimerInterrupt>(f: impl FnOnce() + 'static) {
    B::without_interrupts(|| {
        spawn(move || {
            B::interrupts_on();
            f();
            B::interrupts_off();
        })
    });
}
// …
fn sleep_preemptible<B: TimerInterrupt>(ms: u64) {
    B::without_interrupts(|| sleep_ms(ms));
}
```]

== The demo

Workers 0 to 3 grow their bars 2 pixels (`STEP_PX`) at a time, with 10 ms (`STEP_MS`) of busy-waiting between steps, and never yield. They do 50, 90, 130 and 170 steps. Worker 0 spawns worker 4 after its step 30 (`SPAWN_AFTER_STEP`):

#srcfile("green-tea-preempt/src/main.rs", lines: ((204, 222),), notes: ((208, [The timer can cut in anywhere in here.]), (220, [The sleeper uses almost no CPU, and the scheduler gives it no slices while it sleeps.]),))[```rust
fn busy_worker<B: TimerInterrupt>(mut painter: Painter, column: usize) {
    for step in 1..=STEPS[column] {
        painter.bar(column, (step - 1) * STEP_PX, step * STEP_PX, shade(column));
        painter.label(column, step);
        delay_ms(STEP_MS); // "work": the timer can cut in anywhere in here

        if column == 0 && step == SPAWN_AFTER_STEP {
            spawn_preemptible::<B>(move || sleeper::<B>(painter));
        }
    }
}

fn sleeper<B: TimerInterrupt>(mut painter: Painter) {
    for step in 1..=SLEEPER_STEPS {
        painter.bar(SLEEPER, (step - 1) * STEP_PX, step * STEP_PX, shade(SLEEPER));
        painter.label(SLEEPER, step);
        sleep_preemptible::<B>(SLEEP_MS);
    }
}
```]

`shade` draws with the worker's colour on odd slices and a dimmed one on even slices, so the bands in a bar are the slices it ran in. Everything else happens in the scheduler's hook, with interrupts off. It charges the outgoing task for its time, draws, and only then starts the new slice, so that drawing doesn't eat into it:

#srcfile("green-tea-preempt/src/main.rs", lines: ((144, 166),), notes: ((147, [`account` adds the time since the task was resumed to its entry in `CPU`.]), (158, [`SLICE_MS` is 100.]), (163, [Nothing is runnable: no deadline. The scheduler then spins until the first sleeper is due.]),))[```rust
|switch| {
    // The scheduler, with interrupts off. Draw first, then start the slice, so
    // the drawing doesn't eat into it.
    account(B::ticks());
    let switches = SWITCHES.get() + 1;
    SWITCHES.set(switches);
    match switch {
        Switch::To(id) => {
            let mut slices = SLICES.get();
            slices[id] += 1;
            SLICES.set(slices);
            screen.switch_to(id, switches);
            let now = B::ticks();
            RUNNING.set(Some((id, now)));
            B::set_deadline(now + SLICE_MS * B::ticks_per_ms());
        }
        Switch::Idle => {
            IDLES.set(IDLES.get() + 1);
            screen.idle(switches);
            B::set_deadline(u64::MAX);
        }
    }
},
```]

== How it is exercised

The kernel judges itself. Each busy worker must have run in at least two slices, the sleeper must have woken for each of its 70 steps, and it must have used under 420 ms of CPU, a tenth of the time it spent asleep:

#output(title: "cargo xtask kernel --kernel preempt --target riscv64")[
```
running 4 busy workers that never yield (4400 ms of work, 100 ms slices) and one that sleeps 60 ms between steps
all tasks finished after 182 switches: 47 preemptions, 59 idle waits
time slices per worker: [6, 11, 15, 19, 71]
CPU ms per worker:      [580, 1014, 1447, 1906, 158]
PASS: 47 preemptions; every busy worker ran in several slices, and the sleeper used 158 ms of CPU in 70 wakeups
```
]

The counts add up. The four busy workers took 6, 11, 15 and 19 slices, and all but the last of each ended in a preemption: 5 + 10 + 14 + 18 = 47. The 182 switches are those 51 slices, the sleeper's 71, one for the main task, and 59 idle waits. The sleeper's 71 is one resume per step, and one more to finish.

A worker's work is timed by the clock, so how many slices it needs depends on how fast QEMU ran it, and the counts differ between runs and boards. The verdict doesn't depend on them:

#output(title: "cargo xtask kernel --kernel preempt --target aarch64, then x86_64")[
```
CPU ms per worker:      [553, 1026, 1431, 1905, 70]
PASS: 48 preemptions; every busy worker ran in several slices, and the sleeper used 70 ms of CPU in 70 wakeups
CPU ms per worker:      [513, 920, 1328, 1737, 43]
PASS: 44 preemptions; every busy worker ran in several slices, and the sleeper used 43 ms of CPU in 70 wakeups
```
]

== Limits

- One CPU. The scheduler, the tasks and every `Global` here belong to the boot CPU.
- Round robin with a fixed slice of 100 ms: no priorities, and a task that wakes goes to the back of the queue.
- A task is preempted only while its interrupts are on. One that loops inside `without_interrupts` keeps the CPU for good.
- The main task is started by `run` with interrupts off, and isn't preemptible. It only spawns the first four workers.
- With nothing runnable the scheduler spins (`runtime.rs:57` in `green-tea-bare`) instead of waiting for an interrupt.

A timer, a short handler, and one rule about interrupts. @crate-shell goes back to one task, and gives it a keyboard.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: a3199af6c956
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
