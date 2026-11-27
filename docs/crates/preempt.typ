#import "../lib.typ": *

= green-tea-preempt <crate-preempt>

`green-tea-preempt` is a kernel whose tasks never yield. A timer interrupt ends each task's time slice, and the display shows each slice as a band of shading in a worker's bar. The preemption comes from @crate-scheduler-preempt; this crate is the demo that puts it on screen and judges it. It runs on all three architectures.

#spec(
  ("Source", [#repo("kernels/preempt/src/main.rs"), 190 lines]),
  ("Depends on", [`green-tea-scheduler-preempt` (@crate-scheduler-preempt), for `run`, `spawn` and `sleep_ms`; `green-tea-bare` (@crate-bare), for `TimerInterrupt` and the screen]),
  ("Used by", [nothing: it is a binary]),
  ("Tests", [the `preempt` kernel, which judges its own run, on riscv64, aarch64 and x86_64]),
)

The crate has no public interface. It has `kmain`, `demo`, two kinds of worker, and `shade`. How a timer tick becomes a switch, and the rule about interrupts that makes it safe, are in @crate-scheduler-preempt.

== The demo

Workers 0 to 3 grow their bars 2 pixels (`STEP_PX`) at a time, with 10 ms (`STEP_MS`) of busy-waiting between steps, and never yield. They do 50, 90, 130 and 170 steps. Worker 0 spawns worker 4 after its step 30 (`SPAWN_AFTER_STEP`). `spawn` and `sleep_ms` come from @crate-scheduler-preempt: they make the new task preemptible, and keep interrupts off around the sleep:

#srcfile("kernels/preempt/src/main.rs", lines: ((161, 179),), notes: ((165, [The timer can cut in anywhere in here.]), (177, [The sleeper uses almost no CPU, and the scheduler gives it no slices while it sleeps.]),))[```rust
fn busy_worker<B: TimerInterrupt>(mut painter: Painter, column: usize) {
    for step in 1..=STEPS[column] {
        painter.bar(column, (step - 1) * STEP_PX, step * STEP_PX, shade(column));
        painter.label(column, step);
        delay_ms(STEP_MS); // "work": the timer can cut in anywhere in here

        if column == 0 && step == SPAWN_AFTER_STEP {
            spawn::<B>(move || sleeper::<B>(painter));
        }
    }
}

fn sleeper<B: TimerInterrupt>(mut painter: Painter) {
    for step in 1..=SLEEPER_STEPS {
        painter.bar(SLEEPER, (step - 1) * STEP_PX, step * STEP_PX, shade(SLEEPER));
        painter.label(SLEEPER, step);
        sleep_ms::<B>(SLEEP_MS);
    }
}
```]

`shade` draws with the worker's colour on odd slices and a dimmed one on even slices, so the bands in a bar are the slices it ran in. Everything else happens in the scheduler's hook, with interrupts off. It charges the outgoing task for its time and draws; @crate-scheduler-preempt starts the new slice after the hook returns, so that drawing doesn't eat into it:

#srcfile("kernels/preempt/src/main.rs", lines: ((97, 124),), notes: ((100, [The main task spawns the four busy workers and finishes.]), (103, [`SLICE_MS` is 100.]), (107, [`account` adds the time since the task was resumed to its entry in `CPU`.]), (119, [Nothing is runnable. The scheduler then spins until the first sleeper is due.]),))[```rust
run::<B>(
    move || {
        for column in 0..COLUMNS - 1 {
            spawn::<B>(move || busy_worker::<B>(painter, column));
        }
    },
    SLICE_MS,
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
            }
            Switch::Idle => {
                IDLES.set(IDLES.get() + 1);
                screen.idle(switches);
            }
        }
    },
```]

== How it is exercised

The kernel judges itself. Each busy worker must have run in at least two slices, the sleeper must have woken for each of its 70 steps, and it must have used under 420 ms of CPU, a tenth of the time it spent asleep:

#output(title: "cargo xtask kernel --kernel preempt --target riscv64")[
```
running 4 busy workers that never yield (4400 ms of work, 100 ms slices) and one that sleeps 60 ms between steps
all tasks finished after 181 switches: 44 preemptions, 61 idle waits
time slices per worker: [6, 10, 14, 18, 71]
CPU ms per worker:      [503, 902, 1303, 1704, 59]
PASS: 44 preemptions; every busy worker ran in several slices, and the sleeper used 59 ms of CPU in 70 wakeups
```
]

The counts add up. The four busy workers took 6, 10, 14 and 18 slices, and all but the last of each ended in a preemption: 5 + 9 + 13 + 17 = 44. The 181 switches are those 48 slices, the sleeper's 71, one for the main task, and 61 idle waits. The sleeper's 71 is one resume per step, and one more to finish.

A worker's work is timed by the clock, so how many slices it needs depends on how fast QEMU ran it, and the counts differ between runs and boards. The verdict doesn't depend on them:

#output(title: "cargo xtask kernel --kernel preempt --target aarch64, then x86_64")[
```
CPU ms per worker:      [503, 902, 1303, 1705, 41]
PASS: 44 preemptions; every busy worker ran in several slices, and the sleeper used 41 ms of CPU in 70 wakeups
CPU ms per worker:      [509, 914, 1320, 1727, 35]
PASS: 44 preemptions; every busy worker ran in several slices, and the sleeper used 35 ms of CPU in 70 wakeups
```
]

== Limits

- One CPU, a fixed slice of 100 ms, and round robin, as @crate-scheduler-preempt has them.
- *`shade` reads `SLICES`, a `Global`, from a task with interrupts on, and the hook writes it.* A tick that fell inside that read would leave the borrow flag taken, and the hook's next access would panic (@crate-scheduler-preempt). No recorded run has hit it.
- The counts and CPU times depend on how fast QEMU ran, and only the verdict is checked. They are not a machine's timings.
- It has run only in QEMU.

Five bars, a band per slice, and a verdict. @crate-shell goes back to one task, and gives it a keyboard.
// chapter-status:begin
// stage: research-done
// revision: 3
// body: 8937a03efb69
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
