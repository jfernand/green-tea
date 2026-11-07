//! A bare-metal kernel for QEMU that runs green-tea's cooperative tasks and draws every
//! context switch on a ramfb display. Runs on riscv64 and aarch64 (`virt`) and x86_64
//! (`q35`): `cargo xtask kernel --kernel coop`.
//!
//! Tasks switch only when they call `yield_now`: no interrupts, one CPU. Everything else
//! (boot, drivers, tasks, scheduler) is green-tea-bare; this is just the demo.

#![no_std]
#![no_main]

use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::runtime::{run, spawn};
use green_tea_bare::screen::{COLORS, COLUMNS, Painter, Screen};
use green_tea_bare::task::yield_now;
use green_tea_bare::{Board, Global, Platform, boot, delay_ms, println};

/// Called by the platform's `_start`, on the boot stack, with .bss zeroed.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let fb = boot("green-tea-coop");
    demo(fb);
    println!("halting");
    Board::halt()
}

// ---- The demo -------------------------------------------------------------------------
//
// Five columns, one per worker task. Each time a worker runs it grows its bar by one step
// and yields. Just before each resume, the scheduler's hook moves the highlight to the
// task about to run, bumps the switch counter, and waits a moment so the switches are
// slow enough to watch.

const STEP: usize = 20; // bar growth per turn
const SWITCH_DELAY_MS: u64 = 150;

/// Turns per worker. Worker 0 spawns worker 4 after its third turn.
const TURNS: [usize; COLUMNS] = [6, 10, 14, 18, 8];
const SPAWN_AFTER_TURN: usize = 3;

fn demo(fb: Framebuffer) {
    let mut screen = Screen::new(fb);

    // The main task only spawns the first four workers, so it is resumed once.
    let expected = 1 + TURNS.iter().sum::<usize>();
    println!("running {COLUMNS} workers; expecting {expected} switches");

    let painter = screen.painter();
    static SWITCHES: Global<usize> = Global::new(0);
    run(
        move || {
            for column in 0..COLUMNS - 1 {
                spawn(move || worker(painter, column));
            }
        },
        |id| {
            let switches = SWITCHES.get() + 1;
            SWITCHES.set(switches);
            screen.switch_to(id, switches);
            delay_ms(SWITCH_DELAY_MS);
        },
    );

    let switches = SWITCHES.get();
    screen.finish(switches);
    println!("all tasks finished after {switches} switches (expected {expected})");
    // The line cargo xtask kernel looks for.
    if switches == expected {
        println!("PASS: {switches} switches, as expected");
    } else {
        println!("FAIL: {switches} switches, expected {expected}");
    }
}

fn worker(mut painter: Painter, column: usize) {
    for turn in 1..=TURNS[column] {
        if turn > 1 {
            yield_now(); // the first turn starts as soon as the task is first resumed
        }
        painter.bar(column, (turn - 1) * STEP + 2, turn * STEP, COLORS[column]);
        painter.label(column, turn);

        if column == 0 && turn == SPAWN_AFTER_TURN {
            spawn(move || worker(painter, COLUMNS - 1));
        }
    }
}
