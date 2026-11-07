//! A bare-metal kernel whose tasks never yield. A timer interrupt ends each task's time
//! slice, and the handler switches to the next task with the same `yield_now` the
//! cooperative kernel's tasks call themselves. The ramfb display shows it: every worker
//! grows its bar in a busy loop, and each band of shading is one time slice.
//!
//! riscv64 only so far: `demo` needs `Board: TimerInterrupt`, which only riscv64's board
//! implements. Run it with `cargo xtask kernel --kernel preempt`.
//!
//! How a preemption works:
//! 1. The scheduler arms the timer for one time slice, then resumes a task.
//! 2. When the slice is up the CPU traps into green-tea-bare's trap entry, on the task's
//!    stack. It saves every register there, and calls `on_tick`, which calls `yield_now`.
//! 3. `yield_now` is the ordinary context switch back to the scheduler. The interrupted
//!    task's state is now in two layers on its stack: the trap frame (everything it was
//!    using) under `on_tick`'s frame, with the callee-saved registers in its `Context`.
//! 4. Later the scheduler resumes it: `swap_context` returns into `on_tick`, which returns
//!    to the trap entry, which restores the frame and `sret`s into the busy loop, as if
//!    nothing had happened.
//!
//! The one rule that makes this safe: the scheduler only runs with interrupts off, and
//! tasks only with them on. See [`spawn_preemptible`].

#![no_std]
#![no_main]

use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::runtime::{run, spawn};
use green_tea_bare::screen::{BAR_MAX, COLORS, COLUMNS, Painter, Screen};
use green_tea_bare::task::{in_task, yield_now};
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

// ---- The demo -------------------------------------------------------------------------
//
// Five columns, one per worker. A worker grows its bar a few pixels at a time, with a
// short busy-wait between steps, and never yields. Each time slice it gets is drawn in
// a new shade, so the bands show where the timer cut in. Worker 0 spawns worker 4 part way
// through, to show that spawning works from a preemptible task too.

/// Steps per worker, each `STEP_PX` tall and taking `STEP_MS` of busy-waiting: the longest
/// takes about 1.7 s, or 17 slices.
const STEPS: [usize; COLUMNS] = [50, 90, 130, 170, 70];
const STEP_MS: u64 = 10;
const STEP_PX: usize = 2;
const SPAWN_AFTER_STEP: usize = 30;

const _: () = assert!(STEP_PX * 170 <= BAR_MAX);

/// Time slices each task has been given, by task id (0 is main, 1..=5 the workers). Only
/// the scheduler's hook writes it, while every task is paused.
static SLICES: Global<[usize; COLUMNS + 1]> = Global::new([0; COLUMNS + 1]);

fn demo<B: TimerInterrupt>(fb: Framebuffer) {
    let mut screen = Screen::new(fb);
    let painter = screen.painter();
    println!(
        "running {COLUMNS} workers that never yield; {SLICE_MS} ms slices, {} ms of work",
        STEPS.iter().sum::<usize>() as u64 * STEP_MS
    );

    B::init_timer(on_tick);
    static SWITCHES: Global<usize> = Global::new(0);
    run(
        move || {
            for column in 0..COLUMNS - 1 {
                spawn_preemptible::<B>(move || worker::<B>(painter, column));
            }
        },
        |id| {
            // The scheduler, with interrupts off. Draw first, then start the slice, so
            // the drawing doesn't eat into it.
            let switches = SWITCHES.get() + 1;
            SWITCHES.set(switches);
            let mut slices = SLICES.get();
            slices[id] += 1;
            SLICES.set(slices);
            screen.switch_to(id, switches);
            B::set_deadline(B::ticks() + SLICE_MS * B::ticks_per_ms());
        },
    );
    B::set_deadline(u64::MAX);

    let switches = SWITCHES.get();
    let preemptions = PREEMPTIONS.get();
    screen.finish(switches);
    let slices = SLICES.get();
    println!("all tasks finished after {switches} switches, {preemptions} of them preemptions");
    println!("time slices per worker: {:?}", &slices[1..]);

    // Without preemption every worker would run to the end in its first slice. With it,
    // each needs several: even the shortest has 500 ms of work. (Not exactly 5, because
    // `delay_ms` counts time spent preempted: a step that was cut off finishes as soon as
    // it's resumed.)
    let short = (0..COLUMNS).find(|&c| slices[c + 1] < 2);
    match short {
        None => println!("PASS: {preemptions} preemptions; every worker ran in several slices"),
        Some(c) => println!("FAIL: worker {c} ran in only {} slice(s)", slices[c + 1]),
    }
}

fn worker<B: TimerInterrupt>(mut painter: Painter, column: usize) {
    let id = column + 1;
    for step in 1..=STEPS[column] {
        // Alternate the shade with each new slice.
        let color = if SLICES.get()[id] % 2 == 1 { COLORS[column] } else { dim(COLORS[column]) };
        painter.bar(column, (step - 1) * STEP_PX, step * STEP_PX, color);
        painter.label(column, step);
        delay_ms(STEP_MS); // "work": the timer can cut in anywhere in here

        if column == 0 && step == SPAWN_AFTER_STEP {
            spawn_preemptible::<B>(move || worker::<B>(painter, COLUMNS - 1));
        }
    }
}

/// Two thirds of each channel.
fn dim(color: u32) -> u32 {
    let channel = |shift: u32| (((color >> shift) & 0xff) * 2 / 3) << shift;
    channel(16) | channel(8) | channel(0)
}
