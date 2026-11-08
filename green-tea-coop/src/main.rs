//! A bare-metal kernel for QEMU that runs green-tea's cooperative tasks and draws every
//! context switch on a ramfb display. Runs on riscv64 and aarch64 (`virt`) and x86_64
//! (`q35`): `cargo xtask kernel --kernel coop`.
//!
//! Tasks switch only when they call `yield_now`: no interrupts, one CPU. Everything else
//! (boot, drivers, tasks, scheduler) is green-tea-bare; this is just the demo.
//!
//! With the fw_cfg file `opt/green-tea/test` set to "overflow", it runs a test instead: a
//! task recurses until it overflows its stack, which on a board with guard pages must
//! fault in that task's guard page (`cargo xtask kernel --kernel overflow`).

#![no_std]
#![no_main]

use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::runtime::{Switch, run, spawn};
use green_tea_bare::screen::{COLORS, COLUMNS, Painter, Screen};
use green_tea_bare::task::{current_guard, yield_now};
use green_tea_bare::{Board, Global, Platform, boot, delay_ms, fw_cfg, println, stack};

/// Called by the platform's `_start`, on the boot stack, with .bss zeroed.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let fb = boot("green-tea-coop");
    let mut test = [0u8; 16];
    let len = fw_cfg::read_file("opt/green-tea/test", &mut test).unwrap_or(0);
    match &test[..len] {
        b"" => demo(fb),
        b"overflow" => overflow_test(fb),
        other => panic!("unknown test {:?}", core::str::from_utf8(other)),
    }
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
        |switch| {
            let Switch::To(id) = switch else { return }; // no task here sleeps, so never idle
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

// ---- The overflow test ----------------------------------------------------------------
//
// The bare-metal twin of green-tea-v2's `overflow_hits_the_guard_page`: a task recurses
// until its stack runs out. Passing needs the fault, and at an address in that task's
// own guard page: without guard pages it would run on into the next slot down, silently.

/// The overflowing task's guard page, and how deep it got.
static GUARD: Global<(usize, usize)> = Global::new((0, 0));
static DEPTH: Global<u64> = Global::new(0);

fn overflow_test(fb: Framebuffer) {
    if !Board::GUARD_PAGES {
        println!("FAIL: {} has no guard pages", Board::NAME);
        return;
    }
    static SCREEN: Global<Option<Framebuffer>> = Global::new(None);
    SCREEN.set(Some(fb));
    stack::on_overflow(|addr| {
        let (start, end) = GUARD.get();
        let depth = DEPTH.get();
        if let Some(fb) = SCREEN.get() {
            Screen::new(fb).finish(depth as usize); // green, with the depth reached
        }
        if (start..end).contains(&addr) {
            println!("PASS: overflowed after {depth} frames, into the task's guard page at {addr:#x}");
        } else {
            println!("FAIL: faulted at {addr:#x}, outside the task's guard page {start:#x}..{end:#x}");
        }
        Board::halt()
    });
    println!("overflowing a {} KiB task stack", stack::STACK_SIZE / 1024);
    run(
        || {
            spawn(|| {
                let guard = current_guard();
                GUARD.set((guard.start, guard.end));
                recurse(0);
                println!("FAIL: recursed without faulting");
            })
        },
        |_| {},
    );
}

/// Recurse with a 256-byte frame (well under a page, so it can't skip the guard page)
/// that stays live across the call, so the recursion can't become a loop.
#[inline(never)]
fn recurse(depth: u64) -> u64 {
    DEPTH.set(depth);
    let frame = core::hint::black_box([depth as u8; 256]);
    if depth == u64::MAX {
        return 0;
    }
    recurse(depth + 1) + u64::from(frame[0])
}
