//! Arm's PL011 UART, transmit only: enough for a console. QEMU needs no setup, so the
//! driver just waits while the transmit FIFO is full and writes the byte.

#![no_std]

use core::ptr::{read_volatile, write_volatile};

/// Arm's PL011 (aarch64 `virt`, at 0x0900_0000). Its registers are 32 bits wide.
pub struct Pl011(pub usize);

impl Pl011 {
    pub fn putc(&self, byte: u8) {
        const FR: usize = 0x18; // flag register
        const FR_TXFF: u32 = 1 << 5; // transmit FIFO full
        unsafe {
            while read_volatile((self.0 + FR) as *const u32) & FR_TXFF != 0 {}
            write_volatile(self.0 as *mut u32, u32::from(byte));
        }
    }
}
