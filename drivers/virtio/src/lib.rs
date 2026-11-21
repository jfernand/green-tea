//! virtio: one device model for paravirtual devices (disks, network cards, consoles…),
//! reachable over several transports. A virtio device has:
//!
//! - a status byte, which the driver steps through a fixed handshake (see [`negotiate`]);
//! - 64 feature bits, which the device offers and the driver accepts a subset of;
//! - a device-specific configuration area (a disk's capacity, say);
//! - virtqueues: rings in guest memory through which the driver hands the device
//!   buffers, and the device hands them back when it's done with them ([`queue`]).
//!
//! The transport is how the driver reaches the status, features, configuration and
//! queue registers. It's the only part that differs between machines: memory-mapped
//! registers at a fixed address on riscv64 and aarch64 `virt` ([`mmio`]), a PCI device
//! on x86_64 `q35` ([`pci`]). Both implement [`Transport`]; device drivers like
//! green-tea-virtio-blk are written once, against the trait.
//!
//! Only "modern" virtio (1.0 and later) is implemented: little-endian throughout, and
//! the `VERSION_1` feature required. See the OASIS spec, "Virtual I/O Device (VIRTIO)
//! Version 1.2".

#![no_std]

extern crate alloc;

pub mod mmio;
pub mod pci;
pub mod queue;

/// Device ids, the same on every transport.
pub const ID_NET: u32 = 1;
pub const ID_BLOCK: u32 = 2;
pub const ID_CONSOLE: u32 = 3;
pub const ID_RNG: u32 = 4;
pub const ID_GPU: u32 = 16;
pub const ID_INPUT: u32 = 18;

/// What a device is, for log messages.
pub fn device_name(id: u32) -> &'static str {
    match id {
        ID_NET => "network",
        ID_BLOCK => "block",
        ID_CONSOLE => "console",
        ID_RNG => "entropy",
        ID_GPU => "GPU",
        ID_INPUT => "input",
        _ => "unknown",
    }
}

// Device status bits, set by the driver in this order.
pub const STATUS_ACKNOWLEDGE: u8 = 1; // we've seen the device
pub const STATUS_DRIVER: u8 = 2; // and know how to drive it
pub const STATUS_FEATURES_OK: u8 = 8; // the features we wrote are final
pub const STATUS_DRIVER_OK: u8 = 4; // queues are set up: go
pub const STATUS_FAILED: u8 = 128; // we gave up on it

/// The device follows the 1.0 spec, not the legacy interface. Feature bits 24–40 are
/// about the transport and queues; the ones below 24 belong to the device type.
pub const F_VERSION_1: u64 = 1 << 32;

/// The registers every transport provides, by meaning rather than by address.
pub trait Transport {
    /// The features the device offers.
    fn device_features(&mut self) -> u64;
    /// The features the driver accepts: a subset of the device's.
    fn set_driver_features(&mut self, features: u64);

    fn status(&self) -> u8;
    /// Writing 0 resets the device.
    fn set_status(&mut self, status: u8);

    /// The largest size queue `queue` can have; 0 if there's no such queue.
    fn max_queue_size(&mut self, queue: u16) -> u16;
    /// Whether queue `queue` has already been set up and enabled.
    fn queue_in_use(&mut self, queue: u16) -> bool;
    /// Give queue `queue` its size and the physical addresses of its three parts, and
    /// enable it.
    fn setup_queue(&mut self, queue: u16, size: u16, descriptors: u64, driver: u64, device: u64);
    /// Tell the device there are new buffers in queue `queue`.
    fn notify(&self, queue: u16);

    /// Changes whenever the device changes its configuration area, so a reader can tell
    /// whether a multi-word read was torn.
    fn config_generation(&self) -> u32;
    /// A naturally aligned 32-bit word of the device-specific configuration.
    fn read_config_u32(&self, offset: usize) -> u32;

    /// A 64-bit configuration field, read as two words, again until no change came
    /// between them.
    fn read_config_u64(&self, offset: usize) -> u64 {
        loop {
            let before = self.config_generation();
            let low = self.read_config_u32(offset);
            let high = self.read_config_u32(offset + 4);
            if self.config_generation() == before {
                return u64::from(high) << 32 | u64::from(low);
            }
        }
    }
}

/// Initialization up to the point where the driver sets up its queues: reset the device,
/// say we've found it and can drive it, and agree on features. Returns the features
/// agreed on: those in `supported` that the device offers. The device must offer
/// [`F_VERSION_1`], and that is always accepted.
///
/// The driver then sets up its queues and calls [`driver_ok`].
pub fn negotiate(transport: &mut impl Transport, supported: u64) -> Result<u64, &'static str> {
    transport.set_status(0);
    while transport.status() != 0 {} // a reset may take a moment to finish
    transport.set_status(STATUS_ACKNOWLEDGE);
    transport.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    let offered = transport.device_features();
    if offered & F_VERSION_1 == 0 {
        failed(transport);
        return Err("legacy-only virtio device (no VERSION_1 feature)");
    }
    let features = offered & (supported | F_VERSION_1);
    transport.set_driver_features(features);
    transport.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    // The device clears FEATURES_OK if it can't work with that subset.
    if transport.status() & STATUS_FEATURES_OK == 0 {
        failed(transport);
        return Err("the virtio device rejected our features");
    }
    Ok(features)
}

