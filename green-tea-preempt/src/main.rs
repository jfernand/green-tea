//! A bare-metal kernel whose tasks never yield. A timer interrupt ends each task's time
//! slice, and the handler switches to the next task with the same `yield_now` the
//! cooperative kernel's tasks call themselves. The ramfb display shows it: every worker
//! grows its bar in a busy loop, and each band of shading is one time slice.
//!
//! `demo` needs `Board: TimerInterrupt`, which every board implements. Run it with
//! `cargo xtask kernel --kernel preempt`.
//!
//! How a preemption works:
//! 1. The scheduler arms the timer for one time slice, then resumes a task.
//! 2. When the slice is up the CPU traps into green-tea-bare's trap entry, on the task's
//!    stack. It saves every register there, and calls `on_tick`, which calls `yield_now`.
//! 3. `yield_now` is the ordinary context switch back to the scheduler. The interrupted
//!    task's state is now in two layers on its stack: the trap frame (everything it was
//!    using) under `on_tick`'s frame, with the callee-saved registers in its `Context`.
//! 4. Later the scheduler resumes it: `swap_context` returns into `on_tick`, which returns
//!    to the trap entry, which restores the frame and returns from the trap (`sret`,
//!    `eret`, `iretq`) into the busy loop, as if nothing had happened.
//!
//! The one rule that makes this safe: the scheduler only runs with interrupts off, and
//! tasks only with them on. See [`spawn_preemptible`].

#![no_std]
#![no_main]

use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::runtime::{Switch, run, spawn};
use green_tea_bare::screen::{BAR_MAX, COLORS, COLUMNS, Painter, Screen};
use green_tea_bare::task::{in_task, sleep_ms, yield_now};
use green_tea_bare::{Board, Global, Platform, TimerInterrupt, boot, delay_ms, println};

/// Called by the platform's `_start`, on the boot stack, with .bss zeroed.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let fb = boot("green-tea-preempt");
    demo::<Board>(fb);
    println!("halting");
    Board::halt()
}

// ---- Preemption -----------------------------------------------------------------------

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

/// `spawn`, for a task that may be preempted at any instruction.
///
/// The scheduler runs with interrupts off: a tick while it's moving tasks in and out of
/// the run queue would call `yield_now` from no task at all, or re-enter the queue's
/// `RefCell`. So every way out of a task leaves interrupts off, and every way back in
/// turns them on again:
/// - a preempted task leaves through `on_tick` (the trap turned them off), and comes back
///   through the trap entry's `sret` (which turns them on);
/// - a new task starts here, with interrupts off, and turns them on first thing;
/// - a task that finishes turns them off before it returns.
///
/// `spawn` itself runs with interrupts off too, since it pushes on the run queue.
fn spawn_preemptible<B: TimerInterrupt>(f: impl FnOnce() + 'static) {
    B::without_interrupts(|| {
        spawn(move || {
            B::interrupts_on();
            f();
            B::interrupts_off();
        })
    });
}

/// `sleep_ms`, from a preemptible task. Like any other way out of a task, it must leave
/// with interrupts off; it comes back with them off too, and turns them on again.
fn sleep_preemptible<B: TimerInterrupt>(ms: u64) {
    B::without_interrupts(|| sleep_ms(ms));
}

// ---- The demo -------------------------------------------------------------------------
//
// Five columns, one per worker. Workers 0–3 grow their bars a few pixels at a time, with
// a short busy-wait between steps, and never yield. Each time slice they get is drawn in a
// new shade, so the bands show where the timer cut in.
//
// Worker 4, which worker 0 spawns part way through, is different: between steps it sleeps.
// It wakes, draws, and goes back to sleep, so it uses almost no CPU, and the scheduler
// doesn't give it slices it would only waste. Once it's the only one left, the scheduler
// idles between its steps (the status bar shows an empty box).

/// Busy steps for workers 0–3, each `STEP_PX` tall and taking `STEP_MS` of busy-waiting:
/// the longest takes about 1.7 s, or 17 slices.
const STEPS: [usize; COLUMNS - 1] = [50, 90, 130, 170];
const STEP_MS: u64 = 10;
const STEP_PX: usize = 2;
const SPAWN_AFTER_STEP: usize = 30;

