#import "../lib.typ": *

= green-tea-rt-kernel <crate-rt-kernel>

`green-tea-rt-kernel` is the kernel that puts @crate-rt and @crate-rt-bare to work. It checks the admission test on two task sets, then runs six tasks through events, a mutex and the classic priority-inversion scenario, and prints `PASS` only if every step happened. xtask builds it as the `rt` kernel, for riscv64, aarch64 and x86_64, and for the `pc` board.

#spec(
  ("Source", [#repo("kernels/rt/src/main.rs"), 164 lines]),
  ("Depends on", [`green-tea-bare` (@crate-bare), `green-tea-rt` (@crate-rt), `green-tea-rt-bare` (@crate-rt-bare)]),
  ("Used by", [nothing: it is a binary]),
  ("Tests", [the `rt` kernel, which judges its own run]),
)

The crate has no public interface. Its default features are `green` and `white`, the one profile combination that builds the realtime runtime (see @crate-rt-bare); `board-pc` selects the `pc` board. A `build.rs` passes `green-tea-bare`'s linker script to the linker, as every kernel's does.

== Before the runtime starts

`kmain` boots, then calls `check_admission_and_deadline_reporting`, which runs `admit_fixed_priority` on the two task sets worked through in @crate-rt: the first is admitted, the second refused with `Unschedulable { priority: 3 }`. It then reports a job that completed at tick 11 against a deadline of 10, and checks that `DeadlineStats` counted one miss. These sets are synthetic: they don't describe the tasks this kernel runs.

Then `rt::run::<Board>(main_task, 1, 1)` starts the runtime with one task at priority 1 and a 1 ms slice, and returns when every task has finished.

== The scenario

The main task does all the setup. Every task it spawns outranks it, so each one runs as soon as it is spawned, until it finishes or blocks:

#srcfile("kernels/rt/src/main.rs", lines: ((108, 119),), notes: ((109, [No one waits for event 9 yet: the signal is latched.]), (110, [Priority 2 runs at once, and its `wait_event(9)` returns without blocking.]), (111, [Priority 3 blocks on event 10.]), (112, [Wakes it, and switches to it before returning.]), (116, [Takes the mutex and sleeps for 50 ms holding it.]), (117, [Blocks on the mutex; the low task now runs at priority 7.]), (118, [Spins until the high task has finished.]),))[```rust
fn main_task() {
    rt::signal_event::<Board>(9).expect("latch event before waiter creation");
    rt::spawn::<Board>(2, early_event_waiter).expect("spawn early event waiter");
    rt::spawn::<Board>(3, blocked_event_waiter).expect("spawn blocked event waiter");
    rt::signal_event::<Board>(10).expect("wake blocked event waiter");

    let lock = rt::create_mutex::<Board>().expect("allocate bounded mutex");
    LOCK.set(Some(lock));
    rt::spawn::<Board>(2, low_task).expect("spawn low task");
    rt::spawn::<Board>(7, high_task).expect("spawn high waiter");
    rt::spawn::<Board>(4, medium_task).expect("spawn medium interferer");
}
```]

The last three tasks are the priority-inversion scenario of @crate-rt. When the medium task starts, the low task (base priority 2) is asleep holding the mutex, the high task (7) is blocked on it, and the medium task (4) spins until the high task finishes. The medium task is alone at its priority, so its 1 ms slices end and it is picked again; the main task, at 1, waits below it.

After 50 ms the tick wakes the low task. Without inheritance it would be ready at 2, below the spinning medium task, and would never run: the high task would wait forever, and the kernel would hang until xtask's time limit. With inheritance it is ready at 7, so the tick preempts the medium task for it:

#srcfile("kernels/rt/src/main.rs", lines: ((136, 150),), notes: ((140, [Returns after the tick that wakes it preempts the medium task.]), (141, [7, inherited from the high task, not its own 2.]), (147, [Hands the mutex to the high task, drops back to 2, and switches to the high task before returning.]),))[```rust
fn low_task() {
    rt::lock::<Board>(lock_id()).expect("low task locks mutex");
    LOW_ACQUIRED.store(true, Ordering::Release);
    println!("rt: low priority holds mutex and sleeps");
    rt::sleep_ms::<Board>(50).expect("low task sleeps until timer deadline");
    let inherited = rt::current_effective_priority::<Board>().expect("low task remains current");
    assert_eq!(
        inherited, 7,
        "high waiter priority must be inherited before unlock"
    );
    LOW_INHERITED.store(true, Ordering::Release);
    rt::unlock::<Board>(lock_id()).expect("low task releases mutex");
    LOW_RELEASED.store(true, Ordering::Release);
    println!("rt: inherited owner released mutex");
}
```]

The high task takes the mutex, releases it, sets `HIGH_COMPLETED` and finishes. The medium task sees the flag and finishes; then the low task, back at priority 2, prints its last line; then the main task finishes, and `run` returns. The output shows that order: the high task's line comes before the low task's second line, although the low task released the mutex first.

`kmain` then asserts eight things: the low task took the mutex, inherited priority 7, and released it; the high task completed; the latched event was consumed; the blocked waiter was woken; the medium task ran at least one loop; and `preemptions()` is not zero. The last holds because of the tick that switched from the medium task to the low one: a tick that picks the same task again isn't counted.

== Exercised by

#output(title: "cargo xtask kernel --kernel rt --target riscv64 --headless")[
```
green-tea-rt on riscv64
…
rt: synthetic admission accepted/rejected and deadline miss reported
rt: low priority holds mutex and sleeps
rt: medium-priority interferer started
rt: high-priority waiter completed
rt: inherited owner released mutex
PASS: fixed priorities, admission, timer wakeup, transitive inheritance; max interrupts-off 14639 ticks
```
]

aarch64 and x86_64 print the same lines and differ only in the last number. So does the `pc` board in QEMU, under UEFI firmware:

#output(title: "cargo xtask kernel --board pc --kernel rt --target x86_64 --headless")[
```
green-tea-rt on x86_64 (pc)
framebuffer: 1280x800 XRGB8888
…
PASS: fixed priorities, admission, timer wakeup, transitive inheritance; max interrupts-off 3262966 ticks
```
]

The interrupts-off figure is in each board's own clock: 14639 ticks of riscv64's 10 MHz timer, 79486 on aarch64, 122208 on x86_64, and 3262966 TSC ticks on the `pc` board. The runs were in QEMU, which emulates the CPU in software and shares the host with other work, so the figures show that the counter works, not how long a real CPU keeps interrupts off.

== Limits

- The `rt` kernel has run only in QEMU. It has not run on the physical laptop, so by the plan's rule phase 2 (a high-priority task preempts a busy one) and phase 3 (the priority-inversion scenario no longer stalls) have not passed, although this kernel is the test for both.
- The kernel never has every task asleep at once: the main task stays ready while the low task sleeps. The hang that @crate-rt-bare's Limits describe for that case is therefore not exercised.
- "transitive inheritance" in the `PASS` line overstates this kernel: its scenario has one mutex and one level of inheritance. The transitive case, a chain through two mutexes, is checked only by @crate-rt's host tests.
- No device interrupt takes part: both events are signalled by the main task. The plan's phase 2 also asks for "a task waiting for a key wakes while another sleeps", which this kernel doesn't do.
- The admission check runs on synthetic task sets, and no task here has a declared period or deadline.

Six tasks, one mutex, and a hang that inheritance prevents. @crate-latency measures the timer underneath.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: 3ede91b3a03b
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
