//! A 16550 UART, polled: enough for a console. QEMU needs no setup (baud rate, line
//! format), so the driver just waits for room and writes a byte, or reads one if one
//! has arrived. The registers
//! are the same whether they're memory-mapped ([`Ns16550Mmio`]) or on x86's I/O ports
//! ([`Ns16550Port`]).

#![no_std]

use core::ptr::{read_volatile, write_volatile};

/// Line status register: the transmit holding register is empty.
const LSR_THR_EMPTY: u8 = 1 << 5;
/// Line status register: a received byte is waiting.
const LSR_DATA_READY: u8 = 1 << 0;

/// A 16550 with memory-mapped byte registers (riscv64 `virt`, at 0x1000_0000).
pub struct Ns16550Mmio(pub usize);

impl Ns16550Mmio {
    pub fn putc(&self, byte: u8) {
        unsafe {
            while read_volatile((self.0 + 5) as *const u8) & LSR_THR_EMPTY == 0 {}
            write_volatile(self.0 as *mut u8, byte);
        }
    }

    /// The next received byte, if one has arrived.
    pub fn getc(&self) -> Option<u8> {
        unsafe {
            (read_volatile((self.0 + 5) as *const u8) & LSR_DATA_READY != 0).then(|| read_volatile(self.0 as *const u8))
        }
    }
}

/// A 16550 on I/O ports (x86's COM1, at 0x3f8). The same registers, in port space.
#[cfg(target_arch = "x86_64")]
pub struct Ns16550Port(pub u16);

#[cfg(target_arch = "x86_64")]
impl Ns16550Port {
    pub fn putc(&self, byte: u8) {
        use green_tea_io::port::{inb, outb};
        unsafe {
            while inb(self.0 + 5) & LSR_THR_EMPTY == 0 {}
            outb(self.0, byte);
        }
    }

    /// The next received byte, if one has arrived.
    pub fn getc(&self) -> Option<u8> {
        use green_tea_io::port::inb;
        unsafe { (inb(self.0 + 5) & LSR_DATA_READY != 0).then(|| inb(self.0)) }
    }
}
