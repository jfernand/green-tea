#import "../lib.typ": *

= green-tea-rt <crate-rt>

`green-tea-rt` decides which task runs next under a *fixed-priority* policy: every task has a number from 0 up, and the highest-numbered ready task always runs. Tasks of equal priority take turns in the order they became ready. The crate holds only the decision, not the context switch: it never touches a register, a stack or a timer, so it builds and tests on the host. Its sizes are fixed when it is compiled, and no call allocates. The same file holds a schedulability test, which says whether a set of periodic tasks can always meet its deadlines, and a counter of missed deadlines.

It is phase 2 of the realtime plan in #repo("realtime.md"), with parts of phases 3 and 7 (see Limits for what that plan asks and this crate doesn't do yet). @crate-rt-bare runs it on bare metal.

#spec(
  ("Source", [#repo("schedulers/rt/src/lib.rs"), 1002 lines, 730 without its tests]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-rt-bare` (@crate-rt-bare), `green-tea-rt-kernel` (@crate-rt-kernel)]),
  ("Tests", [10 host tests; the `rt` kernel in QEMU]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Scheduler<N, P, M>`], [the policy's whole state: `N` task records, `P` priorities (at most 64, default 64), `M` mutexes (default `N`); `new` refuses impossible sizes with `InvalidConfiguration`],
  [`TaskId`, `MutexId`], [a task's or a mutex's slot number, a `u16`; `TaskId::from_index` and `index` convert to and from `usize`],
  [`TaskState`], [`Free`, `Ready`, `Running`, `Sleeping`, `WaitingEvent`, `WaitingMutex`],
  [`register(priority)`], [takes a free slot and makes it `Ready`],
  [`next_ready()`], [removes the highest-priority ready task and marks it `Running`],
  [`yield_task(id)`], [puts the running task back at the end of its priority's queue],
  [`finish(id)`], [frees the slot; refused with `TaskHoldsMutex` while the task owns a mutex],
  [`sleep_until(id, now, deadline)`], [blocks the running task until an absolute tick value],
  [`wake_expired(now)`, `next_deadline()`], [readies the sleepers that are due; the earliest deadline still waiting, for the next timer],
  [`wait_event(id, event)`, `signal_event(event)`], [64 events, each a bit; a signal with no waiter is kept until the next wait],
  [`create_mutex()`, `lock(id, m)`, `unlock(id, m)`], [mutexes with priority inheritance and deadlock refusal],
  [`state`, `base_priority`, `effective_priority`], [what the policy knows about one task],
  [`ready_len`, `timer_len`, `live_task_count`, `highest_ready_priority`], [counts, and the priority `next_ready` would pick],
  [`Error`], [`InvalidConfiguration`, `Capacity`, `NoReadyTask`, `InvalidTask`, `InvalidState`, `InvalidPriority`, `InvalidMutex`, `InvalidEvent`, `NotOwner`, `Deadlock`, `TaskHoldsMutex`],
  [`TaskSpec`], [a periodic task for admission: `priority`, `execution`, `period`, `deadline`, `blocking`],
  [`admit_fixed_priority(tasks, interference)`], [response-time analysis: `Ok` if every task meets its deadline, else an `AdmissionError`],
  [`AdmissionError`], [`InvalidTaskSet`, `DuplicatePriority`, `ArithmeticOverflow`, `Unschedulable { priority }`, `Inconclusive { priority }`],
  [`DeadlineStats`], [`jobs` and `misses`; `report_completion(completed_at, deadline)` counts one job, and a miss if it ended late],
)

== Choosing the next task

The scheduler keeps one first-in, first-out queue per priority, and a 64-bit *bitmap* with bit `p` set when queue `p` has a task in it. The queues are linked lists threaded through the task records (`next_ready`), so they need no memory of their own:

#srcfile("schedulers/rt/src/lib.rs", lines: ((95, 107),), notes: ((97, [`M` mutex slots, of which the first `mutex_count` exist.]), (99, [The head and tail of each priority's queue.]), (102, [A counter stamped on a task each time it becomes ready, to rebuild the queues in order.]), (104, [A binary heap of sleepers, earliest deadline first.]), (106, [One bit per event that was signalled with no one waiting.]),))[```rust
pub struct Scheduler<const N: usize, const P: usize = 64, const M: usize = N> {
    tasks: [Task; N],
    mutexes: [Option<Mutex>; M],
    mutex_count: usize,
    ready_head: [Option<TaskId>; P],
    ready_tail: [Option<TaskId>; P],
    ready_bitmap: u64,
    ready_sequence: u64,
    wait_sequence: u64,
    timers: [Timer; N],
    timer_count: usize,
    pending_events: u64,
}
```]

The highest ready priority is the highest set bit. `leading_zeros` is one instruction on all three architectures (`lzcnt` or `bsr` on x86_64, `clz` on aarch64, a short sequence on riscv64 without the bit-manipulation extension), so picking a task costs the same however many tasks there are:

#srcfile("schedulers/rt/src/lib.rs", lines: ((152, 170),), notes: ((156, [With bits 1 and 7 set, `leading_zeros` is 56 and the priority is 7.]), (164, [The queue is now empty: clear its bit.]),))[```rust
pub fn next_ready(&mut self) -> Result<TaskId, Error> {
    if self.ready_bitmap == 0 {
        return Err(Error::NoReadyTask);
    }
    let priority = (63
        - self
            .ready_bitmap
            .leading_zeros()) as usize;
    let id = self.ready_head[priority].ok_or(Error::InvalidState)?;
    let task = &mut self.tasks[id.index()];
    self.ready_head[priority] = task.next_ready;
    task.next_ready = None;
    if self.ready_head[priority].is_none() {
        self.ready_tail[priority] = None;
        self.ready_bitmap &= !(1u64 << priority);
    }
    task.state = TaskState::Running;
    Ok(id)
}
```]

The bitmap is a `u64`, which is why `P` is at most 64. The runtime in @crate-rt-bare uses 8. The test `priority_bitmap_and_fifo_order` registers priorities 1, 7, 7, 1 and checks that they come out as the two 7s in order, then the two 1s.

== Timers

A task that sleeps gives an absolute deadline in the caller's clock ticks, not a delay. The policy keeps sleepers in a binary heap stored in the array `timers`, ordered by deadline and then by task number, so that two equal deadlines always wake in the same order. A heap has the earliest deadline at index 0, and inserting or removing costs about $log_2 N$ swaps: 4 for the runtime's 16 tasks. There is at most one timer per task, so the array never fills before the tasks do.

#srcfile("schedulers/rt/src/lib.rs", lines: ((195, 210), (214, 227),), notes: ((197, [A deadline already past makes the task ready again at once.]), (208, [Moves the new timer up the heap until its parent is earlier.]), (216, [Only the root can be due; stop at the first that isn't.]),))[```rust
pub fn sleep_until(&mut self, id: TaskId, now: u64, deadline: u64) -> Result<(), Error> {
    self.require_state(id, TaskState::Running)?;
    if deadline <= now {
        self.enqueue(id);
        return Ok(());
    }
    if self.timer_count == N {
        return Err(Error::Capacity);
    }
    self.tasks[id.index()].state = TaskState::Sleeping;
    let index = self.timer_count;
    self.timer_count += 1;
    self.timers[index] = Timer { deadline, task: id };
    self.sift_up(index);
    Ok(())
}
// …
pub fn wake_expired(&mut self, now: u64) -> usize {
    let mut woken = 0;
    while self.timer_count != 0 && self.timers[0].deadline <= now {
        let timer = self.pop_timer();
        let task = &self.tasks[timer
            .task
            .index()];
        if task.state == TaskState::Sleeping {
            self.enqueue(timer.task);
            woken += 1;
        }
    }
    woken
}
```]

`next_deadline` reads the root, so that the runtime can arm the hardware timer for exactly the next wakeup instead of ticking at a fixed rate.

== Events

An *event* is a bit number from 0 to 63. `wait_event` blocks the running task on one; `signal_event` readies every task waiting on it and returns how many. A signal that finds no waiter sets the event's bit in `pending_events`, and the next `wait_event` on it consumes the bit and returns `true` without blocking. A device interrupt that arrives before its task starts waiting is therefore not lost, but two such signals count as one. Both calls finish their change of state before they return, so a runtime that keeps interrupts off from its check to its switch can't lose a wakeup in between.

== Mutexes and priority inheritance

*Priority inversion* is the failure that mutexes in a fixed-priority system invite. A low-priority task holds a lock; a high-priority task waits for it; a medium-priority task that needs neither is ready, so it runs instead of the low one, and the high task waits for as long as the medium one computes. The standard fix is *priority inheritance*: while a task holds a lock that others wait for, it runs at the highest priority among them. Each task has a `base_priority`, the one it was registered with, and an `effective_priority`, the one it is queued at.

#srcfile("schedulers/rt/src/lib.rs", lines: ((285, 311),), notes: ((288, [A free mutex: take it and return `true`.]), (298, [Locking a mutex you hold, or closing a cycle of waiters, is refused instead of hanging.]), (307, [A stamp, so that waiters of equal priority get the mutex in arrival order.]), (308, [Every owner's priority again, from scratch.]),))[```rust
pub fn lock(&mut self, id: TaskId, mutex: MutexId) -> Result<bool, Error> {
    self.require_state(id, TaskState::Running)?;
    let index = self.mutex_index(mutex)?;
    let Some(owner) = self.mutexes[index]
        .expect("initialized mutex")
        .owner
    else {
        self.mutexes[index]
            .as_mut()
            .expect("initialized mutex")
            .owner = Some(id);
        return Ok(true);
    };
    if owner == id || self.would_deadlock(id, owner) {
        return Err(Error::Deadlock);
    }
    self.wait_sequence = self
        .wait_sequence
        .wrapping_add(1);
    let task = &mut self.tasks[id.index()];
    task.state = TaskState::WaitingMutex;
    task.waiting_mutex = Some(mutex);
    task.wait_sequence = self.wait_sequence;
    self.recompute_priorities();
    self.rebuild_ready_queues();
    Ok(false)
}
```]

`would_deadlock` follows the chain from the owner: the mutex it waits for, that mutex's owner, and so on, for at most `N` steps. If the chain comes back to the task asking, the lock would never be granted, and `lock` returns `Deadlock`.

`recompute_priorities` resets every task to its base priority, then raises each owner to the highest effective priority among the tasks waiting for its mutexes, and repeats until nothing changes. The repetition makes inheritance *transitive*: if a priority-7 task waits for a mutex held by a priority-2 task, which waits for a mutex held by a priority-1 task, the first pass raises the middle task to 7 and the second raises the last one to 7. A task's place in a queue depends on its priority, so `rebuild_ready_queues` then empties every queue and refills it, sorted by the `ready_sequence` stamps so that tasks of equal priority keep their order.

`unlock` gives the mutex straight to the waiter with the highest effective priority, the earliest among equals, makes it ready, and recomputes. The old owner drops back to whatever it still inherits from other mutexes, or its base. The test `mutex_inheritance_propagates_transitively_and_restores` builds the chain above and checks every priority on the way up and on the way down.

== Admission

`admit_fixed_priority` answers, before anything runs, whether a set of periodic tasks will always meet its deadlines. Each `TaskSpec` releases a job every `period` ticks, needs at most `execution` ticks of CPU per job, must finish within `deadline` ticks of its release, and may wait at most `blocking` ticks for a lower-priority task's lock. `interference` is the caller's bound on scheduler and interrupt work per job.

The test is *response-time analysis*. A task's worst response time $R$ is its own work plus everything that can delay it. Each higher-priority task $j$ releases $ceil(R \/ T_j)$ jobs within $R$, of $C_j$ ticks each:

$ R = C + B + I + sum_(j in "higher") ceil(R / T_j) C_j $

$R$ is on both sides, so the code starts from $R = C + B + I$ and recomputes until the value stops changing. If it ever exceeds the deadline, the set is refused:

#srcfile("schedulers/rt/src/lib.rs", lines: ((663, 689),), notes: ((665, [64 rounds at most; a value still moving after that gives `Inconclusive`.]), (671, [The ceiling of `response / period` in integers.]), (684, [A fixed point: `response` is the worst case.]),))[```rust
let mut response = base;
let mut converged = false;
for _ in 0..64 {
    let mut next = base;
    for higher in tasks
        .iter()
        .filter(|higher| higher.priority > task.priority)
    {
        let jobs = response / higher.period + u64::from(response % higher.period != 0);
        let interference = jobs
            .checked_mul(higher.execution)
            .ok_or(AdmissionError::ArithmeticOverflow)?;
        next = next
            .checked_add(interference)
            .ok_or(AdmissionError::ArithmeticOverflow)?;
    }
    if next > task.deadline {
        return Err(AdmissionError::Unschedulable {
            priority: task.priority,
        });
    }
    if next == response {
        converged = true;
        break;
    }
    response = next;
}
```]

The tests and the `rt` kernel use two sets. In the first, a priority-7 task runs 2 ticks every 10, and a priority-3 task runs 4 every 20 with 1 tick of blocking; interference is 1. The high task's response is $2 + 1 = 3$. The low task starts from $4 + 1 + 1 = 6$, then $6 + ceil(6\/10) dot 2 = 8$, then 8 again: it finishes within 8 of its 20 ticks, and the set is admitted. In the second, a priority-7 task runs 4 every 5, and a priority-3 task runs 4 every 8. The low task goes $4$, $4 + ceil(4\/5) dot 4 = 8$, $4 + ceil(8\/5) dot 4 = 12$, and 12 is past its deadline of 8: `Unschedulable { priority: 3 }`.

The analysis is sound only for the model it states: one CPU, independent tasks, deadlines no longer than periods, and distinct priorities (`DuplicatePriority` otherwise). Its answer is only as good as the `execution` numbers, and the doc comment says so: a measured execution time is not a safe bound.

`DeadlineStats` is the run-time half: `report_completion` counts a job, and a miss when it completed after its absolute deadline (equal is on time). Both counters use checked additions, so a counter that would wrap returns `ArithmeticOverflow` instead.

== Exercised by

The 10 host tests cover the queue order, the capacity and priority checks, timers with equal deadlines, events signalled before and after the wait, transitive inheritance, deadlock refusal, both admission sets, the refusals for a deadline longer than its period, duplicate priorities and overflow, and deadline accounting.

#output(title: "cargo test -p green-tea-rt")[
```
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The `rt` kernel (@crate-rt-kernel) runs the policy under @crate-rt-bare on riscv64, aarch64 and x86_64 in QEMU.

== Limits

- No code from this crate has run on the physical laptop. Its host tests and the `rt` kernel in QEMU are its only evidence, and the plan's phase 2 passes only on the laptop.
- Choosing the next task is constant-time, but other calls scan. `register`, `signal_event`, `ready_len`, `live_task_count` and `finish`'s mutex check look at every slot; `lock`, `unlock` and `finish` call `recompute_priorities` (up to `N` passes over `M` mutexes, each scanning `N` tasks) and `rebuild_ready_queues` (an insertion sort of up to `N` tasks). All of these are bounded by the sizes and none allocates, but they grow with `N`: the plan's "nothing scans in the scheduler" holds only for `next_ready`.
- The policy has no notion of time slices or CPUs: one instance schedules one CPU, and the caller decides when to preempt.
- An event is one bit: signals that arrive while no one waits are merged into one.
- Admission and the deadline counters are separate from scheduling. Nothing ties a `TaskSpec` to a registered task, refuses a task at `register`, or reports a missed deadline by itself; the plan's phase 7 asks for all three.
- There is no earliest-deadline-first policy, which the plan leaves for later.

One bitmap, one heap, and an inheritance rule that holds across chains. @crate-rt-bare gives the policy stacks, a timer and real context switches.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: dde78cad8aa1
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
