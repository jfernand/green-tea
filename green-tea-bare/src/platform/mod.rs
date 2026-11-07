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

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
compile_error!("green-tea-bare supports riscv64, aarch64 and x86_64 so far");

/// What every board provides. Its `_start` (in the same module, as global asm) has
/// already set up the boot stack, zeroed .bss and enabled the FPU when `kmain` runs.
pub trait Platform {
    /// The architecture, for the boot banner.
    const NAME: &'static str;

    /// Bring up what the other functions need (e.g. start x86_64's HPET). Call once, first.
    fn init();

    /// Write a byte to the serial console, waiting for room.
    fn putc(byte: u8);

    /// A free-running counter, never reset.
    fn ticks() -> u64;
    fn ticks_per_ms() -> u64;

    /// Stop for good.
    fn halt() -> !;

    /// How this board reaches QEMU's fw_cfg device.
    type FwCfg: FwCfgPort;
    const FW_CFG: Self::FwCfg;
}

/// fw_cfg's registers: memory-mapped on `virt`, I/O ports on x86. See [`crate::fw_cfg`].
pub trait FwCfgPort {
    /// Select an item; reads start at its first byte.
    fn select(&self, key: u16);
    /// The selected item's next byte.
    fn read_u8(&self) -> u8;
    /// Hand QEMU the physical address of a DMA request, which starts it.
    fn start_dma(&self, address: u64);
}

/// A board that can interrupt the running code when a deadline passes: what preemption
/// needs. Implemented on riscv64 so far.
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
