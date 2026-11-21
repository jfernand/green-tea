//! The machine-specific code, behind two traits.
//!
//! Each build targets exactly one machine, so nothing here is dynamic: [`Board`] names the
//! implementation for the target, and everything calls `Board::...`. What the traits buy
//! is a checked contract. Every board must provide all of [`Platform`], and code that
//! needs more says so in its bounds: green-tea-preempt requires
//! `Board: TimerInterrupt`, so building it for a board without timer interrupts fails
//! with "the trait `TimerInterrupt` is not implemented", not a pile of missing functions.
//!
//! Context switching itself (`Context`, `swap_context`) isn't here: it depends only on
//! the ISA and its calling convention, not on the machine, and lives in green-tea-arch.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "x86_64")]
mod x86_64;

/// The board being built for: QEMU `virt` on riscv64 and aarch64, `q35` on x86_64.
#[cfg(target_arch = "aarch64")]
pub use aarch64::AarchVirt as Board;
#[cfg(target_arch = "riscv64")]
pub use riscv64::RiscvVirt as Board;
#[cfg(target_arch = "x86_64")]
pub use x86_64::Q35 as Board;

pub use green_tea_fw_cfg::{FwCfg, FwCfgPort};
use green_tea_virtio as virtio;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
compile_error!("green-tea-bare supports riscv64, aarch64 and x86_64 so far");

/// What every board provides. Its `_start` (in the same module, as global asm) has
/// already set up the boot stack, zeroed .bss and enabled the FPU when `kmain` runs.
pub trait Platform {
    /// The architecture, for the boot banner.
    const NAME: &'static str;

    /// Whether touching a task stack's guard page faults (see [`crate::stack`]). That
    /// needs paging; boards without it run with the MMU off, or with a plain identity map.
    const GUARD_PAGES: bool;

    /// Whether the heap's pages start out unmapped, and the fault handler maps each one the
    /// first time it is touched (see [`crate::pages`]). That needs paging, and a fault
    /// handler that returns to the code it interrupted.
    const DEMAND_PAGING: bool = false;

    /// Map or unmap the 4 KiB page of the heap at `page`, and forget any translation of it
    /// the CPU has cached. Returns whether the page was mapped before. A board without
    /// `DEMAND_PAGING` has every page mapped, always.
    fn set_mapped(page: usize, mapped: bool) -> bool {
        let _ = (page, mapped);
        true
    }

    /// Whether the page of the heap at `page` is mapped.
    fn is_mapped(page: usize) -> bool {
        let _ = page;
        true
    }

    /// The page table's entries for `addr`, root first: see [`crate::pages::walk`].
    fn walk(addr: usize) -> [Option<crate::pages::Step>; 4] {
        let _ = addr;
        [None; 4]
    }

    /// Bring up what the other functions need (e.g. start x86_64's HPET). Call once, first.
    fn init();

    /// Write a byte to the serial console, waiting for room.
    fn putc(byte: u8);

    /// The next byte received on the serial console, if one has arrived. Polled: nothing
    /// interrupts when a byte comes in.
    fn getc() -> Option<u8>;

    /// A free-running counter, never reset.
    fn ticks() -> u64;
    fn ticks_per_ms() -> u64;

    /// Stop for good.
    fn halt() -> !;

    /// The number of the CPU running this code: 0 for the one that booted, then 1, 2 and so
    /// on for each one started with `start_cpu`. A board that can't start another has CPU 0.
    fn cpu() -> usize {
        0
    }

    /// Start CPU `cpu` (1 or more) on the stack whose top is `stack_top`. The new CPU sets
    /// itself up as `_start` and `init` set up the first, and calls [`crate::cpus::enter`].
    /// An error if the board has no such CPU, or doesn't know how to start one.
    fn start_cpu(_cpu: usize, _stack_top: usize) -> Result<(), &'static str> {
        Err("this board can't start another CPU")
    }

    /// How this board reaches QEMU's fw_cfg device.
    type FwCfg: FwCfgPort;
    const FW_CFG: Self::FwCfg;

    /// The fw_cfg device, ready to use.
    fn fw_cfg() -> FwCfg<Self::FwCfg> {
        FwCfg(Self::FW_CFG)
    }

    /// Call `f` with every virtio device on the board: in `virt`'s virtio-mmio slots on
    /// riscv64 and aarch64, on the PCI bus on x86_64. An error if one is there that the
    /// driver can't use (a legacy virtio-mmio device).
    fn for_each_virtio(f: impl FnMut(virtio::Device)) -> Result<(), &'static str>;

    /// The first virtio device with id `id` (e.g. [`virtio::ID_BLOCK`]).
    fn find_virtio(id: u32) -> Result<Option<virtio::AnyTransport>, &'static str> {
        let mut found = None;
        Self::for_each_virtio(|device| {
            if found.is_none() && device.id == id {
                found = Some(device.transport);
            }
        })?;
        Ok(found)
    }
}

/// A board that can interrupt the running code when a deadline passes: what preemption
/// needs. Every board implements it.
///
/// Interrupt handlers run with interrupts off, on the stack of whatever they interrupted,
/// and with every register of the interrupted code saved, so a handler may switch to
/// another task and come back much later.
pub trait TimerInterrupt: Platform {
    /// Install the trap handler and enable the timer interrupt at its source, so that
    /// `on_tick` runs once every time a deadline set with `set_deadline` passes, while
    /// interrupts are on. Interrupts stay off until `interrupts_on`.
    fn init_timer(on_tick: fn());

    /// Fire one tick when `ticks()` reaches `deadline`, replacing any earlier deadline. A
    /// pending tick is cleared. `u64::MAX` means never.
    fn set_deadline(deadline: u64);

    /// Turn interrupts on for this CPU.
    fn interrupts_on();

    /// Turn interrupts off, returning whether they were on.
    fn interrupts_off() -> bool;

    /// Put interrupts back the way `interrupts_off` found them.
    fn restore_interrupts(were_on: bool) {
        if were_on {
            Self::interrupts_on();
        }
    }

    /// Run `f` with interrupts off: a critical section, on a single CPU.
    fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
        let were_on = Self::interrupts_off();
        let result = f();
        Self::restore_interrupts(were_on);
        result
    }
}