/// Tell the device we've given up on it. The spec forbids the driver to clear a status
/// bit, so this adds to the ones already set.
pub fn failed(transport: &mut impl Transport) {
    let status = transport.status();
    transport.set_status(status | STATUS_FAILED);
}

/// The last step of initialization: the device may now use its queues.
pub fn driver_ok(transport: &mut impl Transport) {
    let status = transport.status();
    transport.set_status(status | STATUS_DRIVER_OK);
}

/// A virtio device on whichever transport this machine has.
pub enum AnyTransport {
    Mmio(mmio::MmioTransport),
    Pci(pci::PciTransport),
}

macro_rules! delegate {
    ($self:ident, $t:ident => $call:expr) => {
        match $self {
            AnyTransport::Mmio($t) => $call,
            AnyTransport::Pci($t) => $call,
        }
    };
}

impl Transport for AnyTransport {
    fn device_features(&mut self) -> u64 {
        delegate!(self, t => t.device_features())
    }
    fn set_driver_features(&mut self, features: u64) {
        delegate!(self, t => t.set_driver_features(features))
    }
    fn status(&self) -> u8 {
        delegate!(self, t => t.status())
    }
    fn set_status(&mut self, status: u8) {
        delegate!(self, t => t.set_status(status))
    }
    fn max_queue_size(&mut self, queue: u16) -> u16 {
        delegate!(self, t => t.max_queue_size(queue))
    }
    fn queue_in_use(&mut self, queue: u16) -> bool {
        delegate!(self, t => t.queue_in_use(queue))
    }
    fn setup_queue(&mut self, queue: u16, size: u16, descriptors: u64, driver: u64, device: u64) {
        delegate!(self, t => t.setup_queue(queue, size, descriptors, driver, device))
    }
    fn notify(&self, queue: u16) {
        delegate!(self, t => t.notify(queue))
    }
    fn config_generation(&self) -> u32 {
        delegate!(self, t => t.config_generation())
    }
    fn read_config_u32(&self, offset: usize) -> u32 {
        delegate!(self, t => t.read_config_u32(offset))
    }
}

/// A device found by a board's probe: what it is, and how to reach it.
pub struct Device {
    pub id: u32,
    pub transport: AnyTransport,
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    /// A device whose status register behaves as the spec says: it can refuse features by
    /// clearing FEATURES_OK.
    struct FakeTransport {
        offered: u64,
        refuse_features: bool,
        status: u8,
    }

    impl Transport for FakeTransport {
        fn device_features(&mut self) -> u64 {
            self.offered
        }
        fn set_driver_features(&mut self, _features: u64) {}
        fn status(&self) -> u8 {
            self.status
        }
        fn set_status(&mut self, status: u8) {
            self.status = status;
            if self.refuse_features {
                self.status &= !STATUS_FEATURES_OK;
            }
        }
        fn max_queue_size(&mut self, _queue: u16) -> u16 {
            0
        }
        fn queue_in_use(&mut self, _queue: u16) -> bool {
            false
        }
        fn setup_queue(&mut self, _queue: u16, _size: u16, _descriptors: u64, _driver: u64, _device: u64) {}
        fn notify(&self, _queue: u16) {}
        fn config_generation(&self) -> u32 {
            0
        }
        fn read_config_u32(&self, _offset: usize) -> u32 {
            0
        }
    }

    #[test]
    fn negotiation_agrees_on_the_common_features() {
        let mut device = FakeTransport { offered: F_VERSION_1 | 0b11, refuse_features: false, status: 0 };
        assert_eq!(negotiate(&mut device, 0b10), Ok(F_VERSION_1 | 0b10));
        assert_eq!(device.status, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    }

    #[test]
    fn giving_up_on_a_legacy_device_keeps_the_status_bits() {
        let mut device = FakeTransport { offered: 0, refuse_features: false, status: 0 };
        assert!(negotiate(&mut device, 0).is_err());
        assert_eq!(device.status, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FAILED);
    }

    #[test]
    fn giving_up_on_refused_features_keeps_the_status_bits() {
        let mut device = FakeTransport { offered: F_VERSION_1, refuse_features: true, status: 0 };
        assert!(negotiate(&mut device, 0).is_err());
        assert_eq!(device.status, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FAILED);
    }
}