/// Worker 4: this many steps, sleeping `SLEEP_MS` after each.
const SLEEPER_STEPS: usize = 70;
const SLEEP_MS: u64 = 60;
const SLEEPER: usize = COLUMNS - 1;

const _: () = assert!(STEP_PX * 170 <= BAR_MAX && STEP_PX * SLEEPER_STEPS <= BAR_MAX);

/// Per task id (0 is main, 1..=5 the workers): time slices given, and CPU time used, in
/// ticks. Only the scheduler's hook writes them, while every task is paused.
static SLICES: Global<[usize; COLUMNS + 1]> = Global::new([0; COLUMNS + 1]);
static CPU: Global<[u64; COLUMNS + 1]> = Global::new([0; COLUMNS + 1]);
/// The task that's running and when it started, for `CPU`.
static RUNNING: Global<Option<(usize, u64)>> = Global::new(None);

/// Charge the running task for the time since it was resumed.
fn account(now: u64) {
    if let Some((id, since)) = RUNNING.replace(None) {
        let mut cpu = CPU.get();
        cpu[id] += now - since;
        CPU.set(cpu);
    }
}

fn demo<B: TimerInterrupt>(fb: Framebuffer) {
    let mut screen = Screen::new(fb);
    let painter = screen.painter();
    println!(
        "running {} busy workers that never yield ({} ms of work, {SLICE_MS} ms slices) and one \
         that sleeps {SLEEP_MS} ms between steps",
        COLUMNS - 1,
        STEPS.iter().sum::<usize>() as u64 * STEP_MS,
    );

    B::init_timer(on_tick);
    static SWITCHES: Global<usize> = Global::new(0);
    static IDLES: Global<usize> = Global::new(0);
    run(
        move || {
            for column in 0..COLUMNS - 1 {
                spawn_preemptible::<B>(move || busy_worker::<B>(painter, column));
            }
        },
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
    );
    account(B::ticks());
    B::set_deadline(u64::MAX);

    let switches = SWITCHES.get();
    let preemptions = PREEMPTIONS.get();
    screen.finish(switches);
    let slices = SLICES.get();
    let cpu_ms = CPU.get().map(|t| t / B::ticks_per_ms());
    println!(
        "all tasks finished after {switches} switches: {preemptions} preemptions, {} idle waits",
        IDLES.get()
    );
    println!("time slices per worker: {:?}", &slices[1..]);
    println!("CPU ms per worker:      {:?}", &cpu_ms[1..]);

    // Without preemption every busy worker would run to the end in its first slice. With
    // it, each needs several: even the shortest has 500 ms of work. (Not exactly 5, because
    // `delay_ms` counts time spent preempted: a step that was cut off finishes as soon as
    // it's resumed.)
    let sleeper_ms = cpu_ms[SLEEPER + 1];
    let sleeper_budget = SLEEPER_STEPS as u64 * SLEEP_MS / 10;
    if let Some(c) = (0..COLUMNS - 1).find(|&c| slices[c + 1] < 2) {
        println!("FAIL: worker {c} ran in only {} slice(s)", slices[c + 1]);
    } else if slices[SLEEPER + 1] < SLEEPER_STEPS {
        println!("FAIL: the sleeper woke {} times for {SLEEPER_STEPS} steps", slices[SLEEPER + 1]);
    } else if sleeper_ms > sleeper_budget {
        // Sleeping must not cost CPU: well under a tenth of the time it spent asleep.
        println!("FAIL: the sleeper used {sleeper_ms} ms of CPU (budget {sleeper_budget} ms)");
    } else {
        println!(
            "PASS: {preemptions} preemptions; every busy worker ran in several slices, and the \
             sleeper used {sleeper_ms} ms of CPU in {SLEEPER_STEPS} wakeups"
        );
    }
}

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

/// The worker's color, alternating between full and dim with each new time slice.
fn shade(column: usize) -> u32 {
    let color = COLORS[column];
    if SLICES.get()[column + 1] % 2 == 1 {
        color
    } else {
        let channel = |shift: u32| (((color >> shift) & 0xff) * 2 / 3) << shift;
        channel(16) | channel(8) | channel(0)
    }
}
