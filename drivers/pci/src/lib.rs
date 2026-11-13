//! PCI configuration space, through ECAM.
//!
//! Every PCI function (a device, or one of up to 8 parts of a multi-function device) has
//! a 4 KiB configuration space: who it is (vendor and device ids), its BARs (where its
//! registers are in memory), and a linked list of capabilities. PCI Express's ECAM maps
//! all of them into memory, one 4 KiB page per function, at
//! `base + (bus << 20 | device << 15 | function << 12)`: 256 buses take 256 MiB.
//!
//! This driver only reads what the firmware set up. On x86_64 `q35`, SeaBIOS has already
//! put ECAM at 0xb000_0000, numbered the buses behind any bridges, and given every BAR an
//! address. (On riscv64 and aarch64 `virt` nothing would have, which is one reason those
//! use virtio over MMIO instead.)

#![no_std]

use core::ptr::{read_volatile, write_volatile};

const VENDOR_ID: usize = 0x00;
const DEVICE_ID: usize = 0x02;
const COMMAND: usize = 0x04;
const STATUS: usize = 0x06;
const HEADER_TYPE: usize = 0x0e;
const BAR0: usize = 0x10;
const SUBSYSTEM_ID: usize = 0x2e;
const CAPABILITIES: usize = 0x34;

const COMMAND_MEMORY: u16 = 1 << 1; // respond to its memory BARs
const COMMAND_BUS_MASTER: u16 = 1 << 2; // allowed to do DMA
const STATUS_CAPABILITIES: u16 = 1 << 4; // has a capability list
const HEADER_MULTI_FUNCTION: u8 = 1 << 7;

/// A PCI Express configuration space window.
#[derive(Clone, Copy)]
pub struct Ecam(pub usize);

/// The address of a function: bus, device (0–31) and function (0–7).
#[derive(Clone, Copy, Debug)]
pub struct Function {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Ecam {
    fn address(&self, f: Function, offset: usize) -> usize {
        self.0 + ((f.bus as usize) << 20 | (f.device as usize) << 15 | (f.function as usize) << 12) + offset
    }

    // Accesses must be naturally aligned and of the register's own width.
    pub fn read_u8(&self, f: Function, offset: usize) -> u8 {
        unsafe { read_volatile(self.address(f, offset) as *const u8) }
    }

    pub fn read_u16(&self, f: Function, offset: usize) -> u16 {
        unsafe { read_volatile(self.address(f, offset) as *const u16) }
    }

    pub fn read_u32(&self, f: Function, offset: usize) -> u32 {
        unsafe { read_volatile(self.address(f, offset) as *const u32) }
    }

    pub fn write_u16(&self, f: Function, offset: usize, value: u16) {
        unsafe { write_volatile(self.address(f, offset) as *mut u16, value) }
    }

    pub fn vendor_id(&self, f: Function) -> u16 {
        self.read_u16(f, VENDOR_ID)
    }

    pub fn device_id(&self, f: Function) -> u16 {
        self.read_u16(f, DEVICE_ID)
    }

    pub fn subsystem_id(&self, f: Function) -> u16 {
        self.read_u16(f, SUBSYSTEM_ID)
    }

    /// Call `f` with every function that's there. Missing ones read as all ones.
    pub fn for_each_function(&self, mut f: impl FnMut(Function)) {
        for bus in 0..=255 {
            for device in 0..32 {
                let first = Function { bus, device, function: 0 };
                if self.vendor_id(first) == 0xffff {
                    continue;
                }
                let functions = if self.read_u8(first, HEADER_TYPE) & HEADER_MULTI_FUNCTION != 0 { 8 } else { 1 };
                for function in 0..functions {
                    let this = Function { bus, device, function };
                    if self.vendor_id(this) != 0xffff {
                        f(this);
                    }
                }
            }
        }
    }

    /// The memory address BAR `index` points at, as the firmware set it: `None` for an
    /// I/O-port BAR or one that wasn't given an address. A 64-bit BAR takes two slots,
    /// low half first.
    pub fn bar(&self, f: Function, index: usize) -> Option<u64> {
        let low = self.read_u32(f, BAR0 + 4 * index);
        if low & 1 != 0 {
            return None; // I/O space
        }
        let address = match (low >> 1) & 0b11 {
            0b00 => u64::from(low & !0xf),
            0b10 if index < 5 => u64::from(low & !0xf) | u64::from(self.read_u32(f, BAR0 + 4 * (index + 1))) << 32,
            _ => return None,
        };
        (address != 0).then_some(address)
    }

    /// Call `f` with the offset and id of each capability in the list.
    pub fn for_each_capability(&self, function: Function, mut f: impl FnMut(usize, u8)) {
        if self.read_u16(function, STATUS) & STATUS_CAPABILITIES == 0 {
            return;
        }
        let mut offset = usize::from(self.read_u8(function, CAPABILITIES) & !0b11);
        while offset != 0 {
            f(offset, self.read_u8(function, offset));
            offset = usize::from(self.read_u8(function, offset + 1) & !0b11);
        }
    }

    /// Let the function answer at its memory BARs, and do DMA.
    pub fn enable(&self, f: Function) {
        let command = self.read_u16(f, COMMAND);
        self.write_u16(f, COMMAND, command | COMMAND_MEMORY | COMMAND_BUS_MASTER);
    }
}
