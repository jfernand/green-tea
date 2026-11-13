//! The virtio PCI transport. A virtio device is a PCI function with vendor 0x1af4, and its
//! registers are in its BARs, in up to four separate structures. Vendor-specific PCI
//! capabilities say which BAR each is in, and at what offset:
//!
//! - common configuration: features, status, and the queue registers;
//! - notifications: where to write to kick a queue, one spot per queue;
//! - ISR status (for legacy interrupts; unused here);
//! - device-specific configuration.
//!
//! Device ids 0x1040 + virtio id are modern-only devices (QEMU's `disable-legacy=on`);
//! 0x1000–0x103f are "transitional" ones that speak both interfaces, and give the virtio
//! id in the subsystem id instead. Either works here, as long as it has the capabilities.

use crate::Transport;
use green_tea_pci::{Ecam, Function};
use core::ptr::{read_volatile, write_volatile};

const VENDOR_VIRTIO: u16 = 0x1af4;
const CAPABILITY_VENDOR: u8 = 0x09;

// The capability: { id, next, length, cfg_type: u8, bar: u8, id: u8, padding: [u8; 2],
// offset: u32, length: u32 }, and for notifications a u32 multiplier after that.
const CAP_CFG_TYPE: usize = 3;
const CAP_BAR: usize = 4;
const CAP_OFFSET: usize = 8;
const CAP_NOTIFY_MULTIPLIER: usize = 16;

const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_DEVICE: u8 = 4;

// struct virtio_pci_common_cfg.
const DEVICE_FEATURE_SELECT: usize = 0x00;
const DEVICE_FEATURE: usize = 0x04;
const DRIVER_FEATURE_SELECT: usize = 0x08;
const DRIVER_FEATURE: usize = 0x0c;
const DEVICE_STATUS: usize = 0x14; // u8
const CONFIG_GENERATION: usize = 0x15; // u8
const QUEUE_SELECT: usize = 0x16; // u16, and so are the next four
const QUEUE_SIZE: usize = 0x18;
const QUEUE_ENABLE: usize = 0x1c;
const QUEUE_NOTIFY_OFF: usize = 0x1e;
const QUEUE_DESC: usize = 0x20; // u64s, written as two u32s, low first
const QUEUE_DRIVER: usize = 0x28;
const QUEUE_DEVICE: usize = 0x30;

/// Queues this driver can notify: the ones it has set up. Enough for a disk (1) or a
/// network card (2).
const MAX_QUEUES: usize = 4;

pub struct PciTransport {
    common: usize,
    notify: usize,
    notify_multiplier: u32,
    device: usize,
    /// Each set-up queue's notification address.
    queue_notify: [usize; MAX_QUEUES],
}

/// If `f` is a virtio device: its virtio id and transport. Turns on its memory decoding
/// and DMA, which the device needs from here on.
pub fn probe(ecam: Ecam, f: Function) -> Option<(u32, PciTransport)> {
    if ecam.vendor_id(f) != VENDOR_VIRTIO {
        return None;
    }
    let id = match ecam.device_id(f) {
        modern @ 0x1040..=0x107f => u32::from(modern - 0x1040),
        0x1000..=0x103f => u32::from(ecam.subsystem_id(f)),
        _ => return None,
    };

    let (mut common, mut notify, mut device) = (None, None, None);
    let mut notify_multiplier = 0;
    ecam.for_each_capability(f, |cap, cap_id| {
        if cap_id != CAPABILITY_VENDOR {
            return;
        }
        let bar = usize::from(ecam.read_u8(f, cap + CAP_BAR));
        let Some(base) = (bar < 6).then(|| ecam.bar(f, bar)).flatten() else { return };
        let address = (base + u64::from(ecam.read_u32(f, cap + CAP_OFFSET))) as usize;
        // The first of each type is the one to use.
        match ecam.read_u8(f, cap + CAP_CFG_TYPE) {
            CFG_COMMON => _ = common.get_or_insert(address),
            CFG_NOTIFY if notify.is_none() => {
                notify = Some(address);
                notify_multiplier = ecam.read_u32(f, cap + CAP_NOTIFY_MULTIPLIER);
            }
            CFG_DEVICE => _ = device.get_or_insert(address),
            _ => {}
        }
    });
    ecam.enable(f);
    Some((
        id,
        PciTransport {
            common: common?,
            notify: notify?,
            notify_multiplier,
            device: device.unwrap_or(0),
            queue_notify: [0; MAX_QUEUES],
        },
    ))
}

impl PciTransport {
    fn read<T>(&self, offset: usize) -> T {
        unsafe { read_volatile((self.common + offset) as *const T) }
    }

    fn write<T>(&self, offset: usize, value: T) {
        unsafe { write_volatile((self.common + offset) as *mut T, value) }
    }

    fn write_u64(&self, offset: usize, value: u64) {
        self.write(offset, value as u32);
        self.write(offset + 4, (value >> 32) as u32);
    }
}

impl Transport for PciTransport {
    fn device_features(&mut self) -> u64 {
        self.write(DEVICE_FEATURE_SELECT, 0u32);
        let low: u32 = self.read(DEVICE_FEATURE);
        self.write(DEVICE_FEATURE_SELECT, 1u32);
        let high: u32 = self.read(DEVICE_FEATURE);
        u64::from(high) << 32 | u64::from(low)
    }

    fn set_driver_features(&mut self, features: u64) {
        self.write(DRIVER_FEATURE_SELECT, 0u32);
        self.write(DRIVER_FEATURE, features as u32);
        self.write(DRIVER_FEATURE_SELECT, 1u32);
        self.write(DRIVER_FEATURE, (features >> 32) as u32);
    }

    fn status(&self) -> u8 {
        self.read(DEVICE_STATUS)
    }

    fn set_status(&mut self, status: u8) {
        self.write(DEVICE_STATUS, status);
    }

    fn max_queue_size(&mut self, queue: u16) -> u16 {
        if usize::from(queue) >= MAX_QUEUES {
            return 0;
        }
        self.write(QUEUE_SELECT, queue);
        if self.read::<u16>(QUEUE_ENABLE) != 0 {
            return 0;
        }
        self.read(QUEUE_SIZE) // the maximum, until the driver writes a smaller one
    }

    fn setup_queue(&mut self, queue: u16, size: u16, descriptors: u64, driver: u64, device: u64) {
        self.write(QUEUE_SELECT, queue);
        self.write(QUEUE_SIZE, size);
        self.write_u64(QUEUE_DESC, descriptors);
        self.write_u64(QUEUE_DRIVER, driver);
        self.write_u64(QUEUE_DEVICE, device);
        let offset: u16 = self.read(QUEUE_NOTIFY_OFF);
        self.queue_notify[usize::from(queue)] = self.notify + usize::from(offset) * self.notify_multiplier as usize;
        self.write(QUEUE_ENABLE, 1u16);
    }

    fn notify(&self, queue: u16) {
        unsafe { write_volatile(self.queue_notify[usize::from(queue)] as *mut u16, queue) };
    }

    fn config_generation(&self) -> u32 {
        u32::from(self.read::<u8>(CONFIG_GENERATION))
    }

    fn read_config_u32(&self, offset: usize) -> u32 {
        assert!(self.device != 0, "this virtio device has no device configuration");
        unsafe { read_volatile((self.device + offset) as *const u32) }
    }
}
