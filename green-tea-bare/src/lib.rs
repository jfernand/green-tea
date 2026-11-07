//! What green-tea's bare-metal kernels (green-tea-coop, green-tea-preempt) share.
//!
//! - [`platform`]: per-machine code behind the [`Platform`] and [`TimerInterrupt`] traits:
//!   boot code, serial port, timer, fw_cfg registers, and (riscv64 only, so far) timer
//!   interrupts. [`Board`] is the implementation for the target being built.
//! - Drivers on top of a `Platform`: [`console`] (`println!`), [`fw_cfg`], [`ramfb`]; and
//!   [`screen`], the demos' layout drawn on the framebuffer.
//! - [`heap`] (a bump allocator), [`Global`] (statics in place of `thread_local!`).
//! - [`task`] and [`runtime`]: green-tea-v2's tasks and round-robin scheduler, ported to
//!   bare metal, on top of green-tea-arch's `swap_context`.
//!
//! A kernel is a `#![no_std] #![no_main]` binary that defines `kmain`: each platform's
//! `_start` sets up a stack, zeroes .bss, and calls it. The panic handler and the global
//! allocator live here.
//!
//! Memory is identity-mapped (no paging beyond x86_64's identity map) and there is one CPU.

#![no_std]

extern crate alloc;

pub mod console;
pub mod fw_cfg;
mod global;
mod heap;
pub mod platform;
pub mod ramfb;
pub mod runtime;
pub mod screen;
pub mod task;

pub use global::Global;
pub use platform::{Board, FwCfgPort, Platform, TimerInterrupt};

use core::panic::PanicInfo;
use ramfb::{Framebuffer, HEIGHT, WIDTH};

/// The start of every kernel: bring up the board, announce `kernel`, find fw_cfg and turn
/// on the display. The banner's "green-tea-" prefix is what `cargo xtask kernel` waits for
/// before echoing the serial log (it skips the firmware's banner).
pub fn boot(kernel: &str) -> Framebuffer {
    Board::init();
    println!("{kernel} on {}", Board::NAME);

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
    fb
}

/// Busy-wait on the platform's timer.
pub fn delay_ms(ms: u64) {
    let ticks = ms * Board::ticks_per_ms();
    let start = Board::ticks();
    while Board::ticks() - start < ticks {}
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("panic: {info}");
    Board::halt()
}
