//! The virtio MMIO transport: a 0x200-byte block of registers per device, at an address
//! the machine fixes. QEMU's `virt` has a row of these slots (8 on riscv64, 32 on
//! aarch64); `-device virtio-blk-device` plugs a disk into a free one, and an empty slot
//! reads as device id 0.
//!
//! QEMU's slots speak the legacy interface (version 1) unless started with
//! `-global virtio-mmio.force-legacy=false`; this driver needs version 2.

use crate::Transport;
use core::ptr::{read_volatile, write_volatile};

const MAGIC_VALUE: usize = 0x000; // "virt", little-endian
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014; // which 32 bits DEVICE_FEATURES shows
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const QUEUE_SEL: usize = 0x030; // which queue the QUEUE_* registers are about
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const STATUS: usize = 0x070;
const QUEUE_DESC_LOW: usize = 0x080;
const QUEUE_DESC_HIGH: usize = 0x084;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DRIVER_HIGH: usize = 0x094;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
const QUEUE_DEVICE_HIGH: usize = 0x0a4;
const CONFIG_GENERATION: usize = 0x0fc;
const CONFIG: usize = 0x100;

const MAGIC: u32 = u32::from_le_bytes(*b"virt");

pub struct MmioTransport {
    base: usize,
}

/// What's in the slot at `base`: `Ok(None)` if it's empty, the device id and its
/// transport if not. A device that only speaks the legacy interface is an error.
pub fn probe(base: usize) -> Result<Option<(u32, MmioTransport)>, &'static str> {
    let transport = MmioTransport { base };
    if transport.read(MAGIC_VALUE) != MAGIC {
        return Ok(None);
    }
    let id = transport.read(DEVICE_ID);
    if id == 0 {
        return Ok(None);
    }
    match transport.read(VERSION) {
        2 => Ok(Some((id, transport))),
        1 => Err("legacy virtio-mmio device: run QEMU with -global virtio-mmio.force-legacy=false"),
        _ => Err("virtio-mmio device of an unknown version"),
    }
}

impl MmioTransport {
    fn read(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    fn write_u64(&self, low: usize, high: usize, value: u64) {
        self.write(low, value as u32);
        self.write(high, (value >> 32) as u32);
    }
}

impl Transport for MmioTransport {
    fn device_features(&mut self) -> u64 {
        self.write(DEVICE_FEATURES_SEL, 0);
        let low = self.read(DEVICE_FEATURES);
        self.write(DEVICE_FEATURES_SEL, 1);
        let high = self.read(DEVICE_FEATURES);
        u64::from(high) << 32 | u64::from(low)
    }

    fn set_driver_features(&mut self, features: u64) {
        self.write(DRIVER_FEATURES_SEL, 0);
        self.write(DRIVER_FEATURES, features as u32);
        self.write(DRIVER_FEATURES_SEL, 1);
        self.write(DRIVER_FEATURES, (features >> 32) as u32);
    }

    fn status(&self) -> u8 {
        self.read(STATUS) as u8
    }

    fn set_status(&mut self, status: u8) {
        self.write(STATUS, u32::from(status));
    }

    fn max_queue_size(&mut self, queue: u16) -> u16 {
        self.write(QUEUE_SEL, u32::from(queue));
        if self.read(QUEUE_READY) != 0 {
            return 0; // already in use
        }
        self.read(QUEUE_NUM_MAX) as u16
    }

    fn setup_queue(&mut self, queue: u16, size: u16, descriptors: u64, driver: u64, device: u64) {
        self.write(QUEUE_SEL, u32::from(queue));
        self.write(QUEUE_NUM, u32::from(size));
        self.write_u64(QUEUE_DESC_LOW, QUEUE_DESC_HIGH, descriptors);
        self.write_u64(QUEUE_DRIVER_LOW, QUEUE_DRIVER_HIGH, driver);
        self.write_u64(QUEUE_DEVICE_LOW, QUEUE_DEVICE_HIGH, device);
        self.write(QUEUE_READY, 1);
    }

    fn notify(&self, queue: u16) {
        self.write(QUEUE_NOTIFY, u32::from(queue));
    }

    fn config_generation(&self) -> u32 {
        self.read(CONFIG_GENERATION)
    }

    fn read_config_u32(&self, offset: usize) -> u32 {
        self.read(CONFIG + offset)
    }
}
