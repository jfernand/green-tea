//! Arm's PL011 UART, polled: enough for a console. The driver waits while the transmit
//! FIFO is full and writes a byte, or reads one if one has arrived.

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

    /// Turn the UART on, with its transmitter and receiver. QEMU resets it with those two
    /// set but UARTEN clear, and sends bytes regardless; the hardware needs all three.
    pub fn enable(&self) {
        const CR: usize = 0x30; // control register
        const CR_UARTEN: u32 = 1 << 0;
        const CR_TXE: u32 = 1 << 8;
        const CR_RXE: u32 = 1 << 9;
        unsafe {
            let cr = (self.0 + CR) as *mut u32;
            write_volatile(cr, read_volatile(cr) | CR_UARTEN | CR_TXE | CR_RXE);
        }
    }

    /// The next received byte, if one has arrived.
    pub fn getc(&self) -> Option<u8> {
        const FR: usize = 0x18;
        const FR_RXFE: u32 = 1 << 4; // receive FIFO empty
        unsafe {
            (read_volatile((self.0 + FR) as *const u32) & FR_RXFE == 0).then(|| read_volatile(self.0 as *const u32) as u8)
        }
    }
}
