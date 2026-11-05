//! QEMU `virt`, aarch64. With no firmware, QEMU loads the ELF and starts `_start` at EL1
//! with the MMU off. That makes every data access a Device access, where unaligned
//! accesses fault; the aarch64-unknown-none target compiles with +strict-align, so the
//! compiler never makes any.

use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};

pub const NAME: &str = "aarch64";

global_asm!(
    r#"
    .section .text.entry, "ax"
    .globl _start
_start:
    // Enable FP/SIMD (CPACR_EL1.FPEN = 0b11) before any Rust runs: the target uses NEON,
    // and swap_context saves d8-d15. Otherwise the first such instruction traps.
    mrs x1, cpacr_el1
    orr x1, x1, #(3 << 20)
    msr cpacr_el1, x1
    isb

    adrp x1, __stack_top
    add x1, x1, :lo12:__stack_top
    mov sp, x1

    // Zero .bss (it holds the framebuffer and the statics).
    adrp x1, __bss_start
    add x1, x1, :lo12:__bss_start
    adrp x2, __bss_end
    add x2, x2, :lo12:__bss_end
1:  cmp x1, x2
    b.hs 2f
    str xzr, [x1], #8
    b 1b
2:  bl kmain
3:  wfi
    b 3b
"#
);

pub fn init() {}

/// The PL011 UART: wait while the transmit FIFO is full, then write the byte.
pub fn putc(byte: u8) {
    const BASE: usize = 0x0900_0000;
    const FR_TXFF: u32 = 1 << 5;
    unsafe {
        while read_volatile((BASE + 0x18) as *const u32) & FR_TXFF != 0 {}
        write_volatile(BASE as *mut u32, u32::from(byte));
    }
}

pub use crate::fw_cfg::mmio::Mmio as FwCfg;
pub const FW_CFG: FwCfg = FwCfg(0x0902_0000);

/// The generic timer's virtual count.
pub fn ticks() -> u64 {
    let t: u64;
    unsafe { asm!("mrs {}, cntvct_el0", out(reg) t) };
    t
}

pub fn ticks_per_ms() -> u64 {
    let freq: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) freq) };
    freq / 1000
}

pub fn halt() -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
