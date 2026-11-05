//! A bare-metal kernel for QEMU that runs green-tea's cooperative tasks and draws every
//! context switch on a ramfb display. Runs on riscv64 and aarch64 (`virt`) and x86_64
//! (`q35`): `cargo xtask kernel`.
//!
//! There is no paging (beyond x86_64's identity map), no interrupts, and one CPU. The
//! context switch itself is green-tea-v2's arch layer, compiled here unchanged. Everything
//! machine-specific (boot code, serial port, fw_cfg registers, timer) is in `platform`.

#![no_std]
#![no_main]

extern crate alloc;

#[cfg_attr(target_arch = "riscv64", path = "../../green-tea-v2/src/arch/riscv64.rs")]
#[cfg_attr(target_arch = "aarch64", path = "../../green-tea-v2/src/arch/aarch64.rs")]
#[cfg_attr(target_arch = "x86_64", path = "../../green-tea-v2/src/arch/x86_64.rs")]
mod arch;
#[cfg_attr(target_arch = "riscv64", path = "platform/riscv64.rs")]
#[cfg_attr(target_arch = "aarch64", path = "platform/aarch64.rs")]
#[cfg_attr(target_arch = "x86_64", path = "platform/x86_64.rs")]
mod platform;

mod console;
mod fw_cfg;
mod global;
mod heap;
mod ramfb;
mod runtime;
mod task;

use core::panic::PanicInfo;
use global::Global;
use ramfb::{Framebuffer, HEIGHT, WIDTH};
use runtime::{run, spawn};
use task::yield_now;

/// Called by each platform's `_start`, on the boot stack, with .bss zeroed.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    platform::init();
    println!("green-tea-kernel on {}", platform::NAME);

    if let Err(e) = fw_cfg::probe() {
        panic!("{e}");
    }
    println!("fw_cfg files:");
    fw_cfg::for_each_file(|f| println!("  {:#06x} {:6} {}", f.select, f.size, f.name()));

    let fb = match Framebuffer::init() {
        Ok(fb) => fb,
        Err(e) => panic!("{e}"),
    };
    println!("ramfb: {WIDTH}x{HEIGHT} XRGB8888");

    demo(fb);

    println!("halting");
    platform::halt()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("panic: {info}");
    platform::halt()
}

// ---- The demo -------------------------------------------------------------------------
//
// Five columns, one per worker task. Each time a worker runs it grows its bar by one step
// and yields. Just before each resume, the scheduler's hook outlines the column of the
// task about to run, bumps the switch counter in the status bar, and waits a moment so
// the switches are slow enough to watch.

const STATUS_H: usize = 60;
const COLUMNS: usize = 5;
const COL_W: usize = WIDTH / COLUMNS;
const BORDER: usize = 4;
const INSET: usize = 12;
const STEP: usize = 20; // bar growth per turn
const SWITCH_DELAY_MS: u64 = 150;

const BACKGROUND: u32 = 0x202428;
const STATUS_BG: u32 = 0x30363c;
const DONE_BG: u32 = 0x2e5a3a;
const IDLE_BORDER: u32 = 0x3c444c;
const ACTIVE_BORDER: u32 = 0xffffff;
const TEXT: u32 = 0xe8e8e8;
const COLORS: [u32; COLUMNS] = [0xe0605a, 0xe8b44a, 0x6cc070, 0x4fa3e0, 0xb07ce0];

/// Turns per worker. Worker 0 spawns worker 4 after its third turn.
const TURNS: [usize; COLUMNS] = [6, 10, 14, 18, 8];
const SPAWN_AFTER_TURN: usize = 3;

fn demo(mut fb: Framebuffer) {
    fb.fill_rect(0, 0, WIDTH, HEIGHT, BACKGROUND);
    fb.fill_rect(0, 0, WIDTH, STATUS_H, STATUS_BG);
    for column in 0..COLUMNS {
        outline_column(&mut fb, column, IDLE_BORDER);
    }

    // The main task only spawns the first four workers, so it is resumed once.
    let expected = 1 + TURNS.iter().sum::<usize>();
    println!("running {COLUMNS} workers; expecting {expected} switches");

    static SWITCHES: Global<usize> = Global::new(0);
    static PREVIOUS: Global<Option<usize>> = Global::new(None);
    run(
        move || {
            for column in 0..COLUMNS - 1 {
                spawn(move || worker(fb, column));
            }
        },
        move |id| {
            // Task ids follow creation order: 0 is main, workers 0..3 are 1..4, and the
            // worker spawned later is 5. So worker `column` has id `column + 1`.
            let column = id.checked_sub(1);
            if let Some(previous) = PREVIOUS.replace(column) {
                outline_column(&mut fb, previous, IDLE_BORDER);
            }
            if let Some(column) = column {
                outline_column(&mut fb, column, ACTIVE_BORDER);
            }
            let switches = SWITCHES.get() + 1;
            SWITCHES.set(switches);
            draw_status(&mut fb, STATUS_BG, switches, Some(id));
            delay_ms(SWITCH_DELAY_MS);
        },
    );

    if let Some(previous) = PREVIOUS.get() {
        outline_column(&mut fb, previous, IDLE_BORDER);
    }
    let switches = SWITCHES.get();
    draw_status(&mut fb, DONE_BG, switches, None);
    println!("all tasks finished after {switches} switches (expected {expected})");
}

fn worker(mut fb: Framebuffer, column: usize) {
    let turns = TURNS[column];
    for turn in 1..=turns {
        if turn > 1 {
            yield_now(); // the first turn starts as soon as the task is first resumed
        }
        let x = column * COL_W + INSET;
        let bottom = HEIGHT - INSET;
        fb.fill_rect(x, bottom - turn * STEP, COL_W - 2 * INSET, STEP - 2, COLORS[column]);
        // The turn number at the top of the column.
        fb.fill_rect(x, STATUS_H + INSET, COL_W - 2 * INSET, 20, BACKGROUND);
        fb.draw_number(x, STATUS_H + INSET, turn, 4, TEXT);

        if column == 0 && turn == SPAWN_AFTER_TURN {
            spawn(move || worker(fb, COLUMNS - 1));
        }
    }
}

fn outline_column(fb: &mut Framebuffer, column: usize, color: u32) {
    fb.outline(column * COL_W, STATUS_H, COL_W, HEIGHT - STATUS_H, BORDER, color);
}

/// The switch count on the left; on the right, the id of the task about to run.
fn draw_status(fb: &mut Framebuffer, background: u32, switches: usize, task: Option<usize>) {
    fb.fill_rect(0, 0, WIDTH, STATUS_H, background);
    fb.draw_number(16, 15, switches, 6, TEXT);
    if let Some(id) = task {
        fb.fill_rect(WIDTH - 60, 15, 36, 30, COLORS[(id + COLUMNS - 1) % COLUMNS]);
        fb.draw_number(WIDTH - 54, 20, id, 4, BACKGROUND);
    }
}

/// Busy-wait on the platform's timer.
fn delay_ms(ms: u64) {
    let ticks = ms * platform::ticks_per_ms();
    let start = platform::ticks();
    while platform::ticks() - start < ticks {}
}
