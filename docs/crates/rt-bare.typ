#import "../lib.typ": *

= green-tea-rt-bare <crate-rt-bare>

`green-tea-rt-bare` runs the policy of @crate-rt on a real CPU. It gives each task a stack and a saved register set, arms the timer for the next moment anything is due, and switches straight from one task to the next. Tasks are plain functions (`fn()`), and their number is fixed: the runtime reserves room for 16 tasks, 8 priorities and 8 mutexes when it starts, and takes nothing from the heap afterwards. It is generic over `green-tea-bare`'s `TimerInterrupt` trait (@crate-bare), so the same code runs on all three architectures.

#spec(
  ("Source", [#repo("schedulers/rt-bare/src/lib.rs"), 534 lines]),
  ("Depends on", [`green-tea-rt` (@crate-rt), the policy; `green-tea-bare` (@crate-bare), for the timer, stacks and `Global`; `green-tea-arch` (@crate-arch), for `Context` and `swap_context`]),
  ("Used by", [`green-tea-rt-kernel` (@crate-rt-kernel)]),
  ("Tests", [none of its own (it builds only for bare-metal targets); the `rt` kernel in QEMU]),
)

The crate has the same profile features as `green-tea-bare`: `green` (the default), `black`, `white` and `board-pc`, each passed through. Its `white` feature also turns on `green-tea-bare`'s `rt-runtime` feature, which marks the kernel as having the `REALTIME_EXECUTION` capability. `green-tea-bare` refuses to build `rt-runtime` without `white`, and refuses `black` with `rt-runtime` ("Black+White runtime integration is not implemented"), so the only combination that builds a realtime kernel is `green` with `white`.

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`MAX_TASKS`, `PRIORITIES`, `MAX_MUTEXES`], [16, 8 and 8: the sizes of the `Scheduler` inside],
  [`TaskEntry`], [`fn()`: what a task runs; when it returns, the task is finished],
  [`run::<B>(main, priority, slice_ms)`], [starts the runtime with one task, and returns when every task has finished],
  [`spawn::<B>(priority, entry)`], [adds a task from inside a running one; switches to it at once if it outranks the caller],
  [`yield_now::<B>()`], [goes to the back of the caller's priority queue],
  [`sleep_until::<B>(deadline)`, `sleep_ms::<B>(ms)`], [blocks until an absolute tick value, or for a number of milliseconds],
  [`wait_event::<B>(event)`, `signal_event::<B>(event)`], [the policy's events; a signal that readies a higher-priority task switches to it before returning],
  [`create_mutex::<B>()`, `lock::<B>(m)`, `unlock::<B>(m)`], [the policy's mutexes, with priority inheritance; an unlock that readies a higher-priority waiter switches to it],
  [`current_effective_priority::<B>()`], [the calling task's priority, raised by any inheritance],
  [`preemptions()`], [how many timer ticks switched tasks],
  [`max_interrupts_off_ticks()`], [the longest stretch the runtime kept interrupts off, in the board's clock ticks],
)

Every function takes the board `B` as a type parameter, as `green-tea-bare`'s own scheduling calls do, and every call except `run` must come from a task.

== The runtime's state

`run` builds one `Runtime`: the policy, one `Slot` per task (its saved `Context`, its `Stack`, its entry function and a `pending_reap` flag), the `boot` context, the `current` task and the slice length in ticks. It is boxed once, before the timer starts, and the pointer is kept in a `Global`. Each task's stack comes from `Stack::try_take`, which hands out a slot of the linker-reserved stack pool (@crate-pool), with a guard page below it; no task creation touches the heap. A pool with no free slot makes `spawn` return `Error::Capacity`.

All access goes through `with_runtime`, which turns interrupts off for the closure, asserts that it runs on CPU 0, and measures how long the closure took. The runtime runs only on the boot CPU, and with interrupts off nothing else can reach the state, so a plain `&mut` is safe there.

== Switching tasks

The round-robin runtime of @crate-scheduler-coop switches through the scheduler: the old task switches to the scheduler's context, which picks a task and switches to it. The realtime plan lists that as an obstacle, because it doubles the cost of every switch. `switch_to_next` picks the next task and switches to it directly:

#srcfile("schedulers/rt-bare/src/lib.rs", lines: ((128, 136), (153, 168),), notes: ((128, [The policy's constant-time choice; no ready task means the boot context runs.]), (153, [The earliest sleeper's deadline or the end of the new task's slice, whichever comes first.]), (160, [One-shot: the timer fires once, at that tick.]), (166, [Returns when some later switch picks this task again.]), (168, [The resumed task frees any finished task's stack.]),))[```rust
let next = match runtime
    .policy
    .next_ready()
{
    Ok(id) => Some(id),
    Err(Error::NoReadyTask) => None,
    Err(error) => panic!("RT scheduler selection failed: {error:?}"),
};
runtime.current = next;
// …
    let deadline = runtime
        .policy
        .next_deadline()
        .unwrap_or(u64::MAX)
        .min(slice);
    (from, to, same, deadline)
});
B::set_deadline(deadline);
record_interrupts_off(B::ticks().saturating_sub(span.started_at));
let switched = !same;
if switched {
    // SAFETY: task contexts and their stacks remain pinned in the global runtime; all
    // scheduling state is stable until the switch returns to this context.
    unsafe { swap_context(from, to) };
    let resumed_at = B::ticks();
    reap_pending::<B>();
```]

The timer is never periodic. It is armed for one moment: the end of the running task's slice, or the next sleeper's deadline if that is sooner. When nothing is ready, the slice is `u64::MAX` and only a sleeper can arm it.

The *boot context* is the code that called `run`. It is resumed whenever no task is ready, and it is where `run` returns once the last task has finished.

A task that returns can't free its own stack while it is still running on it. `task_entry` calls `finish` on the policy, sets the slot's `pending_reap` and switches away; whichever context runs next calls `reap_pending`, which drops the `Stack` and returns it to the pool.

== Preemption

The timer handler is `tick`. It wakes every sleeper whose deadline has passed, sends the running task to the back of its queue, and switches:

#srcfile("schedulers/rt-bare/src/lib.rs", lines: ((177, 199),), notes: ((182, [Due sleepers become ready, each at its priority.]), (187, [A tick that arrives in the boot context has no task to move.]), (196, [The same switch as a voluntary yield; it counts as a preemption only if another task was chosen.]),))[```rust
fn tick<B: TimerInterrupt>() {
    let span = disable_interrupts::<B>();
    with_runtime::<B, _>(|runtime| {
        runtime
            .policy
            .wake_expired(B::ticks());
        if let Some(current) = runtime.current {
            if runtime
                .policy
                .state(current)
                == Ok(TaskState::Running)
            {
                runtime
                    .policy
                    .yield_task(current)
                    .expect("timer preempts a running task");
            }
        }
    });
    if switch_to_next::<B>(span) {
        PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
    }
}
```]

Because the policy picks the highest priority, a slice's end changes tasks only among equals: a busy task alone at its priority is picked again. A higher-priority task preempts at once when it becomes ready, on any of four paths: a tick that wakes it, `spawn`, `signal_event` and `unlock`. The last three compare `highest_ready_priority` with the caller's effective priority, and yield if the new task outranks it.

== Interrupts off

A realtime system has to know how long it can ignore an interrupt. `disable_interrupts` returns an `InterruptSpan` with the previous interrupt state and the tick at which it was turned off; `restore_interrupts` records the length of the span and restores the state. `with_runtime` and `switch_to_next` record their own spans too. `record_interrupts_off` keeps the maximum in an atomic, read by `max_interrupts_off_ticks`. This is the measurement phase 3 of the plan asks for, limited to the runtime's own code: the time the trap entry or a device handler spends with interrupts off isn't counted.

== Exercised by

The `rt` kernel (@crate-rt-kernel) runs every public function of this crate, on riscv64, aarch64 and x86_64 in QEMU and on the `pc` board in QEMU, and checks the order of events.

== Limits

- This runtime has run only in QEMU. None of it has run on the physical laptop, and the plan's phases 2 and 3 pass only there.
- Boot CPU only: every entry point asserts CPU 0. There is no per-CPU instance, and no pinning or isolation (phase 6).
- The sizes are constants: 16 tasks, 8 priorities, 8 mutexes. A kernel can't choose others without editing the crate.
- Interrupts-off times are in the board's clock ticks, not a common unit, and QEMU's values say nothing about hardware.
- *A hang when every task sleeps at once, found by reading the code.* When no task is ready, `run`'s loop calls `B::wait_for_interrupt()` with interrupts off. Only the `pc` board's version turns them on around a `hlt`; the default, used by the riscv64, aarch64 and x86_64 QEMU boards, is a `spin_loop` hint that returns with interrupts still off. The timer interrupt can then never run `tick`, nothing wakes the sleepers, and `run` loops forever. The `rt` kernel never reaches that state, because its main task, at priority 1, stays ready while the others sleep.
- *A late wakeup on the `pc` board, also from reading.* The `pc` board's `wait_for_interrupt` arms its own backup deadline, 2 ms after the halt when USB is present and 50 ms when it isn't, replacing the deadline the runtime had armed for the next sleeper. A sleeper due sooner than that wakes when the backup fires. Neither problem shows in any recorded run.
- The scans of @crate-rt's Limits run with interrupts off, so they lengthen the interrupts-off spans as `N` grows.
- Device interrupts can't preempt a task through this runtime yet: no handler calls `signal_event`. The doc comment on `signal_event` sets the rule for when one does: acknowledge the device first, because the call may switch tasks before it returns.

A fixed pool of stacks, a one-shot timer and one switch per decision. @crate-rt-kernel puts all of it through a priority-inversion scenario.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: d1404f5f1997
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
