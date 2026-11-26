//! What green-tea's bare-metal kernels (green-tea-coop, green-tea-preempt, green-tea-shell)
//! share.
//!
//! - [`platform`]: per-machine code behind the [`Platform`] and [`TimerInterrupt`] traits:
//!   boot code, timers, interrupts and paging, and where each device is (which serial
//!   port, the display, where to look for virtio devices). [`Board`] is the
//!   implementation for the target being built.
//! - The device drivers themselves are crates of their own, in drivers/, re-exported
//!   here: [`fw_cfg`], [`ramfb`], [`pci`], [`virtio`], [`virtio_blk`]. Here are [`console`]
//!   (`println!` on the board's serial port) and [`screen`], the demos' layout drawn on
//!   the framebuffer.
//! - [`heap`] (the global allocator: TLSF, from [`fit`]),
//!   [`arena`] (a bump allocator that can be reset), [`kmem`] (page frames and slab caches,
//!   under the heap: where task records come from), [`stack`]
//!   (task stacks with guard pages, from a pool), [`memory`] (where the linker put each
//!   section), [`Global`] (statics in place of `thread_local!`). The task schedulers are
//!   separate crates under `schedulers/`.
//! - [`event`]: a signal that can be raised from an interrupt handler.
//!
//! A kernel is a `#![no_std] #![no_main]` binary that defines `kmain`: each platform's
//! `_start` sets up a stack, zeroes .bss, and calls it. The panic handler and the global
//! allocator live here.
//!
//! Memory is identity-mapped: aarch64 runs with the MMU off, and riscv64 and x86_64 with
//! an identity map that leaves out the task stacks' guard pages, and each page of the heap
//! until it is first touched ([`pages`]).
//!
//! Everything runs on the CPU that booted, unless a kernel starts the others ([`cpus`]).
//! Then the heap is safe to use from all of them, and nothing else is: the schedulers,
//! console and every [`Global`] belong to the boot CPU.

#![no_std]

#[cfg(all(feature = "green", feature = "black"))]
compile_error!("select only one base profile: green or black");
#[cfg(not(any(feature = "green", feature = "black")))]
compile_error!("select a base profile: green or black");
#[cfg(all(feature = "rt-runtime", not(feature = "white")))]
compile_error!("the realtime runtime requires the additive white profile");
#[cfg(all(feature = "black", feature = "rt-runtime"))]
compile_error!("Black+White runtime integration is not implemented");

extern crate alloc;

pub mod arena;
pub mod console;
pub mod cpus;
mod global;
pub mod heap;
pub mod kmem;
pub mod memory;
pub mod pages;
pub mod platform;
pub mod event;
pub mod screen;
pub mod stack;

pub use global::{Global, GlobalPtr};
pub use {
    green_tea_fit as fit, green_tea_fw_cfg as fw_cfg, green_tea_pci as pci, green_tea_pool as pool, green_tea_ramfb as ramfb,
    green_tea_smp as smp, green_tea_virtio as virtio, green_tea_virtio_blk as virtio_blk,
};
pub use green_tea_framebuffer::Framebuffer;
pub use green_tea_capabilities as capabilities;
#[cfg(feature = "board-pc")]
pub use green_tea_clevo_lights as clevo_lights;
#[cfg(feature = "board-pc")]
pub use green_tea_nvme as nvme;
#[cfg(not(feature = "board-pc"))]
pub use platform::Qemu;
pub use platform::{Board, Platform, TimerInterrupt, VirtioBus};
#[cfg(target_arch = "x86_64")]
pub use platform::{ProcessError, ProcessOutcome, USER_REGION_A, USER_REGION_B, UserProcess};
#[cfg(feature = "board-pc")]
pub use platform::InterruptCounts;
#[cfg(feature = "board-pc")]
pub use platform::UserI8042Lease;
#[cfg(feature = "board-pc")]
pub use platform::EcPort;

use core::panic::PanicInfo;

/// The start of every kernel: bring up the board, announce `kernel`, and turn on the
/// display. The banner's "green-tea-" prefix is what `cargo xtask kernel` waits for before
/// echoing the serial log (it skips the firmware's banner).
pub fn boot(kernel: &str) -> Framebuffer {
    Board::init();
    capabilities::BUILD_PROFILE
        .validate(Board::CAPABILITIES.union(RUNTIME_CAPABILITIES))
        .unwrap_or_else(|missing| panic!("{} profile requires unsupported capability {:?}", capabilities::BUILD_PROFILE.name(), missing.capability));
    memory::validate_kernel_regions()
        .unwrap_or_else(|error| panic!("{} kernel memory regions are unsupported: {error:?}", Board::NAME));
    println!("{kernel} on {}", Board::NAME);
    match Board::display() {
        Ok(fb) => fb,
        Err(e) => panic!("{e}"),
    }
}

#[cfg(feature = "rt-runtime")]
const RUNTIME_CAPABILITIES: capabilities::Capabilities = capabilities::Capabilities::REALTIME_EXECUTION;
#[cfg(not(feature = "rt-runtime"))]
const RUNTIME_CAPABILITIES: capabilities::Capabilities = capabilities::Capabilities::NONE;

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
