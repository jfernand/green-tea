//! Transfer Request Blocks, the 16-byte records the driver and the controller exchange,
//! and the rings they sit in. Ownership of a TRB is decided by its cycle bit: the producer
//! writes its current cycle, and the consumer takes a TRB only if the bit matches what it
//! expects. Each time a ring wraps, the expected value flips.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

pub const NORMAL: u32 = 1;
pub const SETUP: u32 = 2;
pub const DATA: u32 = 3;
pub const STATUS: u32 = 4;
pub const LINK: u32 = 6;
pub const ENABLE_SLOT: u32 = 9;
pub const ADDRESS_DEVICE: u32 = 11;
pub const CONFIGURE_ENDPOINT: u32 = 12;
pub const EVALUATE_CONTEXT: u32 = 13;
pub const TRANSFER_EVENT: u32 = 32;
pub const COMMAND_COMPLETION: u32 = 33;

/// Completion codes.
pub const SUCCESS: u32 = 1;
pub const SHORT_PACKET: u32 = 13;

const CYCLE: u32 = 1 << 0;
const TOGGLE_CYCLE: u32 = 1 << 1;
const INTERRUPT_ON_SHORT: u32 = 1 << 2;
const INTERRUPT_ON_COMPLETION: u32 = 1 << 5;
const IMMEDIATE_DATA: u32 = 1 << 6;
const DIRECTION_IN: u32 = 1 << 16;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trb {
    pub param: u64,
    pub status: u32,
    pub control: u32,
}

const fn kind(k: u32) -> u32 {
    k << 10
}

impl Trb {
    pub const fn kind(&self) -> u32 {
        self.control >> 10 & 0x3f
    }

    pub const fn code(&self) -> u32 {
        self.status >> 24
    }

    pub const fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }

    /// The endpoint a transfer event is for, as a device context index.
    pub const fn endpoint(&self) -> u8 {
        (self.control >> 16 & 0x1f) as u8
    }

    /// The first stage of a control transfer: the 8-byte request, in the TRB itself.
    pub const fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Trb {
        let param = request_type as u64
            | (request as u64) << 8
            | (value as u64) << 16
            | (index as u64) << 32
            | (length as u64) << 48;
        // Transfer type: 0 no data, 2 OUT, 3 IN.
        let transfer = if length == 0 {
            0
        } else if request_type & 0x80 != 0 {
            3
        } else {
            2
        };
        Trb { param, status: 8, control: kind(SETUP) | IMMEDIATE_DATA | transfer << 16 }
    }

    pub const fn data(buffer: u64, length: u16, into_memory: bool) -> Trb {
        Trb { param: buffer, status: length as u32, control: kind(DATA) | if into_memory { DIRECTION_IN } else { 0 } }
    }

    /// The last stage, in the direction opposite to the data (IN if there was none).
    pub const fn status_stage(data_was_in: bool) -> Trb {
        let direction = if data_was_in { 0 } else { DIRECTION_IN };
        Trb { param: 0, status: 0, control: kind(STATUS) | INTERRUPT_ON_COMPLETION | direction }
    }

    /// An interrupt endpoint's transfer: the next report, into `buffer`.
    pub const fn normal(buffer: u64, length: u32) -> Trb {
        Trb { param: buffer, status: length, control: kind(NORMAL) | INTERRUPT_ON_COMPLETION | INTERRUPT_ON_SHORT }
    }

    pub const fn enable_slot() -> Trb {
        Trb { param: 0, status: 0, control: kind(ENABLE_SLOT) }
    }

    /// A command that takes an input context, for a slot.
    pub const fn with_context(command: u32, input: u64, slot: u8) -> Trb {
        Trb { param: input, status: 0, control: kind(command) | (slot as u32) << 24 }
    }
}

/// A ring the driver produces on, ending in a link back to its start.
pub struct Ring {
    trbs: *mut Trb,
    len: usize,
    index: usize,
    cycle: u32,
}

impl Ring {
    /// # Safety
    /// `trbs` must be `len` zeroed TRBs (2 or more), valid for as long as the ring is used.
    pub const unsafe fn new(trbs: *mut Trb, len: usize) -> Ring {
        Ring { trbs, len, index: 0, cycle: CYCLE }
    }

    /// Its address with the cycle bit, as a context or CRCR wants it.
    pub fn dequeue_pointer(&self) -> u64 {
        self.trbs as u64 | u64::from(self.cycle)
    }

    fn write(&mut self, at: usize, trb: Trb) {
        // SAFETY: `at` is within the ring. The cycle bit, in `control`, is written last.
        unsafe {
            let slot = self.trbs.add(at);
            write_volatile(&raw mut (*slot).param, trb.param);
            write_volatile(&raw mut (*slot).status, trb.status);
            fence(Ordering::Release);
            write_volatile(&raw mut (*slot).control, trb.control & !CYCLE | self.cycle);
        }
    }

