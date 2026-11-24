//! The IO APIC: the chip that takes a PC's device interrupt lines and sends each to a CPU
//! as a vector of its choosing. Each input has a 64-bit redirection entry, and the chip
//! has two registers to reach them all: one selects a register, the other is its value.

#![no_std]

const VERSION: u8 = 0x01;
const REDIRECTION: u8 = 0x10;

const LEVEL: u32 = 1 << 15;
const ACTIVE_LOW: u32 = 1 << 13;
const MASKED: u32 = 1 << 16;

/// The chip's registers, so that the tests can use a fake one.
pub trait Registers {
    fn read(&mut self, index: u8) -> u32;
    fn write(&mut self, index: u8, value: u32);
}

/// The registers at a mapped address: the selector at 0, the value at 0x10.
pub struct Mmio(pub usize);

impl Registers for Mmio {
    fn read(&mut self, index: u8) -> u32 {
        // SAFETY: the caller of `IoApic::new` vouches for the address.
        unsafe {
            core::ptr::write_volatile(self.0 as *mut u32, u32::from(index));
            core::ptr::read_volatile((self.0 + 0x10) as *const u32)
        }
    }

    fn write(&mut self, index: u8, value: u32) {
        // SAFETY: as for `read`.
        unsafe {
            core::ptr::write_volatile(self.0 as *mut u32, u32::from(index));
            core::ptr::write_volatile((self.0 + 0x10) as *mut u32, value);
        }
    }
}

/// How a line signals, and where its interrupt goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub vector: u8,
    /// The local APIC id of the CPU. An IO APIC's field for it has 8 bits.
    pub destination: u8,
    pub active_low: bool,
    pub level: bool,
}

/// The low half of a redirection entry: fixed delivery, to a physical APIC id.
const fn low_half(route: Route) -> u32 {
    route.vector as u32 | if route.active_low { ACTIVE_LOW } else { 0 } | if route.level { LEVEL } else { 0 }
}

pub struct IoApic<R> {
    registers: R,
    first: u32,
    inputs: u32,
}

impl<R: Registers> IoApic<R> {
    /// The chip whose first input is global system interrupt `first`.
    pub fn new(mut registers: R, first: u32) -> IoApic<R> {
        let inputs = (registers.read(VERSION) >> 16 & 0xff) + 1;
        IoApic { registers, first, inputs }
    }

    pub fn covers(&self, gsi: u32) -> bool {
        gsi >= self.first && gsi - self.first < self.inputs
    }

    /// Send interrupt `gsi` to `route`. The entry stays masked until it is complete, since
    /// its two halves can't be written at once. `false` if this chip doesn't have that input.
    pub fn route(&mut self, gsi: u32, route: Route) -> bool {
        if !self.covers(gsi) {
            return false;
        }
        let index = REDIRECTION + 2 * (gsi - self.first) as u8;
        self.registers
            .write(index, low_half(route) | MASKED);
        self.registers
            .write(index + 1, u32::from(route.destination) << 24);
        self.registers
            .write(index, low_half(route));
        true
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    struct Fake {
        registers: [u32; 256],
        writes: Vec<(u8, u32)>,
    }

    impl Registers for &mut Fake {
        fn read(&mut self, index: u8) -> u32 {
            self.registers[index as usize]
        }
        fn write(&mut self, index: u8, value: u32) {
            self.registers[index as usize] = value;
            self.writes
                .push((index, value));
        }
    }

    fn fake() -> Fake {
        let mut f = Fake { registers: [0; 256], writes: Vec::new() };
        f.registers[VERSION as usize] = 23 << 16; // 24 inputs
        f
    }

    const KEYBOARD: Route = Route { vector: 0x31, destination: 2, active_low: false, level: false };

    #[test]
    fn an_entry_is_masked_until_both_halves_are_written() {
        let mut f = fake();
        assert!(IoApic::new(&mut f, 0).route(1, KEYBOARD));
        assert_eq!(f.writes, [(0x12, 0x31 | MASKED), (0x13, 2 << 24), (0x12, 0x31)]);
    }

    #[test]
    fn polarity_and_trigger_are_encoded() {
        let mut f = fake();
        IoApic::new(&mut f, 0).route(9, Route { active_low: true, level: true, ..KEYBOARD });
        assert_eq!(f.registers[0x22], 0x31 | ACTIVE_LOW | LEVEL);
    }

    #[test]
    fn inputs_are_counted_from_the_chips_first_interrupt() {
        let mut f = fake();
        let mut chip = IoApic::new(&mut f, 24);
        assert!(!chip.covers(23) && chip.covers(24) && chip.covers(47) && !chip.covers(48));
        assert!(!chip.route(1, KEYBOARD));
        assert!(chip.route(25, KEYBOARD));
        assert_eq!(f.registers[0x12], 0x31);
    }
}
