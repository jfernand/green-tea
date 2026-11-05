//! QEMU `virt`, riscv64. OpenSBI (`-bios default`) jumps to `_start` in S-mode at
//! 0x80200000 with a0 = hart id, a1 = device tree.

use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};

pub const NAME: &str = "riscv64";

global_asm!(
    r#"
    .section .text.entry, "ax"
    .globl _start
_start:
    la sp, __stack_top

    # Zero .bss (it holds the framebuffer and the statics).
    la t0, __bss_start
    la t1, __bss_end
1:  bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b
2:
    # Make sure the FPU is on (sstatus.FS != Off). swap_context saves fs0-fs11, and with
    # FS = Off its first fsd traps as an illegal instruction. OpenSBI happens to hand over
    # with the FPU on, but the SBI spec doesn't promise it, so don't rely on that.
    li t0, 1 << 13
    csrs sstatus, t0

    call kmain
3:  wfi
    j 3b
"#
);

pub fn init() {}

/// The 16550 UART. QEMU needs no setup: wait for room, write the byte.
pub fn putc(byte: u8) {
    const BASE: usize = 0x1000_0000;
    const LSR_THR_EMPTY: u8 = 1 << 5;
    unsafe {
        while read_volatile((BASE + 5) as *const u8) & LSR_THR_EMPTY == 0 {}
        write_volatile(BASE as *mut u8, byte);
    }
}

pub use crate::fw_cfg::mmio::Mmio as FwCfg;
pub const FW_CFG: FwCfg = FwCfg(0x1010_0000);

/// The `time` CSR, which ticks at 10 MHz on QEMU's virt machine.
pub fn ticks() -> u64 {
    let t: u64;
    unsafe { asm!("rdtime {}", out(reg) t) };
    t
}

pub fn ticks_per_ms() -> u64 {
    10_000
}

pub fn halt() -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