    /// Add a TRB, and return its address. The last slot is always the link.
    pub fn push(&mut self, trb: Trb) -> u64 {
        // SAFETY: within the ring.
        let address = unsafe { self.trbs.add(self.index) } as u64;
        self.write(self.index, trb);
        self.index += 1;
        if self.index == self.len - 1 {
            let link = Trb { param: self.trbs as u64, status: 0, control: kind(LINK) | TOGGLE_CYCLE };
            self.write(self.index, link);
            self.index = 0;
            self.cycle ^= CYCLE;
        }
        address
    }
}

/// The ring the controller produces events on.
pub struct EventRing {
    trbs: *const Trb,
    len: usize,
    index: usize,
    cycle: u32,
}

impl EventRing {
    /// # Safety
    /// `trbs` must be `len` zeroed TRBs, valid for as long as the ring is used.
    pub const unsafe fn new(trbs: *const Trb, len: usize) -> EventRing {
        EventRing { trbs, len, index: 0, cycle: CYCLE }
    }

    /// What ERDP should say: the next TRB to be read.
    pub fn dequeue_pointer(&self) -> u64 {
        // SAFETY: within the ring.
        unsafe { self.trbs.add(self.index) as u64 }
    }

    /// The next event, if the controller has written one.
    pub fn pop(&mut self) -> Option<Trb> {
        // SAFETY: within the ring. The cycle bit is read first, and says the rest is there.
        let trb = unsafe {
            let slot = self.trbs.add(self.index);
            let control = read_volatile(&raw const (*slot).control);
            if control & CYCLE != self.cycle {
                return None;
            }
            fence(Ordering::Acquire);
            Trb {
                param: read_volatile(&raw const (*slot).param),
                status: read_volatile(&raw const (*slot).status),
                control,
            }
        };
        self.index += 1;
        if self.index == self.len {
            self.index = 0;
            self.cycle ^= CYCLE;
        }
        Some(trb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec;

    #[test]
    fn a_setup_trb_holds_the_request() {
        let t = Trb::setup(0x80, 6, 0x0100, 0, 18);
        assert_eq!(t.param, 0x0012_0000_0100_0680);
        assert_eq!(t.status, 8);
        assert_eq!(t.kind(), SETUP);
        assert_eq!(t.control >> 16 & 3, 3, "an IN transfer");
    }

    #[test]
    fn a_request_with_no_data_has_transfer_type_0() {
        assert_eq!(Trb::setup(0x00, 9, 1, 0, 0).control >> 16 & 3, 0);
    }

    #[test]
    fn the_status_stage_runs_opposite_to_the_data() {
        assert_eq!(Trb::status_stage(true).control & DIRECTION_IN, 0);
        assert_ne!(Trb::status_stage(false).control & DIRECTION_IN, 0);
    }

    #[test]
    fn a_ring_links_back_and_flips_its_cycle() {
        let mut memory = vec![Trb::default(); 3];
        let base = memory.as_mut_ptr();
        // SAFETY: three zeroed TRBs, which outlive the ring.
        let mut ring = unsafe { Ring::new(base, 3) };
        ring.push(Trb::enable_slot());
        assert_eq!(ring.dequeue_pointer() & 1, 1);
        ring.push(Trb::enable_slot());
        assert_eq!(ring.dequeue_pointer() & 1, 0, "wrapped");
        ring.push(Trb::enable_slot());
        assert_eq!(memory[2].kind(), LINK);
        assert_eq!(memory[2].param, base as u64);
        assert_ne!(memory[2].control & TOGGLE_CYCLE, 0);
        assert_eq!(memory[2].control & CYCLE, 1, "the link has the cycle of the pass it ends");
        assert_eq!(memory[0].control & CYCLE, 0, "the second pass has the other cycle");
    }

    #[test]
    fn an_event_ring_takes_only_the_current_cycle() {
        let mut memory = [Trb::default(); 2];
        memory[0].control = 1 | kind(COMMAND_COMPLETION);
        // SAFETY: two TRBs, which outlive the ring.
        let mut ring = unsafe { EventRing::new(memory.as_ptr(), 2) };
        assert_eq!(ring.pop().map(|t| t.kind()), Some(COMMAND_COMPLETION));
        assert_eq!(ring.pop(), None, "the next one isn't written yet");
    }

    #[test]
    fn an_event_ring_flips_its_cycle_when_it_wraps() {
        let mut memory = vec![Trb { param: 0, status: 0, control: 1 }; 2];
        let base = memory.as_mut_ptr();
        // SAFETY: two TRBs, which outlive the ring.
        let mut ring = unsafe { EventRing::new(base, 2) };
        assert!(ring.pop().is_some() && ring.pop().is_some());
        assert_eq!(ring.dequeue_pointer(), base as u64);
        assert_eq!(ring.pop(), None, "still the old cycle");
        // SAFETY: as above; the second pass writes cycle 0.
        unsafe { (*base).control = 0 };
        assert!(ring.pop().is_some());
    }
}
