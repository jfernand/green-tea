//! x86's I/O ports: a second, 64 KiB address space next to memory, reached only with
//! the `in` and `out` instructions. Legacy devices live here: the 16550 serial port, the
//! 8259 PIC, and QEMU's fw_cfg.

use core::arch::asm;

/// # Safety
/// Writing to a device's port has whatever effect that device gives it.
pub unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nostack, preserves_flags)) };
}

/// # Safety
/// Reading some ports has side effects (e.g. it pops a FIFO).
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nostack, preserves_flags)) };
    value
}

/// # Safety
/// As [`outb`].
pub unsafe fn outw(port: u16, value: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nostack, preserves_flags)) };
}

/// # Safety
/// As [`outb`].
pub unsafe fn outl(port: u16, value: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nostack, preserves_flags)) };
}

/// # Safety
/// As for [`inb`], on a 32-bit port.
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") value, options(nostack, preserves_flags)) };
    value
}
