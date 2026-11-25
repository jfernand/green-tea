//! Timer-preemptive scheduling on top of [`green_tea_scheduler_coop`].
//!
//! The scheduler runs with interrupts off. Tasks start and resume with interrupts on, and
//! return to the scheduler with them off, so task queues and task state cannot be re-entered
//! by a timer handler.

#![no_std]

use core::sync::atomic::{AtomicUsize, Ordering};
use green_tea_bare::TimerInterrupt;
use green_tea_scheduler_coop::{runtime, task};

pub use green_tea_scheduler_coop::runtime::Switch;

static PREEMPTIONS: AtomicUsize = AtomicUsize::new(0);

/// Run `main` and its tasks with timer preemption, giving each runnable task `slice_ms`
/// milliseconds before switching away. `on_switch` runs with interrupts off before each
/// task is resumed and before each idle wait.
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

/// Number of timer ticks that switched away from a running task in the last `run`.
pub fn preemptions() -> usize {
    PREEMPTIONS.load(Ordering::Relaxed)
}

/// Add a task whose body may be preempted at any instruction.
pub fn spawn<B: TimerInterrupt>(func: impl FnOnce() + 'static) {
    B::without_interrupts(|| {
        runtime::spawn(move || {
            B::interrupts_on();
            func();
            B::interrupts_off();
        })
    });
}

/// Whether execution is currently on a task stack rather than the boot stack.
pub fn in_task() -> bool {
    task::in_task()
}

/// Yield from a preemptible task, preserving the interrupt state around the switch.
pub fn yield_now<B: TimerInterrupt>() {
    B::without_interrupts(task::yield_now);
}

/// Sleep from a preemptible task, preserving the interrupt state around the switch.
pub fn sleep_ms<B: TimerInterrupt>(ms: u64) {
    B::without_interrupts(|| task::sleep_ms(ms));
}

fn on_tick<B: TimerInterrupt>() {
    if task::in_task() {
        PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        yield_now::<B>();
    }
}
