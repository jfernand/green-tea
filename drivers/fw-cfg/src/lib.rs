//! QEMU's firmware configuration device (fw_cfg).
//!
//! fw_cfg is a list of numbered items: some fixed (signature, features, file directory) and
//! some named "files", like `etc/ramfb`. You select an item, then read its bytes from the
//! data register, or write it with a DMA request. See qemu's docs/specs/fw_cfg.rst.
//!
//! How the registers are reached differs per machine: memory-mapped on riscv64 and
//! aarch64 `virt` ([`Mmio`]), I/O ports on x86 ([`IoPorts`]). Both are a [`FwCfgPort`],
//! and [`FwCfg`] is the protocol on top, the same everywhere.

#![no_std]

use green_tea_io::dma_fence;
use core::ptr::{read_volatile, write_volatile};

const KEY_SIGNATURE: u16 = 0x0000;
const KEY_ID: u16 = 0x0001;
const KEY_FILE_DIR: u16 = 0x0019;

const ID_DMA: u32 = 1 << 1;

const DMA_ERROR: u32 = 1 << 0;
const DMA_SELECT: u32 = 1 << 3;
const DMA_WRITE: u32 = 1 << 4;

/// fw_cfg's registers, however the machine reaches them.
pub trait FwCfgPort {
    /// Select an item; reads start at its first byte.
    fn select(&self, key: u16);
    /// The selected item's next byte.
    fn read_u8(&self) -> u8;
    /// Hand QEMU the physical address of a DMA request, which starts it.
    fn start_dma(&self, address: u64);
}

/// The fw_cfg device, through `P`.
pub struct FwCfg<P>(pub P);

/// A named item in the file directory.
pub struct File {
    pub name: [u8; 56],
    pub size: u32,
    pub select: u16,
}

impl File {
    pub fn name(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..len]).unwrap_or("?")
    }
}

/// A DMA request, read by QEMU from guest memory. All fields are big-endian.
#[repr(C, align(16))]
struct DmaAccess {
    control: u32,
    length: u32,
    address: u64,
}

impl<P: FwCfgPort> FwCfg<P> {
    fn read(&self, buf: &mut [u8]) {
        for byte in buf {
            *byte = self.0.read_u8();
        }
    }

    fn read_be_u32(&self) -> u32 {
        let mut buf = [0; 4];
        self.read(&mut buf);
        u32::from_be_bytes(buf)
    }

    /// Whether fw_cfg is there and supports DMA, which writing a file needs.
    pub fn probe(&self) -> Result<(), &'static str> {
        let mut signature = [0; 4];
        self.0.select(KEY_SIGNATURE);
        self.read(&mut signature);
        if &signature != b"QEMU" {
            return Err("no fw_cfg device (bad signature)");
        }
        let mut id = [0; 4];
        self.0.select(KEY_ID);
        self.read(&mut id);
        if u32::from_le_bytes(id) & ID_DMA == 0 {
            // The feature bitmap is one of the few little-endian items.
            return Err("fw_cfg has no DMA interface");
        }
        Ok(())
    }

    /// Call `f` with each entry of the file directory: a big-endian count, then 64-byte
    /// entries `{ size: u32, select: u16, reserved: u16, name: [u8; 56] }`, all big-endian.
    pub fn for_each_file(&self, mut f: impl FnMut(&File)) {
        self.0.select(KEY_FILE_DIR);
        let count = self.read_be_u32();
        for _ in 0..count {
            let size = self.read_be_u32();
            let mut rest = [0; 4];
            self.read(&mut rest);
            let mut file = File {
                name: [0; 56],
                size,
                select: u16::from_be_bytes([rest[0], rest[1]]),
            };
            self.read(&mut file.name);
            f(&file);
        }
    }

    /// The selector and size of the file `name`.
    pub fn find_file(&self, name: &str) -> Option<(u16, u32)> {
        let mut found = None;
        self.for_each_file(|file| {
            if found.is_none() && file.name() == name {
                found = Some((file.select, file.size));
            }
        });
        found
    }

    /// Read the file `name` into `buf`, up to its length. `None` if there's no such file.
    /// QEMU adds files from its command line with `-fw_cfg name=opt/...,string=...`.
    pub fn read_file(&self, name: &str, buf: &mut [u8]) -> Option<usize> {
        let (key, size) = self.find_file(name)?;
        let len = buf.len().min(size as usize);
        self.0.select(key);
        self.read(&mut buf[..len]);
        Some(len)
    }

    /// Write `data` to item `key` with a DMA request, and wait for it to finish.
    pub fn dma_write(&self, key: u16, data: &[u8]) -> Result<(), &'static str> {
        let mut access = DmaAccess {
            control: ((u32::from(key) << 16) | DMA_SELECT | DMA_WRITE).to_be(),
            length: (data.len() as u32).to_be(),
            address: (data.as_ptr() as u64).to_be(),
        };
        let address = &raw mut access as u64;
        dma_fence(); // the request and the data are in memory before QEMU looks
        self.0.start_dma(address);
        // QEMU clears the control field when it's done, leaving only the error bit if it
        // failed.
        loop {
            let control = u32::from_be(unsafe { read_volatile(&raw const access.control) });
            if control & DMA_ERROR != 0 {
                return Err("fw_cfg DMA write failed");
            }
            if control == 0 {
                return Ok(());
            }
        }
    }
}

/// The memory-mapped register layout of riscv64 and aarch64 `virt`: data at +0x00,
/// selector at +0x08, DMA address at +0x10. The registers are big-endian: the bytes in
/// memory order are the big-endian representation, so values are converted with `to_be`
/// before a volatile store.
pub struct Mmio(pub usize);

impl FwCfgPort for Mmio {
    fn select(&self, key: u16) {
        unsafe { write_volatile((self.0 + 0x08) as *mut u16, key.to_be()) };
    }

    fn read_u8(&self) -> u8 {
        unsafe { read_volatile(self.0 as *const u8) }
    }

    /// High half first: writing the low half starts the transfer.
    fn start_dma(&self, address: u64) {
        unsafe {
            write_volatile((self.0 + 0x10) as *mut u32, ((address >> 32) as u32).to_be());
            write_volatile((self.0 + 0x14) as *mut u32, (address as u32).to_be());
        }
    }
}

/// fw_cfg on x86, on I/O ports: selector 0x510, data 0x511, DMA address 0x514.
#[cfg(target_arch = "x86_64")]
pub struct IoPorts;

#[cfg(target_arch = "x86_64")]
impl FwCfgPort for IoPorts {
    fn select(&self, key: u16) {
        unsafe { green_tea_io::port::outw(0x510, key) }; // the port selector is little-endian, unlike MMIO
    }

    fn read_u8(&self) -> u8 {
        unsafe { green_tea_io::port::inb(0x511) }
    }

    /// The DMA address register is big-endian: high half at 0x514, then low half at 0x518,
    /// which starts the transfer.
    fn start_dma(&self, address: u64) {
        unsafe {
            green_tea_io::port::outl(0x514, ((address >> 32) as u32).to_be());
            green_tea_io::port::outl(0x518, (address as u32).to_be());
        }
    }
}
